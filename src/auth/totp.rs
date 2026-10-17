//! Time-based one-time passwords (RFC 6238) and recovery codes.
//!
//! The parameters are the ones every authenticator app understands:
//! HMAC-SHA1, 6 digits, 30-second steps. Google Authenticator ignores (or
//! on some platforms mishandles) other algorithms, digit counts and
//! periods, so they are not configurable.
//!
//! A code is accepted for the current step and one step either side, which
//! absorbs about 30 s of clock drift between phone and server plus the
//! time it takes to type. Each accepted step is remembered and never
//! accepted again, so a code seen over someone's shoulder (or in a proxy
//! log) cannot be replayed.
//!
//! Recovery codes carry 80 random bits each, so a plain SHA-256 is enough
//! to store them: unlike passwords they cannot be guessed from a
//! dictionary, and a slow hash would make checking ten of them per login
//! needlessly expensive.

use hmac::{Hmac, Mac};
use password_hash::rand_core::{OsRng, RngCore};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::password::constant_time_eq;

pub const DIGITS: usize = 6;
pub const PERIOD_SECS: u64 = 30;
/// 160 bits, the HMAC-SHA1 block-size recommendation of RFC 4226.
const SECRET_BYTES: usize = 20;
/// Shortest secret accepted when loading `users.json` (RFC 4226 minimum).
const MIN_SECRET_BYTES: usize = 16;
/// Steps accepted either side of the current one.
const WINDOW: u64 = 1;
pub const RECOVERY_CODES: usize = 10;
/// 10 bytes = 80 bits = 16 base32 characters, shown as four groups.
const RECOVERY_CODE_BYTES: usize = 10;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding, as authenticator apps expect it.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decode base32, ignoring case, spaces and padding. `None` on any other
/// character.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !matches!(c, ' ' | '=')) {
        let value = BASE32
            .iter()
            .position(|&b| b == c.to_ascii_uppercase() as u8 && c.is_ascii())?;
        buffer = (buffer << 5) | value as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// A shared TOTP secret. Never printed: `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct TotpSecret(Vec<u8>);

impl TotpSecret {
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; SECRET_BYTES];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// The key as typed into an authenticator app by hand.
    pub fn to_base32(&self) -> String {
        base32_encode(&self.0)
    }

    /// The HOTP value (RFC 4226) for a counter, as a zero-padded string.
    fn code_at(&self, counter: u64) -> String {
        // HMAC accepts keys of any length, so this cannot fail.
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&self.0) else {
            return String::new();
        };
        mac.update(&counter.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = usize::from(digest[digest.len() - 1] & 0x0f);
        let binary = u32::from_be_bytes([
            digest[offset] & 0x7f,
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]);
        format!(
            "{:0width$}",
            binary % 10u32.pow(DIGITS as u32),
            width = DIGITS
        )
    }

    /// The step a valid `code` belongs to, if it matches one within the
    /// window around `now_secs` that is newer than `last_used_step`.
    ///
    /// Every candidate step is computed and compared in constant time, so
    /// the response time does not reveal which step (if any) matched.
    pub fn verify(&self, code: &str, now_secs: u64, last_used_step: u64) -> Option<u64> {
        let code = normalize_code(code)?;
        let current = now_secs / PERIOD_SECS;
        let mut matched = None;
        for step in current.saturating_sub(WINDOW)..=current.saturating_add(WINDOW) {
            let ok = constant_time_eq(self.code_at(step).as_bytes(), code.as_bytes());
            if ok && step > last_used_step && matched.is_none() {
                matched = Some(step);
            }
        }
        matched
    }

    /// `otpauth://` URI for the QR code (Key Uri Format, as defined by
    /// Google Authenticator and followed by every other app).
    pub fn provisioning_uri(&self, issuer: &str, account: &str) -> String {
        let issuer = percent_encode(issuer);
        format!(
            "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits={DIGITS}&period={PERIOD_SECS}",
            account = percent_encode(account),
            secret = self.to_base32(),
        )
    }
}

impl std::fmt::Debug for TotpSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TotpSecret(<redacted>)")
    }
}

impl Serialize for TotpSecret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_base32())
    }
}

