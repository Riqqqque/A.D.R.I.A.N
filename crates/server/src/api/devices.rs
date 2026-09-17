//! Device inventory and the allowlisted light control path — the same
//! policy gate the LLM tool goes through.

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::state::AppState;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/devices/entities", get(list_entities))
        .route("/api/devices/entities/allow", post(set_allowed))
        .route("/api/devices/entities/{id}/command", post(command))
}

async fn list_entities(
    _user: AuthUser,
    State(s): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let rows =
        s.db.q(
            "SELECT entity_id, name, area, domain, state, allowed, updated_at
             FROM entities ORDER BY domain, area, name",
            vec![],
            |r| {
                Ok(json!({
                    "entity_id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "area": r.get::<_, Option<String>>(2)?,
                    "domain": r.get::<_, String>(3)?,
                    "state": r.get::<_, String>(4)?,
                    "allowed": r.get::<_, i64>(5)? != 0,
                    "updated_at": r.get::<_, i64>(6)?,
                }))
            },
        )
        .await?;
    Ok(Json(json!({"entities": rows})))
}

#[derive(Deserialize)]
struct AllowBody {
    entity_id: String,
    allowed: bool,
}

async fn set_allowed(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<AllowBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let n =
        s.db.exec(
            "UPDATE entities SET allowed = ?1 WHERE entity_id = ?2",
            vec![
                P::I(if b.allowed { 1 } else { 0 }),
                P::T(b.entity_id.clone()),
            ],
        )
        .await?;
    if n == 0 {
        return Err(ApiError::NotFound(
            "entity — import from Home Assistant first".into(),
        ));
    }
    s.audit(
        Some(user.id),
        &user.username,
        "entity_allow",
        &b.entity_id,
        &json!({"allowed": b.allowed}),
        "ok",
        "",
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct CommandBody {
    action: String,
    brightness_pct: Option<i64>,
}

/// Execute an authorized light command. Policy: imported + allowlisted +
/// light domain + allowlisted service + bounded params; then observe state.
pub async fn execute_light(
    s: &AppState,
    user: &AuthUser,
    entity_id: &str,
    action: &str,
    brightness_pct: Option<i64>,
) -> Result<Value, ApiError> {
    if !matches!(action, "turn_on" | "turn_off" | "toggle") {
        return Err(ApiError::BadRequest(format!(
            "action '{action}' not in the light service allowlist"
        )));
    }
    if let Some(b) = brightness_pct
        && !(1..=100).contains(&b)
    {
        return Err(ApiError::BadRequest("brightness_pct must be 1-100".into()));
    }
    let allowed =
        s.db.q1(
            "SELECT allowed FROM entities WHERE entity_id = ?1 AND domain = 'light'",
            vec![P::T(entity_id.to_string())],
            |r| r.get::<_, i64>(0),
        )
        .await?;
    match allowed {
        Some(1) => {}
        Some(_) => {
            return Err(ApiError::Forbidden(
                "light is imported but not authorized — enable it under Devices".into(),
            ));
        }
        None => {
            return Err(ApiError::Forbidden(
                "entity is not a known authorized light".into(),
            ));
        }
    }
    let result =
        s.ha.light_command(entity_id, action, brightness_pct)
            .await?;
    let outcome = if result.confirmed {
        "confirmed"
    } else {
        "accepted_unconfirmed"
    };
    s.audit(
        Some(user.id),
        &user.username,
        "control_light",
        entity_id,
        &json!({"action": action, "brightness_pct": brightness_pct}),
        outcome,
        &result.detail,
    )
    .await;
    s.emit(
        "entity_state",
        json!({"entity_id": entity_id, "state": result.observed_state}),
    );
    Ok(json!({
        "ok": true,
        "entity_id": entity_id,
        "action": action,
        "confirmed": result.confirmed,
        "observed_state": result.observed_state,
        "detail": result.detail,
    }))
}

async fn command(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CommandBody>,
) -> Result<Json<Value>, ApiError> {
    let out = execute_light(&s, &user, &id, &b.action, b.brightness_pct).await?;
    Ok(Json(out))
}
