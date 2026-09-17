//! Typed tools the model may propose. Every call goes through: parse ->
//! authenticated scope -> argument validation -> permission check -> execute
//! -> verify -> record -> respond. The model is never the authorization
//! source; there is no generic HA-service tool.

pub mod fastpath;

use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers::{ToolCall, ToolSpec};
use crate::state::AppState;
use crate::{memory, scheduler};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ToolCtx<'a> {
    pub state: &'a Arc<AppState>,
    pub user: &'a AuthUser,
}

pub struct ToolOutcome {
    /// JSON handed back to the model as the tool result.
    pub for_model: Value,
    /// Optional UI-facing event describing what happened.
    pub ui: Option<Value>,
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "get_current_time".into(),
            description: "Get the current date/time in the user's configured timezone.".into(),
            parameters: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "list_lights".into(),
            description:
                "List lights the user has authorized Adrian to control, with current state.".into(),
            parameters: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "control_light".into(),
            description:
                "Turn an authorized light on, off, or toggle it. Only works for allowlisted \
                 light entities; the result reports whether the state change was observed."
                    .into(),
            parameters: schema(
                json!({
                    "entity_id": {"type":"string","description":"allowlisted light entity_id"},
                    "action": {"type":"string","enum":["turn_on","turn_off","toggle"]},
                    "brightness_pct": {"type":"integer","minimum":1,"maximum":100},
                }),
                &["entity_id", "action"],
            ),
        },
        ToolSpec {
            name: "remember".into(),
            description:
                "Persist an explicit user fact/preference. Stored durably with owner, scope \
                 and timestamp — survives restarts."
                    .into(),
            parameters: schema(
                json!({
                    "text": {"type":"string","maxLength":4000},
                    "scope": {"type":"string","enum":["private","household"],"default":"private"},
                }),
                &["text"],
            ),
        },
        ToolSpec {
            name: "recall".into(),
            description: "Search saved facts by keywords within the caller's permitted scope."
                .into(),
            parameters: schema(
                json!({"query": {"type":"string","maxLength":300}}),
                &["query"],
            ),
        },
        ToolSpec {
            name: "set_alarm".into(),
            description: "Schedule a durable alarm/timer. Provide either at_local \
                 ('YYYY-MM-DD HH:MM' or 'HH:MM' in the user's timezone) or in_minutes for an \
                 elapsed timer. Optional recurrence: daily or weekdays. The alarm is committed \
                 to local storage before it is acknowledged and does not need the model or \
                 internet at due time."
                .into(),
            parameters: schema(
                json!({
                    "label": {"type":"string","maxLength":200},
                    "at_local": {"type":"string"},
                    "in_minutes": {"type":"integer","minimum":1,"maximum":10080},
                    "recurrence": {"type":"string","enum":["none","daily","weekdays"],"default":"none"},
                    "endpoint_id": {"type":"string","description":"specific audio endpoint id; omit for default"},
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "list_alarms".into(),
            description: "List scheduled and recent alarms/timers with delivery status.".into(),
            parameters: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "cancel_alarm".into(),
            description: "Cancel a scheduled alarm by id.".into(),
            parameters: schema(json!({"alarm_id": {"type":"string"}}), &["alarm_id"]),
        },
        ToolSpec {
            name: "list_audio_endpoints".into(),
            description: "List configured audio playback endpoints (speakers, browser audio)."
                .into(),
            parameters: schema(json!({}), &[]),
        },
    ]
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest(format!("missing string argument '{key}'")))
}

