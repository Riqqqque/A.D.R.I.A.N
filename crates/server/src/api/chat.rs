//! Chat orchestration: deterministic fast path for unambiguous commands,
//! then a bounded model/tool loop. Model-proposed calls are validated by the
//! tool layer; assistant output only claims what results prove.

use crate::api::devices::execute_light;
use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers::{ChatEvent, ChatMessage, ChatRequest};
use crate::state::AppState;
use crate::tools::{self, ToolCtx, fastpath};
use crate::util;
use crate::{memory, providers};
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/chat", post(chat))
        .route("/api/chat/conversations", get(list_conversations))
        .route("/api/chat/conversations/{id}", get(conversation_history))
}

async fn list_conversations(
    user: AuthUser,
    State(s): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let rows =
        s.db.q(
            "SELECT c.id, c.title, c.created_at,
                    (SELECT content FROM messages m
                      WHERE m.conversation_id = c.id
                      ORDER BY m.id DESC LIMIT 1) AS last_message
             FROM conversations c WHERE c.user_id = ?1
             ORDER BY c.created_at DESC LIMIT 50",
            vec![P::I(user.id)],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "title": r.get::<_, String>(1)?,
                    "created_at": r.get::<_, i64>(2)?,
                    "last_message": r.get::<_, Option<String>>(3)?,
                }))
            },
        )
        .await?;
    Ok(Json(json!({"conversations": rows})))
}

async fn conversation_history(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let owned =
        s.db.q1(
            "SELECT 1 FROM conversations WHERE id = ?1 AND user_id = ?2",
            vec![P::T(id.clone()), P::I(user.id)],
            |r| r.get::<_, i64>(0),
        )
        .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound("conversation".into()));
    }
    let rows =
        s.db.q(
            "SELECT role, content, created_at FROM messages
             WHERE conversation_id = ?1 ORDER BY id LIMIT 200",
            vec![P::T(id)],
            |r| {
                Ok(json!({
                    "role": r.get::<_, String>(0)?,
                    "content": r.get::<_, String>(1)?,
                    "created_at": r.get::<_, i64>(2)?,
                }))
            },
        )
        .await?;
    Ok(Json(json!({"messages": rows})))
}

#[derive(Deserialize)]
struct ChatBody {
    message: String,
    conversation_id: Option<String>,
}

const MAX_TOOL_ROUNDS: usize = 8;

/// Pipeline event sink — lets the SSE route and the satellite utterance
/// route share one implementation.
pub type TurnTx = tokio::sync::mpsc::UnboundedSender<(String, Value)>;

fn emit(tx: &TurnTx, kind: &str, data: Value) {
    let _ = tx.send((kind.to_string(), data));
}

async fn chat(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<ChatBody>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let message = b.message.trim().to_string();
    if message.is_empty() || message.chars().count() > 4000 {
        return Err(ApiError::BadRequest("message must be 1-4000 chars".into()));
    }
    let state = Arc::new(s);
    let conv_id =
        ensure_conversation(&state, &user, b.conversation_id.as_deref(), &message).await?;
    save_message(&state, &conv_id, "user", &message).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    tokio::spawn(async move {
        run_turn(&state, &user, &conv_id, &message, &tx).await;
    });
    let stream = async_stream::stream! {
        while let Some((kind, data)) = rx.recv().await {
            yield Ok(Event::default().event(kind).data(data.to_string()));
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))))
}

/// Find or create the conversation a message belongs to.
pub async fn ensure_conversation(
    state: &Arc<AppState>,
    user: &AuthUser,
    conversation_id: Option<&str>,
    title_seed: &str,
) -> Result<String, ApiError> {
    match conversation_id {
        Some(id) => {
            let owned = state
                .db
                .q1(
                    "SELECT 1 FROM conversations WHERE id = ?1 AND user_id = ?2",
                    vec![P::T(id.to_string()), P::I(user.id)],
                    |r| r.get::<_, i64>(0),
                )
                .await?;
            if owned.is_none() {
                return Err(ApiError::NotFound("conversation".into()));
            }
            Ok(id.to_string())
        }
        None => {
            let id = util::new_id();
            state
                .db
                .exec(
                    "INSERT INTO conversations(id,user_id,title,created_at)
                     VALUES(?1,?2,?3,?4)",
                    vec![
                        P::T(id.clone()),
                        P::I(user.id),
                        P::T(title_seed.chars().take(60).collect::<String>()),
                        P::I(util::now_unix()),
                    ],
                )
                .await?;
            Ok(id)
        }
    }
}

