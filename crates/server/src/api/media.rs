//! Tokenized media: alarm sounds are fetched by speakers/browsers that
//! cannot hold a session, so each delivery mints a short-lived token grant.
//! Only generated/cached sound assets are served — no arbitrary file access.

use crate::error::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

pub fn routes() -> Router<AppState> {
    Router::new().route("/media/{token}", get(media))
}

async fn media(State(s): State<AppState>, Path(token): Path<String>) -> Result<Response, ApiError> {
    if token.len() > 80 {
        return Err(ApiError::BadRequest("bad token".into()));
    }
    let kind = s
        .audio
        .check_media(&token)
        .ok_or_else(|| ApiError::NotFound("media grant expired or unknown".into()))?;
    match kind.as_str() {
        "builtin:tone" => {
            Ok(([(CONTENT_TYPE, "audio/wav")], s.audio.builtin_tone_wav()).into_response())
        }
        other => Err(ApiError::NotFound(format!("media kind '{other}'"))),
    }
}
