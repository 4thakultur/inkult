//! Connections to other Soulseek users.
//!
//! To reach a peer we follow the "modern" order from the protocol
//! documentation: ask the server to have the peer connect to us (indirect)
//! and at the same time look up its address and connect directly. Whichever
//! works first is used. Peers behind a firewall can still be reached as
//! long as one side accepts connections.
//!
//! Incoming connections start with a peer-init message: `PeerInit` (a direct
//! connection, saying who and what kind) or `PierceFirewall` (the answer to
//! one of our indirect requests, identified by its token).
//!
//! Connection addresses come from the server. Loopback, unspecified,
//! multicast and broadcast addresses are refused: a real peer never has
//! them, and connecting would let a forged address reach services running
//! on this machine.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::SoulseekError;
use super::messages::{
    ConnKind, MAX_INIT_FRAME, PeerInit, PeerMessage, SharedDir, UserInfo, peer_frame_limit,
};
use super::session::{PeerAddress, Session, Waiters};
use super::wire::{read_coded_frame, read_frame};
use crate::jobs::lock;

const ADDRESS_TIMEOUT: Duration = Duration::from_secs(10);
const DIRECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long to wait for the peer to answer an indirect connection request.
const INDIRECT_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Peer connections with no traffic for this long are closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Simultaneous peer connections (all kinds) we keep open.
const MAX_CONNECTIONS: usize = 256;
/// Connections being set up at once (inbound handshakes, and outgoing
/// connections the server relays requests for), so a flood of connection
/// attempts cannot pile up sockets and tasks.
const MAX_HANDSHAKES: usize = 64;
/// Outgoing messages buffered per connection.
const SEND_QUEUE: usize = 256;

#[derive(Clone)]
pub(super) struct PeerConn {
    id: u64,
    username: Arc<str>,
    tx: mpsc::Sender<Vec<u8>>,
}

impl PeerConn {
    /// Queue a message on this connection. Dropped (with a log line) when
    /// the connection is gone or hopelessly backed up.
    pub fn send(&self, message: PeerMessage) {
        if self.tx.try_send(message.encode()).is_err() {
            tracing::debug!(peer = %self.username, "peer connection closed or congested; message dropped");
        }
    }

    pub async fn send_wait(&self, message: PeerMessage) -> Result<(), SoulseekError> {
        self.tx
            .send(message.encode())
            .await
            .map_err(|_| SoulseekError::Peer(format!("connection to {} closed", self.username)))
    }

    /// Send an already encoded message.
    pub async fn send_frame(&self, frame: Vec<u8>) -> Result<(), SoulseekError> {
        self.tx
            .send(frame)
            .await
            .map_err(|_| SoulseekError::Peer(format!("connection to {} closed", self.username)))
    }

    fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }
}

pub(super) struct PeerTable {
    conns: Mutex<HashMap<String, PeerConn>>,
    /// One connection attempt per peer at a time.
    connecting: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Our indirect connection requests by token.
    indirect: Mutex<HashMap<u32, oneshot::Sender<TcpStream>>>,
    next_id: AtomicU64,
    /// Bounds all open peer connections, both directions.
    slots: Arc<Semaphore>,
    /// Bounds connections still in their handshake.
    handshakes: Arc<Semaphore>,
    /// Folder listings we asked for, by token.
    pub folders: Waiters<u32, Arc<Vec<SharedDir>>>,
    /// User info we asked for, by username.
    pub infos: Waiters<String, UserInfo>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            indirect: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
            folders: Waiters::new(),
            infos: Waiters::new(),
        }
    }

    pub fn indirect_failed(&self, token: u32) {
        // Dropping the sender wakes the waiter with an error.
        lock(&self.indirect).remove(&token);
    }

    pub fn close_all(&self) {
        lock(&self.conns).clear();
        lock(&self.indirect).clear();
        self.slots.close();
    }

    fn existing(&self, username: &str) -> Option<PeerConn> {
        lock(&self.conns)
            .get(username)
            .filter(|c| c.is_open())
            .cloned()
    }
}

