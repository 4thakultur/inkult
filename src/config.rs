//! Configuration from environment variables.
//!
//! | Variable                           | Default          |
//! |------------------------------------|------------------|
//! | `INKULT_BIND`                      | `127.0.0.1:9876` |
//! | `INKULT_LIBRARY_DIR`               | `./music`        |
//! | `INKULT_DATA_DIR`                  | `./data`         |
//! | `INKULT_STAGING_DIR`               | `<data>/staging` |
//! | `INKULT_MAX_CONCURRENT_DOWNLOADS`  | `2` (1–16)       |
//! | `INKULT_MAX_QUEUED_JOBS`           | `1000` (1–10000) |
//! | `INKULT_MAX_RETRIES`               | `3` (0–10)       |
//! | `INKULT_SEARCH_LIMIT`              | `20` (1–50)      |
//! | `INKULT_PLAYLIST_LIMIT`            | `500` (1–5000)   |
//! | `INKULT_MAX_TRACK_SECS`            | `600` (0–86400), first start only |
//! | `INKULT_YTDLP`                     | `yt-dlp`         |
//! | `INKULT_FFMPEG`                    | `ffmpeg`         |
//! | `INKULT_ALLOWED_HOSTS`             | (none)           |
//! | `INKULT_SOULSEEK_SERVER`           | `server.slsknet.org:2242` |
//! | `INKULT_SOULSEEK_PORTS`            | `2234-2238`      |
//! | `INKULT_SOULSEEK_UPLOAD_SLOTS`     | `2` (1–20)       |
//! | `INKULT_SOULSEEK_UPGRADE_WAIT`     | `60` s (30–3600) |
//! | `INKULT_TOTP_ISSUER`               | `Inkult`, first start only |
//! | `INKULT_TRUSTED_PROXIES`           | (none)           |
//!
//! `INKULT_ALLOWED_HOSTS` is a comma-separated list of the host names the
//! web UI is reached by (e.g. `music.example.com`), needed when Inkult is
//! accessed by a domain name, typically through a reverse proxy. IP
//! addresses and `localhost` are always accepted. Requests naming any other
//! host are refused, which defeats DNS-rebinding attacks: a malicious web
//! page cannot point its own domain at Inkult and script it from a visitor's
//! browser. `*` turns the check off.
//!
//! `INKULT_MAX_TRACK_SECS` and `INKULT_TOTP_ISSUER` only set the initial
//! value of the matching admin-panel setting (see `settings`): they are
//! read once, when `settings.json` does not exist yet, and from then on
//! captains change them in the UI. `INKULT_MAX_TRACK_SECS` is the longest
//! track accepted, in seconds (default 10:00, `0` = no limit); longer
//! uploads are almost always mixes or sets. `INKULT_TOTP_ISSUER` is the
//! name authenticator apps show for this server, e.g. `Inkult (home)`.
//! Who must use two-factor login is only set in the admin panel (it is
//! required for everyone until a captain changes it); the retired
//! `INKULT_REQUIRE_2FA` is ignored with a warning.
//!
//! `INKULT_SOULSEEK_PORTS` lists the ports Soulseek peers can connect to,
//! one per signed-in Soulseek account (`2234-2238`, `2234,2240` or `off`).
//! Forward them on your router for the best results; without an open port,
//! Inkult still works but cannot reach users who are behind a firewall too.
//!
//! `INKULT_TRUSTED_PROXIES` lists the reverse proxies in front of Inkult
//! (addresses or CIDR ranges, e.g. `172.18.0.0/16`). Only connections from
//! them may name the real client in `X-Forwarded-For`; anyone else could
//! forge it. Sign-in limits and connection limits are per client, so behind
//! an unlisted proxy every visitor counts as the same client.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::settings::{MAX_TRACK_SECS_LIMIT, ServerSettings, validate_issuer};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var}: invalid value '{value}': {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
    #[error("{var}: directory {path} is not usable: {source}")]
    Directory {
        var: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub library_dir: PathBuf,
    pub data_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub max_concurrent_downloads: usize,
    pub max_queued_jobs: usize,
    pub max_retries: u32,
    pub search_limit: usize,
    pub playlist_limit: usize,
    pub ytdlp_program: String,
    pub ffmpeg_program: String,
    /// Lowercase host names (no port) accepted in the `Host` header, besides
    /// IP addresses and `localhost`. `["*"]` accepts any.
    pub allowed_hosts: Vec<String>,
    /// `host:port` of the Soulseek server.
    pub soulseek_server: String,
    /// Listening ports for Soulseek sessions (empty: none listen).
    pub soulseek_ports: Vec<u16>,
    pub soulseek_upload_slots: usize,
    /// How long a quality upgrade waits for a Soulseek upload to start
    /// before trying another copy.
    pub soulseek_upgrade_wait: Duration,
    /// Admin-panel settings used when `settings.json` does not exist yet.
    pub initial_settings: ServerSettings,
    /// Set variables that only seed [`Self::initial_settings`].
    pub initial_settings_vars: Vec<&'static str>,
    /// Set variables Inkult no longer reads (warned about at startup).
    pub retired_vars: Vec<&'static str>,
    /// Reverse proxies allowed to report the client address.
    pub trusted_proxies: Vec<IpNet>,
}

