//! Server-wide rules that captains change from the admin panel
//! (`settings.json` in the data directory).
//!
//! These are policy decisions, not deployment details: who must use
//! two-factor login, the name authenticator apps show, the longest
//! track accepted, and who may use Soulseek and share on it. Deployment settings (paths, ports, limits on the
//! machine's resources) stay environment variables.
//!
//! Like the users file, an unreadable settings file stops startup instead
//! of being replaced: silently falling back to defaults could turn
//! mandatory 2FA off without anyone choosing to.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tokio::sync::{Mutex, MutexGuard};

use crate::auth::{
    AuthConfig, DEFAULT_TOTP_ISSUER, MAX_TOTP_ISSUER_CHARS, Role, TwoFactorPolicy, UserId,
};
use crate::domain::length::{MaxTrackLength, TrackLengthLimit};
use crate::soulseek::{MemberRules, SoulseekAccess, SoulseekRules};
use crate::storage::{Visibility, write_atomic};

const SETTINGS_VERSION: u32 = 1;
/// Upper bound for the track-length limit (a day; `0` = no limit).
pub const MAX_TRACK_SECS_LIMIT: u32 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSettings {
    /// Who must use two-factor login. Required for everyone unless a
    /// captain decides otherwise.
    #[serde(default = "default_two_factor")]
    pub two_factor: TwoFactorPolicy,
    /// Name authenticator apps show next to the code.
    #[serde(default = "default_issuer")]
    pub totp_issuer: String,
    /// Longest track accepted, in seconds; `0` = no limit.
    #[serde(default = "default_max_track_secs")]
    pub max_track_secs: u32,
    /// Who may use Soulseek.
    #[serde(default = "default_soulseek")]
    pub soulseek: SoulseekAccess,
    /// Who may share their library on Soulseek. Nobody unless a captain
    /// allows it: shared files are uploaded from the host's address, so
    /// the host decides (users then still opt in one by one). Older files
    /// stored a yes/no here.
    #[serde(
        default = "default_soulseek_sharing",
        deserialize_with = "SoulseekAccess::deserialize_or_bool"
    )]
    pub soulseek_sharing: SoulseekAccess,
    /// Captains' choices for single members over the two switches above
    /// (only members with a choice other than "follow the switch").
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "member_list"
    )]
    pub soulseek_members: BTreeMap<UserId, MemberRules>,
}

/// The member choices are stored as a list: map keys that are numbers do
/// not survive the flattening of the settings file (`SettingsFile`).
mod member_list {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::auth::UserId;
    use crate::soulseek::MemberRules;

    #[derive(Serialize, Deserialize)]
    struct Entry {
        user: UserId,
        #[serde(flatten)]
        rules: MemberRules,
    }

    pub fn serialize<S: Serializer>(
        members: &BTreeMap<UserId, MemberRules>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        members
            .iter()
            .map(|(&user, &rules)| Entry { user, rules })
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<UserId, MemberRules>, D::Error> {
        Ok(Vec::<Entry>::deserialize(d)?
            .into_iter()
            .map(|e| (e.user, e.rules))
            .collect())
    }
}

fn default_soulseek_sharing() -> SoulseekAccess {
    SoulseekAccess::Off
}

fn default_soulseek() -> SoulseekAccess {
    SoulseekAccess::Everyone
}

fn default_two_factor() -> TwoFactorPolicy {
    TwoFactorPolicy::Everyone
}

fn default_issuer() -> String {
    DEFAULT_TOTP_ISSUER.to_owned()
}

fn default_max_track_secs() -> u32 {
    MaxTrackLength::DEFAULT.secs().unwrap_or(0)
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            two_factor: default_two_factor(),
            totp_issuer: default_issuer(),
            max_track_secs: default_max_track_secs(),
            soulseek: default_soulseek(),
            soulseek_sharing: default_soulseek_sharing(),
            soulseek_members: BTreeMap::new(),
        }
    }
}

impl ServerSettings {
    pub fn max_track_length(&self) -> MaxTrackLength {
        MaxTrackLength::from_secs(self.max_track_secs)
    }