/// Whether an address a server or peer gave us may be connected to.
pub(super) fn connectable(ip: Ipv4Addr, port: u32, allow_loopback: bool) -> bool {
    (1..=65535).contains(&port)
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && (allow_loopback || !ip.is_loopback())
}

impl Session {
    /// A `P` connection to `username`, reusing an open one.
    pub(super) async fn peer(self: &Arc<Self>, username: &str) -> Result<PeerConn, SoulseekError> {
        if let Some(conn) = self.peers.existing(username) {
            return Ok(conn);
        }
        let attempt = Arc::clone(
            lock(&self.peers.connecting)
                .entry(username.to_owned())
                .or_default(),
        );
        let _one_at_a_time = attempt.lock().await;
        if let Some(conn) = self.peers.existing(username) {
            return Ok(conn);
        }
        let stream = self.open_connection(username, ConnKind::Peer).await;
        lock(&self.peers.connecting).remove(username);
        let stream = stream?;
        self.attach_peer(username.to_owned(), stream)
            .ok_or_else(|| SoulseekError::Peer("too many open connections".into()))
    }

    /// Send one message to `username`, connecting if needed.
    pub(super) async fn send_to(
        self: &Arc<Self>,
        username: &str,
        message: PeerMessage,
    ) -> Result<(), SoulseekError> {
        self.peer(username).await?.send_wait(message).await
    }

    pub(super) async fn peer_address(&self, username: &str) -> Result<PeerAddress, SoulseekError> {
        let answer = self.addresses.wait(username.to_owned());
        self.send_server(super::messages::ServerRequest::GetPeerAddress(
            username.to_owned(),
        ))?;
        tokio::time::timeout(ADDRESS_TIMEOUT, answer)
            .await
            .map_err(|_| SoulseekError::PeerTimeout {
                username: username.to_owned(),
            })?
            .map_err(|_| SoulseekError::Offline)
    }

    /// Open a connection of `kind` to `username`, direct or indirect, and
    /// complete the peer-init handshake from our side.
    pub(super) async fn open_connection(
        self: &Arc<Self>,
        username: &str,
        kind: ConnKind,
    ) -> Result<TcpStream, SoulseekError> {
        let token = self.next_token();
        let (indirect_tx, indirect_rx) = oneshot::channel();
        lock(&self.peers.indirect).insert(token, indirect_tx);
        let result = self
            .race_connection(username, kind, token, indirect_rx)
            .await;
        lock(&self.peers.indirect).remove(&token);
        result
    }