/// An address range (`10.0.0.0/8`, `fd00::/8`) or one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for IpNet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .trim()
            .parse::<IpAddr>()
            .map_err(|_| format!("'{s}' is not an IP address or CIDR range"))?
            .to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("'{s}': prefix length must be 0–{max}"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build a configuration from any key lookup (used for testing).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |var: &'static str| lookup(var).filter(|v| !v.trim().is_empty());
        let data_dir: PathBuf = get("INKULT_DATA_DIR")
            .unwrap_or_else(|| "./data".into())
            .into();
        Ok(Self {
            bind: parse(get("INKULT_BIND"), "INKULT_BIND", "127.0.0.1:9876")?,
            library_dir: get("INKULT_LIBRARY_DIR")
                .unwrap_or_else(|| "./music".into())
                .into(),
            staging_dir: get("INKULT_STAGING_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("staging")),
            data_dir,
            max_concurrent_downloads: ranged(
                get("INKULT_MAX_CONCURRENT_DOWNLOADS"),
                "INKULT_MAX_CONCURRENT_DOWNLOADS",
                2,
                1..=16,
            )?,
            max_queued_jobs: ranged(
                get("INKULT_MAX_QUEUED_JOBS"),
                "INKULT_MAX_QUEUED_JOBS",
                1000,
                1..=10_000,
            )?,
            max_retries: ranged(get("INKULT_MAX_RETRIES"), "INKULT_MAX_RETRIES", 3, 0..=10)?,
            search_limit: ranged(
                get("INKULT_SEARCH_LIMIT"),
                "INKULT_SEARCH_LIMIT",
                20,
                1..=50,
            )?,
            playlist_limit: ranged(
                get("INKULT_PLAYLIST_LIMIT"),
                "INKULT_PLAYLIST_LIMIT",
                500,
                1..=5000,
            )?,
            ytdlp_program: get("INKULT_YTDLP").unwrap_or_else(|| "yt-dlp".into()),
            ffmpeg_program: get("INKULT_FFMPEG").unwrap_or_else(|| "ffmpeg".into()),
            allowed_hosts: allowed_hosts(get("INKULT_ALLOWED_HOSTS"))?,
            soulseek_server: soulseek_server(get("INKULT_SOULSEEK_SERVER"))?,
            soulseek_ports: port_list(get("INKULT_SOULSEEK_PORTS"))?,
            soulseek_upload_slots: ranged(
                get("INKULT_SOULSEEK_UPLOAD_SLOTS"),
                "INKULT_SOULSEEK_UPLOAD_SLOTS",
                2,
                1..=20,
            )?,
            soulseek_upgrade_wait: Duration::from_secs(ranged(
                get("INKULT_SOULSEEK_UPGRADE_WAIT"),
                "INKULT_SOULSEEK_UPGRADE_WAIT",
                crate::soulseek::DEFAULT_UPGRADE_START_TIMEOUT.as_secs(),
                30..=3600,
            )?),
            initial_settings: initial_settings(&get)?,
            initial_settings_vars: ["INKULT_MAX_TRACK_SECS", "INKULT_TOTP_ISSUER"]
                .into_iter()
                .filter(|var| get(var).is_some())
                .collect(),
            retired_vars: ["INKULT_REQUIRE_2FA"]
                .into_iter()
                .filter(|var| get(var).is_some())
                .collect(),
            trusted_proxies: trusted_proxies(get("INKULT_TRUSTED_PROXIES"))?,
        })
    }

    /// Server settings changed from the admin panel.
    pub fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("library.json")
    }

    /// Accounts and password hashes (owner-readable only).
    pub fn users_path(&self) -> PathBuf {
        self.data_dir.join("users.json")
    }

    /// Soulseek accounts, settings and saved queues (owner-readable only).
    pub fn soulseek_path(&self) -> PathBuf {
        self.data_dir.join("soulseek.json")
    }

    /// Chat rooms, one file per room (owner-readable only).
    pub fn rooms_dir(&self) -> PathBuf {
        self.data_dir.join("rooms")
    }

    /// Create the configured directories and verify they are writable.
    pub fn prepare_directories(&self) -> Result<(), ConfigError> {
        for (var, path) in [
            ("INKULT_LIBRARY_DIR", &self.library_dir),
            ("INKULT_DATA_DIR", &self.data_dir),
            ("INKULT_STAGING_DIR", &self.staging_dir),
        ] {
            let check = || -> std::io::Result<()> {
                std::fs::create_dir_all(path)?;
                let probe = path.join(".inkult-write-test");
                std::fs::write(&probe, b"ok")?;
                std::fs::remove_file(&probe)
            };
            check().map_err(|source| ConfigError::Directory {
                var,
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

fn allowed_hosts(raw: Option<String>) -> Result<Vec<String>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|host| {
            let valid = host == "*"
                || host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                });
            if valid {
                Ok(host.to_ascii_lowercase())
            } else {
                Err(ConfigError::Invalid {
                    var: "INKULT_ALLOWED_HOSTS",
                    value: raw.clone(),
                    reason: format!(
                        "'{host}' is not a host name (give names only, without scheme or port)"
                    ),
                })
            }
        })
        .collect()
}

