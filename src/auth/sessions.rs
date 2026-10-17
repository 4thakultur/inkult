//! Login sessions and failed-login throttling, both in memory.
//!
//! Sessions are deliberately not persisted: a restart logs everyone out,
//! which is a small price for never having live session tokens on disk.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::password;
use super::users::UserId;

/// 256-bit random token, hex encoded; the session cookie's value.
pub const TOKEN_BYTES: usize = 32;
/// Cap on live sessions so a login loop cannot grow memory without bound.
const MAX_SESSIONS: usize = 10_000;
/// Cap per user, so one account logging in over and over can't push
/// everyone else's sessions out of the global cap. Its own least recently
/// used session goes first.
pub(super) const MAX_SESSIONS_PER_USER: usize = 64;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy)]
pub struct SessionPolicy {
    /// A session unused for this long expires.
    pub idle: Duration,
    /// A session expires this long after login however active it is.
    pub absolute: Duration,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(7 * 24 * 3600),
            absolute: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

struct Session {
    user: UserId,
    created: Instant,
    last_seen: Instant,
    password_change_required: bool,
}

/// What a live session token stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionGrant {
    pub user: UserId,
    /// The login used a password that no longer meets the policy: the
    /// session may only be used to choose a new one.
    pub password_change_required: bool,
}

pub struct Sessions {
    policy: SessionPolicy,
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn new(policy: SessionPolicy) -> Self {
        Self {
            policy,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn max_age(&self) -> Duration {
        self.policy.absolute
    }

    fn expired(&self, s: &Session, now: Instant) -> bool {
        now.duration_since(s.last_seen) > self.policy.idle
            || now.duration_since(s.created) > self.policy.absolute
    }

    /// Start a session and return its token.
    pub fn create(&self, user: UserId, password_change_required: bool) -> String {
        let token = password::random_hex(TOKEN_BYTES);
        let now = Instant::now();
        let mut map = lock(&self.map);
        map.retain(|_, s| !self.expired(s, now));
        let own = map.values().filter(|s| s.user == user).count();
        let evict = if own >= MAX_SESSIONS_PER_USER {
            map.iter()
                .filter(|(_, s)| s.user == user)
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(t, _)| t.clone())
        } else if map.len() >= MAX_SESSIONS {
            map.iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(t, _)| t.clone())
        } else {
            None
        };
        if let Some(token) = evict {
            map.remove(&token);
        }
        map.insert(
            token.clone(),
            Session {
                user,
                created: now,
                last_seen: now,
                password_change_required,
            },
        );
        token
    }

    /// What a token grants, refreshing its idle timer.
    pub fn resolve(&self, token: &str) -> Option<SessionGrant> {
        if token.len() != TOKEN_BYTES * 2 {
            return None;
        }
        let now = Instant::now();
        let mut map = lock(&self.map);
        let session = map.get_mut(token)?;
        if self.expired(session, now) {
            map.remove(token);
            return None;
        }
        session.last_seen = now;
        Some(SessionGrant {
            user: session.user,
            password_change_required: session.password_change_required,
        })
    }

    pub fn revoke(&self, token: &str) {
        lock(&self.map).remove(token);
    }

    /// End every session of a user (password change, deletion).
    pub fn revoke_user(&self, user: UserId) {
        lock(&self.map).retain(|_, s| s.user != user);
    }

    /// How many distinct users have used a live session within `window`.
    /// A user signed in on several browsers counts once.
    pub fn active_users(&self, window: Duration) -> usize {
        let now = Instant::now();
        let map = lock(&self.map);
        map.values()
            .filter(|s| !self.expired(s, now) && now.duration_since(s.last_seen) < window)
            .map(|s| s.user)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }
}

/// How long the second login step may take after the password was right.
const CHALLENGE_TTL: Duration = Duration::from_secs(300);
/// Wrong codes allowed per challenge before the password must be entered
/// again. The account's failed-login count and lock still apply on top.
pub const CHALLENGE_ATTEMPTS: u32 = 5;
const MAX_CHALLENGES: usize = 10_000;
/// Pending logins per user: someone who knows one password can't crowd
/// out other users' code steps.
pub(super) const MAX_CHALLENGES_PER_USER: usize = 8;

struct Challenge {
    user: UserId,
    username: String,
    password_change_required: bool,
    created: Instant,
    attempts: u32,
}