    /// Whether switching to `next` can let someone share who could not
    /// before, by any route: the sharing switch, the Soulseek switch, or a
    /// member's choices. Judged on what the rules add up to for every
    /// member with choices (of their rank, from `role_of`; of either rank
    /// if unknown) and for everyone else, of either rank. A member's
    /// sharing choice raised counts even while a switch still keeps them
    /// out, as it lets them share once that switch opens.
    pub fn widens_sharing(&self, next: &Self, role_of: impl Fn(UserId) -> Option<Role>) -> bool {
        const EITHER: [Role; 2] = [Role::Captain, Role::Pirate];
        let member = |settings: &Self, user: Option<UserId>| {
            user.and_then(|user| settings.soulseek_members.get(&user))
                .copied()
                .unwrap_or_default()
        };
        let may_share = |settings: &Self, user, role| {
            member(settings, user).may_share(settings.soulseek, settings.soulseek_sharing, role)
        };
        let members = self
            .soulseek_members
            .keys()
            .chain(next.soulseek_members.keys())
            .copied();
        members.map(Some).chain([None]).any(|user| {
            let roles = match user.and_then(&role_of) {
                Some(role) => &[role][..],
                None => &EITHER[..],
            };
            member(self, user)
                .sharing
                .is_widened_by(member(next, user).sharing)
                || roles
                    .iter()
                    .any(|&role| may_share(next, user, role) && !may_share(self, user, role))
        })
    }

    /// The live Soulseek rules these settings describe.
    fn soulseek_rules(
        &self,
    ) -> (
        SoulseekAccess,
        SoulseekAccess,
        BTreeMap<UserId, MemberRules>,
    ) {
        (
            self.soulseek,
            self.soulseek_sharing,
            self.soulseek_members.clone(),
        )
    }

    pub fn auth(&self) -> AuthConfig {
        AuthConfig {
            two_factor: self.two_factor,
            totp_issuer: self.totp_issuer.clone(),
        }
    }

    /// Check every value; the message names the setting at fault.
    pub fn validate(&self) -> Result<(), String> {
        validate_issuer(&self.totp_issuer)?;
        if self.max_track_secs > MAX_TRACK_SECS_LIMIT {
            return Err(format!(
                "longest track: at most {} hours (0 = no limit)",
                MAX_TRACK_SECS_LIMIT / 3600
            ));
        }
        Ok(())
    }
}

