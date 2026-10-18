//! Per-user Soulseek settings, persisted in `soulseek.json` in the data
//! directory (owner-readable only, since it may hold passwords).
//!
//! A Soulseek password is stored only when the user ticks "stay signed in":
//! Soulseek has no tokens or password reset, so automatic sign-in after a
//! restart needs the password itself. Unfinished downloads are stored too,
//! so a queue that waits hours for a remote slot survives restarts.
//!
//! Like the library index, a corrupt file stops startup instead of being
//! replaced.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::auth::UserId;
use crate::storage::{Visibility, write_atomic};

const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "Soulseek settings {path} are unreadable ({reason}); fix or move the file away — it is never overwritten automatically"
    )]
    Corrupt { path: PathBuf, reason: String },
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A download waiting to be resumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedDownload {
    pub peer: String,
    pub path: String,
    pub size: u64,
    /// Folder artwork fetched for embedding, not a track for the library.
    #[serde(default)]
    pub cover: bool,
    /// The library track this file is a quality upgrade for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<super::upgrade::UpgradeTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Account {
    pub username: String,
    /// Only present when the user chose to stay signed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub share_library: bool,
    #[serde(default)]
    pub wishlist: Vec<String>,
    #[serde(default)]
    pub downloads: Vec<SavedDownload>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    accounts: BTreeMap<UserId, Account>,
}

pub struct Store {
    path: PathBuf,
    data: Mutex<StoreFile>,
}

impl Store {
    pub async fn open(path: PathBuf) -> Result<Self, StoreError> {
        let data = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
                path: path.clone(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => StoreFile {
                version: VERSION,
                accounts: BTreeMap::new(),
            },
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    pub async fn get(&self, user: UserId) -> Option<Account> {
        self.data.lock().await.accounts.get(&user).cloned()
    }

    /// Change (or create) a user's account record and save.
    pub async fn update<T>(&self, user: UserId, f: impl FnOnce(&mut Account) -> T) -> T {
        let mut data = self.data.lock().await;
        let result = f(data.accounts.entry(user).or_default());
        data.version = VERSION;
        persist(&self.path, &data).await;
        result
    }

    pub async fn remove(&self, user: UserId) {
        let mut data = self.data.lock().await;
        if data.accounts.remove(&user).is_some() {
            persist(&self.path, &data).await;
        }
    }
}

/// Save, logging failure: the in-memory state stays authoritative and the
/// next successful save catches up.
async fn persist(path: &Path, data: &StoreFile) {
    let json = match serde_json::to_vec_pretty(data) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!(error = %e, "cannot serialize Soulseek settings");
            return;
        }
    };
    let path = path.to_path_buf();
    let result =
        tokio::task::spawn_blocking(move || write_atomic(&path, &json, Visibility::Private)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "failed to save Soulseek settings"),
        Err(e) => tracing::error!(error = %e, "failed to save Soulseek settings"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_accounts_and_refuses_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("soulseek.json");
        let store = Store::open(path.clone()).await.unwrap();
        store
            .update(UserId(3), |a| {
                a.username = "digger".into();
                a.wishlist.push("rare white label".into());
            })
            .await;
        let reopened = Store::open(path.clone()).await.unwrap();
        let account = reopened.get(UserId(3)).await.unwrap();
        assert_eq!(account.username, "digger");
        assert_eq!(account.password, None);
        assert_eq!(account.wishlist, ["rare white label"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        std::fs::write(&path, b"{oops").unwrap();
        assert!(matches!(
            Store::open(path.clone()).await,
            Err(StoreError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"{oops");
    }
}
