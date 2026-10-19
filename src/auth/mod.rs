//! Accounts, login and sessions.
//!
//! Two roles exist: the **captain** (administrator) manages the crew and
//! sees everything; a **pirate** sees only their own downloads and library.
//!
//! The first captain is created through the web UI with a one-time setup
//! code that is printed to the server log when no accounts exist yet. Only
//! someone with access to the server's output can claim a fresh install,
//! even if it is reachable from the network before anyone has logged in.
//!
//! # Two-factor login
//!
//! Any user can add an authenticator app (TOTP, see [`totp`]). Logging in
//! then takes two steps: the password, which yields a short-lived
//! challenge rather than a session, and a code (or a single-use recovery
//! code) for that challenge. Wrong codes count as failed logins, so they
//! feed the same per-handle throttle and account lock as wrong passwords,
//! and a right password alone no longer clears either: someone who knows
//! the password cannot get unlimited code guesses by re-entering it.
//!
//! Turning 2FA on, off, or replacing the recovery codes asks for the
//! password again (and, while on, a code), so a hijacked browser session
//! can neither lock the owner out with an authenticator they don't have
//! nor quietly remove the second factor. Captains can reset another
//! user's 2FA when a phone is lost; `inkult reset-2fa` does it offline.
//! [`TwoFactorPolicy`] (a server setting captains change in the admin
//! panel) makes 2FA mandatory: users without it then get a session that can
//! only set it up, and nobody can turn theirs off. Moving to a new phone
//! swaps the authenticator in one step ([`Auth::replace_authenticator`]),
//! so the account is never without a second factor.

pub mod password;
pub mod sessions;
pub mod totp;
pub mod users;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore, SemaphorePermit};

use self::sessions::{Challenges, LoginThrottle, SessionGrant, SessionPolicy, Sessions};
use self::totp::{SecondFactor, TotpSecret, TotpState};
pub use self::users::{Role, User, UserId};
use self::users::{UserStore, UserStoreError, normalize_username, now_secs, save_bytes};

/// Argon2 uses ~19 MiB per hash; bound how many run at once so a burst of
/// login attempts cannot exhaust memory or starve the blocking pool.
const MAX_CONCURRENT_HASHES: usize = 4;
/// Requests allowed to wait for a hashing slot. Beyond this the server is
/// being flooded: answering "busy" at once keeps memory flat and the wait
/// short for those already queued, instead of an ever-growing queue that
/// makes every login time out.
const MAX_HASH_QUEUE: usize = 64;
/// A temporary password a captain handed out stops working this long after
/// it was made, so one that leaks from a chat history or a note doesn't
/// stay a way into a fresh account.
pub const TEMPORARY_PASSWORD_TTL_SECS: u64 = 72 * 3600;
/// Throttle key for setup-code guesses (not a valid username).
const SETUP_THROTTLE_KEY: &str = "#setup";
/// Throttle key shared by every login with a malformed handle. Such handles
/// can never log in, and keying on the raw input would let one request park
/// megabytes in the throttle map (or name the setup bucket above).
const INVALID_HANDLE_THROTTLE_KEY: &str = "#invalid-handle";
/// Failed logins in a row that lock an account until a captain unlocks it.
/// The throttle slows guessing down; this caps it for good.
pub const MAX_FAILED_LOGINS: u32 = 10;
/// How long a QR code shown for setting up 2FA stays valid.
const ENROLLMENT_TTL: Duration = Duration::from_secs(600);
pub const DEFAULT_TOTP_ISSUER: &str = "Inkult";
pub const MAX_TOTP_ISSUER_CHARS: usize = 64;

/// Who must use two-factor login (a server setting, see `settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TwoFactorPolicy {
    /// Everyone may turn it on; nobody has to.
    #[serde(rename = "off")]
    Optional,
    /// Captains must: they can change every account.
    #[serde(rename = "captains")]
    Captains,
    #[serde(rename = "all")]
    Everyone,
}

impl TwoFactorPolicy {
    pub fn requires(self, role: Role) -> bool {
        match self {
            Self::Optional => false,
            Self::Captains => role.is_captain(),
            Self::Everyone => true,
        }
    }

    /// Whether switching to `other` stops requiring 2FA from someone.
    pub fn is_weakened_by(self, other: Self) -> bool {
        [Role::Captain, Role::Pirate]
            .into_iter()
            .any(|role| self.requires(role) && !other.requires(role))
    }
}

/// The server settings [`Auth`] applies; replaced at runtime with
/// [`Auth::set_config`] when a captain changes them.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub two_factor: TwoFactorPolicy,
    /// Name the authenticator app shows next to the code.
    pub totp_issuer: String,
}

impl Default for AuthConfig {
    /// Nothing required. Only for tests: the server's own default (2FA for
    /// everyone) comes from its settings.
    fn default() -> Self {
        Self {
            two_factor: TwoFactorPolicy::Optional,
            totp_issuer: DEFAULT_TOTP_ISSUER.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("that handle is already taken")]
    UsernameTaken,
    #[error("wrong handle or password")]
    InvalidCredentials,
    #[error("too many failed attempts; try again in {retry_after_secs} s")]
    Throttled { retry_after_secs: u64 },
    #[error("this server already has a captain; log in instead")]
    SetupUnavailable,
    #[error("wrong setup code; it is printed in the server log")]
    InvalidSetupCode,
    #[error("no such user")]
    UserNotFound,
    #[error("the ship needs at least one captain")]
    LastCaptain,
    #[error("this account is locked after too many failed logins; ask a captain to unlock it")]
    AccountLocked,
    #[error("wrong or already used code")]
    InvalidSecondFactor,
    #[error("this login expired or had too many wrong codes; enter your password again")]
    ChallengeExpired,
    #[error("two-factor login is already on for this account")]
    TwoFactorAlreadyEnabled,
    #[error("two-factor login is not on for this account")]
    TwoFactorNotEnabled,
    #[error(
        "two-factor login is required on this server, so it cannot be turned off; to move to a new phone, use [NEW PHONE]"
    )]
    TwoFactorMandatory,
    #[error("this setup expired; start again to get a new QR code")]
    EnrollmentExpired,
    #[error("this temporary password has expired; ask a captain for a new one")]
    TemporaryPasswordExpired,
    #[error("the server is busy; try again in a few seconds")]
    Busy,
    #[error(transparent)]
    Store(#[from] UserStoreError),
    #[error("internal error: {0}")]
    Internal(String),
}

/// A successful login: who, and the session token for the cookie.
pub struct Login {
    pub user: User,
    pub token: String,
    /// The password used no longer meets the policy; the session is
    /// restricted to changing it (see [`Authenticated`]).
    pub password_change_required: bool,
}

/// The result of the password step.
pub enum LoginOutcome {
    Complete(Login),
    /// The password was right; finish with [`Auth::login_second_factor`].
    SecondFactorRequired {
        challenge: String,
        expires_in_secs: u64,
    },
}

/// The account behind a valid session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub user: User,
    /// Only changing the password is allowed until this is cleared.
    pub password_change_required: bool,
    /// The server requires 2FA for this account and it is not set up:
    /// only setting it up is allowed.
    pub two_factor_enrollment_required: bool,
}

/// An account a captain created, with the generated password to hand over.
pub struct NewAccount {
    pub user: User,
    pub temporary_password: String,
}

/// What a user needs to add Inkult to an authenticator app.
pub struct Enrollment {
    /// Base32 key, for typing in when the QR code can't be scanned.
    pub secret: String,
    pub uri: String,
    pub qr_svg: String,
    pub issuer: String,
    pub account: String,
}

/// 2FA was turned on: a fresh session, and recovery codes to show once.
pub struct TwoFactorEnabled {
    pub login: Login,
    pub recovery_codes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TwoFactorStatus {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_at: Option<u64>,
    pub recovery_codes_left: usize,
    /// The server requires 2FA for this account.
    pub required: bool,
}

struct PendingEnrollment {
    secret: TotpSecret,
    created: Instant,
}

pub struct Auth {
    /// Shared so failed-login bookkeeping can be saved in the background.
    store: Arc<Mutex<UserStore>>,
    saver: Arc<Saver>,
    sessions: Sessions,
    challenges: Challenges,
    throttle: LoginThrottle,
    hashing: Semaphore,
    /// Requests holding or waiting for a hashing slot.
    hash_queue: AtomicUsize,
    /// Read on every use, so a captain's change applies at once.
    config: StdRwLock<AuthConfig>,
    /// Secrets shown as a QR code but not yet confirmed with a code.
    enrollments: StdMutex<HashMap<UserId, PendingEnrollment>>,
    /// Present only while no account exists.
    setup_code: StdMutex<Option<String>>,
    /// Verified against when the user does not exist, so a login for an
    /// unknown name takes as long as one with a wrong password.
    dummy_hash: String,
}

impl Auth {
    pub async fn open(path: PathBuf, config: AuthConfig) -> Result<Self, AuthError> {
        let store = tokio::task::spawn_blocking(move || UserStore::load(path))
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))??;
        let dummy_hash = tokio::task::spawn_blocking(|| password::hash("inkult-dummy-password"))
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?
            .map_err(|e| AuthError::Internal(e.to_string()))?;

