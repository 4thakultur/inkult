//! HTTP API and embedded web UI.
//!
//! Everything under `/api` except the login endpoints requires a session
//! cookie (see [`session`]). The static UI is public; it shows the login
//! screen until a session exists. State changing requests from other
//! origins are rejected, on top of the `SameSite=Strict` cookie, so a
//! malicious page open in the same browser cannot drive the API (CSRF).
//! Requests for host names Inkult was not configured for are refused (DNS
//! rebinding, see `INKULT_ALLOWED_HOSTS`), and every response carries
//! hardening headers.

mod client;
mod redact;
mod rooms;
mod server;
mod session;
mod soulseek;

pub use client::ClientPolicy;
pub use server::serve;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_util::io::ReaderStream;

use crate::app::{
    App, AppError, ConvertRequest, EnqueueRequest, LibraryScope, NewUser, RemoveRequest,
    SettingsUpdate, StreamRequest, UserUpdate,
};
use crate::auth::{AuthError, Authenticated, Login, LoginOutcome, User, UserId};
use crate::domain::provider::ProviderKind;
use crate::jobs::JobId;
use crate::playback::range::ByteRange;
use crate::playback::{PlaybackError, StreamBody, StreamId, audio_mime};
use crate::providers::ProviderError;
use crate::search::RawFilters;
use crate::tools::ToolError;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");
const THEME_JS: &str = include_str!("assets/theme.js");
const ICON_SVG: &str = include_str!("assets/icon.svg");
const FAVICON_PNG: &[u8] = include_bytes!("assets/favicon-32.png");
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("assets/apple-touch-icon.png");

const CSP: &str = "default-src 'self'; img-src 'self' https: data:; style-src 'self'; \
                   script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";

type AppState = Arc<App>;

/// Longest a sign-in request may take, body upload included. Stops a
/// client from holding a request open by sending its body a byte at a time.
const SIGN_IN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
/// Sign-in bodies are a few hundred bytes; refusing more saves parsing
/// (and normalizing) megabytes sent only to waste server time.
const SIGN_IN_BODY_LIMIT: usize = 16 * 1024;

pub fn router(app: AppState, clients: Arc<ClientPolicy>) -> Router {
    let protected = Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/password", post(change_password))
        .route("/api/auth/2fa", get(two_factor_status))
        .route("/api/auth/2fa/setup", post(begin_two_factor))
        .route("/api/auth/2fa/enable", post(enable_two_factor))
        .route("/api/auth/2fa/disable", post(disable_two_factor))
        .route("/api/auth/2fa/replace", post(replace_authenticator))
        .route(
            "/api/auth/2fa/recovery-codes",
            post(regenerate_recovery_codes),
        )
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", patch(update_user).delete(delete_user))
        .route(
            "/api/admin/settings",
            get(server_settings).patch(update_server_settings),
        )
        .route("/api/status", get(status))
        .route("/api/online", get(online))
        .route("/api/search", get(search))
        .route("/api/resolve", post(resolve))
        .route("/api/artist", get(artist))
        .route("/api/downloads", get(list_jobs).post(enqueue))
        .route("/api/downloads/finished", delete(clear_finished))
        .route("/api/downloads/{id}/cancel", post(cancel_job))
        .route("/api/library", get(library))
        .route("/api/library/convert", post(convert))
        .route("/api/library/remove", post(remove_from_library))
        .route("/api/files/{provider}/{id}/{output}", get(download_file))
        .route("/api/stream", post(prepare_stream))
        .route("/api/stream/{id}", get(stream))
        .route("/api/stream/{id}/waveform", get(stream_waveform))
        .merge(soulseek::routes())
        .merge(rooms::routes())
        // Inside `require_user`, which tells it who is asking.
        .route_layer(middleware::from_fn_with_state(
            Arc::new(redact::PathRedactor::new(app.config())),
            redact::hide_server_paths,
        ))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            session::require_user,
        ));
    // The forms anyone can submit: limited per client on top of the
    // per-handle throttle and account lock (see `client`).
    let sign_in = Router::new()
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/login/2fa", post(login_second_factor))
        .route_layer(middleware::from_fn(sign_in_deadline))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&clients),
            client::limit_failures,
        ))
        .layer(DefaultBodyLimit::max(SIGN_IN_BODY_LIMIT));
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/theme.js", get(theme_js))
        .route("/icon.svg", get(icon_svg))
        .route("/favicon-32.png", get(favicon_png))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/logout", post(logout))
        .merge(sign_in)
        .merge(protected)
        .layer(middleware::from_fn_with_state(clients, client::identify))
        .layer(middleware::from_fn(same_origin_writes))
        .layer(middleware::from_fn_with_state(
            Arc::new(HostPolicy::new(&app.config().allowed_hosts)),
            allowed_host,
        ))
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

