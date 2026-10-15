//! Metered provider usage, prices and monthly budgets.
//!
//! Every chat round and speech-to-text call is recorded in `usage_events`
//! with the token counts the provider reported (or an estimate, flagged as
//! such, when it reports none). Cost is computed from the user-entered
//! `model_prices`; no prices are built in because they go stale. Local
//! endpoints are free by rule and never blocked. A cloud model without a
//! price row records a NULL cost and is reported as unpriced, because a
//! budget cannot bound what it cannot price.
//!
//! Budgets are monthly (calendar month in the household timezone) and scoped
//! globally, per provider, or per module (`web`, `satellite`, later
//! `discord`). Crossing `soft_pct` emits one warning per budget per month;
//! reaching the limit refuses further paid calls with the real numbers.

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers::{self, ProviderRow};
use crate::state::AppState;
use crate::util;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

/// Token counts for one provider call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub cached: i64,
    pub output: i64,
    /// True when the provider reported nothing and counts were estimated.
    pub estimated: bool,
}

impl Usage {
    /// Rough count for providers that report no usage: ~4 chars per token.
    pub fn estimate(input_chars: usize, output_chars: usize) -> Self {
        Self {
            input: input_chars.div_ceil(4) as i64,
            cached: 0,
            output: output_chars.div_ceil(4) as i64,
            estimated: true,
        }
    }
}

/// Prices in micro-USD: per million tokens, and per minute of audio.
#[derive(Debug, Clone, Copy, Default)]
pub struct Price {
    pub input_per_mtok: i64,
    /// Cached-input price; falls back to the input price when unset.
    pub cached_per_mtok: Option<i64>,
    pub output_per_mtok: i64,
    pub audio_per_minute: i64,
}

/// Cost of one call in micro-USD (rounded up, so budgets err on the safe side).
pub fn cost_micros(price: &Price, u: &Usage, audio_seconds: f64) -> i64 {
    let cached = u.cached.clamp(0, u.input.max(0));
    let uncached = u.input.max(0) - cached;
    let tokens = uncached as i128 * price.input_per_mtok as i128
        + cached as i128 * price.cached_per_mtok.unwrap_or(price.input_per_mtok) as i128
        + u.output.max(0) as i128 * price.output_per_mtok as i128;
    let token_cost = (tokens + 999_999) / 1_000_000;
    let audio_cost = (audio_seconds.max(0.0) / 60.0 * price.audio_per_minute as f64).ceil() as i128;
    (token_cost + audio_cost).min(i64::MAX as i128) as i64
}

/// `[start, end)` of the calendar month containing `now` in `tz`, plus its
/// `YYYY-MM` label.
pub fn month_window(tz: &TimeZone, now: jiff::Timestamp) -> (i64, i64, String) {
    let z = now.to_zoned(tz.clone());
    let first = jiff::civil::date(z.year(), z.month(), 1);
    let next = if z.month() == 12 {
        jiff::civil::date(z.year() + 1, 1, 1)
    } else {
        jiff::civil::date(z.year(), z.month() + 1, 1)
    };
    let start = first
        .to_zoned(tz.clone())
        .map(|z| z.timestamp().as_second())
        .unwrap_or(0);
    let end = next
        .to_zoned(tz.clone())
        .map(|z| z.timestamp().as_second())
        .unwrap_or(i64::MAX);
    (start, end, format!("{:04}-{:02}", z.year(), z.month()))
}

/// Window for an explicit `YYYY-MM` label.
fn month_window_for(tz: &TimeZone, label: &str) -> Result<(i64, i64, String), ApiError> {
    let bad = || ApiError::BadRequest("month must be YYYY-MM".into());
    let (y, m) = label.split_once('-').ok_or_else(bad)?;
    let (y, m): (i16, i8) = (y.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    let mid = jiff::civil::Date::new(y, m, 15)
        .map_err(|_| bad())?
        .to_zoned(tz.clone())
        .map_err(|_| bad())?
        .timestamp();
    Ok(month_window(tz, mid))
}

/// Which part of the system made a request; budgets can be scoped to it.
pub fn module_for(user: &AuthUser) -> &'static str {
    if user.session_id.starts_with("satellite:") {
        "satellite"
    } else {
        "web"
    }
}

