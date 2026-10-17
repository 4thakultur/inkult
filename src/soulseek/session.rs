//! One signed-in Soulseek account: the server connection and everything
//! that hangs off it.
//!
//! The server connection is supervised: when it drops, the session
//! reconnects with backoff, keeping searches, queues and conversations.
//! It does not reconnect after the server reports that the account signed
//! in elsewhere ("relogged"), since two clients would keep kicking each
//! other off, or when the password stops being accepted.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::downloads::Downloads;
use super::messages::{
    MAX_SERVER_FRAME, PeerMessage, ServerEvent, ServerRequest, UserStatus, decode_server,
};
use super::peer::{PeerConn, PeerTable};
use super::search::Searches;
use super::social::{Browses, Chats};
use super::store::Account;
use super::uploads::Uploads;
use super::wire::{WireError, read_coded_frame};
use super::{Context, PortLease, SoulseekError, now_ms};
use crate::auth::UserId;
use crate::jobs::lock;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The server sends no answer at all to banned accounts, so a login that
/// stays unanswered this long is reported rather than waited on forever.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(20);
/// Keep the connection alive through NAT gateways and detect dead links.
const PING_INTERVAL: Duration = Duration::from_secs(60);
/// How long the server may stay silent before the link counts as dead
/// (it sends regular traffic such as status updates; pings are not answered).
const SERVER_IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);
const MAX_NOTICES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    SignedOut,
    Connecting,
    Online,
    Reconnecting,
    /// Signed in somewhere else; not reconnecting.
    Kicked,
    /// The server stopped accepting the credentials.
    Rejected,
}

/// What the UI shows about the user's Soulseek account.
#[derive(Debug, Clone, Serialize)]
pub struct AccountStatus {
    pub state: ConnState,
    pub username: Option<String>,
    pub message: Option<String>,
    /// Port peers can connect to, if one could be opened.
    pub listen_port: Option<u16>,
    pub remembered: bool,
    pub share_library: bool,
    pub shared_folders: usize,
    pub shared_files: usize,
    pub unread_messages: usize,
    pub active_downloads: usize,
    pub active_uploads: usize,
    pub wishlist: Vec<String>,
    /// Messages from the server administrators.
    pub notices: Vec<String>,
}

impl AccountStatus {
    pub fn signed_out(account: Option<Account>) -> Self {
        let account = account.unwrap_or_default();
        Self {
            state: ConnState::SignedOut,
            username: Some(account.username).filter(|u| !u.is_empty()),
            message: None,
            listen_port: None,
            remembered: false,
            share_library: account.share_library,
            shared_folders: 0,
            shared_files: 0,
            unread_messages: 0,
            active_downloads: account.downloads.len(),
            active_uploads: 0,
            wishlist: account.wishlist,
            notices: Vec::new(),
        }
    }
}

struct ConnInfo {
    state: ConnState,
    message: Option<String>,
    own_ip: Option<Ipv4Addr>,
    notices: VecDeque<String>,
    wishlist_interval: Duration,
}

/// One-shot waiters for answers keyed by e.g. username or token.
pub(super) struct Waiters<K, V> {
    map: Mutex<HashMap<K, Vec<oneshot::Sender<V>>>>,
}

impl<K: Eq + Hash, V: Clone> Waiters<K, V> {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn wait(&self, key: K) -> oneshot::Receiver<V> {
        let (tx, rx) = oneshot::channel();
        lock(&self.map).entry(key).or_default().push(tx);
        rx
    }

    pub fn resolve(&self, key: &K, value: V) -> bool {
        let Some(waiters) = lock(&self.map).remove(key) else {
            return false;
        };
        for waiter in waiters {
            let _ = waiter.send(value.clone());
        }
        true
    }

