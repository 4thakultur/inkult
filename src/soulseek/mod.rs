//! Soulseek: a peer-to-peer network where users share their own music
//! collections. Diggers search it for rare releases, browse other users'
//! shares, and queue downloads that the other user's client uploads when it
//! has a free slot.
//!
//! Inkult speaks the protocol itself (no external client needed):
//!
//! ```text
//! Soulseek (service, one per Inkult)
//!  └─ Session (one per Inkult user who signed in to Soulseek)
//!      ├─ server connection   login, searches, peer addresses, messages
//!      ├─ listener            inbound peer connections (own port per session)
//!      ├─ peers               P connections (requests/results), F (file data)
//!      ├─ searches, browse, user info, private messages, wishlist
//!      ├─ downloads           queue → remote queue → transfer → library import
//!      ├─ upgrades            lossless copies of library tracks (see `upgrade`)
//!      └─ uploads             opt-in sharing of the user's own library
//! ```
//!
//! Above each user's own choices, the captains' rules (`rules`) decide who
//! may use Soulseek and whether anyone may share: everything leaves from
//! the host's address, so that is the host's call.
//!
//! Each Inkult user signs in with their own Soulseek account: Soulseek
//! identities are personal (private messages, other users' ban lists and
//! queues follow the account), and one account cannot be signed in twice.
//! A session listens on its own port from `INKULT_SOULSEEK_PORTS`, because
//! an incoming connection does not say which local account it is for.
//!
//! Peers and their data are untrusted: see `wire` for message limits, and
//! `downloads`/`import` for how received files reach the library.

mod downloads;
mod import;
mod messages;
mod peer;
mod rules;
mod search;
mod session;
mod social;
mod store;
mod upgrade;
mod uploads;
mod wire;

#[cfg(test)]
mod e2e_tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use crate::auth::{Role, UserId};
use crate::domain::length::TrackLengthLimit;
use crate::library::Library;
use crate::tools::ffmpeg::Ffmpeg;

pub use downloads::FileRequest;
pub use rules::{MemberRules, Override, SoulseekAccess, SoulseekRules};
pub use session::{AccountStatus, ConnState, Session};
pub use store::StoreError;
pub use upgrade::{UpgradeError, UpgradeProgress, UpgradeWanted};

/// Longest search query accepted (the network has no documented limit;
/// real queries are short).
pub const MAX_QUERY_CHARS: usize = 200;
/// Longest private message accepted from the UI.
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Soulseek usernames are at most 30 printable ASCII characters.
pub const MAX_USERNAME_CHARS: usize = 30;

#[derive(Debug, thiserror::Error)]
pub enum SoulseekError {
    #[error("not signed in to Soulseek")]
    NotSignedIn,
    #[error("Soulseek is reconnecting; try again in a moment")]
    Offline,
    #[error("Soulseek rejected the sign-in: {0}")]
    LoginRejected(String),
    #[error("cannot reach the Soulseek server: {0}")]
    ServerUnreachable(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Peer(String),
    #[error("{username} is offline")]
    UserOffline { username: String },
    #[error("{username} did not answer in time")]
    PeerTimeout { username: String },
    #[error("this Soulseek account is already signed in by another Inkult user")]
    AccountInUse,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Limit(String),
    #[error("Soulseek is turned off on this server")]
    Disabled,
    #[error("sharing on Soulseek is turned off on this server")]
    SharingDisabled,
}

/// Validate a Soulseek username as typed by a user.
pub fn check_username(raw: &str) -> Result<String, SoulseekError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > MAX_USERNAME_CHARS {
        return Err(SoulseekError::InvalidInput(format!(
            "Soulseek usernames are 1–{MAX_USERNAME_CHARS} characters"
        )));
    }
    if !name.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(SoulseekError::InvalidInput(
            "Soulseek usernames may only contain printable ASCII characters".into(),
        ));
    }
    Ok(name.to_owned())
}

/// Settings shared by all sessions.
#[derive(Debug, Clone)]
pub struct SoulseekConfig {
    /// `host:port` of the Soulseek server.
    pub server: String,
    /// Listening ports handed out to sessions, one each.
    pub ports: Vec<u16>,
    pub upload_slots: usize,
    /// Who may use Soulseek and whether anyone may share (live).
    pub rules: SoulseekRules,
    /// Search results over it are dropped and received files over it are
    /// not imported.
    pub max_track_length: TrackLengthLimit,
    pub staging_dir: PathBuf,
    pub store_path: PathBuf,
    /// Allow peer connections to loopback addresses (tests only; a real
    /// server never reports them, so they would be a confused or malicious
    /// redirect to services on this machine).
    pub allow_loopback: bool,
}