pub fn fmt_usd(micros: i64) -> String {
    let dollars = micros as f64 / 1_000_000.0;
    if dollars != 0.0 && dollars.abs() < 1.0 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

async fn price_for(state: &AppState, row: &ProviderRow) -> Result<Option<Price>, ApiError> {
    state
        .db
        .q1(
            "SELECT input_per_mtok, cached_per_mtok, output_per_mtok, audio_per_minute
             FROM model_prices WHERE provider_id = ?1 AND model = ?2",
            vec![P::T(row.id.clone()), P::T(row.model.clone())],
            |r| {
                Ok(Price {
                    input_per_mtok: r.get(0)?,
                    cached_per_mtok: r.get(1)?,
                    output_per_mtok: r.get(2)?,
                    audio_per_minute: r.get(3)?,
                })
            },
        )
        .await
}

/// Record one provider call. Failures are logged, never surfaced: metering
/// must not break the assistant.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    state: &AppState,
    row: &ProviderRow,
    capability: &str,
    module: &str,
    user_id: Option<i64>,
    usage: &Usage,
    audio_seconds: f64,
) {
    let local = providers::base_is_local(&row.base_url);
    let cost = if local {
        Some(0)
    } else {
        match price_for(state, row).await {
            Ok(Some(p)) => Some(cost_micros(&p, usage, audio_seconds)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "price lookup failed");
                None
            }
        }
    };
    let res = state
        .db
        .exec(
            "INSERT INTO usage_events(at,provider_id,provider_label,model,capability,module,
                user_id,input_tokens,cached_tokens,output_tokens,audio_seconds,cost_micros,
                estimated,local)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            vec![
                P::I(util::now_unix()),
                P::T(row.id.clone()),
                P::T(row.label.clone()),
                P::T(row.model.clone()),
                P::T(capability.to_string()),
                P::T(module.to_string()),
                user_id.map(P::I).unwrap_or(P::Null),
                P::I(usage.input),
                P::I(usage.cached),
                P::I(usage.output),
                P::F(audio_seconds),
                cost.map(P::I).unwrap_or(P::Null),
                P::I(usage.estimated as i64),
                P::I(local as i64),
            ],
        )
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "usage record failed");
    }
}

#[derive(Debug, Clone)]
struct Budget {
    id: String,
    scope: String,
    scope_id: String,
    limit: i64,
    soft_pct: i64,
}

async fn spent(state: &AppState, b: &Budget, start: i64, end: i64) -> Result<i64, ApiError> {
    let (filter, mut params) = match b.scope.as_str() {
        "provider" => ("AND provider_id = ?3", vec![P::T(b.scope_id.clone())]),
        "module" => ("AND module = ?3", vec![P::T(b.scope_id.clone())]),
        _ => ("", vec![]),
    };
    let mut all = vec![P::I(start), P::I(end)];
    all.append(&mut params);
    Ok(state
        .db
        .q1(
            &format!(
                "SELECT COALESCE(SUM(cost_micros),0) FROM usage_events
                 WHERE at >= ?1 AND at < ?2 {filter}"
            ),
            all,
            |r| r.get::<_, i64>(0),
        )
        .await?
        .unwrap_or(0))
}

fn describe(b: &Budget) -> String {
    match b.scope.as_str() {
        "provider" => format!("provider '{}'", b.scope_id),
        "module" => format!("{} module", b.scope_id),
        _ => "household".into(),
    }
}