    async fn race_connection(
        self: &Arc<Self>,
        username: &str,
        kind: ConnKind,
        token: u32,
        indirect: oneshot::Receiver<TcpStream>,
    ) -> Result<TcpStream, SoulseekError> {
        self.send_server(super::messages::ServerRequest::ConnectToPeer {
            token,
            username: username.to_owned(),
            kind,
        })?;
        let direct = async {
            let address = self.peer_address(username).await?;
            if address.ip.is_unspecified() || address.port == 0 {
                return Err(SoulseekError::UserOffline {
                    username: username.to_owned(),
                });
            }
            if !connectable(address.ip, address.port, self.ctx.config.allow_loopback) {
                return Err(SoulseekError::Peer(format!(
                    "{username} has an unusable address"
                )));
            }
            let addr = SocketAddr::from((address.ip, address.port as u16));
            let mut stream = tokio::time::timeout(DIRECT_TIMEOUT, TcpStream::connect(addr))
                .await
                .map_err(|_| SoulseekError::PeerTimeout {
                    username: username.to_owned(),
                })?
                .map_err(|e| SoulseekError::Peer(format!("{username}: {e}")))?;
            let init = PeerInit::PeerInit {
                username: self.username().to_owned(),
                kind: Some(kind),
            };
            stream
                .write_all(&init.encode())
                .await
                .map_err(|e| SoulseekError::Peer(format!("{username}: {e}")))?;
            Ok(stream)
        };
        let indirect = async {
            tokio::time::timeout(INDIRECT_TIMEOUT, indirect)
                .await
                .map_err(|_| SoulseekError::PeerTimeout {
                    username: username.to_owned(),
                })?
                .map_err(|_| {
                    SoulseekError::Peer(format!(
                        "cannot connect to {username} (both sides may be behind a firewall)"
                    ))
                })
        };
        tokio::pin!(direct, indirect);
        let mut direct_error = None;
        let mut indirect_error = None;
        while direct_error.is_none() || indirect_error.is_none() {
            tokio::select! {
                result = &mut direct, if direct_error.is_none() => match result {
                    Ok(stream) => return Ok(stream),
                    // Nobody will answer the indirect request either.
                    Err(e @ SoulseekError::UserOffline { .. }) => return Err(e),
                    Err(e) => direct_error = Some(e),
                },
                result = &mut indirect, if indirect_error.is_none() => match result {
                    Ok(stream) => return Ok(stream),
                    Err(e) => indirect_error = Some(e),
                },
                _ = self.shutdown.cancelled() => return Err(SoulseekError::NotSignedIn),
            }
        }
        Err(indirect_error
            .or(direct_error)
            .unwrap_or(SoulseekError::Offline))
    }