/// What every session needs.
pub(crate) struct Context {
    pub config: SoulseekConfig,
    pub library: Arc<Library>,
    /// For the spectrum check of received lossless files.
    pub ffmpeg: Arc<Ffmpeg>,
    pub store: store::Store,
}

/// A listening port borrowed from the pool; returned when dropped.
pub(crate) struct PortLease {
    port: u16,
    pool: Arc<Mutex<Vec<u16>>>,
}

impl PortLease {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        crate::jobs::lock(&self.pool).push(self.port);
    }
}

pub struct Soulseek {
    ctx: Arc<Context>,
    sessions: Mutex<HashMap<UserId, Arc<Session>>>,
    free_ports: Arc<Mutex<Vec<u16>>>,
    /// Serializes sign-ins so one account cannot be signed in twice by a
    /// race between two users.
    login_lock: tokio::sync::Mutex<()>,
    /// Users whose session was ended because the rules turned Soulseek off
    /// for them, and who had chosen to stay signed in: they are reconnected
    /// once it is back on (see [`Self::resume_suspended`]).
    suspended: Mutex<HashSet<UserId>>,
}

impl Soulseek {
    pub async fn new(
        config: SoulseekConfig,
        library: Arc<Library>,
        ffmpeg: Arc<Ffmpeg>,
    ) -> Result<Self, StoreError> {
        let store = store::Store::open(config.store_path.clone()).await?;
        let mut ports = config.ports.clone();
        ports.reverse(); // hand out the first configured port first
        Ok(Self {
            free_ports: Arc::new(Mutex::new(ports)),
            ctx: Arc::new(Context {
                config,
                library,
                ffmpeg,
                store,
            }),
            sessions: Mutex::new(HashMap::new()),
            login_lock: tokio::sync::Mutex::new(()),
            suspended: Mutex::new(HashSet::new()),
        })
    }

