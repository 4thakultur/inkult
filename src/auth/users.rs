//! User accounts and their persistence (`users.json` in the data directory).
//!
//! Like the library index, an unreadable users file stops startup instead
//! of being replaced: silently starting with no accounts would let anyone
//! with the setup code take over the server.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::password;
use super::totp::TotpState;
use crate::storage::{Visibility, write_atomic};

const USERS_VERSION: u32 = 1;
pub const MAX_USERNAME_CHARS: usize = 32;
const MIN_USERNAME_CHARS: usize = 2;

/// Stable account id. Random rather than sequential so that recreating the
/// users file can never hand an old account's library to a new user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub u64);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What an account may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Administrator: manages the crew and sees every job and library file.
    Captain,
    /// Regular user: sees only their own downloads and library.
    Pirate,
}

impl Role {
    pub fn is_captain(self) -> bool {
        self == Role::Captain
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredUser {
    pub id: UserId,
    /// Normalized (lowercase) login name.
    pub username: String,
    pub role: Role,
    /// Argon2id PHC string.
    pub password_hash: String,
    /// Unix timestamp (seconds).
    pub created_at: u64,
    /// Failed logins in a row since the last success or unlock.
    #[serde(default)]
    pub failed_logins: u32,
    /// Set when too many failed logins locked the account (Unix seconds).
    /// Only a captain, or `inkult unlock` on the server, clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_at: Option<u64>,
    /// Authenticator-app second factor, when the user turned it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<TotpState>,
    /// The password was generated for a captain to hand over: it must be
    /// replaced at the next login, since someone else has seen it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub password_temporary: bool,
    /// When the password was last set (Unix seconds); `None` for accounts
    /// that kept the password they were created with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_changed_at: Option<u64>,
}

/// An account as shown to the application; never carries the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub created_at: u64,
    /// When the account was locked by failed logins, if it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_at: Option<u64>,
    /// Logging in needs a code from an authenticator app.
    pub two_factor_enabled: bool,
    /// Still on a password a captain generated (not yet replaced).
    pub password_temporary: bool,
    /// When that temporary password stops working (Unix seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary_password_expires_at: Option<u64>,
}

impl From<&StoredUser> for User {
    fn from(u: &StoredUser) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            role: u.role,
            created_at: u.created_at,
            locked_at: u.locked_at,
            two_factor_enabled: u.totp.is_some(),
            password_temporary: u.password_temporary,
            temporary_password_expires_at: u.password_temporary.then(|| {
                u.password_changed_at.unwrap_or(u.created_at) + super::TEMPORARY_PASSWORD_TTL_SECS
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserStoreError {
    #[error(
        "users file {path} is unreadable ({reason}); fix or move it away — it is never overwritten automatically"
    )]
    Corrupt { path: PathBuf, reason: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsersFile {
    version: u32,
    users: Vec<StoredUser>,
}

/// All accounts, in memory, mirrored to disk by [`UserStore::save`].
pub struct UserStore {
    path: PathBuf,
    file: UsersFile,
}

/// Normalize and validate a login name: 2–32 of `a-z 0-9 . _ -`, starting
/// with a letter or digit. Case-insensitive, so it is stored lowercase.
pub fn normalize_username(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_lowercase();
    let len = name.chars().count();
    if !(MIN_USERNAME_CHARS..=MAX_USERNAME_CHARS).contains(&len) {
        return Err(format!(
            "handle must be {MIN_USERNAME_CHARS}–{MAX_USERNAME_CHARS} characters"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err("handle may only use letters, digits, '.', '_' and '-'".into());
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("handle must start with a letter or digit".into());
    }
    Ok(name)
}

impl UserStore {
    pub fn load(path: PathBuf) -> Result<Self, UserStoreError> {
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| UserStoreError::Corrupt {
                path: path.clone(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => UsersFile {
                version: USERS_VERSION,
                users: Vec::new(),
            },
            Err(source) => {
                return Err(UserStoreError::Io {
                    context: format!("reading {}", path.display()),
                    source,
                });
            }
        };
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialized contents for [`save_bytes`], taken while the store is
    /// locked so the write reflects one consistent state.
    pub fn snapshot(&self) -> Result<Vec<u8>, UserStoreError> {
        serde_json::to_vec_pretty(&self.file).map_err(|e| UserStoreError::Io {
            context: "serializing users".into(),
            source: io::Error::other(e),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.file.users.is_empty()
    }

    pub fn list(&self) -> Vec<User> {
        let mut users: Vec<User> = self.file.users.iter().map(User::from).collect();
        users.sort_by(|a, b| a.username.cmp(&b.username));
        users
    }

    pub fn get(&self, id: UserId) -> Option<&StoredUser> {
        self.file.users.iter().find(|u| u.id == id)
    }

    pub fn find_by_name(&self, normalized: &str) -> Option<&StoredUser> {
        self.file.users.iter().find(|u| u.username == normalized)
    }

    fn captains(&self) -> usize {
        self.file
            .users
            .iter()
            .filter(|u| u.role.is_captain())
            .count()
    }

    /// Whether `id` is the only remaining captain.
    pub fn is_last_captain(&self, id: UserId) -> bool {
        self.get(id).is_some_and(|u| u.role.is_captain()) && self.captains() == 1
    }

    /// Add an account. The caller has validated the name and hashed the
    /// password; this only guards uniqueness.
    pub fn insert(&mut self, username: String, role: Role, password_hash: String) -> Option<User> {
        if self.find_by_name(&username).is_some() {
            return None;
        }
        let mut id = UserId(password::random_id());
        while self.get(id).is_some() {
            id = UserId(password::random_id());
        }
        let user = StoredUser {
            id,
            username,
            role,
            password_hash,
            created_at: now_secs(),
            failed_logins: 0,
            locked_at: None,
            totp: None,
            password_temporary: false,
            password_changed_at: None,
        };
        let shown = User::from(&user);
        self.file.users.push(user);
        self.file.version = USERS_VERSION;
        Some(shown)
    }

    pub fn get_mut(&mut self, id: UserId) -> Option<&mut StoredUser> {
        self.file.users.iter_mut().find(|u| u.id == id)
    }

    /// Put back a removed account (undoing a failed delete).
    pub fn insert_existing(&mut self, user: StoredUser) -> bool {
        if self.get(user.id).is_some() || self.find_by_name(&user.username).is_some() {
            return false;
        }
        self.file.users.push(user);
        true
    }

    pub fn remove(&mut self, id: UserId) -> Option<StoredUser> {
        let index = self.file.users.iter().position(|u| u.id == id)?;
        Some(self.file.users.remove(index))
    }
}

/// Write a [`UserStore::snapshot`] to disk, owner-readable only since it
/// holds password hashes. Blocking.
pub fn save_bytes(path: &Path, bytes: &[u8]) -> Result<(), UserStoreError> {
    write_atomic(path, bytes, Visibility::Private).map_err(|source| UserStoreError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_normalized_and_validated() {
        assert_eq!(normalize_username("  BlackBeard ").unwrap(), "blackbeard");
        assert_eq!(normalize_username("anne.bonny_1").unwrap(), "anne.bonny_1");
        assert!(normalize_username("x").is_err());
        assert!(normalize_username(&"a".repeat(33)).is_err());
        assert!(normalize_username("../root").is_err());
        assert!(normalize_username("-dash").is_err());
        assert!(normalize_username("spa ce").is_err());
        assert!(normalize_username("émile").is_err());
    }

    #[test]
    fn insert_rejects_duplicates_and_tracks_last_captain() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = UserStore::load(dir.path().join("users.json")).unwrap();
        assert!(store.is_empty());
        let cap = store
            .insert("cap".into(), Role::Captain, "h".into())
            .unwrap();
        assert!(
            store
                .insert("cap".into(), Role::Pirate, "h".into())
                .is_none()
        );
        let pirate = store
            .insert("jack".into(), Role::Pirate, "h".into())
            .unwrap();
        assert!(store.is_last_captain(cap.id));
        assert!(!store.is_last_captain(pirate.id));
        store.get_mut(pirate.id).unwrap().role = Role::Captain;
        assert!(!store.is_last_captain(cap.id));
    }

    #[test]
    fn persists_and_refuses_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let mut store = UserStore::load(path.clone()).unwrap();
        let user = store
            .insert("cap".into(), Role::Captain, "hash".into())
            .unwrap();
        save_bytes(&path, &store.snapshot().unwrap()).unwrap();

        let reopened = UserStore::load(path.clone()).unwrap();
        assert_eq!(reopened.list(), vec![user]);
        assert_eq!(
            reopened.get(reopened.list()[0].id).unwrap().password_hash,
            "hash"
        );

        std::fs::write(&path, b"{oops").unwrap();
        assert!(matches!(
            UserStore::load(path.clone()),
            Err(UserStoreError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"{oops");
    }
}