    /// Register an established `P` connection and start its reader and
    /// writer. Replaces an older connection to the same peer.
    pub(super) fn attach_peer(
        self: &Arc<Self>,
        username: String,
        stream: TcpStream,
    ) -> Option<PeerConn> {
        let permit = Arc::clone(&self.peers.slots).try_acquire_owned().ok()?;
        let (read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(SEND_QUEUE);
        let conn = PeerConn {
            id: self.peers.next_id.fetch_add(1, Ordering::Relaxed),
            username: Arc::from(username.as_str()),
            tx,
        };
        lock(&self.peers.conns).insert(username.clone(), conn.clone());

        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let session = Arc::clone(self);
        let reader_conn = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut reader = BufReader::new(read_half);
            loop {
                let frame = tokio::select! {
                    frame = tokio::time::timeout(IDLE_TIMEOUT, read_coded_frame(&mut reader, peer_frame_limit)) => frame,
                    _ = session.shutdown.cancelled() => break,
                };
                let (code, body) = match frame {
                    Ok(Ok(frame)) => frame,
                    Ok(Err(e)) => {
                        tracing::trace!(peer = %username, error = %e, "peer connection closed");
                        break;
                    }
                    Err(_) => break,
                };
                // Decompressing and parsing big share lists is CPU work.
                let decoded = if body.len() > 64 * 1024 {
                    tokio::task::spawn_blocking(move || PeerMessage::decode(code, &body))
                        .await
                        .unwrap_or_else(|e| Err(super::wire::WireError::Invalid(e.to_string())))
                } else {
                    PeerMessage::decode(code, &body)
                };
                match decoded {
                    Ok(message) => session.on_peer_message(&username, &reader_conn, message),
                    Err(e) => {
                        tracing::debug!(peer = %username, code, error = %e, "bad peer message");
                        session.peer_message_failed(&username, code);
                    }
                }
            }
            let mut conns = lock(&session.peers.conns);
            if conns.get(&username).is_some_and(|c| c.id == reader_conn.id) {
                conns.remove(&username);
            }
        });
        Some(conn)
    }

    pub(super) async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = self.shutdown.cancelled() => return,
            };
            match accepted {
                Ok((stream, _)) => {
                    // Over the limit, the connection is simply closed.
                    if let Ok(permit) = Arc::clone(&self.peers.handshakes).try_acquire_owned() {
                        let session = Arc::clone(&self);
                        tokio::spawn(async move {
                            session.handle_inbound(stream, permit).await;
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accepting a Soulseek peer connection failed");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    async fn handle_inbound(
        self: Arc<Self>,
        mut stream: TcpStream,
        handshake: OwnedSemaphorePermit,
    ) {
        let frame =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream, MAX_INIT_FRAME))
                .await
            {
                Ok(Ok(frame)) => frame,
                _ => return,
            };
        // The handshake is over; established connections count elsewhere.
        drop(handshake);
        match PeerInit::decode(&frame) {
            Ok(PeerInit::PierceFirewall(token)) => {
                if let Some(waiter) = lock(&self.peers.indirect).remove(&token) {
                    let _ = waiter.send(stream);
                }
            }
            Ok(PeerInit::PeerInit {
                username,
                kind: Some(ConnKind::Peer),
            }) => {
                if super::check_username(&username).is_ok() {
                    self.attach_peer(username, stream);
                }
            }
            Ok(PeerInit::PeerInit {
                username,
                kind: Some(ConnKind::File),
            }) => self.file_connection(username, stream).await,
            _ => {}
        }
    }

    /// The server relays a peer's request that we connect to it (it could
    /// not reach us directly).
    pub(super) async fn answer_indirect(
        self: Arc<Self>,
        username: String,
        kind: Option<ConnKind>,
        ip: Ipv4Addr,
        port: u32,
        token: u32,
    ) {
        let Some(kind @ (ConnKind::Peer | ConnKind::File)) = kind else {
            return; // the distributed network is not joined
        };
        let Ok(handshake) = Arc::clone(&self.peers.handshakes).try_acquire_owned() else {
            let _ = self
                .send_server(super::messages::ServerRequest::CantConnectToPeer { token, username });
            return;
        };
        let connected = async {
            if !connectable(ip, port, self.ctx.config.allow_loopback) {
                return None;
            }
            let addr = SocketAddr::from((ip, port as u16));
            let mut stream = tokio::time::timeout(DIRECT_TIMEOUT, TcpStream::connect(addr))
                .await
                .ok()?
                .ok()?;
            stream
                .write_all(&PeerInit::PierceFirewall(token).encode())
                .await
                .ok()?;
            Some(stream)
        }
        .await;
        drop(handshake);
        let Some(stream) = connected else {
            let _ = self
                .send_server(super::messages::ServerRequest::CantConnectToPeer { token, username });
            return;
        };
        match kind {
            ConnKind::Peer => {
                self.attach_peer(username, stream);
            }
            _ => self.file_connection(username, stream).await,
        }
    }

    /// An `F` connection from a peer that is about to upload to us: it
    /// names the transfer by the token from its earlier TransferRequest.
    async fn file_connection(self: Arc<Self>, username: String, mut stream: TcpStream) {
        let Ok(permit) = Arc::clone(&self.peers.slots).try_acquire_owned() else {
            return;
        };
        let token = match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_u32_le()).await {
            Ok(Ok(token)) => token,
            _ => return,
        };
        self.receive_file(username, token, stream, permit).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_local_and_special_addresses() {
        assert!(connectable(Ipv4Addr::new(84, 12, 3, 4), 2234, false));
        assert!(connectable(Ipv4Addr::new(192, 168, 1, 10), 2234, false));
        assert!(!connectable(Ipv4Addr::LOCALHOST, 2234, false));
        assert!(connectable(Ipv4Addr::LOCALHOST, 2234, true));
        assert!(!connectable(Ipv4Addr::UNSPECIFIED, 2234, true));
        assert!(!connectable(Ipv4Addr::BROADCAST, 2234, false));
        assert!(!connectable(Ipv4Addr::new(224, 0, 0, 1), 2234, false));
        assert!(!connectable(Ipv4Addr::new(84, 12, 3, 4), 0, false));
        assert!(!connectable(Ipv4Addr::new(84, 12, 3, 4), 70000, false));
    }
}