fn arg_str_opt<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn arg_i64_opt(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

/// Validate + execute one model-proposed call. Errors are returned to the
/// model as structured results — the tool loop should keep going.
pub async fn dispatch(ctx: &ToolCtx<'_>, call: &ToolCall) -> ToolOutcome {
    if call.arguments.get("_unparseable_arguments").is_some() {
        return ToolOutcome {
            for_model: json!({"ok": false, "error": "tool arguments were not valid JSON"}),
            ui: None,
        };
    }
    match call.name.as_str() {
        "get_current_time" => get_current_time(ctx).await,
        "list_lights" => list_lights(ctx).await,
        "control_light" => control_light(ctx, &call.arguments).await,
        "remember" => remember(ctx, &call.arguments).await,
        "recall" => recall(ctx, &call.arguments).await,
        "set_alarm" => set_alarm(ctx, &call.arguments).await,
        "list_alarms" => list_alarms(ctx).await,
        "cancel_alarm" => cancel_alarm(ctx, &call.arguments).await,
        "list_audio_endpoints" => list_audio_endpoints(ctx).await,
        other => ToolOutcome {
            for_model: json!({"ok": false, "error": format!("unknown tool '{other}'")}),
            ui: None,
        },
    }
}

async fn get_current_time(ctx: &ToolCtx<'_>) -> ToolOutcome {
    let tz = ctx
        .state
        .timezone()
        .await
        .unwrap_or(jiff::tz::TimeZone::UTC);
    let z = jiff::Timestamp::now().to_zoned(tz);
    ToolOutcome {
        for_model: json!({
            "ok": true,
            "iso": z.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "display": z.strftime("%A %Y-%m-%d %H:%M %Z").to_string(),
            "unix": crate::util::now_unix(),
        }),
        ui: None,
    }
}

async fn list_lights(ctx: &ToolCtx<'_>) -> ToolOutcome {
    let rows = ctx
        .state
        .db
        .q(
            "SELECT entity_id, name, area, state FROM entities
             WHERE allowed = 1 AND domain = 'light' ORDER BY name",
            vec![],
            |r| {
                Ok(json!({
                    "entity_id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "area": r.get::<_, Option<String>>(2)?,
                    "state": r.get::<_, String>(3)?,
                }))
            },
        )
        .await;
    match rows {
        Ok(lights) => ToolOutcome {
            for_model: json!({"ok": true, "lights": lights}),
            ui: None,
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}

/// Policy gate for physical action: allowlisted light entities, bounded
/// service/parameters only. Everything is recorded in the action ledger.
async fn control_light(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let res = control_light_inner(ctx, args).await;
    let (for_model, ui) = match res {
        Ok(v) => v,
        Err(e) => (json!({"ok": false, "error": e.to_string()}), None),
    };
    let outcome = if for_model.get("confirmed").and_then(|c| c.as_bool()) == Some(true) {
        "confirmed"
    } else if for_model.get("ok").and_then(|c| c.as_bool()) == Some(true) {
        "accepted_unconfirmed"
    } else {
        "rejected_or_failed"
    };
    ctx.state
        .audit(
            Some(ctx.user.id),
            &ctx.user.username,
            "control_light",
            arg_str(args, "entity_id").unwrap_or("?"),
            args,
            outcome,
            &for_model.to_string(),
        )
        .await;
    ToolOutcome { for_model, ui }
}

async fn control_light_inner(
    ctx: &ToolCtx<'_>,
    args: &Value,
) -> Result<(Value, Option<Value>), ApiError> {
    let entity_id = arg_str(args, "entity_id")?;
    let action = arg_str(args, "action")?;
    if !matches!(action, "turn_on" | "turn_off" | "toggle") {
        return Err(ApiError::BadRequest(format!(
            "action '{action}' is not in the light service allowlist"
        )));
    }
    let brightness = arg_i64_opt(args, "brightness_pct");
    if let Some(b) = brightness
        && !(1..=100).contains(&b)
    {
        return Err(ApiError::BadRequest("brightness_pct must be 1-100".into()));
    }
    // Permission: entity must be imported, be a light, and be allowlisted.
    let allowed = ctx
        .state
        .db
        .q1(
            "SELECT allowed FROM entities WHERE entity_id = ?1 AND domain = 'light'",
            vec![P::T(entity_id.to_string())],
            |r| r.get::<_, i64>(0),
        )
        .await?;
    match allowed {
        Some(1) => {}
        Some(_) => {
            return Err(ApiError::Forbidden(format!(
                "light '{entity_id}' is imported but not authorized — enable it under Devices"
            )));
        }
        None => {
            return Err(ApiError::Forbidden(format!(
                "entity '{entity_id}' is not a known authorized light"
            )));
        }
    }
    let result = ctx
        .state
        .ha
        .light_command(entity_id, action, brightness)
        .await?;
    Ok((
        json!({
            "ok": true,
            "entity_id": entity_id,
            "action": action,
            "confirmed": result.confirmed,
            "observed_state": result.observed_state,
            "detail": result.detail,
        }),
        Some(json!({
            "kind": "light_command",
            "entity_id": entity_id,
            "action": action,
            "confirmed": result.confirmed,
        })),
    ))
}

async fn remember(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let text = match arg_str(args, "text") {
        Ok(t) => t.to_string(),
        Err(e) => {
            return ToolOutcome {
                for_model: json!({"ok": false, "error": e.to_string()}),
                ui: None,
            };
        }
    };
    let scope = arg_str_opt(args, "scope").unwrap_or("private");
    // Guests may only write private-scope facts for themselves.
    let scope = if ctx.user.role == "guest" {
        "private"
    } else {
        scope
    };
    match memory::add(&ctx.state.db, ctx.user.id, &text, scope, "chat").await {
        Ok(f) => ToolOutcome {
            for_model: json!({"ok": true, "fact_id": f.id, "scope": f.scope, "persisted": true}),
            ui: Some(json!({"kind": "fact_saved", "fact_id": f.id})),
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}

async fn recall(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let query = match arg_str(args, "query") {
        Ok(q) => q,
        Err(e) => {
            return ToolOutcome {
                for_model: json!({"ok": false, "error": e.to_string()}),
                ui: None,
            };
        }
    };
    match memory::search(&ctx.state.db, ctx.user.id, query, 8).await {
        Ok(facts) => ToolOutcome {
            for_model: json!({
                "ok": true,
                "facts": facts.iter().map(|f| json!({
                    "id": f.id, "text": f.text, "scope": f.scope,
                    "owner": f.owner, "updated_at": f.updated_at,
                })).collect::<Vec<_>>(),
            }),
            ui: None,
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}

async fn set_alarm(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let spec = scheduler::AlarmSpec {
        label: arg_str_opt(args, "label").unwrap_or(""),
        kind: if arg_i64_opt(args, "in_minutes").is_some()
            && arg_str_opt(args, "at_local").is_none()
        {
            "timer"
        } else {
            "alarm"
        },
        original_request: args.to_string().chars().take(500).collect(),
        at_local: arg_str_opt(args, "at_local"),
        in_minutes: arg_i64_opt(args, "in_minutes"),
        recurrence: arg_str_opt(args, "recurrence").unwrap_or("none"),
        endpoint_id: arg_str_opt(args, "endpoint_id"),
        volume: args.get("volume").and_then(|v| v.as_f64()).unwrap_or(0.7),
        snooze_minutes: 9,
    };
    match scheduler::create(ctx.state, ctx.user.id, spec).await {
        Ok(v) => {
            ctx.state
                .audit(
                    Some(ctx.user.id),
                    &ctx.user.username,
                    "set_alarm",
                    v.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                    args,
                    "scheduled",
                    &v.to_string(),
                )
                .await;
            ToolOutcome {
                for_model: json!({
                    "ok": true,
                    "alarm_id": v.get("id"),
                    "resolved_local": v.get("resolved_local"),
                    "endpoint": v.get("endpoint"),
                    "notes": v.get("notes"),
                    "persisted": true,
                }),
                ui: Some(json!({"kind": "alarm_set", "alarm": v})),
            }
        }
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: Some(json!({"kind": "alarm_failed", "error": e.to_string()})),
        },
    }
}

async fn list_alarms(ctx: &ToolCtx<'_>) -> ToolOutcome {
    match scheduler::list_for(&ctx.state.db, ctx.user.id, ctx.user.is_owner()).await {
        Ok(alarms) => ToolOutcome {
            for_model: json!({"ok": true, "alarms": alarms}),
            ui: None,
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}

async fn cancel_alarm(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
    let id = match arg_str(args, "alarm_id") {
        Ok(i) => i.to_string(),
        Err(e) => {
            return ToolOutcome {
                for_model: json!({"ok": false, "error": e.to_string()}),
                ui: None,
            };
        }
    };
    match scheduler::stop_or_cancel(ctx.state, &id, ctx.user.id, ctx.user.is_owner(), "canceled")
        .await
    {
        Ok(()) => ToolOutcome {
            for_model: json!({"ok": true, "canceled": id}),
            ui: Some(json!({"kind": "alarm_canceled", "alarm_id": id})),
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}

async fn list_audio_endpoints(ctx: &ToolCtx<'_>) -> ToolOutcome {
    let rows = ctx
        .state
        .db
        .q(
            "SELECT id,name,kind,enabled FROM endpoints ORDER BY created_at",
            vec![],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?,
                    "enabled": r.get::<_, i64>(3)? != 0,
                }))
            },
        )
        .await;
    match rows {
        Ok(eps) => ToolOutcome {
            for_model: json!({"ok": true, "endpoints": eps}),
            ui: None,
        },
        Err(e) => ToolOutcome {
            for_model: json!({"ok": false, "error": e.to_string()}),
            ui: None,
        },
    }
}