    /// Reconnect `user`'s remembered account ("stay signed in"), unless
    /// their session is already running. Runs in the background.
    ///
    /// Call this only once `user` has fully logged in to Inkult (second
    /// factor passed, no forced password change or 2FA enrollment pending):
    /// the stored Soulseek password must not be put to use on behalf of
    /// someone who has not proven to be its owner. That is also why nothing
    /// reconnects at startup.
    pub fn resume_remembered(self: &Arc<Self>, user: UserId, role: Role) {
        crate::jobs::lock(&self.suspended).remove(&user);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let Some(account) = this.ctx.store.get(user).await else {
                return;
            };
            let Some(password) = account.password else {
                return;
            };
            // Under the sign-in lock, so logins on two devices at once
            // start one session rather than one replacing the other.
            let _serialized = this.login_lock.lock().await;
            if this.session(user).is_some_and(|s| !s.is_stopped()) {
                return;
            }
            if let Err(e) = this
                .sign_in_locked(user, role, &account.username, &password, true)
                .await
            {
                tracing::warn!(%user, soulseek_user = %account.username, error = %e, "automatic Soulseek sign-in failed");
            }
        });
    }

    /// The user's session, if they signed in (it may be reconnecting).
    pub fn session(&self, user: UserId) -> Option<Arc<Session>> {
        crate::jobs::lock(&self.sessions).get(&user).cloned()
    }

    /// The user's session, or `NotSignedIn`.
    pub fn require(&self, user: UserId) -> Result<Arc<Session>, SoulseekError> {
        self.session(user)
            .filter(|s| !s.is_stopped())
            .ok_or(SoulseekError::NotSignedIn)
    }

    pub async fn status(&self, user: UserId, role: Role) -> AccountStatus {
        match self.session(user) {
            Some(session) => session.status().await,
            None => AccountStatus::signed_out(
                self.ctx.store.get(user).await,
                self.ctx.config.rules.may_share(user, role),
            ),
        }
    }

    /// What a user whom the rules keep off Soulseek is shown: their saved
    /// account, but no live state.
    pub async fn disabled_status(&self, user: UserId, role: Role) -> AccountStatus {
        let mut status = AccountStatus::signed_out(
            self.ctx.store.get(user).await,
            self.ctx.config.rules.may_share(user, role),
        );
        status.state = ConnState::Disabled;
        status.message = Some(SoulseekError::Disabled.to_string());
        status
    }

    /// The rules now keep `user` off Soulseek: end their session. Unlike
    /// signing out, the stored password (if any) is kept, so the account
    /// reconnects when Soulseek is turned back on for them. Unfinished
    /// downloads are saved and resume then, as after a restart.
    pub async fn suspend(&self, user: UserId) {
        let session = crate::jobs::lock(&self.sessions).remove(&user);
        let Some(session) = session else {
            return;
        };
        let was_running = !session.is_stopped();
        session.stop().await;
        let remembered = self
            .ctx
            .store
            .get(user)
            .await
            .is_some_and(|a| a.password.is_some());
        if was_running && remembered {
            crate::jobs::lock(&self.suspended).insert(user);
        }
        tracing::info!(%user, soulseek_user = %session.username(), "Soulseek session ended by the server rules");
    }

    /// Reconnect `user` if their session was ended by [`Self::suspend`].
    /// Call only once the rules allow them Soulseek again, from a request
    /// of theirs with full access (as for [`Self::resume_remembered`]).
    pub fn resume_suspended(self: &Arc<Self>, user: UserId, role: Role) {
        let was_suspended = crate::jobs::lock(&self.suspended).remove(&user);
        if was_suspended {
            self.resume_remembered(user, role);
        }
    }

    /// A captain changed `user`'s rank: end their session if the rules no
    /// longer let them use Soulseek, else apply the sharing rule to it.
    ///
    /// The rank is recorded first, and a sign-in still connecting reads it
    /// when it registers its session (`sign_in_locked`): a session this
    /// misses takes the new rank there, one it finds already has it.
    pub async fn member_changed(&self, user: UserId, role: Role) {
        self.ctx.config.rules.set_rank(user, role);
        if !self.ctx.config.rules.may_use(user, role) {
            self.suspend(user).await;
        } else if let Some(session) = self.session(user).filter(|s| !s.is_stopped()) {
            session.apply_sharing().await;
        }
    }

    /// The sharing rule changed: every session stops or resumes sharing.
    pub async fn apply_sharing_rule(&self) {
        let sessions: Vec<_> = crate::jobs::lock(&self.sessions)
            .values()
            .filter(|s| !s.is_stopped())
            .cloned()
            .collect();
        for session in sessions {
            session.apply_sharing().await;
        }
    }

    fn lease_port(&self) -> Option<PortLease> {
        crate::jobs::lock(&self.free_ports)
            .pop()
            .map(|port| PortLease {
                port,
                pool: Arc::clone(&self.free_ports),
            })
    }

    /// Bind a listening port for a new session: the first free port of the
    /// pool that can actually be bound. `None` when all are taken; the
    /// session then works through outgoing connections only.
    async fn bind_listener(&self) -> Option<(TcpListener, PortLease)> {
        let mut unusable = Vec::new();
        let mut found = None;
        while let Some(lease) = self.lease_port() {
            match TcpListener::bind(("0.0.0.0", lease.port())).await {
                Ok(listener) => {
                    found = Some((listener, lease));
                    break;
                }
                Err(e) => {
                    tracing::warn!(port = lease.port(), error = %e, "cannot listen for Soulseek peers on this port");
                    unusable.push(lease);
                }
            }
        }
        drop(unusable); // back into the pool for a later attempt
        found
    }

    /// Sign `user` in to Soulseek. Replaces any previous session of theirs.
    pub async fn sign_in(
        &self,
        user: UserId,
        role: Role,
        username: &str,
        password: &str,
        remember: bool,
    ) -> Result<AccountStatus, SoulseekError> {
        let username = check_username(username)?;
        if password.is_empty() || password.len() > 128 {
            return Err(SoulseekError::InvalidInput(
                "enter your Soulseek password (at most 128 characters)".into(),
            ));
        }
        let _serialized = self.login_lock.lock().await;
        self.sign_in_locked(user, role, &username, password, remember)
            .await
    }

    /// [`Self::sign_in`] with `login_lock` held and the input validated.
    async fn sign_in_locked(
        &self,
        user: UserId,
        role: Role,
        username: &str,
        password: &str,
        remember: bool,
    ) -> Result<AccountStatus, SoulseekError> {
        let username = username.to_owned();
        let in_use = crate::jobs::lock(&self.sessions).iter().any(|(owner, s)| {
            *owner != user && !s.is_stopped() && s.username().eq_ignore_ascii_case(&username)
        });
        if in_use {
            return Err(SoulseekError::AccountInUse);
        }
        let previous = crate::jobs::lock(&self.sessions).remove(&user);
        if let Some(previous) = previous {
            previous.stop().await;
        }

        let account = self.ctx.store.get(user).await.unwrap_or_default();
        // Downloads queued under another Soulseek account are not resumed
        // under this one: they were requested from peers by that account.
        let account = if account.username.eq_ignore_ascii_case(&username) {
            account
        } else {
            store::Account {
                username: username.clone(),
                ..Default::default()
            }
        };
        let listener = self.bind_listener().await;
        let session = Session::start(
            Arc::clone(&self.ctx),
            user,
            role,
            username.clone(),
            password.to_owned(),
            listener,
            &account,
        )
        .await?;
        self.ctx
            .store
            .update(user, |a| {
                if !a.username.eq_ignore_ascii_case(&username) {
                    *a = store::Account::default();
                }
                a.username = username.clone();
                a.password = remember.then(|| password.to_owned());
            })
            .await;
        // The rules may have turned Soulseek off for this user while the
        // sign-in was connecting, after the sweep that ended the other
        // sessions (`suspend`) had passed. The sweep takes this lock after
        // changing the rules, so checking them under it means either the
        // sweep finds this session or this sees the new rules.
        // The same goes for a rank a captain changed meanwhile, which wins
        // over the one this sign-in began with (see `member_changed`).
        let (admitted, rank_changed) = {
            let mut sessions = crate::jobs::lock(&self.sessions);
            let rank = self.ctx.config.rules.rank(user, role);
            let admitted = self.ctx.config.rules.may_use(user, rank);
            if admitted {
                sessions.insert(user, Arc::clone(&session));
            }
            (admitted, rank != role)
        };
        if !admitted {
            session.stop().await;
            // As if suspended a moment later: a remembered account comes
            // back once Soulseek is on again for them.
            if remember {
                crate::jobs::lock(&self.suspended).insert(user);
            }
            tracing::info!(%user, soulseek_user = %username, "Soulseek sign-in ended by the server rules");
            return Err(SoulseekError::Disabled);
        }
        if rank_changed {
            // It started out sharing (or not) by the old rank.
            session.apply_sharing().await;
        }
        Ok(session.status().await)
    }

    /// Sign out and forget the stored password. Unfinished downloads stay
    /// saved and resume at the next sign-in.
    pub async fn sign_out(&self, user: UserId) {
        let session = crate::jobs::lock(&self.sessions).remove(&user);
        if let Some(session) = session {
            session.stop().await;
        }
        self.ctx.store.update(user, |a| a.password = None).await;
    }

    /// A deleted Inkult account: end its session and forget its settings.
    pub async fn forget_user(&self, user: UserId) {
        let session = crate::jobs::lock(&self.sessions).remove(&user);
        if let Some(session) = session {
            session.stop().await;
        }
        crate::jobs::lock(&self.suspended).remove(&user);
        self.ctx.config.rules.forget_rank(user);
        self.ctx.store.remove(user).await;
    }

    pub async fn shutdown(&self) {
        let sessions: Vec<_> = crate::jobs::lock(&self.sessions)
            .drain()
            .map(|(_, s)| s)
            .collect();
        for session in sessions {
            session.stop().await;
        }
    }
}

/// Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// A peer's file path (backslash separated, as on the wire) split into its
/// folder and file name.
pub(crate) fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('\\').unwrap_or(("", path))
}

/// Lowercase file extension of a peer path.
pub(crate) fn extension(path: &str) -> String {
    let (_, name) = split_path(path);
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Audio formats Inkult can verify, tag and file into the library.
/// (Formats whose codec Inkult cannot identify, such as APE or WavPack,
/// are left out: their quality could not be reported honestly.)
pub(crate) const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "aif", "aiff",
];

pub(crate) fn is_audio(path: &str) -> bool {
    AUDIO_EXTENSIONS.contains(&extension(path).as_str())
}

/// Folder artwork worth fetching to embed: preferred names first.
pub(crate) fn cover_rank(path: &str) -> Option<u8> {
    let ext = extension(path);
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
        return None;
    }
    let (_, name) = split_path(path);
    let stem = name
        .rsplit_once('.')
        .map_or(name, |(s, _)| s)
        .to_ascii_lowercase();
    Some(match stem.as_str() {
        "cover" | "folder" | "front" => 0,
        s if s.contains("front") || s.contains("cover") => 1,
        "album" | "albumart" | "albumartlarge" => 2,
        _ => 3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_usernames() {
        assert_eq!(check_username("  dj digger ").unwrap(), "dj digger");
        assert!(check_username("").is_err());
        assert!(check_username(&"x".repeat(31)).is_err());
        assert!(check_username("naïve").is_err());
        assert!(check_username("tab\there").is_err());
    }

    #[test]
    fn classifies_peer_paths() {
        assert_eq!(
            split_path("@@music\\Artist\\Album\\01.flac"),
            ("@@music\\Artist\\Album", "01.flac")
        );
        assert_eq!(split_path("bare.mp3"), ("", "bare.mp3"));
        assert!(is_audio("a\\b\\Track.FLAC"));
        assert!(!is_audio("a\\b\\cover.jpg"));
        assert!(!is_audio("a\\b\\noext"));
        assert_eq!(cover_rank("x\\Folder.JPG"), Some(0));
        assert_eq!(cover_rank("x\\Album Front Cover.png"), Some(1));
        assert_eq!(cover_rank("x\\scan01.jpg"), Some(3));
        assert_eq!(cover_rank("x\\cover.gif"), None);
    }
}