/// Who a second-factor attempt is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeGrant {
    pub user: UserId,
    pub username: String,
    pub password_change_required: bool,
    /// This was the challenge's last allowed attempt: it is gone now.
    pub last_attempt: bool,
}

/// Logins whose password was right and that wait for a second factor.
/// The token proves the password step happened, without being a session:
/// it opens nothing but [`Challenges::attempt`].
#[derive(Default)]
pub struct Challenges {
    map: Mutex<HashMap<String, Challenge>>,
}

impl Challenges {
    pub fn ttl(&self) -> Duration {
        CHALLENGE_TTL
    }

    pub fn create(&self, user: UserId, username: &str, password_change_required: bool) -> String {
        let token = password::random_hex(TOKEN_BYTES);
        let now = Instant::now();
        let mut map = lock(&self.map);
        map.retain(|_, c| now.duration_since(c.created) < CHALLENGE_TTL);
        let own = map.values().filter(|c| c.user == user).count();
        let evict = if own >= MAX_CHALLENGES_PER_USER {
            map.iter()
                .filter(|(_, c)| c.user == user)
                .min_by_key(|(_, c)| c.created)
                .map(|(t, _)| t.clone())
        } else if map.len() >= MAX_CHALLENGES {
            map.iter()
                .min_by_key(|(_, c)| c.created)
                .map(|(t, _)| t.clone())
        } else {
            None
        };
        if let Some(token) = evict {
            map.remove(&token);
        }
        map.insert(
            token.clone(),
            Challenge {
                user,
                username: username.to_owned(),
                password_change_required,
                created: now,
                attempts: 0,
            },
        );
        token
    }

    /// Reserve one attempt on a challenge. Counted before the code is
    /// checked so parallel guesses can't exceed the limit; the challenge
    /// is dropped with its last attempt. `None` when unknown or expired.
    pub fn attempt(&self, token: &str) -> Option<ChallengeGrant> {
        if token.len() != TOKEN_BYTES * 2 {
            return None;
        }
        let now = Instant::now();
        let mut map = lock(&self.map);
        let challenge = map.get_mut(token)?;
        if now.duration_since(challenge.created) >= CHALLENGE_TTL {
            map.remove(token);
            return None;
        }
        challenge.attempts += 1;
        let grant = ChallengeGrant {
            user: challenge.user,
            username: challenge.username.clone(),
            password_change_required: challenge.password_change_required,
            last_attempt: challenge.attempts >= CHALLENGE_ATTEMPTS,
        };
        if grant.last_attempt {
            map.remove(token);
        }
        Some(grant)
    }

    pub fn remove(&self, token: &str) {
        lock(&self.map).remove(token);
    }

    /// Drop a user's pending logins (password change, 2FA reset, deletion).
    pub fn revoke_user(&self, user: UserId) {
        lock(&self.map).retain(|_, c| c.user != user);
    }
}

/// How a [`LoginThrottle`] slows attempts down.
#[derive(Debug, Clone, Copy)]
pub struct ThrottlePolicy {
    /// Attempts allowed before each further one must wait.
    pub free: u32,
    /// Longest wait between attempts.
    pub max_delay: Duration,
    /// A key's record is forgotten after this long without attempts.
    pub forget_after: Duration,
    /// Keys tracked at most; the stalest record makes room for a new one.
    pub max_tracked: usize,
}

/// Per login name: a handful of guesses, then about a dozen an hour. The
/// account lock (10 failures in a row) is the hard cap behind it.
pub const PER_HANDLE: ThrottlePolicy = ThrottlePolicy {
    free: FREE_FAILURES,
    max_delay: Duration::from_secs(300),
    forget_after: Duration::from_secs(3600),
    max_tracked: 10_000,
};

/// Failures allowed per handle before attempts are slowed down.
pub(super) const FREE_FAILURES: u32 = 5;

struct Failures {
    count: u32,
    last: Instant,
}