/// Gate a paid call. Local providers always pass. Returns soft-limit
/// warnings to show the user; errors with the real numbers when a budget
/// covering this call is used up.
pub async fn check(
    state: &AppState,
    row: &ProviderRow,
    module: &str,
) -> Result<Vec<String>, ApiError> {
    if providers::base_is_local(&row.base_url) {
        return Ok(vec![]);
    }
    let budgets = state
        .db
        .q(
            "SELECT id, scope, scope_id, monthly_limit_micros, soft_pct FROM budgets
             WHERE enabled = 1",
            vec![],
            |r| {
                Ok(Budget {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    scope_id: r.get(2)?,
                    limit: r.get(3)?,
                    soft_pct: r.get(4)?,
                })
            },
        )
        .await?;
    let applicable: Vec<Budget> = budgets
        .into_iter()
        .filter(|b| match b.scope.as_str() {
            "provider" => b.scope_id == row.id,
            "module" => b.scope_id == module,
            _ => true,
        })
        .collect();
    if applicable.is_empty() {
        return Ok(vec![]);
    }
    let tz = state.timezone().await.unwrap_or(TimeZone::UTC);
    let (start, end, label) = month_window(&tz, jiff::Timestamp::now());
    let mut warnings = Vec::new();
    for b in applicable {
        let used = spent(state, &b, start, end).await?;
        if used >= b.limit {
            return Err(ApiError::Forbidden(format!(
                "the {} budget for {label} is used up ({} of {}). Raise it under System, or switch to a local model.",
                describe(&b),
                fmt_usd(used),
                fmt_usd(b.limit)
            )));
        }
        if used as i128 * 100 >= b.limit as i128 * b.soft_pct as i128 {
            let marker = format!("budget_warned:{}:{label}", b.id);
            if state.get_setting(&marker).await?.is_none() {
                state.set_setting(&marker, "1").await?;
                let msg = format!(
                    "The {} budget for {label} is at {} of {} ({}%).",
                    describe(&b),
                    fmt_usd(used),
                    fmt_usd(b.limit),
                    used as i128 * 100 / b.limit.max(1) as i128
                );
                state.emit("budget", json!({"budget_id": b.id, "message": msg}));
                warnings.push(msg);
            }
        }
    }
    Ok(warnings)
}