/// One assistant turn: fast path first, then the bounded model/tool loop.
/// Events are pushed to `tx` as (kind, data); the sender is dropped when
/// the turn finishes so receivers observe end-of-stream.
pub async fn run_turn(
    state: &Arc<AppState>,
    user: &AuthUser,
    cid: &str,
    message: &str,
    tx: &TurnTx,
) {
    // --- deterministic fast path ----------------------------------------
    match fastpath::parse(state, message).await {
        Ok(Some(fastpath::FastPath::LightCommand {
            entity_id,
            name,
            action,
        })) => {
            emit(
                tx,
                "tool",
                json!({"name": "control_light", "status": "executing",
                    "detail": format!("{action} {name}")}),
            );
            match execute_light(state, user, &entity_id, &action, None).await {
                Ok(v) => {
                    let confirmed = v
                        .get("confirmed")
                        .and_then(|c| c.as_bool())
                        .unwrap_or(false);
                    let text = if confirmed {
                        format!(
                            "Done — {} is now {}.",
                            name,
                            v.get("observed_state")
                                .and_then(|s| s.as_str())
                                .unwrap_or("changed")
                        )
                    } else {
                        format!(
                            "The command was sent to {} but I couldn't confirm the state changed. {}",
                            name,
                            v.get("detail").and_then(|d| d.as_str()).unwrap_or("")
                        )
                    };
                    emit(tx, "delta", json!({"text": text}));
                    emit(
                        tx,
                        "tool",
                        json!({"name": "control_light", "status": "done",
                            "detail": v.get("detail")}),
                    );
                    save_message(state, cid, "assistant", &text).await;
                }
                Err(e) => {
                    let text = format!("I couldn't do that: {e}");
                    emit(tx, "delta", json!({"text": text}));
                    save_message(state, cid, "assistant", &text).await;
                }
            }
            emit(tx, "done", json!({"conversation_id": cid}));
            return;
        }
        Ok(Some(fastpath::FastPath::CurrentTime)) => {
            let tz = state.timezone().await.unwrap_or(jiff::tz::TimeZone::UTC);
            let z = jiff::Timestamp::now().to_zoned(tz);
            let text = format!("It's {}.", z.strftime("%A %Y-%m-%d %H:%M %Z"));
            emit(tx, "delta", json!({"text": text}));
            save_message(state, cid, "assistant", &text).await;
            emit(tx, "done", json!({"conversation_id": cid}));
            return;
        }
        _ => {}
    }

    // --- model path ------------------------------------------------------
    let (provider, row) = match load_provider_checked(state).await {
        Ok(v) => v,
        Err(e) => {
            emit(tx, "error", json!({"message": e.to_string()}));
            emit(tx, "done", json!({"conversation_id": cid}));
            return;
        }
    };
    let messages = build_messages(state, user, cid).await;
    let ctx = ToolCtx { state, user };
    let mut history = messages;
    let mut final_text = String::new();
    for _round in 0..MAX_TOOL_ROUNDS {
        let req = ChatRequest {
            messages: history.clone(),
            tools: if provider.capabilities().tools {
                tools::specs()
            } else {
                vec![]
            },
        };
        let mut stream = provider.chat(req);
        let mut round_text = String::new();
        let mut calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut failed: Option<String> = None;
        while let Some(ev) = stream.next().await {
            match ev {
                ChatEvent::Text(t) => {
                    round_text.push_str(&t);
                    emit(tx, "delta", json!({"text": t}));
                }
                ChatEvent::ToolCall(c) => calls.push(c),
                ChatEvent::Done => break,
                ChatEvent::Failed(msg) => {
                    failed = Some(msg);
                    break;
                }
            }
        }
        if let Some(msg) = failed {
            emit(
                tx,
                "error",
                json!({"message": format!("provider error: {msg}")}),
            );
            emit(tx, "done", json!({"conversation_id": cid}));
            if !round_text.is_empty() {
                save_message(state, cid, "assistant", &round_text).await;
            }
            return;
        }
        if calls.is_empty() {
            final_text = round_text;
            break;
        }
        history.push(ChatMessage::assistant(round_text.clone(), calls.clone()));
        for call in calls {
            emit(
                tx,
                "tool",
                json!({
                    "name": call.name, "status": "executing",
                    "detail": call.arguments.to_string().chars().take(160).collect::<String>(),
                }),
            );
            let out = tools::dispatch(&ctx, &call).await;
            let ok = out
                .for_model
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            emit(
                tx,
                "tool",
                json!({
                    "name": call.name,
                    "status": if ok { "done" } else { "failed" },
                    "detail": out.for_model.to_string().chars().take(240).collect::<String>(),
                }),
            );
            if let Some(ui) = out.ui {
                emit(tx, "ui", ui);
            }
            history.push(ChatMessage::tool(
                call.id,
                out.for_model.to_string().chars().take(4000).collect(),
            ));
        }
    }
    if !final_text.is_empty() {
        save_message(state, cid, "assistant", &final_text).await;
    }
    emit(
        tx,
        "done",
        json!({"conversation_id": cid, "provider": row.label, "model": row.model}),
    );
}

