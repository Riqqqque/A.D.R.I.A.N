//! Integration configuration: AI providers and the Home Assistant
//! connection. Secrets are written to the server-side secrets table and
//! never returned by the API.

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers;
use crate::state::AppState;
use crate::util;
use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/integrations/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/api/integrations/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .route("/api/integrations/providers/{id}/test", post(test_provider))
        .route(
            "/api/integrations/providers/{id}/default",
            post(default_provider),
        )
        .route("/api/integrations/ha", get(get_ha).put(put_ha))
        .route("/api/integrations/ha/test", post(test_ha))
        .route("/api/integrations/ha/import", post(import_ha))
}

// ---- providers -----------------------------------------------------------

async fn list_providers(
    _user: AuthUser,
    State(s): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let rows = providers::load_provider_rows(&s.db).await?;
    let mut out = Vec::new();
    for r in rows {
        let has_key = match &r.secret_name {
            Some(n) => s.has_secret(n).await?,
            None => false,
        };
        out.push(json!({
            "id": r.id, "kind": r.kind, "label": r.label, "base_url": r.base_url,
            "model": r.model, "enabled": r.enabled, "is_default": r.is_default,
            "has_key": has_key, "timeout_secs": r.timeout_secs,
            "local": providers::base_is_local(&r.base_url),
        }));
    }
    Ok(Json(json!({"providers": out})))
}

#[derive(Deserialize)]
struct ProviderBody {
    kind: String,
    label: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    timeout_secs: Option<i64>,
    enabled: Option<bool>,
}

fn validate_provider_body(b: &ProviderBody) -> Result<(), ApiError> {
    if !matches!(b.kind.as_str(), "openai_responses" | "openai_compatible") {
        return Err(ApiError::BadRequest(
            "kind must be openai_responses|openai_compatible".into(),
        ));
    }
    if b.label.trim().is_empty() || b.label.len() > 80 {
        return Err(ApiError::BadRequest("label must be 1-80 chars".into()));
    }
    if !b.base_url.starts_with("http://") && !b.base_url.starts_with("https://") {
        return Err(ApiError::BadRequest("base_url must be http(s)".into()));
    }
    if b.model.trim().is_empty() || b.model.len() > 120 {
        return Err(ApiError::BadRequest("model must be 1-120 chars".into()));
    }
    Ok(())
}

async fn create_provider(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<ProviderBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    validate_provider_body(&b)?;
    let id = util::new_id();
    let secret_name = match &b.api_key {
        Some(k) if !k.is_empty() => {
            let name = format!("provider:{id}");
            s.set_secret(&name, k).await?;
            Some(name)
        }
        _ => None,
    };
    let count: i64 =
        s.db.q1("SELECT COUNT(*) FROM providers", vec![], |r| r.get(0))
            .await?
            .unwrap_or(0);
    let extra = json!({"timeout_secs": b.timeout_secs.unwrap_or(90).clamp(5, 600)});
    s.db.exec(
        "INSERT INTO providers(id,kind,label,base_url,model,secret_name,enabled,is_default,extra,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        vec![
            P::T(id.clone()),
            P::T(b.kind),
            P::T(b.label.trim().to_string()),
            P::T(b.base_url.trim_end_matches('/').to_string()),
            P::T(b.model.trim().to_string()),
            secret_name.map(P::T).unwrap_or(P::Null),
            P::I(if b.enabled.unwrap_or(true) { 1 } else { 0 }),
            P::I(if count == 0 { 1 } else { 0 }),
            P::T(extra.to_string()),
            P::I(util::now_unix()),
        ],
    )
    .await?;
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn update_provider(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ProviderBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    validate_provider_body(&b)?;
    if let Some(k) = &b.api_key {
        let name = format!("provider:{id}");
        if k.is_empty() {
            s.db.exec("DELETE FROM secrets WHERE name = ?1", vec![P::T(name)])
                .await?;
        } else {
            s.set_secret(&name, k).await?;
        }
    }
    let extra = json!({"timeout_secs": b.timeout_secs.unwrap_or(90).clamp(5, 600)});
    let n =
        s.db.exec(
            "UPDATE providers SET kind=?1, label=?2, base_url=?3, model=?4,
            enabled=?5, extra=?6 WHERE id=?7",
            vec![
                P::T(b.kind),
                P::T(b.label.trim().to_string()),
                P::T(b.base_url.trim_end_matches('/').to_string()),
                P::T(b.model.trim().to_string()),
                P::I(if b.enabled.unwrap_or(true) { 1 } else { 0 }),
                P::T(extra.to_string()),
                P::T(id.clone()),
            ],
        )
        .await?;
    if n == 0 {
        return Err(ApiError::NotFound("provider".into()));
    }
    Ok(Json(json!({"ok": true})))
}