fn trusted_proxies(raw: Option<String>) -> Result<Vec<IpNet>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse().map_err(|reason| ConfigError::Invalid {
                var: "INKULT_TRUSTED_PROXIES",
                value: raw.clone(),
                reason,
            })
        })
        .collect()
}

fn initial_settings(
    get: &impl Fn(&'static str) -> Option<String>,
) -> Result<ServerSettings, ConfigError> {
    let defaults = ServerSettings::default();
    let totp_issuer = match get("INKULT_TOTP_ISSUER") {
        Some(raw) => {
            let value = raw.trim().to_owned();
            validate_issuer(&value).map_err(|reason| ConfigError::Invalid {
                var: "INKULT_TOTP_ISSUER",
                value: raw,
                reason,
            })?;
            value
        }
        None => defaults.totp_issuer,
    };
    Ok(ServerSettings {
        totp_issuer,
        max_track_secs: ranged(
            get("INKULT_MAX_TRACK_SECS"),
            "INKULT_MAX_TRACK_SECS",
            defaults.max_track_secs,
            0..=MAX_TRACK_SECS_LIMIT,
        )?,
        ..defaults
    })
}

fn soulseek_server(raw: Option<String>) -> Result<String, ConfigError> {
    let value = raw.unwrap_or_else(|| "server.slsknet.org:2242".into());
    let value = value.trim().to_owned();
    let valid = value.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            && port.parse::<u16>().is_ok_and(|p| p > 0)
    });
    if valid {
        Ok(value)
    } else {
        Err(ConfigError::Invalid {
            var: "INKULT_SOULSEEK_SERVER",
            value,
            reason: "expected host:port, e.g. server.slsknet.org:2242".into(),
        })
    }
}