/// Enabled cloud providers whose current model has no price row.
pub async fn unpriced(state: &AppState) -> Result<Vec<String>, ApiError> {
    let rows = providers::load_provider_rows(&state.db).await?;
    let mut out = Vec::new();
    for r in rows
        .iter()
        .filter(|r| r.enabled && !providers::base_is_local(&r.base_url))
    {
        if price_for(state, r).await?.is_none() {
            out.push(format!("{} ({})", r.label, r.model));
        }
    }
    // Models that actually ran this month without a price (e.g. the STT
    // model, which differs from the provider's chat model).
    let tz = state.timezone().await.unwrap_or(TimeZone::UTC);
    let (start, end, _) = month_window(&tz, jiff::Timestamp::now());
    let seen = state
        .db
        .q(
            "SELECT DISTINCT provider_label, model FROM usage_events
             WHERE at >= ?1 AND at < ?2 AND cost_micros IS NULL",
            vec![P::I(start), P::I(end)],
            |r| {
                Ok(format!(
                    "{} ({})",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                ))
            },
        )
        .await?;
    for s in seen {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// Month summary for the UI: totals by provider/model/capability/module and
/// each budget's standing.
pub async fn summary(state: &AppState, month: Option<&str>) -> Result<Value, ApiError> {
    let tz = state.timezone().await.unwrap_or(TimeZone::UTC);
    let (start, end, label) = match month {
        Some(m) => month_window_for(&tz, m)?,
        None => month_window(&tz, jiff::Timestamp::now()),
    };
    let rows = state
        .db
        .q(
            "SELECT provider_id, provider_label, model, capability, module, COUNT(*),
                SUM(input_tokens), SUM(cached_tokens), SUM(output_tokens), SUM(audio_seconds),
                SUM(cost_micros), SUM(cost_micros IS NULL), SUM(estimated), MAX(local)
             FROM usage_events WHERE at >= ?1 AND at < ?2
             GROUP BY provider_id, model, capability, module
             ORDER BY SUM(cost_micros) DESC",
            vec![P::I(start), P::I(end)],
            |r| {
                Ok(json!({
                    "provider_id": r.get::<_, String>(0)?,
                    "provider": r.get::<_, String>(1)?,
                    "model": r.get::<_, String>(2)?,
                    "capability": r.get::<_, String>(3)?,
                    "module": r.get::<_, String>(4)?,
                    "calls": r.get::<_, i64>(5)?,
                    "input_tokens": r.get::<_, i64>(6)?,
                    "cached_tokens": r.get::<_, i64>(7)?,
                    "output_tokens": r.get::<_, i64>(8)?,
                    "audio_seconds": r.get::<_, f64>(9)?,
                    "cost_micros": r.get::<_, Option<i64>>(10)?.unwrap_or(0),
                    "unpriced_calls": r.get::<_, i64>(11)?,
                    "estimated_calls": r.get::<_, i64>(12)?,
                    "local": r.get::<_, i64>(13)? == 1,
                }))
            },
        )
        .await?;
    let total: i64 = rows
        .iter()
        .map(|r| r["cost_micros"].as_i64().unwrap_or(0))
        .sum();
    let budgets = state
        .db
        .q(
            "SELECT id, scope, scope_id, monthly_limit_micros, soft_pct, enabled FROM budgets
             ORDER BY scope, scope_id",
            vec![],
            |r| {
                Ok((
                    Budget {
                        id: r.get(0)?,
                        scope: r.get(1)?,
                        scope_id: r.get(2)?,
                        limit: r.get(3)?,
                        soft_pct: r.get(4)?,
                    },
                    r.get::<_, i64>(5)? == 1,
                ))
            },
        )
        .await?;
    let mut budget_json = Vec::new();
    for (b, enabled) in budgets {
        let used = spent(state, &b, start, end).await?;
        budget_json.push(json!({
            "id": b.id,
            "scope": b.scope,
            "scope_id": b.scope_id,
            "monthly_limit_micros": b.limit,
            "soft_pct": b.soft_pct,
            "enabled": enabled,
            "spent_micros": used,
        }));
    }
    Ok(json!({
        "month": label,
        "total_cost_micros": total,
        "rows": rows,
        "budgets": budget_json,
        "unpriced": unpriced(state).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_uses_cached_price_and_rounds_up() {
        let p = Price {
            input_per_mtok: 2_000_000,      // $2 / 1M
            cached_per_mtok: Some(500_000), // $0.50 / 1M
            output_per_mtok: 8_000_000,     // $8 / 1M
            audio_per_minute: 6_000,        // $0.006 / min
        };
        let u = Usage {
            input: 1_000,
            cached: 400,
            output: 250,
            estimated: false,
        };
        // 600*2 + 400*0.5 + 250*8 = 1200 + 200 + 2000 = 3400 micro-USD
        assert_eq!(cost_micros(&p, &u, 0.0), 3_400);
        // 30 s of audio = 3000 micro-USD
        assert_eq!(cost_micros(&p, &Usage::default(), 30.0), 3_000);
        // Fractions round up.
        let one = Usage {
            input: 1,
            ..Usage::default()
        };
        assert_eq!(cost_micros(&p, &one, 0.0), 2);
        // Cached price falls back to input price.
        let p2 = Price {
            cached_per_mtok: None,
            ..p
        };
        assert_eq!(cost_micros(&p2, &u, 0.0), 1_000 * 2 + 250 * 8);
    }

    #[test]
    fn month_window_follows_local_time_across_dst() {
        let tz = TimeZone::get("America/Denver").unwrap();
        // 2026-03-31 23:30 local (MDT) is still March.
        let late_march = jiff::civil::date(2026, 3, 31)
            .at(23, 30, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp();
        let (start, end, label) = month_window(&tz, late_march);
        assert_eq!(label, "2026-03");
        // March starts in MST (UTC-7) and ends in MDT (UTC-6).
        assert_eq!(
            start,
            "2026-03-01T07:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_second()
        );
        assert_eq!(
            end,
            "2026-04-01T06:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_second()
        );
        assert!(late_march.as_second() < end);
        // December rolls into the next year.
        let dec = "2026-12-15T12:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let (_, end, label) = month_window(&tz, dec);
        assert_eq!(label, "2026-12");
        assert_eq!(
            end,
            "2027-01-01T07:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_second()
        );
    }

    #[test]
    fn estimate_rounds_up() {
        let u = Usage::estimate(9, 1);
        assert_eq!((u.input, u.output, u.estimated), (3, 1, true));
    }
}