/// Slows down guessing per key: a login name, or a client address. After
/// a few attempts each further one must wait exponentially longer (capped),
/// so an attacker gets a handful of guesses per hour while the real user
/// is never locked out for long. Unknown names are tracked the same way so
/// the throttle does not reveal which accounts exist.
///
/// An attempt is counted when it *starts* ([`Self::attempt`]) and forgiven
/// on success. Checking first and counting after the slow password hash
/// would let a burst of parallel requests all pass the check before any of
/// them is recorded.
///
/// When the table is full, the record idle the longest is dropped to make
/// room. Refusing new keys instead would let anyone lock every user out
/// by filling the table with made-up names (a few thousand requests).
/// Dropping a record can at worst return a few guesses to a name an
/// attacker spent thousands of requests flushing, and the account lock
/// still caps those for good.
pub struct LoginThrottle {
    policy: ThrottlePolicy,
    map: Mutex<HashMap<String, Failures>>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new(PER_HANDLE)
    }
}

impl LoginThrottle {
    pub fn new(policy: ThrottlePolicy) -> Self {
        Self {
            policy,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Reserve one attempt for `key`, or `Err(seconds)` to wait first.
    pub fn attempt(&self, key: &str) -> Result<(), u64> {
        let now = Instant::now();
        let policy = self.policy;
        let mut map = lock(&self.map);
        if let Some(f) = map.get_mut(key) {
            if now.duration_since(f.last) >= policy.forget_after {
                f.count = 0;
            }
            let over = f.count.saturating_sub(policy.free);
            if over > 0 {
                // 1 s, 2 s, 4 s, … for each attempt past the free ones.
                let delay = Duration::from_secs(1u64 << (over - 1).min(16)).min(policy.max_delay);
                let waited = now.duration_since(f.last);
                if waited < delay {
                    return Err((delay - waited).as_secs().max(1));
                }
            }
            f.count = f.count.saturating_add(1);
            f.last = now;
            return Ok(());
        }
        if map.len() >= policy.max_tracked {
            map.retain(|_, f| now.duration_since(f.last) < policy.forget_after);
            if map.len() >= policy.max_tracked
                && let Some(stalest) = map
                    .iter()
                    .min_by_key(|(_, f)| f.last)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&stalest);
            }
        }
        map.insert(
            key.to_owned(),
            Failures {
                count: 1,
                last: now,
            },
        );
        Ok(())
    }

    /// The attempt succeeded: forget the key's failures.
    pub fn record_success(&self, key: &str) {
        lock(&self.map).remove(key);
    }