pub async fn save_message(state: &Arc<AppState>, conv_id: &str, role: &str, content: &str) {
    let _ = state
        .db
        .exec(
            "INSERT INTO messages(conversation_id,role,content,created_at) VALUES(?1,?2,?3,?4)",
            vec![
                P::T(conv_id.to_string()),
                P::T(role.to_string()),
                P::T(content.chars().take(8000).collect::<String>()),
                P::I(util::now_unix()),
            ],
        )
        .await;
}

/// Provider selection honoring strict-local mode: cloud egress is blocked —
/// only loopback/private endpoints are permitted.
async fn load_provider_checked(
    state: &Arc<AppState>,
) -> Result<(Arc<dyn providers::ChatProvider>, providers::ProviderRow), ApiError> {
    let (provider, row) = providers::load_provider(&state.db, state.http.clone(), None).await?;
    let strict = state.get_setting("privacy_mode").await?.as_deref() == Some("strict_local");
    if strict && !providers::base_is_local(&row.base_url) {
        let host = reqwest::Url::parse(&row.base_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        return Err(ApiError::Forbidden(format!(
            "strict-local mode blocks cloud egress — provider '{}' ({}) is not a local endpoint",
            row.label, host
        )));
    }
    Ok((provider, row))
}

/// System prompt + recent history + relevant permitted memory context.
async fn build_messages(state: &Arc<AppState>, user: &AuthUser, conv_id: &str) -> Vec<ChatMessage> {
    let settings = state.get_settings().await.unwrap_or_default();
    let name = settings
        .get("assistant_name")
        .cloned()
        .unwrap_or_else(|| "Adrian".into());
    let tz = state.timezone().await.unwrap_or(jiff::tz::TimeZone::UTC);
    let now = jiff::Timestamp::now().to_zoned(tz.clone());
    let tz_name = settings
        .get("timezone")
        .cloned()
        .unwrap_or_else(|| "UTC".into());
    let privacy = settings
        .get("privacy_mode")
        .cloned()
        .unwrap_or_else(|| "hybrid".into());

    let facts = memory::list(&state.db, user.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .take(12)
        .map(|f| format!("- [{}] {}", f.scope, f.text))
        .collect::<Vec<_>>()
        .join("\n");
    let lights = state
        .db
        .q(
            "SELECT entity_id, name, area, state FROM entities WHERE allowed=1 AND domain='light'",
            vec![],
            |r| {
                Ok(format!(
                    "- {} (entity_id: {}, area: {}, state: {})",
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "?".into()),
                    r.get::<_, String>(3)?
                ))
            },
        )
        .await
        .unwrap_or_default()
        .join("\n");

    let system = format!(
        "You are {name}, a self-hosted household assistant for user '{user}' (role: {role}).\n\
         Current time: {now} ({tz}). Privacy mode: {privacy}.\n\
         Rules:\n\
         - Use tools for real actions (lights, alarms, memory). Never claim an action succeeded \
           unless the tool result confirms it; if unconfirmed, say so plainly.\n\
         - Only authorized lights can be controlled; if a device isn't listed, explain it must \
           be enabled under Devices.\n\
         - Scheduling an alarm requires a configured audio endpoint; report that limitation \
           honestly when the tool returns it.\n\
         - Saved facts visible to you:\n{facts}\n\
         - Authorized lights:\n{lights}\n\
         Keep replies concise and spoken-friendly.",
        name = name,
        user = user.username,
        role = user.role,
        now = now.strftime("%A %Y-%m-%d %H:%M:%S %Z"),
        tz = tz_name,
        privacy = privacy,
        facts = if facts.is_empty() {
            "(none)".into()
        } else {
            facts
        },
        lights = if lights.is_empty() {
            "(none authorized)".into()
        } else {
            lights
        },
    );

    let mut msgs = vec![ChatMessage::system(system)];
    // Recent history (best-effort; tool rounds are not replayed).
    if let Ok(rows) = state
        .db
        .q(
            "SELECT role, content FROM messages WHERE conversation_id = ?1
             ORDER BY id DESC LIMIT 20",
            vec![P::T(conv_id.to_string())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .await
    {
        for (role, content) in rows.into_iter().rev() {
            match role.as_str() {
                "user" => msgs.push(ChatMessage::user(content)),
                "assistant" => msgs.push(ChatMessage::assistant(content, vec![])),
                _ => {}
            }
        }
    }
    msgs
}