        let setup_code = store.is_empty().then(|| {
            let hex = password::random_hex(6);
            format!("{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12])
        });
        if let Some(code) = &setup_code {
            // The one secret Inkult logs on purpose: it is single-use, only
            // valid until the first captain exists, and the log is how the
            // server's operator proves they are the operator.
            tracing::warn!(
                "no accounts yet: open the web UI and enter setup code {code} to appoint the first captain"
            );
        }
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            saver: Arc::default(),
            sessions: Sessions::new(SessionPolicy::default()),
            challenges: Challenges::default(),
            throttle: LoginThrottle::default(),
            hashing: Semaphore::new(MAX_CONCURRENT_HASHES),
            hash_queue: AtomicUsize::new(0),
            config: StdRwLock::new(config),
            enrollments: StdMutex::new(HashMap::new()),
            setup_code: StdMutex::new(setup_code),
            dummy_hash,
        })
    }

    fn config(&self) -> AuthConfig {
        self.config
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Apply changed server settings. A stricter 2FA policy restricts
    /// existing sessions on their next request (see [`Self::authenticate`]).
    pub fn set_config(&self, config: AuthConfig) {
        *self.config.write().unwrap_or_else(|p| p.into_inner()) = config;
    }

    pub fn session_max_age(&self) -> Duration {
        self.sessions.max_age()
    }

    pub fn setup_required(&self) -> bool {
        self.setup_code
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// A hashing slot, or [`AuthError::Busy`] when too many requests are
    /// already queued for one.
    async fn hash_slot(&self) -> Result<HashSlot<'_>, AuthError> {
        let ticket = QueueTicket::take(&self.hash_queue, MAX_CONCURRENT_HASHES + MAX_HASH_QUEUE)
            .ok_or(AuthError::Busy)?;
        let permit = self
            .hashing
            .acquire()
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        Ok(HashSlot {
            _ticket: ticket,
            _permit: permit,
        })
    }

    async fn hash(&self, password: &str) -> Result<String, AuthError> {
        let _slot = self.hash_slot().await?;
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || password::hash(&password))
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?
            .map_err(|e| AuthError::Internal(e.to_string()))
    }

    async fn verify(&self, password: &str, hash: String) -> Result<bool, AuthError> {
        let _slot = self.hash_slot().await?;
        let password = password.to_owned();
        Ok(
            tokio::task::spawn_blocking(move || password::verify(&password, &hash))
                .await
                .unwrap_or(false),
        )
    }

    /// Persist the store; on failure undo the in-memory change so memory
    /// never claims something the disk does not have.
    async fn commit(
        &self,
        store: &mut UserStore,
        undo: impl FnOnce(&mut UserStore),
    ) -> Result<(), AuthError> {
        let saved = match self.saver.snapshot(store) {
            Ok(save) => self.saver.write(save).await,
            Err(e) => Err(e),
        };
        saved.inspect_err(|_| undo(store))
    }

    fn validate(username: &str, password: &str) -> Result<String, AuthError> {
        let name = normalize_username(username).map_err(AuthError::InvalidInput)?;
        password::check_policy(password, &name).map_err(AuthError::InvalidInput)?;
        Ok(name)
    }

    /// Appoint the first captain with the setup code from the server log.
    pub async fn setup(
        &self,
        code: &str,
        username: &str,
        password: &str,
    ) -> Result<Login, AuthError> {
        let expected = {
            let guard = self.setup_code.lock().unwrap_or_else(|p| p.into_inner());
            guard.clone().ok_or(AuthError::SetupUnavailable)?
        };
        self.throttle
            .attempt(SETUP_THROTTLE_KEY)
            .map_err(|retry_after_secs| AuthError::Throttled { retry_after_secs })?;
        let normalize = |s: &str| -> String {
            s.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase()
        };
        if !password::constant_time_eq(normalize(code).as_bytes(), normalize(&expected).as_bytes())
        {
            return Err(AuthError::InvalidSetupCode);
        }
        self.throttle.record_success(SETUP_THROTTLE_KEY);
        let name = Self::validate(username, password)?;
        let hash = self.hash(password).await?;

        let mut store = self.store.lock().await;
        if !store.is_empty() {
            return Err(AuthError::SetupUnavailable);
        }
        let user = store
            .insert(name, Role::Captain, hash)
            .ok_or(AuthError::UsernameTaken)?;
        let id = user.id;
        self.commit(&mut store, |s| {
            s.remove(id);
        })
        .await?;
        drop(store);

        *self.setup_code.lock().unwrap_or_else(|p| p.into_inner()) = None;
        tracing::info!(user = %user.username, "first captain appointed");
        let token = self.sessions.create(user.id, false);
        Ok(Login {
            user,
            token,
            password_change_required: false,
        })
    }

    /// The password step. With 2FA on, a right password only yields a
    /// challenge for [`Self::login_second_factor`], and the handle's
    /// throttle and failure count are left as they are until the code is
    /// right too.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginOutcome, AuthError> {
        // Throttle on the normalized name so case variations share one
        // counter. Malformed names share a fixed key; they match no account
        // and are not echoed into the log (newlines would forge log lines).
        let name = normalize_username(username).ok();
        let key = name.as_deref().unwrap_or(INVALID_HANDLE_THROTTLE_KEY);
        self.throttle
            .attempt(key)
            .map_err(|retry_after_secs| AuthError::Throttled { retry_after_secs })?;

        let candidate = match &name {
            Some(name) => {
                let store = self.store.lock().await;
                store
                    .find_by_name(name)
                    .map(|u| (u.id, u.password_hash.clone()))
            }
            None => None,
        };
        let too_long = password.chars().count() > password::MAX_PASSWORD_CHARS;
        let (id, hash) = match candidate {
            Some((id, hash)) if !too_long => (Some(id), hash),
            _ => (None, self.dummy_hash.clone()),
        };
        let password_ok = match self
            .verify(if too_long { "" } else { password }, hash)
            .await
        {
            Ok(ok) => ok,
            Err(e) => {
                // Turned away before anything was checked: not a guess.
                self.throttle.forgive(key);
                return Err(e);
            }
        };

        let Some(id) = id else {
            tracing::warn!(handle = %key, "failed login");
            return Err(AuthError::InvalidCredentials);
        };
        let user = self.settle_login(id, password_ok).await?;
        // Accounts created before the current policy may still have a weak
        // password. The password is only ever seen in clear here, so this
        // is where it can be checked; the session then only allows a change.
        let password_change_required =
            user.password_temporary || password::check_policy(password, &user.username).is_err();
        if user.two_factor_enabled {
            tracing::info!(user = %user.username, "password accepted; waiting for the authenticator code");
            let challenge =
                self.challenges
                    .create(user.id, &user.username, password_change_required);
            return Ok(LoginOutcome::SecondFactorRequired {
                challenge,
                expires_in_secs: self.challenges.ttl().as_secs(),
            });
        }
        self.throttle.record_success(key);
        if user.password_temporary {
            tracing::info!(user = %user.username, "logged in with a temporary password; a new one is required");
        } else if password_change_required {
            tracing::warn!(user = %user.username, "logged in with a password below the policy; a new one is required");
        } else {
            tracing::info!(user = %user.username, "logged in");
        }
        let token = self.sessions.create(user.id, password_change_required);
        Ok(LoginOutcome::Complete(Login {
            user,
            token,
            password_change_required,
        }))
    }

    /// The second login step: an authenticator code or a recovery code for
    /// a challenge from [`Self::login`].
    pub async fn login_second_factor(
        &self,
        challenge: &str,
        code: &str,
    ) -> Result<Login, AuthError> {
        let grant = self
            .challenges
            .attempt(challenge)
            .ok_or(AuthError::ChallengeExpired)?;
        // The handle's throttle still counts the password step; codes add
        // to it, so starting over with the password buys no free guesses.
        self.throttle
            .attempt(&grant.username)
            .map_err(|retry_after_secs| AuthError::Throttled { retry_after_secs })?;

        let id = grant.user;
        let mut store = self.store.lock().await;
        let Some(user) = store.get_mut(id) else {
            self.challenges.remove(challenge);
            return Err(AuthError::ChallengeExpired);
        };
        if user.locked_at.is_some() {
            self.challenges.remove(challenge);
            tracing::warn!(user = %grant.username, "login refused: account is locked");
            return Err(AuthError::AccountLocked);
        }
        // A captain reset this user's 2FA meanwhile: start over.
        let Some(totp) = user.totp.as_mut() else {
            self.challenges.remove(challenge);
            return Err(AuthError::ChallengeExpired);
        };
        let previous = totp.clone();
        let Some(factor) = totp.accept(code, now_secs()) else {
            user.failed_logins = user.failed_logins.saturating_add(1);
            let failures = user.failed_logins;
            let locked = failures >= MAX_FAILED_LOGINS;
            if locked {
                user.locked_at = Some(now_secs());
                self.challenges.remove(challenge);
                tracing::warn!(user = %grant.username, failures, "account locked after too many failed logins; a captain must unlock it");
            } else {
                tracing::warn!(user = %grant.username, failures, "wrong authenticator code");
            }
            self.persist(&store, "recording a failed login").await;
            return Err(if locked {
                AuthError::AccountLocked
            } else if grant.last_attempt {
                AuthError::ChallengeExpired
            } else {
                AuthError::InvalidSecondFactor
            });
        };
        let previous_failures = std::mem::take(&mut user.failed_logins);
        let shown = User::from(&*user);
        // The used step (or recovery code) must be on disk before the
        // login counts, or a restart would let the same code in again.
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(id) {
                u.totp = Some(previous);
                u.failed_logins = previous_failures;
            }
        })
        .await?;
        drop(store);
        self.challenges.remove(challenge);
        self.throttle.record_success(&grant.username);
        match factor {
            SecondFactor::Totp => tracing::info!(user = %shown.username, "logged in"),
            SecondFactor::RecoveryCode { remaining } => tracing::warn!(
                user = %shown.username,
                remaining,
                "logged in with a recovery code"
            ),
        }
        let token = self.sessions.create(id, grant.password_change_required);
        Ok(Login {
            user: shown,
            token,
            password_change_required: grant.password_change_required,
        })
    }

    /// Record a verified login attempt against the account's lock. Checked
    /// under the store lock after verifying, so parallel attempts cannot
    /// slip past a lock set while their hashes were being computed.
    ///
    /// A locked account refuses even the right password. Only someone who
    /// knows that password is told the account is locked; everyone else gets
    /// the usual "wrong handle or password", so locking cannot be used to
    /// find out which handles exist.
    async fn settle_login(&self, id: UserId, password_ok: bool) -> Result<User, AuthError> {
        let mut store = self.store.lock().await;
        let Some(user) = store.get_mut(id) else {
            return Err(AuthError::InvalidCredentials);
        };
        let name = user.username.clone();
        if user.locked_at.is_some() {
            tracing::warn!(user = %name, "login refused: account is locked");
            return Err(if password_ok {
                AuthError::AccountLocked
            } else {
                AuthError::InvalidCredentials
            });
        }
        if password_ok {
            let shown = User::from(&*user);
            if shown
                .temporary_password_expires_at
                .is_some_and(|at| now_secs() >= at)
            {
                tracing::warn!(user = %name, "login refused: the temporary password expired");
                return Err(AuthError::TemporaryPasswordExpired);
            }
            // With 2FA the login is not over: the count is reset when the
            // code is right too.
            if user.totp.is_none() && user.failed_logins > 0 {
                user.failed_logins = 0;
                self.persist(&store, "resetting failed-login count").await;
            }
            return Ok(shown);
        }
        user.failed_logins = user.failed_logins.saturating_add(1);
        let failures = user.failed_logins;
        if failures >= MAX_FAILED_LOGINS {
            user.locked_at = Some(now_secs());
            tracing::warn!(user = %name, failures, "account locked after too many failed logins; a captain must unlock it");
        } else {
            tracing::warn!(user = %name, failures, "failed login");
        }
        drop(store);
        // The count and lock above are already in effect; only the disk
        // write is deferred. Waiting for it (an fsync) made wrong passwords
        // for existing handles measurably slower than for unknown ones,
        // which told an attacker which handles exist.
        self.persist_soon("recording a failed login");
        Err(AuthError::InvalidCredentials)
    }

    /// [`Self::persist`] from a background task. The store is locked only
    /// to take the snapshot, never during the disk write: a write holding
    /// it would delay the next login instead, which leaks just the same.
    fn persist_soon(&self, what: &'static str) {
        let store = Arc::clone(&self.store);
        let saver = Arc::clone(&self.saver);
        tokio::spawn(async move {
            let save = saver.snapshot(&*store.lock().await);
            let saved = match save {
                Ok(save) => saver.write(save).await,
                Err(e) => Err(e),
            };
            if let Err(e) = saved {
                tracing::error!(error = %e, "{what}: cannot save accounts; kept in memory only");
            }
        });
    }

    /// Best-effort save of login bookkeeping. Unlike [`Self::commit`] a
    /// failed write is not undone in memory: a lock that only lives until
    /// the next restart still beats no lock at all.
    async fn persist(&self, store: &UserStore, what: &str) {
        let saved = match self.saver.snapshot(store) {
            Ok(save) => self.saver.write(save).await,
            Err(e) => Err(e),
        };
        if let Err(e) = saved {
            tracing::error!(error = %e, "{what}: cannot save accounts; kept in memory only");
        }
    }

    /// Lift a lock set by failed logins (captain action).
    pub async fn unlock(&self, id: UserId) -> Result<User, AuthError> {
        let mut store = self.store.lock().await;
        let user = store.get_mut(id).ok_or(AuthError::UserNotFound)?;
        let previous = (user.failed_logins, user.locked_at);
        user.failed_logins = 0;
        user.locked_at = None;
        let shown = User::from(&*user);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(id) {
                (u.failed_logins, u.locked_at) = previous;
            }
        })
        .await?;
        tracing::info!(user = %shown.username, "account unlocked");
        Ok(shown)
    }

    pub fn logout(&self, token: &str) {
        self.sessions.revoke(token);
    }

    /// The current account behind a session token. Roles are read live, so
    /// a promotion, demotion or deletion takes effect on the next request.
    pub async fn authenticate(&self, token: &str) -> Option<Authenticated> {
        let SessionGrant {
            user: id,
            password_change_required,
        } = self.sessions.resolve(token)?;
        let user = self.store.lock().await.get(id).map(User::from);
        let Some(user) = user else {
            self.sessions.revoke(token);
            return None;
        };
        // Read live like the role: the policy applies to sessions that
        // predate it, and turning 2FA off under a policy that requires it
        // restricts the session at once.
        let two_factor_enrollment_required = self.two_factor_enrollment_required(&user);
        Some(Authenticated {
            user,
            password_change_required,
            two_factor_enrollment_required,
        })
    }

    /// The policy requires 2FA for this account and it is not on yet.
    pub fn two_factor_enrollment_required(&self, user: &User) -> bool {
        self.two_factor_mandatory(user) && !user.two_factor_enabled
    }

    /// The server's policy makes 2FA mandatory for this account.
    pub fn two_factor_mandatory(&self, user: &User) -> bool {
        self.config().two_factor.requires(user.role)
    }

    pub async fn list(&self) -> Vec<User> {
        self.store.lock().await.list()
    }

    pub async fn usernames(&self) -> HashMap<UserId, String> {
        self.store
            .lock()
            .await
            .list()
            .into_iter()
            .map(|u| (u.id, u.username))
            .collect()
    }

    /// A captain signs on a new user. Inkult generates the password, so
    /// the captain never picks (or reuses) one; it is temporary, and at
    /// their first login the user must choose their own (and, when the
    /// policy requires it, set up two-factor login) before anything else.
    pub async fn create_user(&self, username: &str, role: Role) -> Result<NewAccount, AuthError> {
        let temporary_password = password::generate();
        let user = self
            .add_user(username, &temporary_password, role, true)
            .await?;
        Ok(NewAccount {
            user,
            temporary_password,
        })
    }

    /// Add an account. `onboarding`: the password is temporary (accounts
    /// created by a captain).
    async fn add_user(
        &self,
        username: &str,
        password: &str,
        role: Role,
        onboarding: bool,
    ) -> Result<User, AuthError> {
        let name = Self::validate(username, password)?;
        if self.store.lock().await.find_by_name(&name).is_some() {
            return Err(AuthError::UsernameTaken);
        }
        let hash = self.hash(password).await?;
        let mut store = self.store.lock().await;
        let mut user = store
            .insert(name, role, hash)
            .ok_or(AuthError::UsernameTaken)?;
        let id = user.id;
        if onboarding && let Some(stored) = store.get_mut(id) {
            stored.password_temporary = true;
            user = User::from(&*stored);
        }
        self.commit(&mut store, |s| {
            s.remove(id);
        })
        .await?;
        tracing::info!(user = %user.username, ?role, "account created");
        Ok(user)
    }

    pub async fn set_role(&self, id: UserId, role: Role) -> Result<User, AuthError> {
        let mut store = self.store.lock().await;
        if role == Role::Pirate && store.is_last_captain(id) {
            return Err(AuthError::LastCaptain);
        }
        let user = store.get_mut(id).ok_or(AuthError::UserNotFound)?;
        let previous = user.role;
        user.role = role;
        let shown = User::from(&*user);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(id) {
                u.role = previous;
            }
        })
        .await?;
        tracing::info!(user = %shown.username, ?role, "role changed");
        Ok(shown)
    }

    /// A captain resets a password: Inkult generates a temporary one for
    /// the captain to hand over, which the user must replace at their next
    /// login (the captain has seen it). It also lifts a failed-login lock,
    /// since a new password is the usual way to help a locked-out user.
    /// All of the user's sessions end.
    pub async fn reset_password(&self, id: UserId) -> Result<String, AuthError> {
        let temporary = password::generate();
        self.replace_password(id, &temporary, true, true).await?;
        Ok(temporary)
    }

    /// Store a new password and end the user's sessions; returns the
    /// updated account. `temporary`: it must be replaced at next login.
    async fn replace_password(
        &self,
        id: UserId,
        password: &str,
        unlock: bool,
        temporary: bool,
    ) -> Result<User, AuthError> {
        let username = self
            .store
            .lock()
            .await
            .get(id)
            .map(|u| u.username.clone())
            .ok_or(AuthError::UserNotFound)?;
        password::check_policy(password, &username).map_err(AuthError::InvalidInput)?;
        let hash = self.hash(password).await?;
        let mut store = self.store.lock().await;
        let user = store.get_mut(id).ok_or(AuthError::UserNotFound)?;
        let previous = (
            std::mem::replace(&mut user.password_hash, hash),
            user.failed_logins,
            user.locked_at,
            std::mem::replace(&mut user.password_temporary, temporary),
            user.password_changed_at.replace(now_secs()),
        );
        if unlock {
            user.failed_logins = 0;
            user.locked_at = None;
        }
        let shown = User::from(&*user);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(id) {
                (
                    u.password_hash,
                    u.failed_logins,
                    u.locked_at,
                    u.password_temporary,
                    u.password_changed_at,
                ) = previous;
            }
        })
        .await?;
        drop(store);
        self.sessions.revoke_user(id);
        self.challenges.revoke_user(id);
        if temporary {
            tracing::info!(user = %shown.username, "password reset to a temporary one");
        } else {
            tracing::info!(user = %shown.username, "password changed");
        }
        Ok(shown)
    }

    /// A user changing their own password: the current one is required
    /// (and throttled like a login), other sessions end, and a fresh
    /// session is returned for this browser.
    pub async fn change_own_password(
        &self,
        user: &User,
        current: &str,
        new: &str,
    ) -> Result<Login, AuthError> {
        password::check_policy(new, &user.username).map_err(AuthError::InvalidInput)?;
        if new == current {
            // Above all for a temporary password: the captain knows it.
            return Err(AuthError::InvalidInput(
                "choose a password different from the current one".into(),
            ));
        }
        self.check_current_password(user, current).await?;
        self.throttle.record_success(&user.username);
        // Changing one's own password from a session that predates a lock
        // does not lift it: unlocking is the captain's call.
        let user = self.replace_password(user.id, new, false, false).await?;
        let token = self.sessions.create(user.id, false);
        Ok(Login {
            user,
            token,
            password_change_required: false,
        })
    }

    /// Verify a signed-in user's password before a sensitive change.
    /// Throttled like a login. The caller clears the throttle
    /// (`record_success`) only once every check of its operation passed,
    /// so a right password can't be used to reset the count while a
    /// second factor is being guessed.
    async fn check_current_password(&self, user: &User, password: &str) -> Result<(), AuthError> {
        self.throttle
            .attempt(&user.username)
            .map_err(|retry_after_secs| AuthError::Throttled { retry_after_secs })?;
        let hash = self
            .store
            .lock()
            .await
            .get(user.id)
            .map(|u| u.password_hash.clone())
            .ok_or(AuthError::UserNotFound)?;
        if password.chars().count() > password::MAX_PASSWORD_CHARS {
            return Err(AuthError::InvalidCredentials);
        }
        match self.verify(password, hash).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(AuthError::InvalidCredentials),
            Err(e) => {
                self.throttle.forgive(&user.username);
                Err(e)
            }
        }
    }

    fn enrollments(&self) -> std::sync::MutexGuard<'_, HashMap<UserId, PendingEnrollment>> {
        self.enrollments.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub async fn two_factor_status(&self, user: &User) -> Result<TwoFactorStatus, AuthError> {
        let store = self.store.lock().await;
        let stored = store.get(user.id).ok_or(AuthError::UserNotFound)?;
        Ok(TwoFactorStatus {
            enabled: stored.totp.is_some(),
            enabled_at: stored.totp.as_ref().map(|t| t.enabled_at),
            recovery_codes_left: stored.totp.as_ref().map_or(0, |t| t.recovery_codes.len()),
            required: self.two_factor_mandatory(&User::from(stored)),
        })
    }

    /// Start adding an authenticator: a new secret, kept in memory until
    /// [`Self::enable_two_factor`] (or, with 2FA already on,
    /// [`Self::replace_authenticator`]) confirms the app produces right
    /// codes. Starting again replaces it, so only the newest QR code works.
    pub async fn begin_two_factor(&self, user: &User) -> Result<Enrollment, AuthError> {
        if self.store.lock().await.get(user.id).is_none() {
            return Err(AuthError::UserNotFound);
        }
        let secret = TotpSecret::generate();
        let issuer = self.config().totp_issuer;
        let uri = secret.provisioning_uri(&issuer, &user.username);
        let qr_svg = totp::qr_svg(&uri)
            .ok_or_else(|| AuthError::Internal("cannot draw the QR code".into()))?;
        let enrollment = Enrollment {
            secret: secret.to_base32(),
            uri,
            qr_svg,
            issuer,
            account: user.username.clone(),
        };
        let now = Instant::now();
        let mut pending = self.enrollments();
        pending.retain(|_, p| now.duration_since(p.created) < ENROLLMENT_TTL);
        pending.insert(
            user.id,
            PendingEnrollment {
                secret,
                created: now,
            },
        );
        Ok(enrollment)
    }

    /// Turn 2FA on with the pending secret, once the user proved both their
    /// password and that their app produces the right codes. Every other
    /// session ends: they were opened without the second factor.
    pub async fn enable_two_factor(
        &self,
        user: &User,
        password: &str,
        code: &str,
    ) -> Result<TwoFactorEnabled, AuthError> {
        let secret = {
            let pending = self.enrollments();
            pending
                .get(&user.id)
                .filter(|p| p.created.elapsed() < ENROLLMENT_TTL)
                .map(|p| p.secret.clone())
                .ok_or(AuthError::EnrollmentExpired)?
        };
        self.check_current_password(user, password).await?;
        let now = now_secs();
        let step = secret
            .verify(code, now, 0)
            .ok_or(AuthError::InvalidSecondFactor)?;
        self.throttle.record_success(&user.username);

        let (recovery_codes, hashes) = totp::generate_recovery_codes();
        let mut store = self.store.lock().await;
        let stored = store.get_mut(user.id).ok_or(AuthError::UserNotFound)?;
        if stored.totp.is_some() {
            return Err(AuthError::TwoFactorAlreadyEnabled);
        }
        stored.totp = Some(TotpState {
            secret,
            enabled_at: now,
            last_step: step,
            recovery_codes: hashes,
        });
        let shown = User::from(&*stored);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(user.id) {
                u.totp = None;
            }
        })
        .await?;
        drop(store);
        self.enrollments().remove(&user.id);
        self.sessions.revoke_user(user.id);
        self.challenges.revoke_user(user.id);
        tracing::info!(user = %shown.username, "two-factor login turned on");
        let password_change_required =
            shown.password_temporary || password::check_policy(password, &shown.username).is_err();
        let token = self.sessions.create(user.id, password_change_required);
        Ok(TwoFactorEnabled {
            login: Login {
                user: shown,
                token,
                password_change_required,
            },
            recovery_codes,
        })
    }

    /// Check the password and a code (or recovery code), then apply
    /// `change` to the user's 2FA state and save it. The code's use is
    /// saved with it, so it can't be used again.
    async fn change_two_factor<T>(
        &self,
        user: &User,
        password: &str,
        code: &str,
        change: impl FnOnce(&mut Option<TotpState>) -> T,
    ) -> Result<T, AuthError> {
        self.check_current_password(user, password).await?;
        let mut store = self.store.lock().await;
        let stored = store.get_mut(user.id).ok_or(AuthError::UserNotFound)?;
        let previous = stored.totp.clone();
        let totp = stored.totp.as_mut().ok_or(AuthError::TwoFactorNotEnabled)?;
        if totp.accept(code, now_secs()).is_none() {
            tracing::warn!(user = %user.username, "wrong authenticator code for a 2FA change");
            return Err(AuthError::InvalidSecondFactor);
        }
        let result = change(&mut stored.totp);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(user.id) {
                u.totp = previous;
            }
        })
        .await?;
        self.throttle.record_success(&user.username);
        Ok(result)
    }

    /// Turn 2FA off (password and a current code required). Sessions stay.
    /// Refused while the policy requires 2FA for the account.
    pub async fn disable_two_factor(
        &self,
        user: &User,
        password: &str,
        code: &str,
    ) -> Result<(), AuthError> {
        if self.two_factor_mandatory(user) {
            return Err(AuthError::TwoFactorMandatory);
        }
        self.change_two_factor(user, password, code, |totp| *totp = None)
            .await?;
        tracing::warn!(user = %user.username, "two-factor login turned off");
        Ok(())
    }

    /// Move to a new authenticator (a new phone) in one step: the pending
    /// secret from [`Self::begin_two_factor`] replaces the current one once
    /// the password, a code from the current app (or a recovery code) and
    /// a code from the new app are all right. The account is never without
    /// a second factor, which is why this works even when 2FA cannot be
    /// turned off. Recovery codes stay; other sessions end, like when 2FA
    /// is turned on.
    pub async fn replace_authenticator(
        &self,
        user: &User,
        password: &str,
        current_code: &str,
        new_code: &str,
    ) -> Result<Login, AuthError> {
        let secret = {
            let pending = self.enrollments();
            pending
                .get(&user.id)
                .filter(|p| p.created.elapsed() < ENROLLMENT_TTL)
                .map(|p| p.secret.clone())
                .ok_or(AuthError::EnrollmentExpired)?
        };
        // Checked before the current code is spent, so a typo in the new
        // one does not burn a recovery code.
        let new_step = secret
            .verify(new_code, now_secs(), 0)
            .ok_or(AuthError::InvalidSecondFactor)?;
        self.change_two_factor(user, password, current_code, |totp| {
            if let Some(totp) = totp {
                totp.secret = secret;
                totp.last_step = new_step;
                totp.enabled_at = now_secs();
            }
        })
        .await?;
        self.enrollments().remove(&user.id);
        self.sessions.revoke_user(user.id);
        self.challenges.revoke_user(user.id);
        tracing::info!(user = %user.username, "authenticator replaced");
        let password_change_required =
            user.password_temporary || password::check_policy(password, &user.username).is_err();
        let token = self.sessions.create(user.id, password_change_required);
        let user = self
            .store
            .lock()
            .await
            .get(user.id)
            .map(User::from)
            .ok_or(AuthError::UserNotFound)?;
        Ok(Login {
            user,
            token,
            password_change_required,
        })
    }

    /// Prove it is really the account's owner before a sensitive change
    /// (such as relaxing the 2FA policy): the password, and a code (or
    /// recovery code) when 2FA is on. The code is spent.
    pub async fn confirm_identity(
        &self,
        user: &User,
        password: &str,
        code: Option<&str>,
    ) -> Result<(), AuthError> {
        let has_two_factor = self
            .store
            .lock()
            .await
            .get(user.id)
            .ok_or(AuthError::UserNotFound)?
            .totp
            .is_some();
        if has_two_factor {
            let code = code.ok_or(AuthError::InvalidSecondFactor)?;
            self.change_two_factor(user, password, code, |_| ()).await
        } else {
            self.check_current_password(user, password).await?;
            self.throttle.record_success(&user.username);
            Ok(())
        }
    }

    /// Replace all recovery codes; the old ones stop working.
    pub async fn regenerate_recovery_codes(
        &self,
        user: &User,
        password: &str,
        code: &str,
    ) -> Result<Vec<String>, AuthError> {
        let (codes, hashes) = totp::generate_recovery_codes();
        self.change_two_factor(user, password, code, |totp| {
            if let Some(totp) = totp {
                totp.recovery_codes = hashes;
            }
        })
        .await?;
        tracing::info!(user = %user.username, "recovery codes replaced");
        Ok(codes)
    }

    /// Remove another user's 2FA (captain action, for a lost phone). Their
    /// sessions end, so whoever logs in next proves the password again.
    pub async fn reset_two_factor(&self, id: UserId) -> Result<User, AuthError> {
        let mut store = self.store.lock().await;
        let user = store.get_mut(id).ok_or(AuthError::UserNotFound)?;
        let previous = user.totp.take().ok_or(AuthError::TwoFactorNotEnabled)?;
        let shown = User::from(&*user);
        self.commit(&mut store, |s| {
            if let Some(u) = s.get_mut(id) {
                u.totp = Some(previous);
            }
        })
        .await?;
        drop(store);
        self.sessions.revoke_user(id);
        self.challenges.revoke_user(id);
        tracing::warn!(user = %shown.username, "two-factor login reset by a captain");
        Ok(shown)
    }

    pub async fn delete(&self, id: UserId) -> Result<User, AuthError> {
        let mut store = self.store.lock().await;
        if store.is_last_captain(id) {
            return Err(AuthError::LastCaptain);
        }
        let removed = store.remove(id).ok_or(AuthError::UserNotFound)?;
        let shown = User::from(&removed);
        self.commit(&mut store, |s| {
            let _ = s.insert_existing(removed);
        })
        .await?;
        drop(store);
        self.sessions.revoke_user(id);
        self.challenges.revoke_user(id);
        self.enrollments().remove(&id);
        tracing::info!(user = %shown.username, "account deleted");
        Ok(shown)
    }
}