async fn delete_provider(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    s.db.exec(
        "DELETE FROM providers WHERE id = ?1",
        vec![P::T(id.clone())],
    )
    .await?;
    s.db.exec(
        "DELETE FROM secrets WHERE name = ?1",
        vec![P::T(format!("provider:{id}"))],
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn default_provider(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    s.db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute("UPDATE providers SET is_default = 0", [])?;
        let n = tx.execute(
            "UPDATE providers SET is_default = 1 WHERE id = ?1",
            rusqlite::params![id],
        )?;
        tx.commit()?;
        if n == 0 {
            return Err(ApiError::NotFound("provider".into()));
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn test_provider(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let (provider, row) =
        providers::load_provider(&s.db, &s.secret_box, s.http.clone(), Some(&id)).await?;
    match provider.test().await {
        Ok(msg) => Ok(Json(json!({
            "ok": true, "detail": msg, "kind": row.kind, "model": row.model,
        }))),
        Err(e) => Ok(Json(json!({
            "ok": false, "detail": e.to_string(), "kind": row.kind, "model": row.model,
        }))),
    }
}

// ---- home assistant ------------------------------------------------------

async fn get_ha(_user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let row = s
        .db
        .q1(
            "SELECT url, secret_name, enabled, last_ok_at, last_error FROM ha_connection WHERE id = 1",
            vec![],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .await?;
    let status = s.ha.status_json_now();
    let configured = match &row {
        Some((url, secret, enabled, last_ok, last_err)) => json!({
            "configured": true,
            "url": url,
            "has_token": s.has_secret(secret).await?,
            "enabled": *enabled != 0,
            "last_ok_at": last_ok,
            "last_error": last_err,
        }),
        None => json!({"configured": false}),
    };
    Ok(Json(json!({"connection": configured, "live": status})))
}

#[derive(Deserialize)]
struct HaBody {
    url: String,
    token: Option<String>,
    enabled: Option<bool>,
}

async fn put_ha(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<HaBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let url = b.url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ApiError::BadRequest("url must be http(s)".into()));
    }
    // Blank token keeps the existing secret; first save requires one.
    let tok = b.token.as_deref().map(str::trim).unwrap_or("");
    let secret_name = if tok.is_empty() {
        let existing: Option<String> =
            s.db.q1(
                "SELECT secret_name FROM ha_connection WHERE id = 1",
                vec![],
                |r| r.get(0),
            )
            .await?;
        existing.ok_or_else(|| ApiError::BadRequest("token required on first save".into()))?
    } else {
        if tok.len() < 10 {
            return Err(ApiError::BadRequest(
                "token looks too short — paste a HA long-lived access token".into(),
            ));
        }
        let name = "home_assistant:token".to_string();
        s.set_secret(&name, tok).await?;
        name
    };
    s.db.exec(
        "INSERT INTO ha_connection(id,url,secret_name,enabled) VALUES(1,?1,?2,?3)
         ON CONFLICT(id) DO UPDATE SET url=excluded.url, secret_name=excluded.secret_name,
            enabled=excluded.enabled",
        vec![
            P::T(url),
            P::T(secret_name),
            P::I(if b.enabled.unwrap_or(true) { 1 } else { 0 }),
        ],
    )
    .await?;
    s.ha.notify_reload();
    Ok(Json(json!({"ok": true})))
}

async fn test_ha(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    match s.ha.test_connection().await {
        Ok(msg) => Ok(Json(json!({"ok": true, "detail": msg}))),
        Err(e) => Ok(Json(json!({"ok": false, "detail": e.to_string()}))),
    }
}

async fn import_ha(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let count = s.ha.import_entities().await?;
    s.emit("entities", json!({"event": "imported", "count": count}));
    Ok(Json(json!({"ok": true, "imported": count})))
}
