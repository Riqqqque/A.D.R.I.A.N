//! Alarm CRUD + control. Creation persists before responding; control
//! commands (snooze/stop/cancel) are durable and reconcile deliveries.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::scheduler::{self, AlarmSpec};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/alarms", get(list).post(create))
        .route("/api/alarms/{id}/snooze", post(snooze))
        .route("/api/alarms/{id}/stop", post(stop))
        .route("/api/alarms/{id}", delete(cancel))
}

async fn list(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let alarms = scheduler::list_for(&s.db, user.id, user.is_owner()).await?;
    Ok(Json(json!({"alarms": alarms})))
}

#[derive(Deserialize, Serialize)]
struct CreateBody {
    label: Option<String>,
    at_local: Option<String>,
    in_minutes: Option<i64>,
    recurrence: Option<String>,
    endpoint_id: Option<String>,
    volume: Option<f64>,
    snooze_minutes: Option<i64>,
}

async fn create(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    let spec = AlarmSpec {
        label: b.label.as_deref().unwrap_or(""),
        kind: if b.in_minutes.is_some() && b.at_local.is_none() {
            "timer"
        } else {
            "alarm"
        },
        original_request: "web_ui".to_string(),
        at_local: b.at_local.as_deref(),
        in_minutes: b.in_minutes,
        recurrence: b.recurrence.as_deref().unwrap_or("none"),
        endpoint_id: b.endpoint_id.as_deref(),
        volume: b.volume.unwrap_or(0.7),
        snooze_minutes: b.snooze_minutes.unwrap_or(9),
    };
    let created = scheduler::create(&s, user.id, spec).await?;
    s.audit(
        Some(user.id),
        &user.username,
        "set_alarm",
        created.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
        &serde_json::to_value(&b).unwrap_or_default(),
        "scheduled",
        &created.to_string(),
    )
    .await;
    Ok(Json(json!({"ok": true, "alarm": created})))
}

async fn snooze(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = scheduler::snooze(&s, &id, user.id, user.is_owner()).await?;
    Ok(Json(json!({"ok": true, "alarm": v})))
}

async fn stop(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    scheduler::stop_or_cancel(&s, &id, user.id, user.is_owner(), "done").await?;
    Ok(Json(json!({"ok": true})))
}

async fn cancel(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    scheduler::stop_or_cancel(&s, &id, user.id, user.is_owner(), "canceled").await?;
    Ok(Json(json!({"ok": true})))
}
