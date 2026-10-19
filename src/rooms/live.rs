//! Live state shared by every open Inkult tab: who is online, and nudges
//! telling tabs that something they show has changed.
//!
//! Each tab keeps a WebSocket open (`/api/live`, see `web::live`). The open
//! socket *is* the tab's presence: it is `online` while the tab is on
//! screen and `idle` while it is hidden, and it is gone the moment the
//! socket closes (tab closed, logout, crash, lost network), so no timer has
//! to guess. A user is as present as their most present tab, and status
//! belongs to the user, so every room shows the same thing.
//!
//! Where a proxy refuses WebSockets, tabs fall back to reporting over
//! plain requests with a heartbeat ([`PresenceBoard::report`]); those
//! reports expire if the heartbeat stops.
//!
//! Nudges carry no data, only who should look again; the tabs then fetch
//! what changed through the normal, permission-checked endpoints. That way
//! a nudge can never leak a private room's content.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, broadcast};

use crate::auth::UserId;

/// A fallback tab heartbeats every 15 s; this allows for two late beats.
const ONLINE_EXPIRY: Duration = Duration::from_secs(45);
/// Browsers slow down the timers of hidden tabs (Chrome to once a minute
/// after five minutes), so a hidden tab's heartbeat can be ~60 s apart.
const IDLE_EXPIRY: Duration = Duration::from_secs(150);
/// Tabs tracked per user; beyond this the stalest fallback tab is dropped,
/// or a new socket is refused.
const MAX_TABS_PER_USER: usize = 16;
const MAX_TAB_ID_CHARS: usize = 64;
/// Nudges waiting for a slow socket; one that falls behind just refreshes.
const NUDGE_BUFFER: usize = 64;
/// Open sockets on the whole server. Upgraded connections leave the HTTP
/// server's connection caps, so they are bounded here instead.
const MAX_SOCKETS: usize = 512;
/// Sockets a user may open per [`RATE_WINDOW`]. A tab opens one per page
/// load and reconnects with backoff, so only a script opens more; each
/// open and close tells every tab to refresh, which a loop would turn
/// into load on everyone.
const MAX_CONNECTS: usize = 30;
/// Fallback status reports per user per [`RATE_WINDOW`] (a heartbeat every
/// 15 s per tab, plus one per visibility change), for the same reason.
const MAX_REPORTS: usize = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Where a member is right now, best first; member lists sort by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Inkult is open and on screen.
    Online,
    /// Inkult is open, but not on screen.
    Idle,
    Offline,
}

/// Who a nudge is for.
#[derive(Debug, Clone)]
pub enum Audience {
    Everyone,
    /// Members of a private room: nobody else may learn it changed.
    Only(Arc<[UserId]>),
}