async fn sign_in_deadline(request: Request, next: Next) -> Response {
    match tokio::time::timeout(SIGN_IN_DEADLINE, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(json!({ "error": "the request took too long" })),
        )
            .into_response(),
    }
}

/// Which `Host` headers are accepted. IP literals and `localhost` cannot be
/// used for DNS rebinding (the attacker's page would need its own domain to
/// resolve to Inkult), so only other names must be configured.
struct HostPolicy {
    any: bool,
    names: Vec<String>,
}

impl HostPolicy {
    fn new(allowed: &[String]) -> Self {
        Self {
            any: allowed.iter().any(|h| h == "*"),
            names: allowed.to_vec(),
        }
    }

    fn allows(&self, host_header: &str) -> bool {
        if self.any {
            return true;
        }
        // A bare `host[:port]` only; no user info, path or query that the
        // URL parser would quietly accept.
        if !host_header
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        {
            return false;
        }
        let Ok(url) = url::Url::parse(&format!("http://{host_header}/")) else {
            return false;
        };
        match url.host() {
            Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => true,
            Some(url::Host::Domain(name)) => {
                let name = name.trim_end_matches('.');
                name == "localhost" || self.names.iter().any(|n| n == name)
            }
            None => false,
        }
    }
}

async fn allowed_host(
    State(policy): State<Arc<HostPolicy>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()));
    match host {
        Some(host) if policy.allows(host) => next.run(request).await,
        _ => {
            tracing::warn!(host = ?host, "request for an unknown host name refused; add it to INKULT_ALLOWED_HOSTS if it is yours");
            (
                StatusCode::MISDIRECTED_REQUEST,
                Json(json!({
                    "error": "this host name is not allowed; add it to INKULT_ALLOWED_HOSTS"
                })),
            )
                .into_response()
        }
    }
}

/// Hardening headers for every response; handlers may set stricter ones.
async fn security_headers(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let defaults = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, CSP),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("permissions-policy"),
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
    ];
    for (name, value) in defaults {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    // API answers hold per-user data: keep them out of shared caches.
    if is_api {
        headers
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
    }
    response
}

