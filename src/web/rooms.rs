//! `/api/rooms/…`: the Messages tab's chat rooms. Permission checks live in
//! [`crate::rooms`]; handlers only translate HTTP.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app::{App, AppError, ShareRequest};
use crate::auth::User;
use crate::rooms::{RoomError, RoomId};

type AppState = Arc<App>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms", get(list).post(create))
        .route("/api/rooms/unread", get(unread))
        .route("/api/rooms/join-private", post(join_private))
        .route("/api/rooms/{id}", get(view).patch(update).delete(remove))
        .route("/api/rooms/{id}/join", post(join))
        .route("/api/rooms/{id}/leave", post(leave))
        .route("/api/rooms/{id}/messages", post(send))
        .route("/api/rooms/{id}/tracks", post(share))
        .route("/api/rooms/{id}/tracks/{seq}/claim", post(claim))
}

pub fn status_code(e: &RoomError) -> StatusCode {
    match e {
        RoomError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        RoomError::NameTaken(_) => StatusCode::CONFLICT,
        RoomError::NotFound | RoomError::NoSuchTrack => StatusCode::NOT_FOUND,
        RoomError::NotMember | RoomError::NotOwner | RoomError::BadCredentials => {
            StatusCode::FORBIDDEN
        }
        RoomError::Limit(_) | RoomError::Throttled => StatusCode::TOO_MANY_REQUESTS,
        RoomError::Store(_) | RoomError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn list(State(app): State<AppState>, Extension(user): Extension<User>) -> impl IntoResponse {
    Json(app.rooms().list(&user).await)
}

async fn unread(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
) -> impl IntoResponse {
    Json(json!({ "unread": app.rooms().unread_total(&user).await }))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    topic: Option<String>,
    /// Makes the room private: hidden from non-members, joined with it.
    #[serde(default)]
    password: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<CreateBody>,
) -> Result<impl IntoResponse, AppError> {
    let view = app
        .rooms()
        .create(
            &user,
            &body.name,
            body.topic.as_deref(),
            body.password.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Deserialize)]
struct ViewParams {
    /// Only messages after this `seq` (for polling).
    after: Option<u64>,
}

async fn view(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Query(params): Query<ViewParams>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.room_view(&user, RoomId(id), params.after).await?))
}

#[derive(Deserialize)]
struct UpdateBody {
    /// New topic; null or blank clears it.
    topic: Option<String>,
}

async fn update(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Json(body): Json<UpdateBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        app.rooms()
            .set_topic(&user, RoomId(id), body.topic.as_deref())
            .await?,
    ))
}

async fn remove(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    app.rooms().delete(&user, RoomId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn join(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(app.join_room(&user, RoomId(id)).await?))
}

#[derive(Deserialize)]
struct JoinPrivateBody {
    name: String,
    password: String,
}

async fn join_private(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Json(body): Json<JoinPrivateBody>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        app.join_private_room(&user, &body.name, &body.password)
            .await?,
    ))
}

async fn leave(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    app.rooms().leave(&user, RoomId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
}

async fn send(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Json(body): Json<SendBody>,
) -> Result<impl IntoResponse, AppError> {
    let message = app.rooms().post(&user, RoomId(id), &body.text).await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// Share one of the user's library tracks in the room.
async fn share(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path(id): Path<u64>,
    Json(body): Json<ShareRequest>,
) -> Result<impl IntoResponse, AppError> {
    let message = app.share_track(&user, RoomId(id), body).await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// Add a track shared in the room to the user's library.
async fn claim(
    State(app): State<AppState>,
    Extension(user): Extension<User>,
    Path((id, seq)): Path<(u64, u64)>,
) -> Result<impl IntoResponse, AppError> {
    app.claim_shared_track(&user, RoomId(id), seq).await?;
    Ok(StatusCode::NO_CONTENT)
}