impl Audience {
    pub fn includes(&self, user: UserId) -> bool {
        match self {
            Self::Everyone => true,
            Self::Only(users) => users.contains(&user),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("invalid tab id")]
    InvalidTab,
    #[error("too many open Inkult tabs for this account")]
    TooManyTabs,
    #[error("too many live connections on this server; try again later")]
    ServerFull,
    #[error("too many status updates; slow down")]
    Throttled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabId {
    /// Held by an open socket; never expires.
    Socket(u64),
    /// Reported over plain requests; expires without a heartbeat.
    Reported(String),
}

struct Tab {
    id: TabId,
    /// Hash of the session token, so a logout can drop the session's tabs
    /// without the token itself being kept here.
    session: [u8; 32],
    status: Status,
    at: Instant,
    /// Wakes the socket task to close the socket (sockets only).
    close: Option<Arc<Notify>>,
}

impl Tab {
    fn status_at(&self, now: Instant) -> Status {
        let expiry = match (&self.id, self.status) {
            (TabId::Socket(_), status) => return status,
            (_, Status::Online) => ONLINE_EXPIRY,
            (_, Status::Idle) => IDLE_EXPIRY,
            (_, Status::Offline) => return Status::Offline,
        };
        if now.saturating_duration_since(self.at) < expiry {
            self.status
        } else {
            Status::Offline
        }
    }

    fn close(&self) {
        if let Some(close) = &self.close {
            close.notify_one();
        }
    }
}

#[derive(Default)]
struct Tabs {
    by_user: HashMap<UserId, Vec<Tab>>,
    next_socket: u64,
    /// Recent socket opens and fallback reports per user, for rate limits.
    connects: HashMap<UserId, VecDeque<Instant>>,
    reports: HashMap<UserId, VecDeque<Instant>>,
}

/// Count one more event for `user` unless `max` happened in the window.
fn allow(
    events: &mut HashMap<UserId, VecDeque<Instant>>,
    user: UserId,
    max: usize,
    now: Instant,
) -> bool {
    let recent = events.entry(user).or_default();
    while recent
        .front()
        .is_some_and(|&t| now.saturating_duration_since(t) >= RATE_WINDOW)
    {
        recent.pop_front();
    }
    if recent.len() >= max {
        return false;
    }
    recent.push_back(now);
    true
}

impl Tabs {
    fn sockets(&self) -> usize {
        self.by_user
            .values()
            .flatten()
            .filter(|t| matches!(t.id, TabId::Socket(_)))
            .count()
    }

    fn status_at(&self, user: UserId, now: Instant) -> Status {
        self.by_user
            .get(&user)
            .and_then(|tabs| tabs.iter().map(|t| t.status_at(now)).min())
            .unwrap_or(Status::Offline)
    }
}

struct Shared {
    tabs: Mutex<Tabs>,
    nudges: broadcast::Sender<Audience>,
}

/// Cheap to clone; all clones share the same state.
#[derive(Clone)]
pub struct PresenceBoard(Arc<Shared>);

impl Default for PresenceBoard {
    fn default() -> Self {
        Self(Arc::new(Shared {
            tabs: Mutex::default(),
            nudges: broadcast::channel(NUDGE_BUFFER).0,
        }))
    }
}

impl PresenceBoard {
    /// Nudges for one socket to forward to its tab.
    pub fn subscribe(&self) -> broadcast::Receiver<Audience> {
        self.0.nudges.subscribe()
    }

    /// Tell the tabs of `audience` to look again.
    pub fn nudge(&self, audience: Audience) {
        // No receivers just means no tab is open.
        let _ = self.0.nudges.send(audience);
    }

    /// Register a tab whose socket just opened. It stays present until the
    /// returned [`SocketTab`] is dropped.
    pub fn connect(
        &self,
        user: UserId,
        session_token: &str,
        status: Status,
    ) -> Result<SocketTab, LiveError> {
        let close = Arc::new(Notify::new());
        let id = self.change(user, |tabs| {
            let now = Instant::now();
            if tabs.sockets() >= MAX_SOCKETS {
                return Err(LiveError::ServerFull);
            }
            if !allow(&mut tabs.connects, user, MAX_CONNECTS, now) {
                return Err(LiveError::Throttled);
            }
            let list = tabs.by_user.entry(user).or_default();
            list.retain(|t| t.status_at(now) != Status::Offline);
            if list.len() >= MAX_TABS_PER_USER {
                // Make room by dropping the stalest fallback tab, if any;
                // sockets are never closed to admit another.
                let stalest = list
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| matches!(t.id, TabId::Reported(_)))
                    .min_by_key(|(_, t)| t.at)
                    .map(|(i, _)| i)
                    .ok_or(LiveError::TooManyTabs)?;
                list.swap_remove(stalest);
            }
            let id = tabs.next_socket;
            tabs.next_socket += 1;
            tabs.by_user.entry(user).or_default().push(Tab {
                id: TabId::Socket(id),
                session: fingerprint(session_token),
                status: status.min(Status::Idle),
                at: now,
                close: Some(Arc::clone(&close)),
            });
            Ok(id)
        })?;
        Ok(SocketTab {
            board: self.clone(),
            user,
            id,
            close,
        })
    }

    /// Record what a tab without a socket says about itself.
    pub fn report(
        &self,
        user: UserId,
        session_token: &str,
        tab: &str,
        status: Status,
    ) -> Result<(), LiveError> {
        if tab.is_empty()
            || tab.len() > MAX_TAB_ID_CHARS
            || !tab
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(LiveError::InvalidTab);
        }
        let id = TabId::Reported(tab.to_owned());
        self.change(user, |tabs| {
            let now = Instant::now();
            if !allow(&mut tabs.reports, user, MAX_REPORTS, now) {
                return Err(LiveError::Throttled);
            }
            let list = tabs.by_user.entry(user).or_default();
            list.retain(|t| t.id != id && t.status_at(now) != Status::Offline);
            if status == Status::Offline {
                return Ok(());
            }
            if list.len() >= MAX_TABS_PER_USER {
                let Some(stalest) = list
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| matches!(t.id, TabId::Reported(_)))
                    .min_by_key(|(_, t)| t.at)
                    .map(|(i, _)| i)
                else {
                    return Err(LiveError::TooManyTabs);
                };
                list.swap_remove(stalest);
            }
            list.push(Tab {
                id,
                session: fingerprint(session_token),
                status,
                at: now,
                close: None,
            });
            Ok(())
        })
    }

    pub fn status(&self, user: UserId) -> Status {
        self.lock().status_at(user, Instant::now())
    }

    /// Users with Inkult open (online or idle).
    pub fn present_users(&self) -> usize {
        let now = Instant::now();
        let tabs = self.lock();
        tabs.by_user
            .keys()
            .filter(|&&user| tabs.status_at(user, now) != Status::Offline)
            .count()
    }

    /// The session was logged out: its tabs are gone and their sockets close.
    pub fn end_session(&self, session_token: &str) {
        let session = fingerprint(session_token);
        let users: Vec<UserId> = self
            .lock()
            .by_user
            .iter()
            .filter(|(_, tabs)| tabs.iter().any(|t| t.session == session))
            .map(|(&user, _)| user)
            .collect();
        for user in users {
            let _ = self.change(user, |tabs| {
                if let Some(list) = tabs.by_user.get_mut(&user) {
                    list.retain(|t| {
                        let keep = t.session != session;
                        if !keep {
                            t.close();
                        }
                        keep
                    });
                }
                Ok::<_, LiveError>(())
            });
        }
    }

    /// The account was deleted, or all its sessions were revoked.
    pub fn forget(&self, user: UserId) {
        let _ = self.change(user, |tabs| {
            for tab in tabs.by_user.remove(&user).unwrap_or_default() {
                tab.close();
            }
            tabs.connects.remove(&user);
            tabs.reports.remove(&user);
            Ok::<_, LiveError>(())
        });
    }

    /// Apply `edit` to `user`'s tabs and nudge everyone if their status
    /// changed as a result.
    fn change<T>(
        &self,
        user: UserId,
        edit: impl FnOnce(&mut Tabs) -> Result<T, LiveError>,
    ) -> Result<T, LiveError> {
        let now = Instant::now();
        let (result, changed) = {
            let mut tabs = self.lock();
            let before = tabs.status_at(user, now);
            let result = edit(&mut tabs);
            if tabs.by_user.get(&user).is_some_and(Vec::is_empty) {
                tabs.by_user.remove(&user);
            }
            (result, tabs.status_at(user, now) != before)
        };
        if changed {
            self.nudge(Audience::Everyone);
        }
        result
    }

    fn lock(&self) -> MutexGuard<'_, Tabs> {
        // A panic while holding this lock leaves at worst a stale status;
        // keep serving rather than poisoning every later request.
        self.0.tabs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A tab held by an open socket. Dropping it (socket closed) takes the