impl<'de> Deserialize<'de> for TotpSecret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match base32_decode(&text) {
            Some(bytes) if bytes.len() >= MIN_SECRET_BYTES => Ok(Self(bytes)),
            _ => Err(serde::de::Error::custom("invalid TOTP secret")),
        }
    }
}

/// The code an authenticator app shows at `now_secs` for a base32 key.
#[cfg(test)]
pub fn code_for(secret_base32: &str, now_secs: u64) -> String {
    let secret = TotpSecret(base32_decode(secret_base32).unwrap_or_default());
    secret.code_at(now_secs / PERIOD_SECS)
}

/// Exactly six digits, after dropping the spaces and dashes some apps show
/// in the middle (`123 456`).
fn normalize_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (code.len() == DIGITS && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A QR code of `text` as a standalone SVG document: black modules on a
/// white background with the 4-module quiet zone scanners need, whatever
/// the UI theme is.
pub fn qr_svg(text: &str) -> Option<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    const BORDER: i32 = 4;
    let size = qr.size();
    let dim = size + BORDER * 2;
    let mut path = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + BORDER, y + BORDER));
            }
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" shape-rendering=\"crispEdges\">\
         <rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/><path d=\"{path}\" fill=\"#000000\"/></svg>"
    ))
}

/// A user's second factor as stored in `users.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotpState {
    pub secret: TotpSecret,
    /// When it was turned on (Unix seconds).
    pub enabled_at: u64,
    /// Newest time step already used to log in; older or equal steps are
    /// refused so a code works once.
    #[serde(default)]
    pub last_step: u64,
    /// SHA-256 (hex) of each unused recovery code.
    #[serde(default)]
    pub recovery_codes: Vec<String>,
}

/// Which kind of second factor was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondFactor {
    Totp,
    RecoveryCode { remaining: usize },
}

impl TotpState {
    /// Check an authenticator code or a recovery code, and record its use:
    /// the TOTP step can't be used again and a recovery code is gone. The
    /// caller persists the change (or restores a copy if it can't).
    pub fn accept(&mut self, input: &str, now_secs: u64) -> Option<SecondFactor> {
        if normalize_code(input).is_some() {
            let step = self.secret.verify(input, now_secs, self.last_step)?;
            self.last_step = step;
            return Some(SecondFactor::Totp);
        }
        let hash = hash_recovery_code(&normalize_recovery_code(input)?);
        // Compare against every stored hash so timing doesn't reveal the
        // position of a match.
        let mut found = None;
        for (i, stored) in self.recovery_codes.iter().enumerate() {
            if constant_time_eq(stored.as_bytes(), hash.as_bytes()) && found.is_none() {
                found = Some(i);
            }
        }
        self.recovery_codes.remove(found?);
        Some(SecondFactor::RecoveryCode {
            remaining: self.recovery_codes.len(),
        })
    }
}

/// Fresh recovery codes: the codes to show the user once, and the hashes
/// to store.
pub fn generate_recovery_codes() -> (Vec<String>, Vec<String>) {
    (0..RECOVERY_CODES)
        .map(|_| {
            let mut bytes = [0u8; RECOVERY_CODE_BYTES];
            OsRng.fill_bytes(&mut bytes);
            let raw = base32_encode(&bytes);
            let shown = raw
                .as_bytes()
                .chunks(4)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
                .join("-");
            let hash = hash_recovery_code(&raw);
            (shown, hash)
        })
        .unzip()
}

/// Uppercase base32 without separators. Digits that base32 never uses are
/// read as the letters they are mistaken for when copied from paper.
fn normalize_recovery_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            other => other,
        })
        .collect();
    let expected_len = (RECOVERY_CODE_BYTES * 8).div_ceil(5);
    (code.len() == expected_len && code.bytes().all(|b| BASE32.contains(&b))).then_some(code)
}

