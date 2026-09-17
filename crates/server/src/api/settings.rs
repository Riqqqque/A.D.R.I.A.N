//! Settings read/write. Reads are available to any authenticated user;
//! writes are owner-only and validated.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings", get(get_settings))
        .route("/api/settings", put(put_settings))
}

async fn get_settings(_user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let settings = s.get_settings().await?;
    let flags = s.setup_flags().await?;
    Ok(Json(json!({"settings": settings, "setup": flags})))
}

#[derive(Deserialize)]
struct PutBody {
    timezone: Option<String>,
    assistant_name: Option<String>,
    privacy_mode: Option<String>,
    public_url: Option<String>,
}

async fn put_settings(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<PutBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    if let Some(tz) = &b.timezone {
        jiff::tz::TimeZone::get(tz)
            .map_err(|_| ApiError::BadRequest(format!("unknown IANA timezone '{tz}'")))?;
        s.set_setting("timezone", tz).await?;
    }
    if let Some(name) = &b.assistant_name {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(ApiError::BadRequest(
                "assistant_name must be 1-64 chars".into(),
            ));
        }
        s.set_setting("assistant_name", name).await?;
    }
    if let Some(mode) = &b.privacy_mode {
        if !matches!(mode.as_str(), "hybrid" | "strict_local") {
            return Err(ApiError::BadRequest(
                "privacy_mode must be hybrid|strict_local".into(),
            ));
        }
        s.set_setting("privacy_mode", mode).await?;
    }
    if let Some(url) = &b.public_url {
        let url = url.trim();
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ApiError::BadRequest(
                "public_url must be an http(s) URL reachable by speakers".into(),
            ));
        }
        s.set_setting("public_url", url.trim_end_matches('/'))
            .await?;
    }
    s.emit("settings", json!({"event": "updated"}));
    Ok(Json(json!({"ok": true})))
}