/// `2234-2238`, `2234,2236` or `off`.
fn port_list(raw: Option<String>) -> Result<Vec<u16>, ConfigError> {
    let value = raw.unwrap_or_else(|| "2234-2238".into());
    if value.trim().eq_ignore_ascii_case("off") {
        return Ok(Vec::new());
    }
    let invalid = |reason: &str| ConfigError::Invalid {
        var: "INKULT_SOULSEEK_PORTS",
        value: value.clone(),
        reason: reason.to_owned(),
    };
    let port = |s: &str| {
        s.trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p >= 1024)
            .ok_or_else(|| invalid("ports must be numbers from 1024 to 65535"))
    };
    let mut ports = Vec::new();
    for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((start, end)) => {
                let (start, end) = (port(start)?, port(end)?);
                if start > end || end - start >= 64 {
                    return Err(invalid("a range must be ascending and at most 64 ports"));
                }
                ports.extend(start..=end);
            }
            None => ports.push(port(part)?),
        }
    }
    let mut seen = std::collections::HashSet::new();
    ports.retain(|p| seen.insert(*p));
    if ports.is_empty() {
        return Err(invalid("give at least one port, or 'off'"));
    }
    Ok(ports)
}

fn parse<T: FromStr>(
    raw: Option<String>,
    var: &'static str,
    default: &str,
) -> Result<T, ConfigError>
where
    T::Err: std::fmt::Display,
{
    let value = raw.unwrap_or_else(|| default.to_owned());
    value
        .trim()
        .parse()
        .map_err(|e: T::Err| ConfigError::Invalid {
            var,
            value: value.clone(),
            reason: e.to_string(),
        })
}