    /// Drop all waiters; their receivers see a closed channel.
    pub fn clear(&self) {
        lock(&self.map).clear();
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PeerAddress {
    pub ip: Ipv4Addr,
    pub port: u32,
}

#[derive(Debug, Clone)]
pub(super) struct Stats {
    pub avg_speed: u32,
    pub uploads: u32,
    pub files: u32,
    pub dirs: u32,
}

pub struct Session {
    pub(super) owner: UserId,
    username: String,
    password: String,
    pub(super) ctx: Arc<Context>,
    listen: Mutex<Option<PortLease>>,
    listen_port: Option<u16>,
    server_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    conn: Mutex<ConnInfo>,
    tokens: AtomicU32,
    stopped: AtomicBool,
    pub(super) shutdown: CancellationToken,

    pub(super) peers: PeerTable,
    pub(super) addresses: Waiters<String, PeerAddress>,
    pub(super) statuses: Waiters<String, UserStatus>,
    pub(super) stats: Waiters<String, Stats>,

    pub(super) searches: Mutex<Searches>,
    pub(super) browses: Browses,
    pub(super) chats: Mutex<Chats>,
    pub(super) downloads: Downloads,
    pub(super) uploads: Uploads,
}

/// Why a server connection ended.
enum ConnectionEnd {
    Shutdown,
    Relogged,
    Lost(String),
}

impl Session {
    /// Sign in and start the session. Fails (and leaves nothing running)
    /// when the first sign-in does not succeed, so the user sees why.
    pub(super) async fn start(
        ctx: Arc<Context>,
        owner: UserId,
        username: String,
        password: String,
        listener: Option<(TcpListener, PortLease)>,
        account: &Account,
    ) -> Result<Arc<Self>, SoulseekError> {
        let (stream, own_ip) = connect_and_login(&ctx.config.server, &username, &password).await?;
        tracing::info!(%owner, soulseek_user = %username, "signed in to Soulseek");

        let (listener, lease) = match listener {
            Some((listener, lease)) => (Some(listener), Some(lease)),
            None => (None, None),
        };
        let listen_port = lease.as_ref().map(PortLease::port);
        // Tokens identify our requests on the network; starting from the
        // clock makes collisions with a previous run's requests unlikely.
        let first_token = (now_ms() as u32).wrapping_mul(2_654_435_761) | 1;
        let session = Arc::new(Self {
            owner,
            username,
            password,
            listen: Mutex::new(lease),
            listen_port,
            server_tx: Mutex::new(None),
            conn: Mutex::new(ConnInfo {
                state: ConnState::Online,
                message: None,
                own_ip: Some(own_ip),
                notices: VecDeque::new(),
                wishlist_interval: Duration::from_secs(12 * 60),
            }),
            tokens: AtomicU32::new(first_token),
            stopped: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            peers: PeerTable::new(),
            addresses: Waiters::new(),
            statuses: Waiters::new(),
            stats: Waiters::new(),
            searches: Mutex::new(Searches::default()),
            browses: Browses::default(),
            chats: Mutex::new(Chats::default()),
            downloads: Downloads::new(&account.downloads),
            uploads: Uploads::new(account.share_library),
            ctx,
        });
        lock(&session.searches).set_wishlist(&account.wishlist);

        if let Some(listener) = listener {
            tokio::spawn(Arc::clone(&session).accept_loop(listener));
        }
        tokio::spawn(Arc::clone(&session).supervise(stream));
        tokio::spawn(Arc::clone(&session).download_driver());
        tokio::spawn(Arc::clone(&session).upload_driver());
        tokio::spawn(Arc::clone(&session).wishlist_loop());
        session.refresh_shares().await;
        Ok(session)
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// End the session: close every connection and stop background work.
    /// Unfinished downloads are saved first so they resume next time.
    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.save_downloads().await;
        self.shutdown.cancel();
        self.set_state(ConnState::SignedOut, None);
        lock(&self.server_tx).take();
        lock(&self.listen).take();
        self.peers.close_all();
        tracing::info!(owner = %self.owner, soulseek_user = %self.username, "Soulseek session ended");
    }

    /// Stop after the server ended the session for good (kicked/rejected),
    /// keeping the session around so the UI can say why.
    async fn halt(&self, state: ConnState, message: String) {
        self.save_downloads().await;
        self.stopped.store(true, Ordering::SeqCst);
        self.shutdown.cancel();
        lock(&self.server_tx).take();
        lock(&self.listen).take();
        self.peers.close_all();
        self.set_state(state, Some(message));
    }

    pub(super) fn next_token(&self) -> u32 {
        self.tokens.fetch_add(1, Ordering::Relaxed)
    }

    fn set_state(&self, state: ConnState, message: Option<String>) {
        let mut conn = lock(&self.conn);
        conn.state = state;
        conn.message = message;
    }

    pub(super) fn is_online(&self) -> bool {
        lock(&self.conn).state == ConnState::Online && lock(&self.server_tx).is_some()
    }

    pub(super) fn wishlist_interval(&self) -> Duration {
        lock(&self.conn).wishlist_interval
    }

    /// Queue a message to the server; fails while disconnected.
    pub(super) fn send_server(&self, request: ServerRequest) -> Result<(), SoulseekError> {
        let tx = lock(&self.server_tx)
            .clone()
            .ok_or(SoulseekError::Offline)?;
        tx.try_send(request.encode())
            .map_err(|_| SoulseekError::Offline)
    }

    pub async fn status(&self) -> AccountStatus {
        let account = self.ctx.store.get(self.owner).await.unwrap_or_default();
        let (state, message, notices) = {
            let conn = lock(&self.conn);
            (
                conn.state,
                conn.message.clone(),
                conn.notices.iter().cloned().collect(),
            )
        };
        let (shared_folders, shared_files) = self.uploads.share_counts();
        AccountStatus {
            state,
            username: Some(self.username.clone()),
            message,
            listen_port: self.listen_port.filter(|_| !self.is_stopped()),
            remembered: account.password.is_some(),
            share_library: self.uploads.sharing(),
            shared_folders,
            shared_files,
            unread_messages: lock(&self.chats).unread(),
            active_downloads: self.downloads.active_count(),
            active_uploads: self.uploads.active_count(),
            wishlist: account.wishlist,
            notices,
        }
    }

    // ---------- server connection ----------

    async fn supervise(self: Arc<Self>, first: TcpStream) {
        let mut stream = Some(first);
        let mut failures: u32 = 0;
        loop {
            let current = match stream.take() {
                Some(s) => s,
                None => {
                    let delay = Duration::from_secs(5)
                        .saturating_mul(2u32.saturating_pow(failures.min(8)))
                        .min(MAX_RECONNECT_DELAY);
                    self.set_state(
                        ConnState::Reconnecting,
                        Some(format!("Reconnecting in {} s", delay.as_secs())),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = self.shutdown.cancelled() => return,
                    }
                    self.set_state(ConnState::Connecting, None);
                    match connect_and_login(&self.ctx.config.server, &self.username, &self.password)
                        .await
                    {
                        Ok((s, ip)) => {
                            lock(&self.conn).own_ip = Some(ip);
                            tracing::info!(soulseek_user = %self.username, "reconnected to Soulseek");
                            s
                        }
                        Err(e @ SoulseekError::LoginRejected(_)) => {
                            tracing::warn!(soulseek_user = %self.username, error = %e, "Soulseek sign-in rejected; not retrying");
                            self.halt(ConnState::Rejected, e.to_string()).await;
                            return;
                        }
                        Err(e) => {
                            failures = failures.saturating_add(1);
                            tracing::warn!(soulseek_user = %self.username, error = %e, retry = failures, "Soulseek reconnect failed");
                            continue;
                        }
                    }
                }
            };
            self.set_state(ConnState::Online, None);
            match Arc::clone(&self).run_connection(current).await {
                ConnectionEnd::Shutdown => return,
                ConnectionEnd::Relogged => {
                    tracing::warn!(soulseek_user = %self.username, "Soulseek account signed in elsewhere");
                    self.halt(
                        ConnState::Kicked,
                        "This Soulseek account signed in from another client, which ended this session. \
                         Sign in again here to take it back."
                            .into(),
                    )
                    .await;
                    return;
                }
                ConnectionEnd::Lost(reason) => {
                    tracing::warn!(soulseek_user = %self.username, %reason, "lost connection to the Soulseek server");
                    failures = 1;
                }
            }
        }
    }

    async fn run_connection(self: Arc<Self>, stream: TcpStream) -> ConnectionEnd {
        let (read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        *lock(&self.server_tx) = Some(tx);
        self.announce();

        let mut reader = BufReader::new(read_half);
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let end = loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break ConnectionEnd::Shutdown,
                _ = ping.tick() => {
                    let _ = self.send_server(ServerRequest::ServerPing);
                }
                frame = tokio::time::timeout(
                    SERVER_IDLE_LIMIT,
                    read_coded_frame(&mut reader, |_| MAX_SERVER_FRAME),
                ) => {
                    let (code, body) = match frame {
                        Ok(Ok(frame)) => frame,
                        Ok(Err(e)) => break ConnectionEnd::Lost(e.to_string()),
                        Err(_) => break ConnectionEnd::Lost("server silent for too long".into()),
                    };
                    match decode_server(code, &body) {
                        Ok(ServerEvent::Relogged) => break ConnectionEnd::Relogged,
                        Ok(event) => Arc::clone(&self).on_server_event(event),
                        Err(e) => tracing::debug!(code, error = %e, "undecodable server message"),
                    }
                }
            }
        };
        lock(&self.server_tx).take();
        writer.abort();
        // Nobody will answer pending lookups on this connection.
        self.addresses.clear();
        self.statuses.clear();
        self.stats.clear();
        end
    }

    /// Tell the server how to reach us and what we share.
    fn announce(&self) {
        if let Some(port) = self.listen_port {
            let _ = self.send_server(ServerRequest::SetWaitPort(u32::from(port)));
        }
        let (dirs, files) = self.uploads.share_counts();
        let _ = self.send_server(ServerRequest::SharedFoldersFiles {
            dirs: dirs as u32,
            files: files as u32,
        });
        let _ = self.send_server(ServerRequest::SetStatus(2));
    }

    pub(super) fn announce_shares(&self) {
        let (dirs, files) = self.uploads.share_counts();
        let _ = self.send_server(ServerRequest::SharedFoldersFiles {
            dirs: dirs as u32,
            files: files as u32,
        });
    }

    fn on_server_event(self: Arc<Self>, event: ServerEvent) {
        match event {
            ServerEvent::PeerAddress { username, ip, port } => {
                self.addresses.resolve(&username, PeerAddress { ip, port });
            }
            ServerEvent::UserStatus {
                username, status, ..
            } => {
                self.statuses.resolve(&username, status);
            }
            ServerEvent::UserStats {
                username,
                avg_speed,
                uploads,
                files,
                dirs,
            } => {
                self.stats.resolve(
                    &username,
                    Stats {
                        avg_speed,
                        uploads,
                        files,
                        dirs,
                    },
                );
            }
            ServerEvent::ConnectToPeer {
                username,
                kind,
                ip,
                port,
                token,
            } => {
                tokio::spawn(self.answer_indirect(username, kind, ip, port, token));
            }
            ServerEvent::CantConnectToPeer { token } => self.peers.indirect_failed(token),
            ServerEvent::PrivateMessage {
                id,
                timestamp,
                username,
                message,
            } => {
                let _ = self.send_server(ServerRequest::MessageAcked(id));
                lock(&self.chats).received(&username, &message, timestamp);
            }
            ServerEvent::FileSearch {
                username,
                token,
                query,
            } => {
                tokio::spawn(self.answer_user_search(username, token, query));
            }
            ServerEvent::AdminMessage(message) => {
                tracing::info!(%message, "Soulseek server notice");
                let mut conn = lock(&self.conn);
                conn.notices.push_back(message);
                while conn.notices.len() > MAX_NOTICES {
                    conn.notices.pop_front();
                }
            }
            ServerEvent::WishlistInterval(secs) => {
                // Never more often than once a minute, whatever the server says.
                lock(&self.conn).wishlist_interval = Duration::from_secs(u64::from(secs).max(60));
            }
            ServerEvent::LoginOk { .. }
            | ServerEvent::LoginRejected { .. }
            | ServerEvent::Relogged
            | ServerEvent::Other(_) => {}
        }
    }

    /// A message from a peer on one of its `P` connections. `conn` is the
    /// connection it came on, for answers. The peer's identity is the
    /// connection's username, never a name inside the message.
    pub(super) fn on_peer_message(
        self: &Arc<Self>,
        username: &str,
        conn: &PeerConn,
        message: PeerMessage,
    ) {
        match message {
            PeerMessage::SearchReply(mut reply) => {
                // Mixes and sets: not wanted in the library, so not offered.
                self.ctx
                    .config
                    .max_track_length
                    .get()
                    .drop_too_long(&mut reply.files, |f| f.attrs.duration_secs.map(f64::from));
                lock(&self.searches).add_reply(username, reply);
            }
            PeerMessage::SharedFileList(dirs) => self.browses.received(username, dirs),
            PeerMessage::FolderContents { token, dirs, .. } => {
                self.peers.folders.resolve(&token, Arc::new(dirs));
            }
            PeerMessage::UserInfo(info) => {
                self.peers.infos.resolve(&username.to_owned(), info);
            }
            PeerMessage::GetShareFileList => {
                tokio::spawn(Arc::clone(self).send_share_list(conn.clone(), username.to_owned()));
            }
            PeerMessage::FolderContentsRequest { token, folder } => {
                tokio::spawn(Arc::clone(self).send_folder_contents(conn.clone(), token, folder));
            }
            PeerMessage::UserInfoRequest => self.send_user_info(conn),
            PeerMessage::TransferRequest {
                direction: 1,
                token,
                filename,
                size,
            } => {
                let reply = self.downloads.offered(username, token, &filename, size);
                conn.send(reply);
            }
            PeerMessage::TransferRequest {
                token, filename, ..
            } => {
                // A legacy download request: queue it like a QueueUpload and
                // answer "Queued", as the protocol documentation advises.
                let reply = self.queue_upload_request(username, &filename);
                conn.send(match reply {
                    Ok(()) => PeerMessage::TransferResponse {
                        token,
                        allowed: false,
                        reason: Some("Queued".into()),
                    },
                    Err(reason) => PeerMessage::TransferResponse {
                        token,
                        allowed: false,
                        reason: Some(reason),
                    },
                });
            }
            PeerMessage::TransferResponse {
                token,
                allowed,
                reason,
            } => self.uploads.answered(username, token, allowed, reason),
            PeerMessage::QueueUpload { filename } => {
                if let Err(reason) = self.queue_upload_request(username, &filename) {
                    conn.send(PeerMessage::UploadDenied { filename, reason });
                }
            }
            PeerMessage::PlaceInQueueRequest { filename } => {
                if let Some(place) = self.uploads.place(username, &filename) {
                    conn.send(PeerMessage::PlaceInQueueResponse { filename, place });
                }
            }
            PeerMessage::PlaceInQueueResponse { filename, place } => {
                self.downloads.place_reported(username, &filename, place);
            }
            PeerMessage::UploadFailed { filename } => {
                self.downloads.upload_failed(username, &filename);
            }
            PeerMessage::UploadDenied { filename, reason } => {
                self.downloads.denied(username, &filename, &reason);
            }
            PeerMessage::Unknown(code) => {
                tracing::trace!(peer = %username, code, "ignoring peer message");
            }
        }
    }
}

/// Open a server connection and sign in. Returns our IP as the server sees it.
async fn connect_and_login(
    server: &str,
    username: &str,
    password: &str,
) -> Result<(TcpStream, Ipv4Addr), SoulseekError> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(server))
        .await
        .map_err(|_| SoulseekError::ServerUnreachable(format!("{server} did not answer")))?
        .map_err(|e| SoulseekError::ServerUnreachable(format!("{server}: {e}")))?;
    let login = ServerRequest::Login {
        username: username.to_owned(),
        password: password.to_owned(),
    };
    stream
        .write_all(&login.encode())
        .await
        .map_err(|e| SoulseekError::ServerUnreachable(e.to_string()))?;

    let answer = tokio::time::timeout(LOGIN_TIMEOUT, async {
        loop {
            let (code, body) = read_coded_frame(&mut stream, |_| MAX_SERVER_FRAME).await?;
            if code == super::messages::server_code::LOGIN {
                return decode_server(code, &body);
            }
        }
    })
    .await
    .map_err(|_| {
        SoulseekError::ServerUnreachable(
            "the server did not answer the sign-in (it does not answer banned accounts)".into(),
        )
    })?
    .map_err(|e: WireError| SoulseekError::ServerUnreachable(e.to_string()))?;

    match answer {
        ServerEvent::LoginOk { own_ip, .. } => Ok((stream, own_ip)),
        ServerEvent::LoginRejected { reason, detail } => Err(SoulseekError::LoginRejected(
            login_rejection(&reason, detail.as_deref()),
        )),
        _ => Err(SoulseekError::ServerUnreachable(
            "unexpected answer to sign-in".into(),
        )),
    }
}

