//! Health + diagnostics: real component status, explicit missing-config
//! states, consistent online backups, and the action ledger. No fake
//! "connected" states.

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers;
use crate::state::AppState;
use crate::util;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/diagnostics", get(diagnostics))
        .route("/api/diagnostics/backup", post(backup))
        .route("/api/diagnostics/ledger", get(ledger))
}

/// Unauthenticated liveness probe (no details beyond "alive").
async fn health() -> Json<Value> {
    Json(json!({"ok": true, "service": "adrian", "version": env!("CARGO_PKG_VERSION")}))
}

async fn diagnostics(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let db_info =
        s.db.call(|c| {
            let version: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
            let journal: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            Ok(json!({
                "ok": true, "sqlite_version": version,
                "journal_mode": journal, "synchronous": sync,
            }))
        })
        .await
        .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));

    let provider_rows = providers::load_provider_rows(&s.db)
        .await
        .unwrap_or_default();
    let providers: Vec<Value> = provider_rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id, "kind": r.kind, "label": r.label, "model": r.model,
                "enabled": r.enabled, "is_default": r.is_default,
            })
        })
        .collect();

    let counts = |sql: &'static str| {
        let db = s.db.clone();
        async move {
            db.q1(sql, vec![], |r| r.get::<_, i64>(0))
                .await
                .ok()
                .flatten()
                .unwrap_or(0)
        }
    };
    let users = counts("SELECT COUNT(*) FROM users").await;
    let entities = counts("SELECT COUNT(*) FROM entities").await;
    let lights = counts("SELECT COUNT(*) FROM entities WHERE allowed=1 AND domain='light'").await;
    let endpoints = counts("SELECT COUNT(*) FROM endpoints WHERE enabled=1").await;
    let scheduled = counts(
        "SELECT COUNT(*) FROM alarms WHERE status IN ('scheduled','ringing') AND deleted_at IS NULL",
    )
    .await;
    let facts = counts("SELECT COUNT(*) FROM facts WHERE deleted_at IS NULL").await;

    let flags = s.setup_flags().await?;
    let mut warnings = Vec::new();
    if flags["provider_configured"] == json!(false) {
        warnings.push("no AI provider configured — chat is unavailable");
    }
    if flags["ha_configured"] == json!(false) {
        warnings.push("Home Assistant is not connected — device control is unavailable");
    }
    if flags["audio_endpoint_configured"] == json!(false) {
        warnings.push("no audio endpoint — alarms cannot make sound yet");
    }
    warnings.push(
        "preview build: the local database and backups are stored unencrypted — see docs/SECURITY.md",
    );

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": s.started.elapsed().as_secs(),
        "user": {"id": user.id, "role": user.role},
        "db": db_info,
        "providers": providers,
        "home_assistant": s.ha.status_json_now(),
        "counts": {
            "users": users, "entities": entities, "allowed_lights": lights,
            "audio_endpoints": endpoints, "scheduled_alarms": scheduled,
            "facts": facts,
        },
        "warnings": warnings,
    })))
}

/// Consistent online backup into <data>/backups — not a filesystem copy of
/// the live db.
async fn backup(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let dir = s.config.data_dir.join("backups");
    std::fs::create_dir_all(&dir).map_err(ApiError::internal)?;
    let dest = dir.join(format!("adrian-{}.db", util::now_unix()));
    s.db.backup_to(dest.clone()).await?;
    Ok(Json(json!({
        "ok": true,
        "backup": dest.display().to_string(),
        "note": "backup contains local state; verify a restore before relying on it",
    })))
}

async fn ledger(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let rows =
        s.db.q(
            "SELECT id, user_id, actor, action, target, outcome, detail, created_at
             FROM action_ledger ORDER BY id DESC LIMIT 100",
            vec![],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "user_id": r.get::<_, Option<i64>>(1)?,
                    "actor": r.get::<_, String>(2)?,
                    "action": r.get::<_, String>(3)?,
                    "target": r.get::<_, String>(4)?,
                    "outcome": r.get::<_, String>(5)?,
                    "detail": r.get::<_, String>(6)?,
                    "created_at": r.get::<_, i64>(7)?,
                }))
            },
        )
        .await?;
    let _ = P::Null; // keep import used if query shape changes
    Ok(Json(json!({"entries": rows})))
}