/// `inkult unlock <handle>`: lift a failed-login lock from the command line,
/// for when every captain is locked. Like [`reset_password_offline`], only
/// while the server is stopped.
pub fn unlock_offline(path: PathBuf, username: &str) -> Result<(), String> {
    let name = normalize_username(username)?;
    let mut store = UserStore::load(path).map_err(|e| e.to_string())?;
    let user = store
        .find_by_name(&name)
        .map(|u| u.id)
        .and_then(|id| store.get_mut(id))
        .ok_or_else(|| format!("no account named '{name}'"))?;
    user.failed_logins = 0;
    user.locked_at = None;
    let bytes = store.snapshot().map_err(|e| e.to_string())?;
    save_bytes(store.path(), &bytes).map_err(|e| e.to_string())
}

/// Orders the writes of `users.json`. Every snapshot gets a revision,
/// taken with the store locked, so revisions follow the order of the
/// changes; a write is skipped when a newer revision is already on disk.
/// That lets background saves write without holding the store lock, with
/// no risk of an older snapshot landing last.
#[derive(Default)]
struct Saver {
    revision: AtomicU64,
    /// Newest revision on disk; also serializes the writes.
    written: Mutex<u64>,
}

struct PendingSave {
    path: PathBuf,
    bytes: Vec<u8>,
    revision: u64,
}