fn login_rejection(reason: &str, detail: Option<&str>) -> String {
    match reason {
        "INVALIDPASS" => "wrong password for this Soulseek username".into(),
        "INVALIDUSERNAME" => format!(
            "invalid username{}",
            detail.map(|d| format!(" ({d})")).unwrap_or_default()
        ),
        "EMPTYPASSWORD" => "the password is empty".into(),
        "INVALIDVERSION" => "the server no longer accepts this client version".into(),
        "SVRFULL" => "the server is full; try again later".into(),
        "SVRPRIVATE" => "the server does not accept new accounts right now".into(),
        other => other.chars().take(100).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_rejections() {
        assert_eq!(
            login_rejection("INVALIDPASS", None),
            "wrong password for this Soulseek username"
        );
        assert_eq!(
            login_rejection("INVALIDUSERNAME", Some("Nick too long.")),
            "invalid username (Nick too long.)"
        );
    }

    #[tokio::test]
    async fn waiters_resolve_and_clear() {
        let waiters: Waiters<String, u32> = Waiters::new();
        let a = waiters.wait("x".into());
        let b = waiters.wait("x".into());
        assert!(waiters.resolve(&"x".to_owned(), 7));
        assert_eq!((a.await.unwrap(), b.await.unwrap()), (7, 7));
        let c = waiters.wait("y".into());
        waiters.clear();
        assert!(c.await.is_err());
    }
}