/// tab off the board.
pub struct SocketTab {
    board: PresenceBoard,
    user: UserId,
    id: u64,
    close: Arc<Notify>,
}

impl SocketTab {
    /// The tab went on or off screen.
    pub fn set(&self, status: Status) {
        let status = status.min(Status::Idle);
        let _ = self.board.change(self.user, |tabs| {
            if let Some(tab) = tabs
                .by_user
                .get_mut(&self.user)
                .and_then(|list| list.iter_mut().find(|t| t.id == TabId::Socket(self.id)))
            {
                tab.status = status;
                tab.at = Instant::now();
            }
            Ok::<_, LiveError>(())
        });
    }

    /// Resolves when the server wants this socket closed (logout, account
    /// removed).
    pub async fn closed(&self) {
        self.close.notified().await;
    }
}

impl Drop for SocketTab {
    fn drop(&mut self) {
        let _ = self.board.change(self.user, |tabs| {
            if let Some(list) = tabs.by_user.get_mut(&self.user) {
                list.retain(|t| t.id != TabId::Socket(self.id));
            }
            Ok::<_, LiveError>(())
        });
    }
}

fn fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: UserId = UserId(1);

    fn nudged(rx: &mut broadcast::Receiver<Audience>) -> bool {
        let mut any = false;
        while rx.try_recv().is_ok() {
            any = true;
        }
        any
    }

    #[test]
    fn a_socket_is_present_exactly_while_open() {
        let board = PresenceBoard::default();
        let mut rx = board.subscribe();
        assert_eq!(board.status(ALICE), Status::Offline);
        let tab = board.connect(ALICE, "s1", Status::Online).unwrap();
        assert_eq!(board.status(ALICE), Status::Online);
        assert!(nudged(&mut rx));
        tab.set(Status::Idle);
        assert_eq!(board.status(ALICE), Status::Idle);
        assert!(nudged(&mut rx));
        // Same status again: nothing to tell anyone.
        tab.set(Status::Idle);
        assert!(!nudged(&mut rx));
        drop(tab);
        assert_eq!(board.status(ALICE), Status::Offline);
        assert!(nudged(&mut rx));
        assert_eq!(board.present_users(), 0);
    }

    #[test]
    fn best_tab_wins() {
        let board = PresenceBoard::default();
        let a = board.connect(ALICE, "s1", Status::Idle).unwrap();
        let b = board.connect(ALICE, "s1", Status::Online).unwrap();
        assert_eq!(board.status(ALICE), Status::Online);
        b.set(Status::Idle);
        assert_eq!(board.status(ALICE), Status::Idle);
        a.set(Status::Online);
        drop(b);
        assert_eq!(board.status(ALICE), Status::Online);
        drop(a);
        assert_eq!(board.status(ALICE), Status::Offline);
    }

    #[test]
    fn sockets_never_expire_but_reports_do() {
        let board = PresenceBoard::default();
        let _tab = board.connect(ALICE, "s1", Status::Idle).unwrap();
        let later = Instant::now() + IDLE_EXPIRY * 10;
        assert_eq!(board.lock().status_at(ALICE, later), Status::Idle);

        let bob = UserId(2);
        board.report(bob, "s2", "a", Status::Online).unwrap();
        let now = Instant::now();
        assert_eq!(board.lock().status_at(bob, now), Status::Online);
        assert_eq!(
            board.lock().status_at(bob, now + ONLINE_EXPIRY),
            Status::Offline
        );
        board.report(bob, "s2", "a", Status::Idle).unwrap();
        let now = Instant::now();
        assert_eq!(
            board.lock().status_at(bob, now + ONLINE_EXPIRY),
            Status::Idle
        );
        board.report(bob, "s2", "a", Status::Offline).unwrap();
        assert_eq!(board.status(bob), Status::Offline);
    }

    #[tokio::test]
    async fn logout_closes_only_that_sessions_sockets() {
        let board = PresenceBoard::default();
        let laptop = board.connect(ALICE, "laptop", Status::Online).unwrap();
        let phone = board.connect(ALICE, "phone", Status::Idle).unwrap();
        board.end_session("laptop");
        assert_eq!(board.status(ALICE), Status::Idle);
        // The laptop's socket task is told to close; the phone's is not.
        tokio::time::timeout(Duration::from_secs(1), laptop.closed())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), phone.closed())
                .await
                .is_err()
        );
        board.forget(ALICE);
        assert_eq!(board.status(ALICE), Status::Offline);
        tokio::time::timeout(Duration::from_secs(1), phone.closed())
            .await
            .unwrap();
        // Dropping after being forgotten is harmless.
        drop(laptop);
        drop(phone);
        assert_eq!(board.status(ALICE), Status::Offline);
    }

    #[test]
    fn churn_and_report_floods_are_refused() {
        let board = PresenceBoard::default();
        for _ in 0..MAX_CONNECTS {
            drop(board.connect(ALICE, "s", Status::Online).unwrap());
        }
        assert!(matches!(
            board.connect(ALICE, "s", Status::Online),
            Err(LiveError::Throttled)
        ));
        // Others are not held back by one user's loop.
        let _bob = board.connect(UserId(2), "s", Status::Online).unwrap();

        for i in 0..MAX_REPORTS {
            let status = if i % 2 == 0 {
                Status::Online
            } else {
                Status::Idle
            };
            board.report(UserId(3), "s", "t", status).unwrap();
        }
        assert!(matches!(
            board.report(UserId(3), "s", "t", Status::Online),
            Err(LiveError::Throttled)
        ));
    }

    #[test]
    fn the_server_caps_open_sockets() {
        let board = PresenceBoard::default();
        let sockets: Vec<SocketTab> = (0..MAX_SOCKETS as u64)
            .map(|i| {
                board
                    .connect(UserId(100 + i / 8), "s", Status::Idle)
                    .unwrap()
            })
            .collect();
        assert!(matches!(
            board.connect(UserId(1), "s", Status::Idle),
            Err(LiveError::ServerFull)
        ));
        drop(sockets);
        assert!(board.connect(UserId(1), "s", Status::Idle).is_ok());
    }

    #[test]
    fn tabs_are_checked_and_bounded() {
        let board = PresenceBoard::default();
        for bad in ["", "a b", "../x", &"x".repeat(MAX_TAB_ID_CHARS + 1)] {
            assert!(board.report(ALICE, "s", bad, Status::Online).is_err());
        }
        let sockets: Vec<SocketTab> = (0..MAX_TABS_PER_USER)
            .map(|_| board.connect(ALICE, "s", Status::Idle).unwrap())
            .collect();
        assert!(matches!(
            board.connect(ALICE, "s", Status::Idle),
            Err(LiveError::TooManyTabs)
        ));
        assert!(matches!(
            board.report(ALICE, "s", "t", Status::Idle),
            Err(LiveError::TooManyTabs)
        ));
        drop(sockets);
        for i in 0..MAX_TABS_PER_USER * 2 {
            board
                .report(ALICE, "s", &format!("t{i}"), Status::Idle)
                .unwrap();
        }
        assert_eq!(board.lock().by_user[&ALICE].len(), MAX_TABS_PER_USER);
        // A socket still gets in, replacing the stalest report.
        let _socket = board.connect(ALICE, "s", Status::Online).unwrap();
        assert_eq!(board.status(ALICE), Status::Online);
    }
}
