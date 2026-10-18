//! `/api/soulseek/…`: the Soulseek tab. Every route acts on the logged-in
//! user's own Soulseek session; handlers only translate HTTP.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app::{App, AppError};
use crate::auth::User;
use crate::soulseek::{FileRequest, SoulseekError};

type AppState = Arc<App>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/soulseek", get(status))
        .route("/api/soulseek/login", post(sign_in))
        .route("/api/soulseek/logout", post(sign_out))
        .route("/api/soulseek/settings", post(settings))
        .route("/api/soulseek/searches", get(searches).post(search))
        .route(
            "/api/soulseek/searches/{id}",
            get(results).delete(remove_search),
        )
        .route("/api/soulseek/searches/{id}/stop", post(stop_search))
        .route("/api/soulseek/wishlist", post(wishlist_add))
        .route("/api/soulseek/wishlist/remove", post(wishlist_remove))
        .route("/api/soulseek/browse", post(browse))
        .route("/api/soulseek/browse/{username}", get(browse_view))
        .route("/api/soulseek/browse/{username}/files", get(browse_files))
        .route("/api/soulseek/users/{username}", get(user_details))
        .route("/api/soulseek/downloads", get(downloads).post(download))
        .route("/api/soulseek/downloads/finished", delete(clear_downloads))
        .route("/api/soulseek/downloads/{id}/cancel", post(cancel_download))
        .route("/api/soulseek/downloads/{id}/retry", post(retry_download))
        .route("/api/soulseek/uploads", get(uploads))
        .route("/api/soulseek/uploads/finished", delete(clear_uploads))
        .route("/api/soulseek/uploads/{id}/cancel", post(cancel_upload))
        .route("/api/soulseek/chats", get(conversations))
        .route(
            "/api/soulseek/chats/{username}",
            get(conversation)
                .post(send_message)
                .delete(close_conversation),
        )
}

pub fn status_code(e: &SoulseekError) -> StatusCode {
    match e {
        SoulseekError::InvalidInput(_) | SoulseekError::LoginRejected(_) => StatusCode::BAD_REQUEST,
        SoulseekError::NotSignedIn | SoulseekError::AccountInUse => StatusCode::CONFLICT,
        SoulseekError::Offline => StatusCode::SERVICE_UNAVAILABLE,
        SoulseekError::NotFound | SoulseekError::UserOffline { .. } => StatusCode::NOT_FOUND,
        SoulseekError::Disabled | SoulseekError::SharingDisabled => StatusCode::FORBIDDEN,
        SoulseekError::PeerTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        SoulseekError::Limit(_) => StatusCode::TOO_MANY_REQUESTS,
        SoulseekError::ServerUnreachable(_) | SoulseekError::Peer(_) => StatusCode::BAD_GATEWAY,
    }
}

async fn status(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    Json(app.soulseek_status(&user).await)
}

#[derive(Deserialize)]
struct SignInBody {
    username: String,
    password: String,
    #[serde(default)]
    remember: bool,
}

async fn sign_in(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<SignInBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        app.soulseek_sign_in(&user, &body.username, &body.password, body.remember)
            .await?,
    ))
}

async fn sign_out(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    app.soulseek_sign_out(&user).await;
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct SettingsBody {
    share_library: bool,
}

async fn settings(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<SettingsBody>,
) -> Result<impl IntoResponse, AppError> {
    app.soulseek(&user)?.set_sharing(body.share_library).await?;
    Ok(Json(app.soulseek_status(&user).await))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    /// Search only this user's shares.
    #[serde(default)]
    user: Option<String>,
}

async fn search(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<SearchBody>,
) -> Result<impl IntoResponse, AppError> {
    let target = body.user.as_deref().filter(|u| !u.trim().is_empty());
    Ok((
        StatusCode::CREATED,
        Json(app.soulseek(&user)?.search(&body.query, target)?),
    ))
}

async fn searches(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.searches()))
}

#[derive(Deserialize)]
struct ResultsParams {
    #[serde(default)]
    from: usize,
}

async fn results(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Query(params): Query<ResultsParams>,
) -> Result<impl IntoResponse, AppError> {
    app.soulseek(&user)?
        .search_results(id, params.from)
        .map(Json)
        .ok_or(AppError::NotFound)
}

async fn stop_search(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if app.soulseek(&user)?.stop_search(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

async fn remove_search(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if app.soulseek(&user)?.remove_search(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(Deserialize)]
struct WishlistBody {
    term: String,
}

async fn wishlist_add(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<WishlistBody>,
) -> Result<impl IntoResponse, AppError> {
    let list = app.soulseek(&user)?.edit_wishlist(&body.term, true).await?;
    Ok(Json(json!({ "wishlist": list })))
}

async fn wishlist_remove(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<WishlistBody>,
) -> Result<impl IntoResponse, AppError> {
    let list = app
        .soulseek(&user)?
        .edit_wishlist(&body.term, false)
        .await?;
    Ok(Json(json!({ "wishlist": list })))
}

#[derive(Deserialize)]
struct BrowseBody {
    username: String,
    #[serde(default)]
    refresh: bool,
}

async fn browse(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<BrowseBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        app.soulseek(&user)?.browse(&body.username, body.refresh)?,
    ))
}

async fn browse_view(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    app.soulseek(&user)?
        .browse_view(&username)
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[derive(Deserialize)]
struct FilesParams {
    dir: String,
}

async fn browse_files(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
    Query(params): Query<FilesParams>,
) -> Result<impl IntoResponse, AppError> {
    app.soulseek(&user)?
        .browse_files(&username, &params.dir)
        .map(Json)
        .ok_or(AppError::NotFound)
}

async fn user_details(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.user_details(&username).await?))
}

/// Either explicit files or a whole folder (fetched from the peer).
#[derive(Deserialize)]
struct DownloadBody {
    username: String,
    #[serde(default)]
    files: Vec<FileRequest>,
    #[serde(default)]
    folder: Option<String>,
}

async fn download(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<DownloadBody>,
) -> Result<impl IntoResponse, AppError> {
    let session = app.soulseek(&user)?;
    let queued = match body.folder {
        Some(folder) => session.download_folder(&body.username, &folder).await?,
        None => session.download_files(&body.username, body.files).await?,
    };
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": queued }))))
}

async fn downloads(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.downloads()))
}

async fn cancel_download(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if app.soulseek(&user)?.cancel_download(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

async fn retry_download(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if app.soulseek(&user)?.retry_download(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

async fn clear_downloads(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        json!({ "removed": app.soulseek(&user)?.clear_finished_downloads() }),
    ))
}

async fn uploads(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.uploads()))
}

async fn cancel_upload(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if app.soulseek(&user)?.cancel_upload(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

async fn clear_uploads(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        json!({ "removed": app.soulseek(&user)?.clear_finished_uploads() }),
    ))
}

async fn conversations(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.conversations()))
}

async fn conversation(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.soulseek(&user)?.conversation(&username)))
}

#[derive(Deserialize)]
struct MessageBody {
    text: String,
}

async fn send_message(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
    Json(body): Json<MessageBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        app.soulseek(&user)?.send_message(&username, &body.text)?,
    ))
}

async fn close_conversation(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    app.soulseek(&user)?.close_conversation(&username);
    Ok(StatusCode::NO_CONTENT)
}