/// The issuer is part of the `otpauth://` label, where `:` separates it
/// from the account name; apps disagree on how to undo escaping.
pub fn validate_issuer(issuer: &str) -> Result<(), String> {
    if issuer.trim().is_empty() {
        Err("authenticator name: must not be empty".into())
    } else if issuer.chars().count() > MAX_TOTP_ISSUER_CHARS {
        Err(format!(
            "authenticator name: at most {MAX_TOTP_ISSUER_CHARS} characters"
        ))
    } else if issuer.contains(':') || issuer.chars().any(char::is_control) {
        Err("authenticator name: must not contain ':' or control characters".into())
    } else {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error(
        "settings file {path} is unreadable ({reason}); fix or move it away — it is never overwritten automatically"
    )]
    Corrupt { path: PathBuf, reason: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

/// The settings of the running server, and the shared handles that apply
/// them. Changes go through [`Self::lock`] and [`Self::replace`], so two
/// captains saving at once cannot interleave.
pub struct Settings {
    path: PathBuf,
    current: Mutex<ServerSettings>,
    track_limit: TrackLengthLimit,
    soulseek: SoulseekRules,
}

impl Settings {
    /// Load `settings.json`, or create it from `initial` on the first
    /// start (saved at once, so the environment it was seeded from no
    /// longer matters afterwards). Says whether it was created.
    pub async fn open(
        path: PathBuf,
        initial: &ServerSettings,
    ) -> Result<(Self, bool), SettingsError> {
        let (settings, created) = {
            let path = path.clone();
            let initial = initial.clone();
            tokio::task::spawn_blocking(move || match load(&path)? {
                Some(settings) => Ok((settings, false)),
                None => save(&path, &initial).map(|()| (initial, true)),
            })
            .await
            .map_err(|e| SettingsError::Io {
                context: "loading settings".into(),
                source: io::Error::other(e),
            })??
        };
        Ok((
            Self {
                path,
                track_limit: TrackLengthLimit::new(settings.max_track_length()),
                soulseek: {
                    let (access, sharing, members) = settings.soulseek_rules();
                    SoulseekRules::new(access, sharing, members)
                },
                current: Mutex::new(settings),
            },
            created,
        ))
    }

    pub async fn get(&self) -> ServerSettings {
        self.current.lock().await.clone()
    }

    /// The live track-length limit, for the parts of Inkult that apply it.
    pub fn track_limit(&self) -> TrackLengthLimit {
        self.track_limit.clone()
    }

    /// The live Soulseek rules, for the Soulseek service.
    pub fn soulseek_rules(&self) -> SoulseekRules {
        self.soulseek.clone()
    }

    pub async fn lock(&self) -> MutexGuard<'_, ServerSettings> {
        self.current.lock().await
    }

    /// Save `next` and make it current. On a failed save nothing changes.
    pub async fn replace(
        &self,
        current: &mut MutexGuard<'_, ServerSettings>,
        next: ServerSettings,
    ) -> Result<(), SettingsError> {
        let path = self.path.clone();
        let to_save = next.clone();
        tokio::task::spawn_blocking(move || save(&path, &to_save))
            .await
            .map_err(|e| SettingsError::Io {
                context: "saving settings".into(),
                source: io::Error::other(e),
            })??;
        self.track_limit.set(next.max_track_length());
        let (access, sharing, members) = next.soulseek_rules();
        self.soulseek.set(access, sharing, members);
        **current = next;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: ServerSettings,
}

/// Read the settings, or `None` when the file does not exist yet.
pub fn load(path: &Path) -> Result<Option<ServerSettings>, SettingsError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SettingsError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    let corrupt = |reason: String| SettingsError::Corrupt {
        path: path.to_path_buf(),
        reason,
    };
    let file: SettingsFile = serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
    file.settings.validate().map_err(corrupt)?;
    Ok(Some(file.settings))
}

/// Write the settings atomically. Blocking; call from `spawn_blocking`.
pub fn save(path: &Path, settings: &ServerSettings) -> Result<(), SettingsError> {
    let file = SettingsFile {
        version: SETTINGS_VERSION,
        settings: settings.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| SettingsError::Io {
        context: "serializing settings".into(),
        source: io::Error::other(e),
    })?;
    // Not secret, but only Inkult needs it, like the other state files.
    write_atomic(path, &bytes, Visibility::Private).map_err(|source| SettingsError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_none_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(load(&path).unwrap(), None);
        let settings = ServerSettings {
            two_factor: TwoFactorPolicy::Captains,
            totp_issuer: "Inkult (home)".into(),
            max_track_secs: 0,
            soulseek: SoulseekAccess::Captains,
            soulseek_sharing: SoulseekAccess::Captains,
            soulseek_members: BTreeMap::from([(
                UserId(7),
                MemberRules {
                    soulseek: crate::soulseek::Override::Allowed,
                    sharing: crate::soulseek::Override::Blocked,
                },
            )]),
        };
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap(), Some(settings));
    }

    #[test]
    fn defaults_require_two_factor_from_everyone() {
        let settings = ServerSettings::default();
        assert_eq!(settings.two_factor, TwoFactorPolicy::Everyone);
        assert_eq!(settings.max_track_length(), MaxTrackLength::DEFAULT);
        // Fields missing from an older file take their default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, br#"{"version":1,"max_track_secs":900}"#).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.two_factor, TwoFactorPolicy::Everyone);
        assert_eq!(loaded.max_track_secs, 900);
        // Soulseek stays usable after an upgrade, but sharing waits for a
        // captain to allow it.
        assert_eq!(loaded.soulseek, SoulseekAccess::Everyone);
        assert_eq!(loaded.soulseek_sharing, SoulseekAccess::Off);
        // Sharing used to be a yes/no.
        for (stored, read) in [
            ("true", SoulseekAccess::Everyone),
            ("false", SoulseekAccess::Off),
            ("\"captains\"", SoulseekAccess::Captains),
        ] {
            std::fs::write(
                &path,
                format!(r#"{{"version":1,"soulseek_sharing":{stored}}}"#),
            )
            .unwrap();
            assert_eq!(load(&path).unwrap().unwrap().soulseek_sharing, read);
        }
    }

    #[test]
    fn unreadable_or_invalid_file_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Corrupt { .. })));
        std::fs::write(&path, br#"{"version":1,"two_factor":"sometimes"}"#).unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Corrupt { .. })));
        std::fs::write(&path, br#"{"version":1,"totp_issuer":"a:b"}"#).unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Corrupt { .. })));
    }

    #[tokio::test]
    async fn first_start_saves_the_initial_settings_then_ignores_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let initial = ServerSettings {
            max_track_secs: 900,
            ..ServerSettings::default()
        };
        let (settings, created) = Settings::open(path.clone(), &initial).await.unwrap();
        assert!(created);
        assert_eq!(settings.track_limit().get().secs(), Some(900));

        let mut guard = settings.lock().await;
        let next = ServerSettings {
            max_track_secs: 0,
            ..guard.clone()
        };
        let limit = settings.track_limit();
        let rules = settings.soulseek_rules();
        assert_eq!(rules.sharing(), SoulseekAccess::Off);
        let next = ServerSettings {
            soulseek: SoulseekAccess::Off,
            soulseek_sharing: SoulseekAccess::Captains,
            ..next
        };
        settings.replace(&mut guard, next).await.unwrap();
        drop(guard);
        assert_eq!(limit.get(), MaxTrackLength::UNLIMITED);
        assert_eq!(rules.access(), SoulseekAccess::Off);
        assert_eq!(rules.sharing(), SoulseekAccess::Captains);

        let (reopened, created) = Settings::open(path, &initial).await.unwrap();
        assert!(!created);
        assert_eq!(reopened.get().await.max_track_secs, 0);
    }

    #[test]
    fn a_member_allowed_to_share_widens_sharing() {
        use crate::soulseek::Override;
        let before = ServerSettings::default();
        let with = |sharing| ServerSettings {
            soulseek_members: BTreeMap::from([(
                UserId(7),
                MemberRules {
                    soulseek: Override::Auto,
                    sharing,
                },
            )]),
            ..ServerSettings::default()
        };
        assert!(before.widens_sharing(&with(Override::Allowed), |_| None));
        assert!(!before.widens_sharing(&with(Override::Blocked), |_| None));
        assert!(with(Override::Blocked).widens_sharing(&before, |_| None));
        assert!(!with(Override::Allowed).widens_sharing(&before, |_| None));
    }

    /// Sharing needs Soulseek: letting more people use it lets more share
    /// whenever the sharing switch already covers them.
    #[test]
    fn letting_more_use_soulseek_can_widen_sharing() {
        use crate::soulseek::Override;
        let captains_only = ServerSettings {
            soulseek: SoulseekAccess::Captains,
            soulseek_sharing: SoulseekAccess::Everyone,
            ..ServerSettings::default()
        };
        let everyone = ServerSettings {
            soulseek: SoulseekAccess::Everyone,
            ..captains_only.clone()
        };
        assert!(
            captains_only.widens_sharing(&everyone, |_| None),
            "pirates can share"
        );
        let off = ServerSettings {
            soulseek: SoulseekAccess::Off,
            ..captains_only.clone()
        };
        assert!(
            off.widens_sharing(&captains_only, |_| None),
            "captains can share"
        );
        // Not when the sharing switch keeps them out anyway.
        let no_sharing = |soulseek| ServerSettings {
            soulseek,
            soulseek_sharing: SoulseekAccess::Off,
            ..ServerSettings::default()
        };
        assert!(
            !no_sharing(SoulseekAccess::Off)
                .widens_sharing(&no_sharing(SoulseekAccess::Everyone), |_| None)
        );

        // A member unblocked, or let in past the switch.
        let member = |soulseek| ServerSettings {
            soulseek_members: BTreeMap::from([(
                UserId(7),
                MemberRules {
                    soulseek,
                    sharing: Override::Auto,
                },
            )]),
            ..captains_only.clone()
        };
        assert!(member(Override::Blocked).widens_sharing(&captains_only, |_| None));
        assert!(captains_only.widens_sharing(&member(Override::Allowed), |_| None));
        assert!(!member(Override::Allowed).widens_sharing(&captains_only, |_| None));

        // Unblocking a pirate whom the captains-only sharing switch keeps
        // from sharing anyway lets nobody share; unblocking a captain does.
        let captains_share = |soulseek| ServerSettings {
            soulseek: SoulseekAccess::Everyone,
            soulseek_sharing: SoulseekAccess::Captains,
            ..member(soulseek)
        };
        let unblock = |role| {
            captains_share(Override::Blocked)
                .widens_sharing(&captains_share(Override::Auto), |_| Some(role))
        };
        assert!(!unblock(Role::Pirate));
        assert!(unblock(Role::Captain));
    }

    #[test]
    fn validation_names_the_setting() {
        let mut settings = ServerSettings {
            totp_issuer: "  ".into(),
            ..ServerSettings::default()
        };
        assert!(settings.validate().unwrap_err().contains("authenticator"));
        settings.totp_issuer = "Inkult".into();
        settings.max_track_secs = MAX_TRACK_SECS_LIMIT + 1;
        assert!(settings.validate().unwrap_err().contains("longest track"));
    }
}