fn hash_recovery_code(normalized: &str) -> String {
    Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 6238 appendix B secret for SHA-1.
    fn rfc_secret() -> TotpSecret {
        TotpSecret(b"12345678901234567890".to_vec())
    }

    #[test]
    fn matches_rfc_6238_test_vectors() {
        // Appendix B lists 8-digit values; 6-digit codes are their last six.
        let secret = rfc_secret();
        for (time, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(secret.code_at(time / PERIOD_SECS), expected, "t={time}");
            assert!(secret.verify(expected, time, 0).is_some(), "t={time}");
        }
    }

    #[test]
    fn accepts_neighbouring_steps_only() {
        let secret = rfc_secret();
        let now = 1_234_567_890;
        let step = now / PERIOD_SECS;
        for offset in [-1i64, 0, 1] {
            let code = secret.code_at(step.wrapping_add_signed(offset));
            assert_eq!(
                secret.verify(&code, now, 0),
                Some(step.wrapping_add_signed(offset))
            );
        }
        for offset in [-3i64, -2, 2, 3] {
            let code = secret.code_at(step.wrapping_add_signed(offset));
            assert_eq!(secret.verify(&code, now, 0), None, "offset {offset}");
        }
    }

    #[test]
    fn refuses_replays_and_malformed_codes() {
        let secret = rfc_secret();
        let now = 1_234_567_890;
        let step = now / PERIOD_SECS;
        let code = secret.code_at(step);
        assert_eq!(secret.verify(&code, now, step), None);
        assert_eq!(secret.verify(&code, now, step - 1), Some(step));
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(secret.verify(&spaced, now, 0), Some(step));
        for bad in ["", "12345", "1234567", "abcdef", "１２３４５６", "12345\n"] {
            assert_eq!(secret.verify(bad, now, 0), None, "{bad:?}");
        }
    }

    #[test]
    fn state_accepts_each_factor_once() {
        let (codes, hashes) = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(
            codes
                .iter()
                .all(|c| c.len() == 19 && c.matches('-').count() == 3)
        );
        let mut state = TotpState {
            secret: rfc_secret(),
            enabled_at: 0,
            last_step: 0,
            recovery_codes: hashes,
        };
        let now = 1_234_567_890;
        let code = state.secret.code_at(now / PERIOD_SECS);
        assert_eq!(state.accept(&code, now), Some(SecondFactor::Totp));
        assert_eq!(state.accept(&code, now), None);

        // Recovery codes: case and separators don't matter; each works once.
        let typed = codes[3].to_lowercase().replace('-', " ");
        assert_eq!(
            state.accept(&typed, now),
            Some(SecondFactor::RecoveryCode { remaining: 9 })
        );
        assert_eq!(state.accept(&codes[3], now), None);
        assert_eq!(state.accept("AAAA-AAAA-AAAA-AAAA", now), None);
        assert_eq!(state.accept("not a code", now), None);
    }

    #[test]
    fn recovery_codes_forgive_lookalike_digits() {
        assert_eq!(
            normalize_recovery_code("o0i1-b8aa-aaaa-aaaa").as_deref(),
            Some("OOIIBBAAAAAAAAAA")
        );
        assert_eq!(normalize_recovery_code("AAAA-AAAA-AAAA-AAA9"), None);
        assert_eq!(normalize_recovery_code("AAAA"), None);
    }

    #[test]
    fn base32_round_trips() {
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(
            base32_encode(b"12345678901234567890"),
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
        assert_eq!(base32_decode("mzxw 6ytb oi==").unwrap(), b"foobar");
        assert_eq!(base32_decode("MZ1"), None);
        let secret = TotpSecret::generate();
        assert_eq!(base32_decode(&secret.to_base32()).unwrap(), secret.0);
        assert_eq!(secret.to_base32().len(), 32);
    }

    #[test]
    fn secret_serializes_as_base32_and_rejects_garbage() {
        let json = serde_json::to_string(&rfc_secret()).unwrap();
        assert_eq!(json, "\"GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\"");
        assert_eq!(
            serde_json::from_str::<TotpSecret>(&json).unwrap(),
            rfc_secret()
        );
        assert!(serde_json::from_str::<TotpSecret>("\"MZXW6\"").is_err());
        assert!(serde_json::from_str::<TotpSecret>("\"!!!\"").is_err());
        assert_eq!(format!("{:?}", rfc_secret()), "TotpSecret(<redacted>)");
    }

    #[test]
    fn provisioning_uri_and_qr() {
        let uri = rfc_secret().provisioning_uri("Inkult (home)", "jack");
        assert_eq!(
            uri,
            "otpauth://totp/Inkult%20%28home%29:jack?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\
             &issuer=Inkult%20%28home%29&algorithm=SHA1&digits=6&period=30"
        );
        let svg = qr_svg(&uri).unwrap();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains("fill=\"#ffffff\""));
    }
}