impl Saver {
    /// Serialize the store. Call with it locked.
    fn snapshot(&self, store: &UserStore) -> Result<PendingSave, AuthError> {
        Ok(PendingSave {
            path: store.path().to_path_buf(),
            bytes: store.snapshot()?,
            revision: self.revision.fetch_add(1, Ordering::SeqCst) + 1,
        })
    }

    async fn write(&self, save: PendingSave) -> Result<(), AuthError> {
        let mut written = self.written.lock().await;
        if *written >= save.revision {
            return Ok(());
        }
        let PendingSave { path, bytes, .. } = save;
        tokio::task::spawn_blocking(move || save_bytes(&path, &bytes))
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))??;
        *written = save.revision;
        Ok(())
    }
}

/// One place in the hashing queue; released when dropped, including when
/// the waiting request is cancelled.
struct QueueTicket<'a>(&'a AtomicUsize);

impl<'a> QueueTicket<'a> {
    fn take(queue: &'a AtomicUsize, limit: usize) -> Option<Self> {
        if queue.fetch_add(1, Ordering::SeqCst) >= limit {
            queue.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(queue))
    }
}

impl Drop for QueueTicket<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct HashSlot<'a> {
    _ticket: QueueTicket<'a>,
    _permit: SemaphorePermit<'a>,
}