fn provider_status(e: &ProviderError) -> StatusCode {
    match e {
        ProviderError::InvalidUrl(_) | ProviderError::UnsupportedUrl => StatusCode::BAD_REQUEST,
        ProviderError::Unavailable { .. } | ProviderError::NoAudio { .. } => StatusCode::NOT_FOUND,
        ProviderError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::Auth(e) => match e {
                AuthError::InvalidInput(_) => StatusCode::BAD_REQUEST,
                AuthError::UsernameTaken => StatusCode::CONFLICT,
                AuthError::InvalidCredentials
                | AuthError::InvalidSetupCode
                | AuthError::InvalidSecondFactor => StatusCode::UNAUTHORIZED,
                // The challenge is gone: the UI goes back to the password.
                AuthError::ChallengeExpired => StatusCode::GONE,
                AuthError::TwoFactorAlreadyEnabled
                | AuthError::TwoFactorNotEnabled
                | AuthError::EnrollmentExpired => StatusCode::CONFLICT,
                AuthError::TwoFactorMandatory => StatusCode::FORBIDDEN,
                AuthError::Throttled { retry_after_secs } => {
                    let message = self.to_string();
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, retry_after_secs.to_string())],
                        Json(json!({ "error": message })),
                    )
                        .into_response();
                }
                AuthError::SetupUnavailable | AuthError::LastCaptain => StatusCode::CONFLICT,
                AuthError::UserNotFound => StatusCode::NOT_FOUND,
                AuthError::AccountLocked => StatusCode::LOCKED,
                AuthError::TemporaryPasswordExpired => StatusCode::FORBIDDEN,
                AuthError::Busy => {
                    let message = self.to_string();
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::RETRY_AFTER, "5")],
                        Json(json!({ "error": message })),
                    )
                        .into_response();
                }
                AuthError::Store(_) | AuthError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
            AppError::DependencyMissing(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Enqueue(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::Provider(e) => provider_status(e),
            AppError::Soulseek(e) => soulseek::status_code(e),
            AppError::Rooms(e) => rooms::status_code(e),
            AppError::Library(_) | AppError::Settings(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Playback(e) => match e {
                PlaybackError::UnknownStream | PlaybackError::FileMissing => StatusCode::NOT_FOUND,
                PlaybackError::NoAudio => StatusCode::UNPROCESSABLE_ENTITY,
                PlaybackError::Waveform(_) => StatusCode::BAD_GATEWAY,
                PlaybackError::RangeNotSatisfiable { total } => {
                    let range = format!("bytes */{total}");
                    return (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, range)],
                    )
                        .into_response();
                }
                PlaybackError::Provider(e) => provider_status(e),
                PlaybackError::Tool(ToolError::Busy { .. }) => StatusCode::SERVICE_UNAVAILABLE,
                PlaybackError::Tool(ToolError::NotFound { .. }) => StatusCode::SERVICE_UNAVAILABLE,
                PlaybackError::Tool(_) | PlaybackError::Upstream { .. } => StatusCode::BAD_GATEWAY,
                PlaybackError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            },
        };
        // An internal failure's text names files and paths on the server;
        // it goes to the log, and the client gets a plain message.
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
            return (
                status,
                Json(json!({ "error": "internal error; the details are in the server log" })),
            )
                .into_response();
        }
        if status.is_server_error() {
            tracing::warn!(error = %self, "request failed");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Reject non-GET requests whose `Origin` differs from the `Host` they were
/// sent to. Same-origin `fetch` calls always pass; requests without `Origin`
/// (curl, scripts) are allowed since they cannot carry a victim's browser
/// context. Browsers that send `Sec-Fetch-Site` are also held to it, as a
/// second check that doesn't depend on `Origin`.
async fn same_origin_writes(request: Request, next: Next) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && (!origin_matches_host(request.headers()) || cross_site_fetch(request.headers()))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin request rejected" })),
        )
            .into_response();
    }
    next.run(request).await
}

/// The browser says the request comes from another site (`cross-site`) or
/// a sibling subdomain (`same-site`, e.g. another app on the same domain).
fn cross_site_fetch(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.eq_ignore_ascii_case("cross-site") || v.eq_ignore_ascii_case("same-site")
        })
}

fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| url::Url::parse(o).ok())
        .map(|u| match u.port() {
            Some(port) => format!("{}:{port}", u.host_str().unwrap_or_default()),
            None => u.host_str().unwrap_or_default().to_owned(),
        });
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

