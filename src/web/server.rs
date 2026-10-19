//! The HTTP server loop, hardened against connection exhaustion.
//!
//! `axum::serve` sets no timer, so hyper's header-read timeout never fires:
//! a client could open thousands of connections, send half a request on
//! each and hold them forever (slowloris), starving everyone else. This
//! loop serves HTTP/1.1 (all a browser or reverse proxy needs; Inkult has
//! no TLS, so no HTTP/2) with:
//!
//! - a deadline for receiving each request's headers,
//! - a cap on open connections overall, and per client address (reverse
//!   proxies in `INKULT_TRUSTED_PROXIES` are exempt from the per-client cap:
//!   they carry everyone's connections and are expected to limit clients
//!   themselves).
//!
//! Connections are served with upgrades for the live socket (`/api/live`).
//! hyper-util's graceful shutdown does not take upgradeable HTTP/1
//! connections (only through its HTTP/2-capable builder, which Inkult does
//! not need), so shutdown is signalled to each connection here instead.
//! An upgraded socket leaves these counts once upgraded; it is limited
//! per account instead (see `rooms::live`).

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tower_service::Service;

use super::client::{ClientPolicy, PeerAddr};

/// Time allowed to send a request's headers, counted from when the server
/// starts waiting for them. A browser sends them in one go; only a stalled
/// or malicious client takes this long.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Open connections at most. A browser keeps up to six per server.
const MAX_CONNECTIONS: usize = 1024;
/// Open connections per client address (a household behind one address,
/// several browsers each, plus audio streams).
const MAX_CONNECTIONS_PER_CLIENT: usize = 64;
/// How long open connections may finish at shutdown before being dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Open connections per client address.
type PerClient = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One open connection counted against its client; released on drop.
struct ClientSlot {
    counts: PerClient,
    ip: IpAddr,
}

impl ClientSlot {
    fn take(counts: &PerClient, ip: IpAddr) -> Option<Self> {
        let mut map = counts.lock().unwrap_or_else(|p| p.into_inner());
        let open = map.entry(ip).or_default();
        if *open >= MAX_CONNECTIONS_PER_CLIENT {
            return None;
        }
        *open += 1;
        Some(Self {
            counts: Arc::clone(counts),
            ip,
        })
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut map = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(open) = map.get_mut(&self.ip) {
            *open -= 1;
            if *open == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// Serve `router` until `shutdown` resolves, then give open connections
/// [`SHUTDOWN_GRACE`] to finish.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    clients: Arc<ClientPolicy>,
    shutdown: impl Future<Output = ()>,
) {
    let (stopping, stop) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let per_client: PerClient = Arc::default();
    tokio::pin!(shutdown);

    loop {
        // At the cap, stop accepting: new connections wait in the kernel's
        // backlog until a slot frees (stalled ones time out quickly).
        let permit = tokio::select! {
            permit = Arc::clone(&connections).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
            () = &mut shutdown => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Usually out of file descriptors: back off briefly
                    // instead of spinning.
                    tracing::warn!(error = %e, "cannot accept a connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let ip = peer.ip().to_canonical();
        let slot = if clients.is_trusted_proxy(ip) {
            None
        } else {
            match ClientSlot::take(&per_client, ip) {
                Some(slot) => Some(slot),
                None => {
                    tracing::debug!(client = %ip, "connection refused: too many open connections from this client");
                    continue;
                }
            }
        };
        let _ = stream.set_nodelay(true);

        let router = router.clone();
        let service = hyper::service::service_fn(move |mut request: hyper::Request<Incoming>| {
            request.extensions_mut().insert(PeerAddr(ip));
            let mut router = router.clone();
            async move { router.call(request.map(Body::new)).await }
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(TokioIo::new(stream), service)
            // The live socket (`/api/live`) is an upgraded connection.
            .with_upgrades();
        let mut stop = stop.clone();
        tokio::spawn(async move {
            let mut connection = std::pin::pin!(connection);
            let result = tokio::select! {
                result = connection.as_mut() => result,
                // The guard `wait_for` returns must not live across awaits.
                () = async { drop(stop.wait_for(|stopping| *stopping).await) } => {
                    // Finish the request in flight, then close.
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(e) = result {
                tracing::debug!(client = %ip, error = %e, "connection ended with an error");
            }
            drop(slot);
            drop(permit);
        });
    }

    drop(listener);
    let _ = stopping.send(true);
    // Every open connection holds a permit: having them all back means
    // every connection has finished.
    let all = u32::try_from(MAX_CONNECTIONS).unwrap_or(u32::MAX);
    tokio::select! {
        _ = connections.acquire_many(all) => {}
        () = tokio::time::sleep(SHUTDOWN_GRACE) => {
            tracing::info!("closing connections still open after {} s", SHUTDOWN_GRACE.as_secs());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_slots_are_capped_and_released() {
        let counts: PerClient = Arc::default();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let slots: Vec<_> = (0..MAX_CONNECTIONS_PER_CLIENT)
            .map(|_| ClientSlot::take(&counts, ip).unwrap())
            .collect();
        assert!(ClientSlot::take(&counts, ip).is_none());
        assert!(ClientSlot::take(&counts, "203.0.113.10".parse().unwrap()).is_some());
        drop(slots);
        assert!(counts.lock().unwrap().is_empty());
        assert!(ClientSlot::take(&counts, ip).is_some());
    }
}
