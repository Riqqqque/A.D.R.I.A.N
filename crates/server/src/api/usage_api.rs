//! Usage, prices and monthly budgets. Owner-only: spending is household
//! administration, not something every member should see or change.

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::state::AppState;
use crate::{providers, usage, util};
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/usage", get(summary))
        .route(
            "/api/usage/prices",
            get(list_prices).put(put_price).delete(delete_price),
        )
        .route("/api/usage/budgets", get(summary_budgets).post(put_budget))
        .route("/api/usage/budgets/{id}", delete(delete_budget))
}

/// Known modules a budget can be scoped to.
const MODULES: &[&str] = &["web", "satellite", "discord"];

fn usd_to_micros(v: f64, field: &str) -> Result<i64, ApiError> {
    if !v.is_finite() || !(0.0..=1_000_000.0).contains(&v) {
        return Err(ApiError::BadRequest(format!(
            "{field} must be between 0 and 1000000 USD"
        )));
    }
    Ok((v * 1_000_000.0).round() as i64)
}

#[derive(Deserialize)]
struct MonthQuery {
    month: Option<String>,
}

async fn summary(
    user: AuthUser,
    State(s): State<AppState>,
    Query(q): Query<MonthQuery>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    Ok(Json(usage::summary(&s, q.month.as_deref()).await?))
}

async fn summary_budgets(
    user: AuthUser,
    State(s): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let v = usage::summary(&s, None).await?;
    Ok(Json(json!({"month": v["month"], "budgets": v["budgets"]})))
}

async fn list_prices(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let rows =
        s.db.q(
            "SELECT provider_id, model, input_per_mtok, cached_per_mtok, output_per_mtok,
                audio_per_minute, updated_at FROM model_prices ORDER BY provider_id, model",
            vec![],
            |r| {
                Ok(json!({
                    "provider_id": r.get::<_, String>(0)?,
                    "model": r.get::<_, String>(1)?,
                    "input_per_mtok_micros": r.get::<_, i64>(2)?,
                    "cached_per_mtok_micros": r.get::<_, Option<i64>>(3)?,
                    "output_per_mtok_micros": r.get::<_, i64>(4)?,
                    "audio_per_minute_micros": r.get::<_, i64>(5)?,
                    "updated_at": r.get::<_, i64>(6)?,
                }))
            },
        )
        .await?;
    Ok(Json(json!({"prices": rows})))
}

#[derive(Deserialize)]
struct PriceBody {
    provider_id: String,
    model: String,
    /// USD per million input tokens.
    input_per_mtok: f64,
    /// USD per million cached input tokens (defaults to the input price).
    cached_per_mtok: Option<f64>,
    /// USD per million output tokens.
    output_per_mtok: f64,
    /// USD per minute of transcribed audio.
    #[serde(default)]
    audio_per_minute: f64,
}