fn asset(content_type: &'static str, body: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

async fn theme_js() -> Response {
    asset("text/javascript; charset=utf-8", THEME_JS)
}

async fn icon_svg() -> Response {
    asset("image/svg+xml", ICON_SVG)
}

async fn favicon_png() -> Response {
    asset("image/png", FAVICON_PNG)
}

async fn apple_touch_icon() -> Response {
    asset("image/png", APPLE_TOUCH_ICON_PNG)
}

// ---------- accounts ----------

/// Public: what the login screen needs to know before anyone logs in.
async fn auth_state(State(app): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "setup_required": app.setup_required(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The logged-in account as the UI sees it.
fn account_json(
    user: &User,
    password_change_required: bool,
    two_factor_enrollment_required: bool,
) -> serde_json::Value {
    json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
        "created_at": user.created_at,
        "two_factor_enabled": user.two_factor_enabled,
        "password_temporary": user.password_temporary,
        "password_change_required": password_change_required,
        "two_factor_enrollment_required": two_factor_enrollment_required,
    })
}

/// Answer a successful login with the user and the session cookie;
/// `extra` fields are added to the account JSON.
fn logged_in_with(
    app: &App,
    login: Login,
    request_headers: &HeaderMap,
    extra: serde_json::Value,
) -> Response {
    let cookie = session::set_cookie(
        &login.token,
        app.session_max_age().as_secs(),
        request_headers,
    );
    let mut body = account_json(
        &login.user,
        login.password_change_required,
        app.two_factor_enrollment_required(&login.user),
    );
    if let (Some(body), serde_json::Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    (
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Json(body),
    )
        .into_response()
}

fn logged_in(app: &App, login: Login, request_headers: &HeaderMap) -> Response {
    logged_in_with(app, login, request_headers, json!({}))
}

#[derive(Deserialize)]
struct SetupBody {
    code: String,
    username: String,
    password: String,
}

async fn setup(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetupBody>,
) -> Result<Response, AppError> {
    let login = app
        .setup(&body.code, &body.username, &body.password)
        .await?;
    Ok(logged_in(&app, login, &headers))
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, AppError> {
    match app.login(&body.username, &body.password).await? {
        LoginOutcome::Complete(login) => Ok(logged_in(&app, login, &headers)),
        LoginOutcome::SecondFactorRequired {
            challenge,
            expires_in_secs,
        } => Ok((
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(json!({
                "second_factor_required": true,
                "challenge": challenge,
                "expires_in_secs": expires_in_secs,
            })),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct SecondFactorBody {
    challenge: String,
    code: String,
}

async fn login_second_factor(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecondFactorBody>,
) -> Result<Response, AppError> {
    let login = app.login_second_factor(&body.challenge, &body.code).await?;
    Ok(logged_in(&app, login, &headers))
}

async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = session::token(&headers) {
        app.logout(&token);
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::clear_cookie())],
    )
        .into_response()
}

async fn me(Extension(session): Extension<Authenticated>) -> impl IntoResponse {
    Json(account_json(
        &session.user,
        session.password_change_required,
        session.two_factor_enrollment_required,
    ))
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

async fn change_password(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> Result<Response, AppError> {
    let login = app
        .change_own_password(&user, &body.current, &body.new)
        .await?;
    Ok(logged_in(&app, login, &headers))
}

async fn two_factor_status(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.two_factor_status(&user).await?))
}

/// A new secret and its QR code. Never cached: it is a credential.
async fn begin_two_factor(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    let enrollment = app.begin_two_factor(&user).await?;
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(json!({
            "secret": enrollment.secret,
            "uri": enrollment.uri,
            "qr_svg": enrollment.qr_svg,
            "issuer": enrollment.issuer,
            "account": enrollment.account,
            "digits": crate::auth::totp::DIGITS,
            "period": crate::auth::totp::PERIOD_SECS,
        })),
    ))
}

/// Re-authentication for a 2FA change: the password, plus a code.
#[derive(Deserialize)]
struct TwoFactorBody {
    password: String,
    code: String,
}

async fn enable_two_factor(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
    Json(body): Json<TwoFactorBody>,
) -> Result<Response, AppError> {
    let enabled = app
        .enable_two_factor(&user, &body.password, &body.code)
        .await?;
    Ok(logged_in_with(
        &app,
        enabled.login,
        &headers,
        json!({ "recovery_codes": enabled.recovery_codes }),
    ))
}

async fn disable_two_factor(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<TwoFactorBody>,
) -> Result<impl IntoResponse, AppError> {
    app.disable_two_factor(&user, &body.password, &body.code)
        .await?;
    Ok(Json(app.two_factor_status(&user).await?))
}

/// Move to a new authenticator: the password, a code from the current
/// app (or a recovery code), and one from the new app.
#[derive(Deserialize)]
struct ReplaceAuthenticatorBody {
    password: String,
    code: String,
    new_code: String,
}

async fn replace_authenticator(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
    Json(body): Json<ReplaceAuthenticatorBody>,
) -> Result<Response, AppError> {
    let login = app
        .replace_authenticator(&user, &body.password, &body.code, &body.new_code)
        .await?;
    Ok(logged_in(&app, login, &headers))
}

async fn regenerate_recovery_codes(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<TwoFactorBody>,
) -> Result<impl IntoResponse, AppError> {
    let codes = app
        .regenerate_recovery_codes(&user, &body.password, &body.code)
        .await?;
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(json!({ "recovery_codes": codes })),
    ))
}

async fn list_users(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.crew(&user).await?))
}

