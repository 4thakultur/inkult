//! `/api/live`: one WebSocket per open tab, carrying the tab's presence
//! (see [`crate::rooms::live`]) and the server's "look again" nudges.
//!
//! Messages are small JSON objects. The tab sends `{"status":"online"}` or
//! `{"status":"idle"}` when it goes on or off screen; the server sends
//! `{"type":"hello","version":…}` once (the app.js build, so a tab left
//! open across an update can tell) and `{"type":"changed"}` whenever
//! something the tab shows may have changed.
//!
//! `/api/presence` is the fallback for tabs that cannot open a socket
//! (a reverse proxy without WebSocket support): plain status reports with
//! a heartbeat.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, sleep_until};

use crate::app::{App, AppError};
use crate::auth::User;
use crate::rooms::live::{LiveError, Status};
use crate::web::{APP_JS_TAG, cross_site_fetch, origin_matches_host, session};

type AppState = Arc<App>;

/// How often the server pings the tab and re-checks its session, so a
/// revoked session (password change, account deleted) loses its socket.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A tab that has not answered for this long is gone (browsers answer
/// pings by themselves, even in background tabs).
const SILENCE_LIMIT: Duration = Duration::from_secs(50);
/// Tabs only ever send a status; anything bigger is not from Inkult. The
/// frame cap matters too: by default a frame may be 16 MiB, buffered
/// before the message cap is checked.
const MAX_MESSAGE_BYTES: usize = 1024;
/// A client that stops reading would otherwise hold its task in `send`
/// forever, and its tab on the board with it.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Status changes from one tab are applied at most this often (the latest
/// wins), and "changed" is sent at most this often: a tab flipping its
/// status in a loop would otherwise make every other tab refetch each time.
const STATUS_GAP: Duration = Duration::from_millis(250);
const PUSH_GAP: Duration = Duration::from_millis(250);
/// Messages a tab may send per [`FLOOD_WINDOW`]; past that it is not a
/// browser tab switching, and the socket is closed.
const FLOOD_MAX: usize = 60;
const FLOOD_WINDOW: Duration = Duration::from_secs(10);
/// Fallback reports are a few dozen bytes.
const MAX_REPORT_BYTES: usize = 1024;
/// Close codes the UI understands (4000–4999 are free for applications).
const CLOSE_POLICY: u16 = 1008;
const CLOSE_SESSION_ENDED: u16 = 4401;
const CLOSE_REFUSED: u16 = 4429;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/live", get(connect)).route(
        "/api/presence",
        post(report).layer(DefaultBodyLimit::max(MAX_REPORT_BYTES)),
    )
}

#[derive(Deserialize)]
struct ConnectQuery {
    /// Whether the tab is on screen as it connects.
    status: Option<Status>,
}

#[derive(Deserialize)]
struct TabMessage {
    status: Status,
}

async fn connect(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
    Query(query): Query<ConnectQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // A WebSocket handshake is a GET, so the general same-origin check for
    // writes does not cover it; without this, any site the user visits
    // could open a socket with their cookie.
    if !origin_matches_host(&headers) || cross_site_fetch(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin request rejected" })),
        )
            .into_response();
    }
    // The session layer let the request in, so the cookie is there.
    let token = session::token(&headers).unwrap_or_default();
    let status = query.status.unwrap_or(Status::Online);
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| run(app, user, token, status, socket))
}

