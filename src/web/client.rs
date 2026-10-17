//! Who a request comes from, and per-client limits on the sign-in forms.
//!
//! The per-handle throttle and account lock stop guessing at one account,
//! but not one client trying a common password on many handles (spraying)
//! or flooding the forms. Those are limited per client address here.
//!
//! The address is the TCP peer, unless that peer is a reverse proxy listed
//! in `INKULT_TRUSTED_PROXIES`: then the client is taken from
//! `X-Forwarded-For`, read from the right and skipping trusted hops, since
//! everything left of the last trusted proxy could be made up by the client.

use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::auth::sessions::{LoginThrottle, ThrottlePolicy};
use crate::config::IpNet;

/// The TCP peer of the connection, set by the server loop.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub IpAddr);

/// The client a request comes from, after trusted proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

impl ClientIp {
    /// The throttle key: the address, or for IPv6 its /64, since one
    /// household or server usually holds a whole /64 and could otherwise
    /// rotate through billions of addresses.
    fn key(self) -> String {
        match self.0 {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => {
                let prefix = u128::from(ip) & (u128::MAX << 64);
                format!("{}/64", Ipv6Addr::from(prefix))
            }
        }
    }
}

/// Failed sign-in attempts per client: generous enough for a household
/// behind one address mistyping now and then, then slowing to a few dozen
/// an hour. Successful attempts are handed back.
const PER_CLIENT: ThrottlePolicy = ThrottlePolicy {
    free: 20,
    max_delay: Duration::from_secs(900),
    forget_after: Duration::from_secs(3600),
    max_tracked: 50_000,
};

pub struct ClientPolicy {
    trusted_proxies: Vec<IpNet>,
    failures: LoginThrottle,
    warned_untrusted_forwarding: AtomicBool,
}

impl ClientPolicy {
    pub fn new(trusted_proxies: Vec<IpNet>) -> Self {
        Self {
            trusted_proxies,
            failures: LoginThrottle::new(PER_CLIENT),
            warned_untrusted_forwarding: AtomicBool::new(false),
        }
    }

    pub fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|net| net.contains(ip))
    }

    /// The client behind `peer`, using `X-Forwarded-For` only when `peer`
    /// is a trusted proxy.
    pub fn client(&self, peer: IpAddr, headers: &HeaderMap) -> ClientIp {
        let peer = peer.to_canonical();
        let forwarded = || {
            headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .collect::<Vec<_>>()
        };
        if !self.is_trusted_proxy(peer) {
            if headers.contains_key("x-forwarded-for")
                && !self
                    .warned_untrusted_forwarding
                    .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    peer = %peer,
                    "a request from {peer} carries X-Forwarded-For, but {peer} is not in INKULT_TRUSTED_PROXIES: \
                     if it is your reverse proxy, list it, or every visitor counts as one client for sign-in limits"
                );
            }
            return ClientIp(peer);
        }
        let mut client = peer;
        for hop in forwarded().into_iter().rev() {
            // A malformed entry ends the chain: what's left of it can't be
            // trusted, so the last good hop is the client.
            let Ok(ip) = hop.parse::<IpAddr>().map(|ip| ip.to_canonical()) else {
                break;
            };
            client = ip;
            if !self.is_trusted_proxy(ip) {
                break;
            }
        }
        ClientIp(client)
    }
}

/// Work out the client of every request (see [`ClientPolicy::client`]).
pub async fn identify(
    State(policy): State<Arc<ClientPolicy>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<PeerAddr>()
        .map_or(IpAddr::from([0, 0, 0, 0]), |p| p.0);
    let client = policy.client(peer, request.headers());
    request.extensions_mut().insert(client);
    next.run(request).await
}

/// Limit failed attempts per client on the public sign-in forms (login,
/// authenticator code, first-captain setup). Every attempt is counted when
/// it starts and handed back if it succeeds, so parallel requests can't
/// slip past. Failures are logged with the client address, in a fixed
/// format a tool such as fail2ban can match.
pub async fn limit_failures(
    State(policy): State<Arc<ClientPolicy>>,
    request: Request,
    next: Next,
) -> Response {
    let client = request
        .extensions()
        .get::<ClientIp>()
        .copied()
        .unwrap_or(ClientIp(IpAddr::from([0, 0, 0, 0])));
    let key = client.key();
    let path = request.uri().path().to_owned();
    if let Err(retry_after) = policy.failures.attempt(&key) {
        tracing::warn!(client = %client.0, path, "sign-in refused: too many failed attempts from this client");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(json!({
                "error": format!("too many failed sign-in attempts from your network; try again in {retry_after} s"),
            })),
        )
            .into_response();
    }
    let response = next.run(request).await;
    let status = response.status();
    if status.is_success() || status == StatusCode::SERVICE_UNAVAILABLE {
        policy.failures.forgive(&key);
    } else if matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::GONE
            | StatusCode::LOCKED
            | StatusCode::TOO_MANY_REQUESTS
    ) {
        tracing::warn!(client = %client.0, path, status = status.as_u16(), "failed sign-in attempt");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in xff {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forwarded_for_is_only_believed_from_trusted_proxies() {
        let policy = ClientPolicy::new(vec!["172.18.0.0/16".parse().unwrap()]);
        // A direct client can't pick its own address.
        assert_eq!(
            policy.client(ip("203.0.113.9"), &headers(&["1.2.3.4"])),
            ClientIp(ip("203.0.113.9"))
        );
        // Behind the proxy: the hop the proxy saw, not what the client
        // wrote further left.
        assert_eq!(
            policy.client(ip("172.18.0.2"), &headers(&["6.6.6.6, 203.0.113.9"])),
            ClientIp(ip("203.0.113.9"))
        );
        // Chained trusted proxies are skipped; split headers are joined.
        assert_eq!(
            policy.client(
                ip("172.18.0.2"),
                &headers(&["6.6.6.6", "203.0.113.9, 172.18.0.5"])
            ),
            ClientIp(ip("203.0.113.9"))
        );
        // Garbage ends the chain at the last good hop.
        assert_eq!(
            policy.client(ip("172.18.0.2"), &headers(&["203.0.113.9, nonsense"])),
            ClientIp(ip("172.18.0.2"))
        );
        assert_eq!(
            policy.client(ip("172.18.0.2"), &headers(&[])),
            ClientIp(ip("172.18.0.2"))
        );
        // IPv4-mapped IPv6 peers (dual-stack listeners) match IPv4 ranges.
        assert_eq!(
            policy.client(ip("::ffff:172.18.0.2"), &headers(&["203.0.113.9"])),
            ClientIp(ip("203.0.113.9"))
        );
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_64() {
        assert_eq!(
            ClientIp(ip("2001:db8:1:2:aaaa::1")).key(),
            "2001:db8:1:2::/64"
        );
        assert_eq!(
            ClientIp(ip("2001:db8:1:2:ffff::9")).key(),
            ClientIp(ip("2001:db8:1:2::1")).key()
        );
        assert_eq!(ClientIp(ip("192.0.2.1")).key(), "192.0.2.1");
    }
}
