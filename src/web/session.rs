//! Session cookie handling and the authentication middleware.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::app::App;

pub const COOKIE_NAME: &str = "inkult_session";

/// The session token from the request's `Cookie` header(s), if any.
pub fn token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Whether the browser reached us over HTTPS (directly, or through a
/// reverse proxy that says so). Only decides the cookie's `Secure` flag; a
/// spoofed header can only make the spoofer's own cookie stricter.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
}

/// `Set-Cookie` value for a new session. `HttpOnly` keeps it away from
/// scripts; `SameSite=Strict` keeps other sites from riding on it.
pub fn set_cookie(token: &str, max_age_secs: u64, request_headers: &HeaderMap) -> HeaderValue {
    let secure = if is_https(request_headers) {
        "; Secure"
    } else {
        ""
    };
    let value = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{secure}"
    );
    // The token is hex, so the value is always a valid header.
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static("inkult_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// What a session whose password must be changed may still reach: who am
/// I, and the password change itself (logout is public).
const PASSWORD_CHANGE_PATHS: [&str; 2] = ["/api/auth/me", "/api/auth/password"];
/// What a session that must set up two-factor login may still reach. The
/// password comes first: a weak one must be replaced before this.
const TWO_FACTOR_ENROLLMENT_PATHS: [&str; 5] = [
    "/api/auth/me",
    "/api/auth/password",
    "/api/auth/2fa",
    "/api/auth/2fa/setup",
    "/api/auth/2fa/enable",
];

/// Reject requests without a valid session; otherwise make the user
/// available to handlers as `Extension<User>` (and the full
/// [`Authenticated`](crate::auth::Authenticated) as an extension too).
pub async fn require_user(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    let session = match token(request.headers()) {
        Some(token) => app.authenticate(&token).await,
        None => None,
    };
    let Some(session) = session else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::SET_COOKIE, clear_cookie())],
            Json(json!({ "error": "not logged in", "login_required": true })),
        )
            .into_response();
    };
    if session.password_change_required && !PASSWORD_CHANGE_PATHS.contains(&request.uri().path()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "choose a new password first: yours is temporary or too weak",
                "password_change_required": true,
            })),
        )
            .into_response();
    }
    if session.two_factor_enrollment_required
        && !TWO_FACTOR_ENROLLMENT_PATHS.contains(&request.uri().path())
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "two-factor login is required for your account; set it up first",
                "two_factor_enrollment_required": true,
            })),
        )
            .into_response();
    }
    request.extensions_mut().insert(session.user.clone());
    request.extensions_mut().insert(session);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_session_cookie_among_others() {
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; inkult_session=abc ; b=2"),
        );
        assert_eq!(token(&h).as_deref(), Some("abc"));
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            HeaderValue::from_static("inkult_sessionx=nope"),
        );
        h.append(header::COOKIE, HeaderValue::from_static("inkult_session="));
        assert_eq!(token(&h), None);
    }

    #[test]
    fn cookie_flags() {
        let plain = set_cookie("ff00", 60, &HeaderMap::new());
        let plain = plain.to_str().unwrap();
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        assert!(plain.contains("Max-Age=60") && !plain.contains("Secure"));

        let mut proxied = HeaderMap::new();
        proxied.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(
            set_cookie("ff00", 60, &proxied)
                .to_str()
                .unwrap()
                .ends_with("; Secure")
        );
    }
}