async fn create_user(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<NewUser>,
) -> Result<impl IntoResponse, AppError> {
    let account = app.create_user(&user, body).await?;
    // The only time this password is shown; never cache it.
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(json!({
            "user": account.user,
            "temporary_password": account.temporary_password,
        })),
    ))
}

async fn update_user(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Json(body): Json<UserUpdate>,
) -> Result<impl IntoResponse, AppError> {
    Ok(match app.update_user(&user, UserId(id), body).await? {
        Some(temporary_password) => (
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(json!({ "temporary_password": temporary_password })),
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

async fn delete_user(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    app.delete_user(&user, UserId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn server_settings(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.server_settings(&user).await?))
}

async fn update_server_settings(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<SettingsUpdate>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.update_server_settings(&user, body).await?))
}

// ---------- music ----------

async fn status(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    Json(app.status(&user).await)
}

async fn online(State(app): State<AppState>) -> impl IntoResponse {
    Json(json!({ "online": app.online_count() }))
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    provider: Option<String>,
    year_from: Option<String>,
    year_to: Option<String>,
    label: Option<String>,
    exclude: Option<String>,
    min_duration: Option<String>,
    max_duration: Option<String>,
    #[serde(default)]
    include_unknown: bool,
}

async fn search(
    State(app): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, AppError> {
    let provider = match params.provider.as_deref() {
        None | Some("" | "all") => None,
        Some(p) => Some(
            p.parse::<ProviderKind>()
                .map_err(|e| AppError::BadRequest(e.to_string()))?,
        ),
    };
    let filters = RawFilters {
        year_from: params.year_from,
        year_to: params.year_to,
        label: params.label,
        exclude: params.exclude,
        min_duration_secs: params.min_duration,
        max_duration_secs: params.max_duration,
        include_unknown: params.include_unknown,
    };
    Ok(Json(app.search(&params.q, provider, &filters).await?))
}

#[derive(Deserialize)]
struct ResolveBody {
    url: String,
}

async fn resolve(
    State(app): State<AppState>,
    Json(body): Json<ResolveBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.resolve(&body.url).await?))
}

#[derive(Deserialize)]
struct ArtistParams {
    url: String,
    /// Also list the tracks the artist reposted (SoundCloud).
    #[serde(default)]
    reposts: bool,
}

async fn artist(
    State(app): State<AppState>,
    Query(params): Query<ArtistParams>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.artist(&params.url, params.reposts).await?))
}

async fn enqueue(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<EnqueueRequest>,
) -> Result<impl IntoResponse, AppError> {
    Ok((StatusCode::ACCEPTED, Json(app.enqueue(&user, body).await?)))
}

async fn list_jobs(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    Json(app.jobs(&user).await)
}

async fn cancel_job(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<JobId>,
) -> Result<impl IntoResponse, AppError> {
    app.cancel_job(&user, id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_finished(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    Json(json!({ "removed": app.clear_finished_jobs(&user) }))
}

#[derive(Deserialize)]
struct LibraryParams {
    #[serde(default)]
    scope: LibraryScope,
}

async fn library(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Query(params): Query<LibraryParams>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.library(&user, params.scope).await?))
}

async fn convert(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<ConvertRequest>,
) -> Result<impl IntoResponse, AppError> {
    Ok((StatusCode::ACCEPTED, Json(app.convert(&user, body).await?)))
}

async fn remove_from_library(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<RemoveRequest>,
) -> Result<impl IntoResponse, AppError> {
    let removal = app.remove_from_library(&user, body).await?;
    Ok(Json(json!({ "removal": removal })))
}

/// Stream a library file to the browser as an attachment.
async fn download_file(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path((provider, id, output)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    let file = app.library_file(&user, &provider, &id, &output).await?;
    let handle = tokio::fs::File::open(&file.path)
        .await
        .map_err(|_| AppError::NotFound)?;
    let size = handle.metadata().await.map(|m| m.len()).ok();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(audio_mime(&file.file_name)),
    );
    if let Ok(value) = HeaderValue::from_str(&content_disposition(&file.file_name)) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Some(size) = size {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    let body = Body::from_stream(ReaderStream::new(handle));
    Ok((headers, body).into_response())
}

async fn prepare_stream(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<StreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.prepare_stream(&user, body).await?))
}