    /// Hand back one reserved attempt: it succeeded, or it was turned away
    /// before anything was checked. Unlike [`Self::record_success`], other
    /// failures stay counted (for a client address: one good account must
    /// not wipe the record of guesses at others).
    pub fn forgive(&self, key: &str) {
        if let Some(f) = lock(&self.map).get_mut(key) {
            f.count = f.count.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_resolve_expire_and_revoke() {
        let sessions = Sessions::new(SessionPolicy::default());
        let user = |s: &Sessions, t: &str| s.resolve(t).map(|g| g.user);
        let a = sessions.create(UserId(1), false);
        let b = sessions.create(UserId(1), false);
        let c = sessions.create(UserId(2), true);
        assert_eq!(a.len(), 64);
        assert_eq!(user(&sessions, &a), Some(UserId(1)));
        assert_eq!(user(&sessions, "nope"), None);
        assert_eq!(
            sessions.resolve(&c),
            Some(SessionGrant {
                user: UserId(2),
                password_change_required: true
            })
        );
        sessions.revoke(&a);
        assert_eq!(user(&sessions, &a), None);
        sessions.revoke_user(UserId(1));
        assert_eq!(user(&sessions, &b), None);
        assert_eq!(user(&sessions, &c), Some(UserId(2)));

        let short = Sessions::new(SessionPolicy {
            idle: Duration::ZERO,
            absolute: Duration::from_secs(60),
        });
        let t = short.create(UserId(3), false);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.resolve(&t), None);
    }

    #[test]
    fn active_users_counts_each_user_once() {
        let sessions = Sessions::new(SessionPolicy::default());
        assert_eq!(sessions.active_users(Duration::from_secs(60)), 0);
        sessions.create(UserId(1), false);
        sessions.create(UserId(1), false);
        let b = sessions.create(UserId(2), false);
        assert_eq!(sessions.active_users(Duration::from_secs(60)), 2);
        sessions.revoke(&b);
        assert_eq!(sessions.active_users(Duration::from_secs(60)), 1);
        assert_eq!(sessions.active_users(Duration::ZERO), 0);
    }

    #[test]
    fn challenges_allow_limited_attempts() {
        let challenges = Challenges::default();
        let token = challenges.create(UserId(7), "jack", true);
        assert_eq!(challenges.attempt("short"), None);
        for i in 1..=CHALLENGE_ATTEMPTS {
            let grant = challenges.attempt(&token).unwrap();
            assert_eq!(grant.user, UserId(7));
            assert_eq!(grant.username, "jack");
            assert!(grant.password_change_required);
            assert_eq!(grant.last_attempt, i == CHALLENGE_ATTEMPTS);
        }
        assert_eq!(challenges.attempt(&token), None);

        let a = challenges.create(UserId(7), "jack", false);
        let b = challenges.create(UserId(8), "anne", false);
        challenges.revoke_user(UserId(7));
        assert_eq!(challenges.attempt(&a), None);
        assert!(challenges.attempt(&b).is_some());
        challenges.remove(&b);
        assert_eq!(challenges.attempt(&b), None);
    }

    #[test]
    fn throttle_counts_attempts_before_they_finish() {
        // Parallel requests all reserve before any verifies a password, so
        // a burst gets no more guesses than sequential requests would.
        let throttle = std::sync::Arc::new(LoginThrottle::default());
        let passed = (0..64)
            .map(|_| {
                let t = std::sync::Arc::clone(&throttle);
                std::thread::spawn(move || t.attempt("jack").is_ok())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|ok| *ok)
            .count();
        assert_eq!(passed, FREE_FAILURES as usize + 1);
    }

    #[test]
    fn a_full_throttle_makes_room_instead_of_locking_everyone_out() {
        // Filling the table with made-up names once refused every new
        // name, the real users' included, for an hour.
        let throttle = LoginThrottle::new(ThrottlePolicy {
            max_tracked: 100,
            ..PER_HANDLE
        });
        for _ in 0..=FREE_FAILURES {
            throttle.attempt("victim").unwrap();
        }
        for i in 0..1000 {
            assert!(throttle.attempt(&format!("made-up-{i}")).is_ok());
        }
        assert!(throttle.attempt("cap").is_ok());
        assert!(lock(&throttle.map).len() <= 100);
    }

    #[test]
    fn forgive_hands_back_one_attempt() {
        let throttle = LoginThrottle::new(ThrottlePolicy {
            free: 2,
            ..PER_HANDLE
        });
        // `free + 1` attempts pass before the first wait; a forgiven one
        // doesn't count towards them.
        for _ in 0..3 {
            throttle.attempt("10.0.0.1").unwrap();
        }
        throttle.forgive("10.0.0.1");
        throttle.attempt("10.0.0.1").unwrap();
        assert!(throttle.attempt("10.0.0.1").is_err());
        throttle.forgive("nobody");
    }

    #[test]
    fn one_user_cannot_crowd_out_other_sessions_or_challenges() {
        let sessions = Sessions::new(SessionPolicy::default());
        let other = sessions.create(UserId(2), false);
        let first = sessions.create(UserId(1), false);
        for _ in 0..MAX_SESSIONS_PER_USER * 3 {
            sessions.create(UserId(1), false);
        }
        assert!(sessions.resolve(&other).is_some());
        assert!(sessions.resolve(&first).is_none());
        assert_eq!(
            lock(&sessions.map)
                .values()
                .filter(|s| s.user == UserId(1))
                .count(),
            MAX_SESSIONS_PER_USER
        );

        let challenges = Challenges::default();
        let other = challenges.create(UserId(2), "anne", false);
        for _ in 0..MAX_CHALLENGES_PER_USER * 3 {
            challenges.create(UserId(1), "jack", false);
        }
        assert!(challenges.attempt(&other).is_some());
        assert_eq!(lock(&challenges.map).len(), MAX_CHALLENGES_PER_USER + 1);
    }

    #[test]
    fn throttle_kicks_in_after_free_failures() {
        let throttle = LoginThrottle::default();
        for _ in 0..=FREE_FAILURES {
            assert!(throttle.attempt("jack").is_ok());
        }
        assert!(throttle.attempt("jack").is_err());
        assert!(throttle.attempt("anne").is_ok());
        throttle.record_success("jack");
        assert!(throttle.attempt("jack").is_ok());
    }
}
