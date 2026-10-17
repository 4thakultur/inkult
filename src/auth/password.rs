//! Password hashing (Argon2id, PHC string format) and random tokens.

use argon2::Argon2;
use password_hash::rand_core::{OsRng, RngCore};
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

/// Accepted password length in characters. The upper bound keeps a single
/// login request from making the server hash megabytes of input.
pub const MIN_PASSWORD_CHARS: usize = 12;
pub const MAX_PASSWORD_CHARS: usize = 128;

/// Minimum zxcvbn score: the top one, at least 10^10 estimated guesses.
/// Length alone lets through `aaaaaaaaaaaa` or `password1234`; zxcvbn also
/// catches common passwords, dictionary words, keyboard walks, repeats,
/// sequences, dates and l33t substitutions. Score 3 still admitted names
/// glued together (`inkult-inkult`, about 10^8 guesses), which would not
/// survive offline cracking if `users.json` ever leaked. Three unrelated
/// words or 12 random characters reach score 4 easily.
const MIN_STRENGTH: zxcvbn::Score = zxcvbn::Score::Four;

/// Check a new password against the policy; `Err` explains the problem.
/// `username` counts as a guessable word, so it cannot carry the password.
pub fn check_policy(password: &str, username: &str) -> Result<(), String> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        ));
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(format!(
            "password must be at most {MAX_PASSWORD_CHARS} characters"
        ));
    }
    if password.chars().any(char::is_control) {
        return Err("password must not contain control characters".into());
    }
    let estimate = zxcvbn::zxcvbn(password, &[username, "inkult", "captain", "pirate"]);
    if estimate.score() < MIN_STRENGTH {
        // zxcvbn's sentences end with a period already.
        let mut message = String::from("password is too easy to guess.");
        if let Some(feedback) = estimate.feedback() {
            if let Some(warning) = feedback.warning() {
                message.push_str(&format!(" {warning}"));
            }
            if let Some(tip) = feedback.suggestions().first() {
                message.push_str(&format!(" {tip}"));
            }
        }
        message.push_str(" A few unrelated words make a strong, memorable password.");
        return Err(message);
    }
    Ok(())
}

/// Hash with Argon2id and the crate's recommended parameters
/// (19 MiB, 2 passes). CPU- and memory-heavy: call from a blocking task.
pub fn hash(password: &str) -> Result<String, password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// Verify a password against a stored PHC hash string. An unparsable hash
/// verifies nothing. Blocking, like [`hash`].
pub fn verify(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Characters of generated passwords: lowercase letters and digits minus
/// the look-alikes (`0 o 1 l i`), so a password read aloud or copied from
/// a note comes through intact.
const GENERATED_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const GENERATED_GROUPS: usize = 5;
const GENERATED_GROUP_LEN: usize = 4;

/// A random password for an account a captain creates or resets: 20
/// characters from a 31-character alphabet (about 99 bits), in dashed
/// groups of four (`k7mq-3xtz-…`) that are easy to pass on. It is
/// temporary: the user must replace it at their first login.
pub fn generate() -> String {
    let mut groups = Vec::with_capacity(GENERATED_GROUPS);
    for _ in 0..GENERATED_GROUPS {
        let mut group = String::with_capacity(GENERATED_GROUP_LEN);
        while group.len() < GENERATED_GROUP_LEN {
            // Rejection sampling: 248 is the largest multiple of the
            // alphabet size below 256, so every character is equally likely.
            let mut byte = [0u8; 1];
            OsRng.fill_bytes(&mut byte);
            if usize::from(byte[0]) < 256 - 256 % GENERATED_ALPHABET.len() {
                group.push(
                    GENERATED_ALPHABET[usize::from(byte[0]) % GENERATED_ALPHABET.len()] as char,
                );
            }
        }
        groups.push(group);
    }
    groups.join("-")
}

/// `bytes` of OS randomness, hex encoded.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random integer below 2^52, so it survives a round trip through
/// JavaScript numbers in the UI.
pub fn random_id() -> u64 {
    OsRng.next_u64() >> 12
}

/// Equality whose running time does not depend on where inputs differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_are_salted() {
        let a = hash("correct horse battery").unwrap();
        let b = hash("correct horse battery").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("$argon2id$"));
        assert!(verify("correct horse battery", &a));
        assert!(!verify("wrong horse battery", &a));
        assert!(!verify("anything", "not a hash"));
    }

    #[test]
    fn policy() {
        assert!(check_policy("short", "jack").is_err());
        assert!(check_policy(&"x".repeat(129), "jack").is_err());
        assert!(check_policy("tidal raven\nlamp", "jack").is_err());
        assert!(check_policy("tidal raven copper lamp", "jack").is_ok());
        // Weak passwords that a length-only rule accepted.
        for weak in [
            "aaaaaaaaaaaa",
            "123456789012",
            "password1234",
            "qwertyuiop12",
            "Password123!",
            "P@ssw0rd2024",
            "inkult-inkult",
            "abcdefghijkl",
            "blackbeard1234",
            "blackbeard-blackbeard",
            "captain-pirate",
            "Inkult2026!!",
        ] {
            assert!(check_policy(weak, "blackbeard").is_err(), "{weak}");
        }
        let message = check_policy("password1234", "jack").unwrap_err();
        assert!(
            message.starts_with("password is too easy to guess"),
            "{message}"
        );
    }

    #[test]
    fn policy_is_fast_at_max_length() {
        // zxcvbn's matching grows with length; the cap keeps it cheap.
        let long = "Zq9!".repeat(MAX_PASSWORD_CHARS / 4);
        let started = std::time::Instant::now();
        let _ = check_policy(&long, "jack");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn generated_passwords_are_strong_and_readable() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let p = generate();
            assert_eq!(p.len(), 24, "{p}");
            assert_eq!(p.split('-').count(), 5, "{p}");
            assert!(
                p.bytes()
                    .all(|b| b == b'-' || GENERATED_ALPHABET.contains(&b)),
                "{p}"
            );
            assert!(!p.contains(['0', 'o', '1', 'l', 'i']), "{p}");
            assert!(check_policy(&p, "jack").is_ok(), "{p}");
            assert!(seen.insert(p));
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(random_hex(32).len(), 64);
        assert!(random_id() < 1 << 52);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