/// `inkult reset-2fa <handle>`: remove an account's two-factor login from
/// the command line, for a captain who lost their authenticator and their
/// recovery codes. Only while the server is stopped, like the others.
pub fn reset_two_factor_offline(path: PathBuf, username: &str) -> Result<(), String> {
    let name = normalize_username(username)?;
    let mut store = UserStore::load(path).map_err(|e| e.to_string())?;
    let user = store
        .find_by_name(&name)
        .map(|u| u.id)
        .and_then(|id| store.get_mut(id))
        .ok_or_else(|| format!("no account named '{name}'"))?;
    if user.totp.take().is_none() {
        return Err(format!("'{name}' does not use two-factor login"));
    }
    let bytes = store.snapshot().map_err(|e| e.to_string())?;
    save_bytes(store.path(), &bytes).map_err(|e| e.to_string())
}

/// `inkult passwd <handle>`: set a password from the command line, for a
/// locked-out captain; also lifts a failed-login lock. Must run while the
/// server is stopped, since a running server keeps its own copy of the
/// accounts and would overwrite the change.
pub fn reset_password_offline(
    path: PathBuf,
    username: &str,
    new_password: &str,
) -> Result<(), String> {
    let name = normalize_username(username)?;
    password::check_policy(new_password, &name)?;
    let mut store = UserStore::load(path).map_err(|e| e.to_string())?;
    let hash = password::hash(new_password).map_err(|e| e.to_string())?;
    let user = store
        .find_by_name(&name)
        .map(|u| u.id)
        .and_then(|id| store.get_mut(id))
        .ok_or_else(|| format!("no account named '{name}'"))?;
    user.password_hash = hash;
    user.password_temporary = false;
    user.password_changed_at = Some(now_secs());
    user.failed_logins = 0;
    user.locked_at = None;
    let bytes = store.snapshot().map_err(|e| e.to_string())?;
    save_bytes(store.path(), &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    impl LoginOutcome {
        /// The session of a login that needed no second factor.
        fn done(self) -> Login {
            match self {
                LoginOutcome::Complete(login) => login,
                LoginOutcome::SecondFactorRequired { .. } => panic!("2FA was not expected"),
            }
        }
    }

    async fn auth() -> (tempfile::TempDir, Auth) {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path().join("users.json"), AuthConfig::default())
            .await
            .unwrap();
        (dir, auth)
    }

    fn code(auth: &Auth) -> String {
        auth.setup_code.lock().unwrap().clone().unwrap()
    }

    #[tokio::test]
    async fn setup_needs_code_and_happens_once() {
        let (dir, auth) = auth().await;
        assert!(auth.setup_required());
        assert!(matches!(
            auth.setup("0000-0000-0000", "cap", "tidal raven copper lamp")
                .await,
            Err(AuthError::InvalidSetupCode)
        ));
        let c = code(&auth).to_uppercase().replace('-', " ");
        let login = auth
            .setup(&c, "Cap", "tidal raven copper lamp")
            .await
            .unwrap();
        assert_eq!(login.user.role, Role::Captain);
        assert_eq!(login.user.username, "cap");
        assert!(!auth.setup_required());
        assert_eq!(
            auth.authenticate(&login.token).await.map(|a| a.user),
            Some(login.user)
        );
        assert!(matches!(
            auth.setup(&c, "cap2", "tidal raven copper lamp").await,
            Err(AuthError::SetupUnavailable)
        ));

        // A restart with accounts present needs no setup.
        let reopened = Auth::open(dir.path().join("users.json"), AuthConfig::default())
            .await
            .unwrap();
        assert!(!reopened.setup_required());
    }

    #[tokio::test]
    async fn login_roles_and_guards() {
        let (_dir, auth) = auth().await;
        let cap = auth
            .setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap()
            .user;
        let jack = auth
            .add_user("Jack", "rusty anchor violet moon", Role::Pirate, false)
            .await
            .unwrap();
        assert!(matches!(
            auth.add_user("JACK", "rusty anchor violet moon", Role::Pirate, false)
                .await,
            Err(AuthError::UsernameTaken)
        ));
        assert!(matches!(
            auth.login("jack", "wrong password").await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            auth.login("nobody", "rusty anchor violet moon").await,
            Err(AuthError::InvalidCredentials)
        ));
        let login = auth
            .login("JACK", "rusty anchor violet moon")
            .await
            .unwrap()
            .done();
        assert_eq!(login.user, jack);

        // The last captain can be neither demoted nor deleted.
        assert!(matches!(
            auth.set_role(cap.id, Role::Pirate).await,
            Err(AuthError::LastCaptain)
        ));
        assert!(matches!(
            auth.delete(cap.id).await,
            Err(AuthError::LastCaptain)
        ));

        // A password reset ends existing sessions.
        let temporary = auth.reset_password(jack.id).await.unwrap();
        assert_eq!(auth.authenticate(&login.token).await, None);
        assert!(auth.login("jack", &temporary).await.is_ok());

        // Deleting ends sessions too.
        let login = auth.login("jack", &temporary).await.unwrap().done();
        auth.delete(jack.id).await.unwrap();
        assert_eq!(auth.authenticate(&login.token).await, None);
    }

    #[tokio::test]
    async fn own_password_change_requires_current() {
        let (_dir, auth) = auth().await;
        let login = auth
            .setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        assert!(matches!(
            auth.change_own_password(&login.user, "wrong", "quiet orbit maple drum")
                .await,
            Err(AuthError::InvalidCredentials)
        ));
        let fresh = auth
            .change_own_password(
                &login.user,
                "tidal raven copper lamp",
                "quiet orbit maple drum",
            )
            .await
            .unwrap();
        assert_eq!(auth.authenticate(&login.token).await, None);
        assert!(auth.authenticate(&fresh.token).await.is_some());
    }

    #[tokio::test]
    async fn weak_legacy_password_restricts_the_session() {
        // An account whose password predates the policy still logs in, but
        // only into a session that must change the password first.
        let (_dir, auth) = auth().await;
        auth.setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        let weak = password::hash("1234567890").unwrap();
        let jack = {
            let mut store = auth.store.lock().await;
            store.insert("jack".into(), Role::Pirate, weak).unwrap()
        };
        let login = auth.login("jack", "1234567890").await.unwrap().done();
        assert!(login.password_change_required);
        let session = auth.authenticate(&login.token).await.unwrap();
        assert!(session.password_change_required);

        // A weak replacement is refused; a strong one ends the restriction
        // and the restricted session with it.
        assert!(matches!(
            auth.change_own_password(&jack, "1234567890", "0987654321ab")
                .await,
            Err(AuthError::InvalidInput(_))
        ));
        let fresh = auth
            .change_own_password(&jack, "1234567890", "rusty anchor violet moon")
            .await
            .unwrap();
        assert!(!fresh.password_change_required);
        assert_eq!(auth.authenticate(&login.token).await, None);
        let session = auth.authenticate(&fresh.token).await.unwrap();
        assert!(!session.password_change_required);
        let login = auth
            .login("jack", "rusty anchor violet moon")
            .await
            .unwrap()
            .done();
        assert!(!login.password_change_required);
    }

    #[tokio::test]
    async fn weak_passwords_are_refused_everywhere() {
        let (_dir, auth) = auth().await;
        for weak in [
            "1234567890ab",
            "aaaaaaaaaaaa",
            "password1234",
            "cap-cap-cap-cap",
        ] {
            assert!(matches!(
                auth.setup(&code(&auth), "cap", weak).await,
                Err(AuthError::InvalidInput(_))
            ));
        }
        auth.setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        assert!(matches!(
            auth.add_user("jack", "jack12345678", Role::Pirate, false)
                .await,
            Err(AuthError::InvalidInput(_))
        ));
        let jack = auth
            .add_user("jack", "rusty anchor violet moon", Role::Pirate, false)
            .await
            .unwrap();
        assert!(matches!(
            auth.change_own_password(&jack, "rusty anchor violet moon", "qwertyuiop12")
                .await,
            Err(AuthError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn malformed_handles_share_one_throttle_bucket() {
        let (_dir, auth) = auth().await;
        // Garbage handles, including the setup bucket's own key, all count
        // against one fixed key: neither memory nor the setup throttle can
        // be driven from the login form.
        for i in 0..=sessions::FREE_FAILURES as usize {
            let handle = format!("#setup\n{}", "x".repeat(i * 1000));
            assert!(matches!(
                auth.login(&handle, "whatever").await,
                Err(AuthError::InvalidCredentials)
            ));
        }
        assert!(matches!(
            auth.login("#setup", "whatever").await,
            Err(AuthError::Throttled { .. })
        ));
        assert!(matches!(
            auth.setup("0000-0000-0000", "cap", "tidal raven copper lamp")
                .await,
            Err(AuthError::InvalidSetupCode)
        ));
        assert!(matches!(
            auth.login("cap", "whatever").await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    /// A failed login that skips the throttle's waiting, so a test can
    /// reach the lock without sleeping through the backoff.
    async fn fail(auth: &Auth, name: &str) -> AuthError {
        auth.throttle.record_success(name);
        auth.login(name, "wrong guess entirely")
            .await
            .err()
            .unwrap()
    }

    #[tokio::test]
    async fn failed_logins_lock_until_a_captain_unlocks() {
        let (dir, auth) = auth().await;
        let cap = auth
            .setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap()
            .user;
        let jack = auth
            .add_user("jack", "rusty anchor violet moon", Role::Pirate, false)
            .await
            .unwrap();
        let session = auth
            .login("jack", "rusty anchor violet moon")
            .await
            .unwrap()
            .done();

        // Only failures in a row count: a success starts over.
        for _ in 1..MAX_FAILED_LOGINS {
            assert!(matches!(
                fail(&auth, "jack").await,
                AuthError::InvalidCredentials
            ));
        }
        auth.throttle.record_success("jack");
        auth.login("jack", "rusty anchor violet moon")
            .await
            .unwrap()
            .done();
        for _ in 0..MAX_FAILED_LOGINS {
            assert!(matches!(
                fail(&auth, "jack").await,
                AuthError::InvalidCredentials
            ));
        }

        // Locked: the right password is refused too, and only someone who
        // knows it learns why. Unknown handles look like wrong passwords.
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", "rusty anchor violet moon").await,
            Err(AuthError::AccountLocked)
        ));
        assert!(matches!(
            fail(&auth, "jack").await,
            AuthError::InvalidCredentials
        ));
        assert!(matches!(
            fail(&auth, "nobody").await,
            AuthError::InvalidCredentials
        ));
        assert!(
            auth.list()
                .await
                .iter()
                .any(|u| u.id == jack.id && u.locked_at.is_some())
        );
        // Sessions opened before the lock survive, so an attacker locking
        // an account does not also log its owner out.
        assert!(auth.authenticate(&session.token).await.is_some());

        // The lock survives a restart.
        let reopened = Auth::open(dir.path().join("users.json"), AuthConfig::default())
            .await
            .unwrap();
        assert!(matches!(
            reopened.login("jack", "rusty anchor violet moon").await,
            Err(AuthError::AccountLocked)
        ));

        // Changing one's own password from that session does not unlock.
        let own = auth
            .change_own_password(&jack, "rusty anchor violet moon", "quiet orbit maple drum")
            .await
            .unwrap();
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", "quiet orbit maple drum").await,
            Err(AuthError::AccountLocked)
        ));
        assert!(auth.authenticate(&own.token).await.is_some());

        // A captain unlocks, which also resets the count.
        auth.unlock(jack.id).await.unwrap();
        auth.throttle.record_success("jack");
        auth.login("jack", "quiet orbit maple drum")
            .await
            .unwrap()
            .done();
        for _ in 1..MAX_FAILED_LOGINS {
            fail(&auth, "jack").await;
        }
        auth.throttle.record_success("jack");
        auth.login("jack", "quiet orbit maple drum")
            .await
            .unwrap()
            .done();

        // A captain's password reset unlocks too; captains lock like anyone.
        for _ in 0..MAX_FAILED_LOGINS {
            fail(&auth, "cap").await;
        }
        auth.throttle.record_success("cap");
        assert!(matches!(
            auth.login("cap", "tidal raven copper lamp").await,
            Err(AuthError::AccountLocked)
        ));
        let other = auth
            .add_user("anne", "brass kettle under sleet", Role::Captain, false)
            .await
            .unwrap();
        assert_eq!(other.locked_at, None);
        let temporary = auth.reset_password(cap.id).await.unwrap();
        auth.login("cap", &temporary).await.unwrap().done();
        assert!(matches!(
            auth.unlock(UserId(1)).await,
            Err(AuthError::UserNotFound)
        ));
    }

    #[tokio::test]
    async fn offline_unlock() {
        let (dir, auth) = auth().await;
        auth.setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        for _ in 0..MAX_FAILED_LOGINS {
            fail(&auth, "cap").await;
        }
        drop(auth);
        let path = dir.path().join("users.json");
        assert!(unlock_offline(path.clone(), "nobody").is_err());
        unlock_offline(path.clone(), "CAP").unwrap();
        let auth = Auth::open(path, AuthConfig::default()).await.unwrap();
        auth.login("cap", "tidal raven copper lamp")
            .await
            .unwrap()
            .done();
    }

    // ---------- accounts created by a captain ----------

    #[tokio::test]
    async fn new_accounts_get_a_temporary_password_and_follow_the_2fa_policy() {
        let (_dir, auth) = auth().await;
        auth.set_config(AuthConfig {
            two_factor: TwoFactorPolicy::Everyone,
            ..AuthConfig::default()
        });
        auth.setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        let account = auth.create_user("Anne", Role::Pirate).await.unwrap();
        let temporary = account.temporary_password;
        assert!(password::check_policy(&temporary, "anne").is_ok());
        assert!(account.user.password_temporary);

        // First login: only a password change is allowed, and 2FA is due.
        let login = auth.login("anne", &temporary).await.unwrap().done();
        assert!(login.password_change_required);
        let session = auth.authenticate(&login.token).await.unwrap();
        assert!(session.password_change_required);
        assert!(session.two_factor_enrollment_required);
        // The captain knows the temporary password: it can't be kept.
        assert!(matches!(
            auth.change_own_password(&login.user, &temporary, &temporary)
                .await,
            Err(AuthError::InvalidInput(_))
        ));
        auth.throttle.record_success("anne");
        let changed = auth
            .change_own_password(&login.user, &temporary, "quiet orbit maple drum")
            .await
            .unwrap();
        assert!(!changed.password_change_required && !changed.user.password_temporary);
        let session = auth.authenticate(&changed.token).await.unwrap();
        assert!(!session.password_change_required);
        assert!(session.two_factor_enrollment_required);
        assert!(
            auth.two_factor_status(&changed.user)
                .await
                .unwrap()
                .required
        );
        assert!(matches!(
            auth.login("anne", &temporary).await,
            Err(AuthError::InvalidCredentials)
        ));

        // Setting up 2FA lifts the last restriction.
        let enrollment = auth.begin_two_factor(&changed.user).await.unwrap();
        let code = totp::code_for(&enrollment.secret, now_secs());
        let enabled = auth
            .enable_two_factor(&changed.user, "quiet orbit maple drum", &code)
            .await
            .unwrap();
        let session = auth.authenticate(&enabled.login.token).await.unwrap();
        assert!(!session.password_change_required && !session.two_factor_enrollment_required);

        // A captain's 2FA reset makes setting it up due again, as long as
        // the policy requires it.
        let user = auth.reset_two_factor(enabled.login.user.id).await.unwrap();
        assert!(auth.two_factor_enrollment_required(&user));
        auth.set_config(AuthConfig::default());
        assert!(!auth.two_factor_enrollment_required(&user));
    }

    #[tokio::test]
    async fn mandatory_2fa_cannot_be_turned_off_but_can_move_to_a_new_phone() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (old_secret, recovery, login) = enroll(&auth, &jack).await;
        auth.set_config(AuthConfig {
            two_factor: TwoFactorPolicy::Everyone,
            ..AuthConfig::default()
        });
        let code = fresh_code(&auth, jack.id, &old_secret).await;
        assert!(matches!(
            auth.disable_two_factor(&jack, JACK_PW, &code).await,
            Err(AuthError::TwoFactorMandatory)
        ));
        assert!(auth.two_factor_status(&jack).await.unwrap().enabled);

        // New phone, old one lost: a recovery code stands in for it.
        let enrollment = auth.begin_two_factor(&jack).await.unwrap();
        let new_secret = enrollment.secret.clone();
        let new_code = totp::code_for(&new_secret, now_secs());
        let wrong = if new_code == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert!(matches!(
            auth.replace_authenticator(&jack, JACK_PW, &recovery[0], wrong)
                .await,
            Err(AuthError::InvalidSecondFactor)
        ));
        // The typo in the new code did not spend the recovery code.
        let replaced = auth
            .replace_authenticator(&jack, JACK_PW, &recovery[0], &new_code)
            .await
            .unwrap();
        assert_eq!(auth.authenticate(&login.token).await, None);
        assert!(auth.authenticate(&replaced.token).await.is_some());
        let status = auth.two_factor_status(&jack).await.unwrap();
        assert!(status.enabled && status.required);
        assert_eq!(status.recovery_codes_left, recovery.len() - 1);

        // Only the new app's codes log in now.
        auth.throttle.record_success("jack");
        let c = challenge(&auth, "jack", JACK_PW).await;
        let stale = fresh_code(&auth, jack.id, &old_secret).await;
        assert!(matches!(
            auth.login_second_factor(&c, &stale).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        let c = challenge(&auth, "jack", JACK_PW).await;
        let code = fresh_code(&auth, jack.id, &new_secret).await;
        auth.login_second_factor(&c, &code).await.unwrap();
        // The pending secret was used up.
        assert!(matches!(
            auth.replace_authenticator(&jack, JACK_PW, &recovery[1], &new_code)
                .await,
            Err(AuthError::EnrollmentExpired)
        ));
    }

    #[tokio::test]
    async fn confirming_identity_needs_the_code_when_2fa_is_on() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        auth.confirm_identity(&jack, JACK_PW, None).await.unwrap();
        assert!(matches!(
            auth.confirm_identity(&jack, "wrong password here", None)
                .await,
            Err(AuthError::InvalidCredentials)
        ));
        auth.throttle.record_success("jack");
        let (secret, _, _) = enroll(&auth, &jack).await;
        assert!(matches!(
            auth.confirm_identity(&jack, JACK_PW, None).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        let code = fresh_code(&auth, jack.id, &secret).await;
        auth.confirm_identity(&jack, JACK_PW, Some(&code))
            .await
            .unwrap();
        // Spent: the same code can't confirm twice.
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.confirm_identity(&jack, JACK_PW, Some(&code)).await,
            Err(AuthError::InvalidSecondFactor)
        ));
    }

    #[test]
    fn weakening_a_policy() {
        use TwoFactorPolicy::*;
        assert!(Everyone.is_weakened_by(Captains));
        assert!(Captains.is_weakened_by(Optional));
        assert!(!Optional.is_weakened_by(Everyone));
        assert!(!Captains.is_weakened_by(Everyone));
        assert!(!Everyone.is_weakened_by(Everyone));
    }

    #[tokio::test]
    async fn temporary_passwords_expire() {
        let (_dir, auth) = auth().await;
        auth.setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        let account = auth.create_user("anne", Role::Pirate).await.unwrap();
        let id = account.user.id;
        let expires = account.user.temporary_password_expires_at.unwrap();
        assert!(expires >= now_secs() + TEMPORARY_PASSWORD_TTL_SECS - 5);
        {
            let mut store = auth.store.lock().await;
            let user = store.get_mut(id).unwrap();
            user.created_at -= TEMPORARY_PASSWORD_TTL_SECS + 1;
        }
        // Only the right password learns it expired.
        assert!(matches!(
            auth.login("anne", &account.temporary_password).await,
            Err(AuthError::TemporaryPasswordExpired)
        ));
        assert!(matches!(
            auth.login("anne", "wrong password guess").await,
            Err(AuthError::InvalidCredentials)
        ));
        // A reset hands out a fresh one with a fresh deadline.
        let temporary = auth.reset_password(id).await.unwrap();
        auth.throttle.record_success("anne");
        assert!(auth.login("anne", &temporary).await.is_ok());
    }

    #[tokio::test]
    async fn failed_logins_reach_the_disk_in_the_background() {
        let (dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", "wrong password guess").await,
            Err(AuthError::InvalidCredentials)
        ));
        // In effect at once in memory...
        assert_eq!(
            auth.store.lock().await.get(jack.id).unwrap().failed_logins,
            1
        );
        // ...and saved shortly after.
        let path = dir.path().join("users.json");
        for _ in 0..200 {
            let saved = UserStore::load(path.clone()).unwrap();
            if saved.get(jack.id).unwrap().failed_logins == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the failed login was never saved");
    }

    #[tokio::test]
    async fn an_older_snapshot_never_overwrites_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let mut store = UserStore::load(path.clone()).unwrap();
        let saver = Saver::default();
        let old = saver.snapshot(&store).unwrap();
        store
            .insert("cap".into(), Role::Captain, "h".into())
            .unwrap();
        let new = saver.snapshot(&store).unwrap();
        saver.write(new).await.unwrap();
        saver.write(old).await.unwrap();
        assert!(UserStore::load(path).unwrap().find_by_name("cap").is_some());
    }

    #[test]
    fn hash_queue_tickets_are_capped_and_released() {
        let queue = AtomicUsize::new(0);
        let tickets: Vec<_> = (0..3)
            .map(|_| QueueTicket::take(&queue, 3).unwrap())
            .collect();
        assert!(QueueTicket::take(&queue, 3).is_none());
        assert_eq!(queue.load(Ordering::SeqCst), 3);
        drop(tickets);
        assert_eq!(queue.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn captain_reset_issues_a_temporary_password() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let temporary = auth.reset_password(jack.id).await.unwrap();
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Err(AuthError::InvalidCredentials)
        ));
        let login = auth.login("jack", &temporary).await.unwrap().done();
        assert!(login.password_change_required && login.user.password_temporary);
        // Accounts made before this feature don't get 2FA forced by a reset.
        let session = auth.authenticate(&login.token).await.unwrap();
        assert!(!session.two_factor_enrollment_required);

        // With 2FA on, the code still comes before the password change.
        let changed = auth
            .change_own_password(&login.user, &temporary, "quiet orbit maple drum")
            .await
            .unwrap();
        let enrollment = auth.begin_two_factor(&changed.user).await.unwrap();
        let code = totp::code_for(&enrollment.secret, now_secs());
        auth.enable_two_factor(&changed.user, "quiet orbit maple drum", &code)
            .await
            .unwrap();
        let temporary = auth.reset_password(jack.id).await.unwrap();
        let c = challenge(&auth, "jack", &temporary).await;
        auth.store
            .lock()
            .await
            .get_mut(jack.id)
            .unwrap()
            .totp
            .as_mut()
            .unwrap()
            .last_step = 0;
        let login = auth
            .login_second_factor(&c, &totp::code_for(&enrollment.secret, now_secs()))
            .await
            .unwrap();
        assert!(login.password_change_required);
    }

    // ---------- two-factor login ----------

    const JACK_PW: &str = "rusty anchor violet moon";

    /// A captain and a pirate `jack`, plus a session for jack.
    async fn crew(auth: &Auth) -> (User, Login) {
        auth.setup(&code(auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        auth.add_user("jack", JACK_PW, Role::Pirate, false)
            .await
            .unwrap();
        let login = auth.login("jack", JACK_PW).await.unwrap().done();
        (login.user.clone(), login)
    }

    /// Turn on 2FA for `user`; returns the base32 secret and recovery codes.
    async fn enroll(auth: &Auth, user: &User) -> (String, Vec<String>, Login) {
        let enrollment = auth.begin_two_factor(user).await.unwrap();
        let code = totp::code_for(&enrollment.secret, now_secs());
        let enabled = auth.enable_two_factor(user, JACK_PW, &code).await.unwrap();
        (enrollment.secret, enabled.recovery_codes, enabled.login)
    }

    /// A code the server has not seen yet: tests log in more often than
    /// once per 30 s, so forget the last used step first.
    async fn fresh_code(auth: &Auth, id: UserId, secret: &str) -> String {
        let mut store = auth.store.lock().await;
        store.get_mut(id).unwrap().totp.as_mut().unwrap().last_step = 0;
        totp::code_for(secret, now_secs())
    }

    async fn challenge(auth: &Auth, name: &str, password: &str) -> String {
        auth.throttle.record_success(name);
        match auth.login(name, password).await.unwrap() {
            LoginOutcome::SecondFactorRequired { challenge, .. } => challenge,
            LoginOutcome::Complete(_) => panic!("expected a second factor"),
        }
    }

    #[tokio::test]
    async fn enabling_needs_password_code_and_a_fresh_qr() {
        let (_dir, auth) = auth().await;
        let (jack, session) = crew(&auth).await;
        assert!(matches!(
            auth.enable_two_factor(&jack, JACK_PW, "123456").await,
            Err(AuthError::EnrollmentExpired)
        ));
        let enrollment = auth.begin_two_factor(&jack).await.unwrap();
        assert_eq!(enrollment.account, "jack");
        assert!(
            enrollment
                .uri
                .starts_with("otpauth://totp/Inkult:jack?secret=")
        );
        let code = totp::code_for(&enrollment.secret, now_secs());
        assert!(matches!(
            auth.enable_two_factor(&jack, "wrong password here", &code)
                .await,
            Err(AuthError::InvalidCredentials)
        ));
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(matches!(
            auth.enable_two_factor(&jack, JACK_PW, wrong).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        // Starting over replaces the secret: the first QR code is dead.
        let second = auth.begin_two_factor(&jack).await.unwrap();
        assert_ne!(second.secret, enrollment.secret);
        auth.throttle.record_success("jack");
        let code = totp::code_for(&second.secret, now_secs());
        let enabled = auth.enable_two_factor(&jack, JACK_PW, &code).await.unwrap();
        assert_eq!(enabled.recovery_codes.len(), totp::RECOVERY_CODES);
        assert!(enabled.login.user.two_factor_enabled);

        // Sessions opened without the second factor end; the new one lives.
        assert_eq!(auth.authenticate(&session.token).await, None);
        assert!(auth.authenticate(&enabled.login.token).await.is_some());
        // A second enable is refused (a new phone goes through
        // `replace_authenticator`).
        let again = auth.begin_two_factor(&jack).await.unwrap();
        let code = totp::code_for(&again.secret, now_secs());
        assert!(matches!(
            auth.enable_two_factor(&jack, JACK_PW, &code).await,
            Err(AuthError::TwoFactorAlreadyEnabled)
        ));
        let status = auth.two_factor_status(&jack).await.unwrap();
        assert!(status.enabled && !status.required);
        assert_eq!(status.recovery_codes_left, totp::RECOVERY_CODES);
    }

    #[tokio::test]
    async fn login_takes_a_code_once() {
        let (dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (secret, recovery, _) = enroll(&auth, &jack).await;

        // The password alone gives no session.
        let c = challenge(&auth, "jack", JACK_PW).await;
        assert_eq!(auth.authenticate(&c).await, None);
        let code = fresh_code(&auth, jack.id, &secret).await;
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(matches!(
            auth.login_second_factor(&c, wrong).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        let login = auth.login_second_factor(&c, &code).await.unwrap();
        assert!(auth.authenticate(&login.token).await.is_some());
        // The challenge is spent, and the code can't be replayed.
        assert!(matches!(
            auth.login_second_factor(&c, &code).await,
            Err(AuthError::ChallengeExpired)
        ));
        let c = challenge(&auth, "jack", JACK_PW).await;
        assert!(matches!(
            auth.login_second_factor(&c, &code).await,
            Err(AuthError::InvalidSecondFactor)
        ));

        // A recovery code works instead, once.
        auth.throttle.record_success("jack");
        auth.login_second_factor(&c, &recovery[0]).await.unwrap();
        let c = challenge(&auth, "jack", JACK_PW).await;
        assert!(matches!(
            auth.login_second_factor(&c, &recovery[0]).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        let status = auth.two_factor_status(&jack).await.unwrap();
        assert_eq!(status.recovery_codes_left, totp::RECOVERY_CODES - 1);

        // Used codes stay used after a restart; 2FA stays on.
        drop(auth);
        let auth = Auth::open(dir.path().join("users.json"), AuthConfig::default())
            .await
            .unwrap();
        let c = challenge(&auth, "jack", JACK_PW).await;
        assert!(matches!(
            auth.login_second_factor(&c, &code).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        assert!(matches!(
            auth.login_second_factor(&c, &recovery[0]).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        auth.login_second_factor(&c, &recovery[1]).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_password_never_reaches_the_code_step() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        enroll(&auth, &jack).await;
        assert!(matches!(
            auth.login("jack", "wrong password here").await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            auth.login_second_factor(&"ab".repeat(32), "123456").await,
            Err(AuthError::ChallengeExpired)
        ));
    }

    #[tokio::test]
    async fn challenge_ends_after_too_many_wrong_codes() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (secret, _, _) = enroll(&auth, &jack).await;
        let c = challenge(&auth, "jack", JACK_PW).await;
        let code = fresh_code(&auth, jack.id, &secret).await;
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for i in 1..=sessions::CHALLENGE_ATTEMPTS {
            auth.throttle.record_success("jack");
            let err = auth.login_second_factor(&c, wrong).await.err().unwrap();
            if i < sessions::CHALLENGE_ATTEMPTS {
                assert!(matches!(err, AuthError::InvalidSecondFactor), "{err}");
            } else {
                assert!(matches!(err, AuthError::ChallengeExpired), "{err}");
            }
        }
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login_second_factor(&c, &code).await,
            Err(AuthError::ChallengeExpired)
        ));
    }

    #[tokio::test]
    async fn wrong_codes_lock_the_account_and_passwords_dont_reset_the_count() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (secret, _, _) = enroll(&auth, &jack).await;
        let code = fresh_code(&auth, jack.id, &secret).await;
        let wrong = if code == "000000" { "111111" } else { "000000" };
        // Someone who knows the password re-enters it between guesses: the
        // right password must not reset the count of wrong codes.
        for _ in 0..MAX_FAILED_LOGINS - 1 {
            let c = challenge(&auth, "jack", JACK_PW).await;
            auth.throttle.record_success("jack");
            assert!(matches!(
                auth.login_second_factor(&c, wrong).await,
                Err(AuthError::InvalidSecondFactor)
            ));
        }
        let c = challenge(&auth, "jack", JACK_PW).await;
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login_second_factor(&c, wrong).await,
            Err(AuthError::AccountLocked)
        ));
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Err(AuthError::AccountLocked)
        ));

        // Unlocked, a right code resets the count.
        auth.unlock(jack.id).await.unwrap();
        let c = challenge(&auth, "jack", JACK_PW).await;
        let code = fresh_code(&auth, jack.id, &secret).await;
        auth.login_second_factor(&c, &code).await.unwrap();
        assert_eq!(
            auth.store.lock().await.get(jack.id).unwrap().failed_logins,
            0
        );
    }

    #[tokio::test]
    async fn code_attempts_are_throttled_with_the_password() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (secret, _, _) = enroll(&auth, &jack).await;
        let code = fresh_code(&auth, jack.id, &secret).await;
        let wrong = if code == "000000" { "111111" } else { "000000" };
        // One password step plus a challenge's worth of wrong codes uses up
        // the free attempts: the next password entry has to wait.
        let c = challenge(&auth, "jack", JACK_PW).await;
        for _ in 0..sessions::CHALLENGE_ATTEMPTS {
            assert!(auth.login_second_factor(&c, wrong).await.is_err());
        }
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Err(AuthError::Throttled { .. })
        ));
    }

    #[tokio::test]
    async fn disabling_and_new_recovery_codes_need_password_and_code() {
        let (_dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (secret, old_codes, login) = enroll(&auth, &jack).await;

        let code = fresh_code(&auth, jack.id, &secret).await;
        assert!(matches!(
            auth.regenerate_recovery_codes(&jack, JACK_PW, "not a code")
                .await,
            Err(AuthError::InvalidSecondFactor)
        ));
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.regenerate_recovery_codes(&jack, "wrong password here", &code)
                .await,
            Err(AuthError::InvalidCredentials)
        ));
        auth.throttle.record_success("jack");
        let new_codes = auth
            .regenerate_recovery_codes(&jack, JACK_PW, &code)
            .await
            .unwrap();
        assert_ne!(new_codes, old_codes);
        let c = challenge(&auth, "jack", JACK_PW).await;
        assert!(matches!(
            auth.login_second_factor(&c, &old_codes[0]).await,
            Err(AuthError::InvalidSecondFactor)
        ));

        // Disabling: a recovery code counts as the second factor too.
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.disable_two_factor(&jack, JACK_PW, &old_codes[1]).await,
            Err(AuthError::InvalidSecondFactor)
        ));
        auth.throttle.record_success("jack");
        auth.disable_two_factor(&jack, JACK_PW, &new_codes[0])
            .await
            .unwrap();
        assert!(auth.authenticate(&login.token).await.is_some());
        assert!(matches!(
            auth.disable_two_factor(&jack, JACK_PW, &new_codes[1]).await,
            Err(AuthError::TwoFactorNotEnabled)
        ));
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Ok(LoginOutcome::Complete(_))
        ));
    }

    #[tokio::test]
    async fn captain_reset_ends_sessions_and_pending_logins() {
        let (dir, auth) = auth().await;
        let (jack, _) = crew(&auth).await;
        let (_, _, login) = enroll(&auth, &jack).await;
        let c = challenge(&auth, "jack", JACK_PW).await;
        let user = auth.reset_two_factor(jack.id).await.unwrap();
        assert!(!user.two_factor_enabled);
        assert_eq!(auth.authenticate(&login.token).await, None);
        assert!(matches!(
            auth.login_second_factor(&c, "123456").await,
            Err(AuthError::ChallengeExpired)
        ));
        assert!(matches!(
            auth.reset_two_factor(jack.id).await,
            Err(AuthError::TwoFactorNotEnabled)
        ));
        auth.throttle.record_success("jack");
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Ok(LoginOutcome::Complete(_))
        ));

        // Offline: same effect, server stopped.
        enroll(&auth, &jack).await;
        drop(auth);
        let path = dir.path().join("users.json");
        assert!(reset_two_factor_offline(path.clone(), "nobody").is_err());
        reset_two_factor_offline(path.clone(), "JACK").unwrap();
        assert!(reset_two_factor_offline(path.clone(), "jack").is_err());
        let auth = Auth::open(path, AuthConfig::default()).await.unwrap();
        assert!(matches!(
            auth.login("jack", JACK_PW).await,
            Ok(LoginOutcome::Complete(_))
        ));
    }

    #[tokio::test]
    async fn policy_restricts_sessions_until_enrolled() {
        let dir = tempfile::tempdir().unwrap();
        let config = AuthConfig {
            two_factor: TwoFactorPolicy::Captains,
            totp_issuer: "Inkult (home)".into(),
        };
        let auth = Auth::open(dir.path().join("users.json"), config)
            .await
            .unwrap();
        let cap = auth
            .setup(&code(&auth), "cap", "tidal raven copper lamp")
            .await
            .unwrap();
        let session = auth.authenticate(&cap.token).await.unwrap();
        assert!(session.two_factor_enrollment_required);
        let pirate = auth
            .add_user("jack", JACK_PW, Role::Pirate, false)
            .await
            .unwrap();
        assert!(!auth.two_factor_enrollment_required(&pirate));

        let enrollment = auth.begin_two_factor(&cap.user).await.unwrap();
        assert!(
            enrollment
                .uri
                .starts_with("otpauth://totp/Inkult%20%28home%29:cap?")
        );
        let code = totp::code_for(&enrollment.secret, now_secs());
        let enabled = auth
            .enable_two_factor(&cap.user, "tidal raven copper lamp", &code)
            .await
            .unwrap();
        let session = auth.authenticate(&enabled.login.token).await.unwrap();
        assert!(!session.two_factor_enrollment_required);
        assert!(auth.two_factor_status(&cap.user).await.unwrap().required);
    }

    #[test]
    fn offline_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let mut store = UserStore::load(path.clone()).unwrap();
        store
            .insert(
                "cap".into(),
                Role::Captain,
                password::hash("salt marsh lantern fog").unwrap(),
            )
            .unwrap();
        save_bytes(&path, &store.snapshot().unwrap()).unwrap();
        assert!(
            reset_password_offline(path.clone(), "nobody", "cobalt hinge wander seven").is_err()
        );
        assert!(reset_password_offline(path.clone(), "cap", "short").is_err());
        store
            .get_mut(store.find_by_name("cap").unwrap().id)
            .unwrap()
            .locked_at = Some(1);
        save_bytes(&path, &store.snapshot().unwrap()).unwrap();
        reset_password_offline(path.clone(), "CAP", "cobalt hinge wander seven").unwrap();
        let store = UserStore::load(path).unwrap();
        assert_eq!(store.find_by_name("cap").unwrap().locked_at, None);
        let hash = &store.find_by_name("cap").unwrap().password_hash;
        assert!(password::verify("cobalt hinge wander seven", hash));
    }
}