fn ranged<T>(
    raw: Option<String>,
    var: &'static str,
    default: T,
    range: std::ops::RangeInclusive<T>,
) -> Result<T, ConfigError>
where
    T: FromStr + PartialOrd + std::fmt::Display + Copy,
    T::Err: std::fmt::Display,
{
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value: T = parse(Some(raw.clone()), var, "")?;
    if !range.contains(&value) {
        return Err(ConfigError::Invalid {
            var,
            value: raw,
            reason: format!("must be between {} and {}", range.start(), range.end()),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_are_valid() {
        let c = config(&[]).unwrap();
        assert_eq!(c.bind.to_string(), "127.0.0.1:9876");
        assert_eq!(c.max_concurrent_downloads, 2);
        assert_eq!(c.soulseek_upgrade_wait, Duration::from_secs(60));
        assert_eq!(c.staging_dir, PathBuf::from("./data/staging"));
        assert_eq!(c.initial_settings, ServerSettings::default());
        assert!(c.initial_settings_vars.is_empty() && c.retired_vars.is_empty());
    }

    #[test]
    fn upgrade_wait_is_bounded() {
        let c = config(&[("INKULT_SOULSEEK_UPGRADE_WAIT", "600")]).unwrap();
        assert_eq!(c.soulseek_upgrade_wait, Duration::from_secs(600));
        assert!(config(&[("INKULT_SOULSEEK_UPGRADE_WAIT", "5")]).is_err());
        assert!(config(&[("INKULT_SOULSEEK_UPGRADE_WAIT", "3601")]).is_err());
    }

    #[test]
    fn parses_initial_settings() {
        let c = config(&[("INKULT_MAX_TRACK_SECS", "0")]).unwrap();
        assert_eq!(c.initial_settings.max_track_secs, 0);
        assert_eq!(c.initial_settings_vars, ["INKULT_MAX_TRACK_SECS"]);
        let c = config(&[("INKULT_MAX_TRACK_SECS", "900")]).unwrap();
        assert_eq!(c.initial_settings.max_track_secs, 900);
        assert!(config(&[("INKULT_MAX_TRACK_SECS", "-1")]).is_err());
        assert!(config(&[("INKULT_MAX_TRACK_SECS", "86401")]).is_err());
        assert_eq!(
            config(&[("INKULT_TOTP_ISSUER", " Inkult (home) ")])
                .unwrap()
                .initial_settings
                .totp_issuer,
            "Inkult (home)"
        );
        assert!(config(&[("INKULT_TOTP_ISSUER", "a:b")]).is_err());
        assert!(config(&[("INKULT_TOTP_ISSUER", &"x".repeat(65))]).is_err());
        // The 2FA policy is not read from the environment any more.
        let c = config(&[("INKULT_REQUIRE_2FA", "off")]).unwrap();
        assert_eq!(c.retired_vars, ["INKULT_REQUIRE_2FA"]);
        assert_eq!(c.initial_settings, ServerSettings::default());
    }

    #[test]
    fn rejects_out_of_range_and_garbage() {
        assert!(config(&[("INKULT_MAX_CONCURRENT_DOWNLOADS", "0")]).is_err());
        assert!(config(&[("INKULT_MAX_CONCURRENT_DOWNLOADS", "lots")]).is_err());
        assert!(config(&[("INKULT_BIND", "nope")]).is_err());
        assert_eq!(
            config(&[("INKULT_MAX_RETRIES", "5")]).unwrap().max_retries,
            5
        );
    }

    #[test]
    fn parses_allowed_hosts() {
        assert!(config(&[]).unwrap().allowed_hosts.is_empty());
        assert_eq!(
            config(&[("INKULT_ALLOWED_HOSTS", " Music.Example.com, nas.lan ,")])
                .unwrap()
                .allowed_hosts,
            ["music.example.com", "nas.lan"]
        );
        assert!(config(&[("INKULT_ALLOWED_HOSTS", "https://music.example.com")]).is_err());
        assert!(config(&[("INKULT_ALLOWED_HOSTS", "music.example.com:443")]).is_err());
    }

    #[test]
    fn parses_soulseek_settings() {
        let c = config(&[]).unwrap();
        assert_eq!(c.soulseek_server, "server.slsknet.org:2242");
        assert_eq!(c.soulseek_ports, [2234, 2235, 2236, 2237, 2238]);
        assert_eq!(
            config(&[("INKULT_SOULSEEK_PORTS", "2234, 2240-2241,2234")])
                .unwrap()
                .soulseek_ports,
            [2234, 2240, 2241]
        );
        assert!(
            config(&[("INKULT_SOULSEEK_PORTS", "off")])
                .unwrap()
                .soulseek_ports
                .is_empty()
        );
        for bad in ["80", "3000-2000", "1024-9000", "x", ","] {
            assert!(config(&[("INKULT_SOULSEEK_PORTS", bad)]).is_err(), "{bad}");
        }
        assert!(config(&[("INKULT_SOULSEEK_SERVER", "localhost:2242")]).is_ok());
        assert!(config(&[("INKULT_SOULSEEK_SERVER", "http://x:1")]).is_err());
        assert!(config(&[("INKULT_SOULSEEK_SERVER", "nohost")]).is_err());
    }

    #[test]
    fn parses_trusted_proxies() {
        assert!(config(&[]).unwrap().trusted_proxies.is_empty());
        let c = config(&[(
            "INKULT_TRUSTED_PROXIES",
            "172.18.0.0/16, 10.0.0.7 ,fd00::/8,::ffff:192.168.1.2",
        )])
        .unwrap();
        let has = |ip: &str| {
            c.trusted_proxies
                .iter()
                .any(|n| n.contains(ip.parse().unwrap()))
        };
        assert!(has("172.18.4.2") && has("10.0.0.7") && has("fd12::1"));
        assert!(has("192.168.1.2") && has("::ffff:172.18.0.9"));
        assert!(!has("172.19.0.1") && !has("10.0.0.8") && !has("fe80::1"));
        assert!(
            "0.0.0.0/0"
                .parse::<IpNet>()
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        for bad in ["proxy.lan", "10.0.0.0/33", "::/129", "10.0.0.1/x"] {
            assert!(config(&[("INKULT_TRUSTED_PROXIES", bad)]).is_err(), "{bad}");
        }
    }

    #[test]
    fn prepares_writable_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("music");
        let data = tmp.path().join("data");
        let c = config(&[
            ("INKULT_LIBRARY_DIR", lib.to_str().unwrap()),
            ("INKULT_DATA_DIR", data.to_str().unwrap()),
        ])
        .unwrap();
        c.prepare_directories().unwrap();
        assert!(lib.is_dir() && data.join("staging").is_dir());
    }
}