#[derive(Deserialize)]
struct StreamParams {
    /// Offset in seconds, for streams that cannot be seeked by byte range.
    start: Option<f64>,
}

/// Audio for the browser's `<audio>` element, with byte-range support
/// where the underlying stream allows it.
async fn stream(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<StreamId>,
    Query(params): Query<StreamParams>,
    request_headers: HeaderMap,
) -> Result<Response, AppError> {
    let range = request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(ByteRange::parse);
    let stream = app.open_stream(&user, id, range, params.start).await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(stream.content_type),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        HeaderValue::from_static(if stream.ranges { "bytes" } else { "none" }),
    );
    if let Some(len) = stream.content_length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    let status = match stream
        .content_range
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        Some(value) => {
            headers.insert(header::CONTENT_RANGE, value);
            StatusCode::PARTIAL_CONTENT
        }
        None => StatusCode::OK,
    };
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    let body = match stream.body {
        StreamBody::File(file) => Body::from_stream(ReaderStream::new(file)),
        StreamBody::Pipe(pipe) => Body::from_stream(ReaderStream::new(pipe)),
        StreamBody::Capture(reader) => Body::from_stream(ReaderStream::new(reader)),
        StreamBody::Upstream(response) => Body::new(reqwest::Body::from(response)),
    };
    Ok((status, headers, body).into_response())
}

/// Loudness over time of a prepared stream, for the player's seek bar.
async fn stream_waveform(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<StreamId>,
) -> Result<impl IntoResponse, AppError> {
    let waveform = app.stream_waveform(&user, id).await?;
    Ok(Json(waveform.as_ref().clone()))
}

/// `attachment` disposition with an ASCII fallback name plus the exact
/// UTF-8 name (RFC 6266 / RFC 5987), so non-ASCII titles survive.
fn content_disposition(file_name: &str) -> String {
    let fallback: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = file_name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    #[test]
    fn content_disposition_handles_unicode_and_quotes() {
        assert_eq!(
            content_disposition("01 - Café \"Live\".mp3"),
            "attachment; filename=\"01 - Caf_ _Live_.mp3\"; \
             filename*=UTF-8''01%20-%20Caf%C3%A9%20%22Live%22.mp3"
        );
        assert_eq!(audio_mime("x.opus"), "audio/ogg");
        assert_eq!(audio_mime("x.aiff"), "audio/aiff");
    }

    #[test]
    fn host_policy() {
        let default = HostPolicy::new(&[]);
        for ok in [
            "localhost:9876",
            "127.0.0.1:9876",
            "[::1]:9876",
            "192.168.1.5",
            "LOCALHOST",
        ] {
            assert!(default.allows(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:9876",
            "localhost.evil.example",
            "a@localhost",
            "localhost/x",
            "",
        ] {
            assert!(!default.allows(bad), "{bad}");
        }
        let configured = HostPolicy::new(&["music.example.com".into()]);
        assert!(configured.allows("Music.Example.com"));
        assert!(configured.allows("music.example.com:443"));
        assert!(!configured.allows("evil.example"));
        assert!(HostPolicy::new(&["*".into()]).allows("anything.example"));
    }

    #[test]
    fn origin_check() {
        assert!(origin_matches_host(&headers(None, "localhost:8080")));
        assert!(origin_matches_host(&headers(
            Some("http://localhost:8080"),
            "localhost:8080"
        )));
        assert!(origin_matches_host(&headers(
            Some("https://music.example"),
            "music.example"
        )));
        assert!(!origin_matches_host(&headers(
            Some("https://evil.example"),
            "localhost:8080"
        )));
        assert!(!origin_matches_host(&headers(
            Some("null"),
            "localhost:8080"
        )));
    }
    #[test]
    fn sec_fetch_site_blocks_other_sites() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert("sec-fetch-site", HeaderValue::from_str(v).unwrap());
            h
        };
        assert!(cross_site_fetch(&with("cross-site")));
        assert!(cross_site_fetch(&with("Same-Site")));
        assert!(!cross_site_fetch(&with("same-origin")));
        assert!(!cross_site_fetch(&with("none")));
        assert!(!cross_site_fetch(&HeaderMap::new()));
    }
}