async fn run(app: AppState, user: User, token: String, status: Status, mut socket: WebSocket) {
    let board = app.rooms().presence().clone();
    let tab = match board.connect(user.id, &token, status) {
        Ok(tab) => tab,
        Err(e) => {
            tracing::debug!(user = %user.username, error = %e, "live socket refused");
            close(&mut socket, CLOSE_REFUSED, &e.to_string()).await;
            return;
        }
    };
    let mut nudges = board.subscribe();
    let hello = json!({ "type": "hello", "version": APP_JS_TAG.as_str() });
    if send_json(&mut socket, &hello).await.is_err() {
        return;
    }
    tracing::debug!(user = %user.username, "live socket opened");
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut last_heard = Instant::now();
    let mut received: VecDeque<Instant> = VecDeque::new();
    // Held back by the gaps above until their time comes.
    let mut pending_status: Option<Status> = None;
    let mut status_at = Instant::now();
    let mut pending_push = false;
    let mut push_at = Instant::now();
    loop {
        let wake = [
            pending_status.map(|_| status_at),
            pending_push.then_some(push_at),
        ]
        .into_iter()
        .flatten()
        .min();
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let now = Instant::now();
                last_heard = now;
                received.push_back(now);
                while received.front().is_some_and(|&t| now - t >= FLOOD_WINDOW) {
                    received.pop_front();
                }
                if received.len() > FLOOD_MAX {
                    tracing::warn!(user = %user.username, "live socket flooding; closing");
                    close(&mut socket, CLOSE_POLICY, "too many messages").await;
                    break;
                }
                match message {
                    Message::Text(text) => match serde_json::from_str::<TabMessage>(&text) {
                        Ok(m) => pending_status = Some(m.status),
                        Err(e) => tracing::debug!(user = %user.username, error = %e, "ignoring live message"),
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            nudge = nudges.recv() => {
                match nudge {
                    Ok(audience) if !audience.includes(user.id) => continue,
                    Ok(_) | Err(RecvError::Lagged(_)) => pending_push = true,
                    Err(RecvError::Closed) => break,
                }
            }
            () = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    tracing::debug!(user = %user.username, "live socket silent; closing");
                    break;
                }
                if app.authenticate(&token).await.is_none() {
                    close(&mut socket, CLOSE_SESSION_ENDED, "session ended").await;
                    break;
                }
                if send(&mut socket, Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
            () = tab.closed() => {
                close(&mut socket, CLOSE_SESSION_ENDED, "session ended").await;
                break;
            }
        }
        let now = Instant::now();
        if let Some(status) = pending_status
            && now >= status_at
        {
            tab.set(status);
            pending_status = None;
            status_at = now + STATUS_GAP;
        }
        if pending_push && now >= push_at {
            // Nothing is told to a session that has ended since the last
            // ping, not even that something changed.
            if app.authenticate(&token).await.is_none() {
                close(&mut socket, CLOSE_SESSION_ENDED, "session ended").await;
                break;
            }
            if send_json(&mut socket, &json!({ "type": "changed" }))
                .await
                .is_err()
            {
                break;
            }
            pending_push = false;
            push_at = now + PUSH_GAP;
        }
    }
    tracing::debug!(user = %user.username, "live socket closed");
    // Dropping `tab` takes it off the board and tells the others.
}

async fn send(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    match tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

async fn send_json(socket: &mut WebSocket, value: &serde_json::Value) -> Result<(), ()> {
    send(socket, Message::Text(value.to_string().into())).await
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    // The tab may already be gone; there is nobody to tell then.
    let _ = send(socket, Message::Close(Some(frame))).await;
}

#[derive(Deserialize)]
struct ReportBody {
    /// Random id the tab picked for itself when it loaded.
    tab: String,
    status: Status,
}

/// A tab without a socket saying whether it is on screen, hidden or
/// closing.
///
/// The body is read as JSON whatever its declared type: a closing tab's
/// goodbye comes from `navigator.sendBeacon`, which can only send it as
/// `text/plain`. The Origin check in front of every POST still applies.
async fn report(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let body: ReportBody =
        serde_json::from_slice(&body).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let token = session::token(&headers).unwrap_or_default();
    app.rooms()
        .presence()
        .report(user.id, &token, &body.tab, body.status)
        .map_err(|e| match e {
            LiveError::Throttled => AppError::TooBusy(e.to_string()),
            _ => AppError::BadRequest(e.to_string()),
        })?;
    // Logged once checked: the tab id is the client's choice.
    tracing::debug!(user = %user.username, tab = %body.tab, status = ?body.status, "presence report");
    Ok(StatusCode::NO_CONTENT)
}