async fn put_price(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<PriceBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let model = b.model.trim();
    if model.is_empty() || model.len() > 200 {
        return Err(ApiError::BadRequest("model must be 1-200 chars".into()));
    }
    let rows = providers::load_provider_rows(&s.db).await?;
    if !rows.iter().any(|r| r.id == b.provider_id) {
        return Err(ApiError::NotFound(format!("provider '{}'", b.provider_id)));
    }
    let input = usd_to_micros(b.input_per_mtok, "input_per_mtok")?;
    let cached = b
        .cached_per_mtok
        .map(|v| usd_to_micros(v, "cached_per_mtok"))
        .transpose()?;
    let output = usd_to_micros(b.output_per_mtok, "output_per_mtok")?;
    let audio = usd_to_micros(b.audio_per_minute, "audio_per_minute")?;
    s.db.exec(
        "INSERT INTO model_prices(provider_id,model,input_per_mtok,cached_per_mtok,
            output_per_mtok,audio_per_minute,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(provider_id,model) DO UPDATE SET input_per_mtok=excluded.input_per_mtok,
            cached_per_mtok=excluded.cached_per_mtok, output_per_mtok=excluded.output_per_mtok,
            audio_per_minute=excluded.audio_per_minute, updated_at=excluded.updated_at",
        vec![
            P::T(b.provider_id.clone()),
            P::T(model.to_string()),
            P::I(input),
            cached.map(P::I).unwrap_or(P::Null),
            P::I(output),
            P::I(audio),
            P::I(util::now_unix()),
        ],
    )
    .await?;
    s.audit(
        Some(user.id),
        "user",
        "usage.price.set",
        &format!("{}/{model}", b.provider_id),
        &json!({"input_per_mtok": b.input_per_mtok, "output_per_mtok": b.output_per_mtok}),
        "ok",
        "",
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct PriceKey {
    provider_id: String,
    model: String,
}

async fn delete_price(
    user: AuthUser,
    State(s): State<AppState>,
    Query(k): Query<PriceKey>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let n =
        s.db.exec(
            "DELETE FROM model_prices WHERE provider_id = ?1 AND model = ?2",
            vec![P::T(k.provider_id), P::T(k.model)],
        )
        .await?;
    if n == 0 {
        return Err(ApiError::NotFound("price".into()));
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct BudgetBody {
    scope: String,
    #[serde(default)]
    scope_id: String,
    /// Monthly limit in USD.
    monthly_limit: f64,
    soft_pct: Option<i64>,
    enabled: Option<bool>,
}

/// Create or replace the budget for a scope (one per scope).
async fn put_budget(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<BudgetBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let scope_id = match b.scope.as_str() {
        "global" => String::new(),
        "provider" => {
            let rows = providers::load_provider_rows(&s.db).await?;
            if !rows.iter().any(|r| r.id == b.scope_id) {
                return Err(ApiError::NotFound(format!("provider '{}'", b.scope_id)));
            }
            b.scope_id.clone()
        }
        "module" => {
            if !MODULES.contains(&b.scope_id.as_str()) {
                return Err(ApiError::BadRequest(format!(
                    "module must be one of {}",
                    MODULES.join(", ")
                )));
            }
            b.scope_id.clone()
        }
        _ => {
            return Err(ApiError::BadRequest(
                "scope must be global|provider|module".into(),
            ));
        }
    };
    let limit = usd_to_micros(b.monthly_limit, "monthly_limit")?;
    let soft = b.soft_pct.unwrap_or(80);
    if !(1..=100).contains(&soft) {
        return Err(ApiError::BadRequest("soft_pct must be 1-100".into()));
    }
    let id =
        s.db.q1(
            "SELECT id FROM budgets WHERE scope = ?1 AND scope_id = ?2",
            vec![P::T(b.scope.clone()), P::T(scope_id.clone())],
            |r| r.get::<_, String>(0),
        )
        .await?
        .unwrap_or_else(util::new_id);
    s.db.exec(
        "INSERT INTO budgets(id,scope,scope_id,monthly_limit_micros,soft_pct,enabled,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(id) DO UPDATE SET monthly_limit_micros=excluded.monthly_limit_micros,
            soft_pct=excluded.soft_pct, enabled=excluded.enabled",
        vec![
            P::T(id.clone()),
            P::T(b.scope.clone()),
            P::T(scope_id.clone()),
            P::I(limit),
            P::I(soft),
            P::I(b.enabled.unwrap_or(true) as i64),
            P::I(util::now_unix()),
        ],
    )
    .await?;
    // A changed limit deserves a fresh soft warning this month.
    s.db.exec(
        "DELETE FROM settings WHERE key LIKE ?1",
        vec![P::T(format!("budget_warned:{id}:%"))],
    )
    .await?;
    s.audit(
        Some(user.id),
        "user",
        "usage.budget.set",
        &format!("{}:{scope_id}", b.scope),
        &json!({"monthly_limit": b.monthly_limit, "soft_pct": soft}),
        "ok",
        "",
    )
    .await;
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn delete_budget(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    let n =
        s.db.exec("DELETE FROM budgets WHERE id = ?1", vec![P::T(id.clone())])
            .await?;
    if n == 0 {
        return Err(ApiError::NotFound("budget".into()));
    }
    s.db.exec(
        "DELETE FROM settings WHERE key LIKE ?1",
        vec![P::T(format!("budget_warned:{id}:%"))],
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}
