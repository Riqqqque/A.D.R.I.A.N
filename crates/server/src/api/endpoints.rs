//! Audio playback endpoints: HA media players and opt-in browser audio.
//! A browser endpoint is a real output path — it receives play/stop commands
//! over an authenticated channel and must ack actual <audio> playback. It is
//! not a notification toast.

use crate::audio::AudioCommand;
use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::state::AppState;
use crate::util;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/endpoints", get(list).post(create))
        .route("/api/endpoints/{id}", delete(remove))
        .route("/api/endpoints/{id}/test", post(test))
        .route("/api/audio/register", post(register_browser))
        .route("/api/audio/channel/{id}", get(audio_channel))
        .route("/api/audio/ack", post(audio_ack))
}

async fn list(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows =
        s.db.q(
            "SELECT id,name,kind,config,enabled,created_at FROM endpoints ORDER BY created_at",
            vec![],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?,
                    "config": serde_json::from_str::<Value>(&r.get::<_, String>(3)?)
                        .unwrap_or(json!({})),
                    "enabled": r.get::<_, i64>(4)? != 0,
                    "created_at": r.get::<_, i64>(5)?,
                }))
            },
        )
        .await?;
    let _ = user;
    Ok(Json(json!({"endpoints": rows})))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    kind: String,
    entity_id: Option<String>,
    satellite_id: Option<String>,
}

async fn create(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    if b.name.trim().is_empty() || b.name.len() > 80 {
        return Err(ApiError::BadRequest("name must be 1-80 chars".into()));
    }
    let (kind, config) = match b.kind.as_str() {
        "ha_media_player" => {
            let entity = b.entity_id.as_deref().unwrap_or("").trim().to_string();
            if !entity.starts_with("media_player.") {
                return Err(ApiError::BadRequest(
                    "entity_id must be a media_player.* entity".into(),
                ));
            }
            ("ha_media_player", json!({"entity_id": entity}))
        }
        "browser" => {
            return Err(ApiError::BadRequest(
                "browser endpoints are created from the browser itself — use 'Enable alarm audio on this device'"
                    .into(),
            ));
        }
        "satellite" => {
            let sat_id = b.satellite_id.as_deref().unwrap_or("").trim().to_string();
            let exists =
                s.db.q1(
                    "SELECT 1 FROM satellites WHERE id = ?1 AND user_id = ?2 AND revoked = 0",
                    vec![P::T(sat_id.clone()), P::I(user.id)],
                    |r| r.get::<_, i64>(0),
                )
                .await?
                .is_some();
            if !exists {
                return Err(ApiError::BadRequest(
                    "satellite_id must be a paired, unrevoked satellite of yours".into(),
                ));
            }
            ("satellite", json!({"satellite_id": sat_id}))
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "endpoint kind '{other}' unsupported"
            )));
        }
    };
    let id = util::new_id();
    s.db.exec(
        "INSERT INTO endpoints(id,name,kind,config,enabled,created_at) VALUES(?1,?2,?3,?4,1,?5)",
        vec![
            P::T(id.clone()),
            P::T(b.name.trim().to_string()),
            P::T(kind.to_string()),
            P::T(config.to_string()),
            P::I(util::now_unix()),
        ],
    )
    .await?;
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn remove(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require_owner()?;
    s.db.exec(
        "DELETE FROM endpoints WHERE id = ?1",
        vec![P::T(id.clone())],
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

/// Send the builtin tone to an endpoint and report the outcome honestly.
async fn test(
    _user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let row =
        s.db.q1(
            "SELECT name, kind, config FROM endpoints WHERE id = ?1 AND enabled = 1",
            vec![P::T(id.clone())],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .await?
        .ok_or_else(|| ApiError::NotFound("endpoint".into()))?;
    let (name, kind, cfg_raw) = row;
    let cfg: Value = serde_json::from_str(&cfg_raw).unwrap_or(json!({}));
    let token = s.audio.grant_media("builtin:tone");
    match kind.as_str() {
        "ha_media_player" => {
            let entity = cfg.get("entity_id").and_then(|e| e.as_str()).unwrap_or("");
            let base = s
                .get_setting("public_url")
                .await?
                .unwrap_or_else(|| format!("http://{}", s.config.bind));
            let url = format!("{}/media/{token}", base.trim_end_matches('/'));
            match s.ha.play_media(entity, &url, Some(0.7)).await {
                Ok(()) => Ok(Json(json!({
                    "ok": true,
                    "detail": format!("playback requested on {entity} — HA accepted; confirm you heard it"),
                }))),
                Err(e) => Ok(Json(json!({"ok": false, "detail": e.to_string()}))),
            }
        }
        "satellite" => {
            let sat_id = cfg
                .get("satellite_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let delivery = util::new_id();
            let mut env = adrian_protocol::Envelope::new(adrian_protocol::Message::Speak {
                media_url: format!("/media/{token}"),
                text: Some(format!("Test — {name}")),
            });
            env.request_id = delivery.clone();
            // Arm before sending so a fast ack cannot be missed.
            let ack = s.audio.arm_ack(&delivery);
            match s.satellites.send(&sat_id, &env) {
                Ok(()) => match s.audio.await_ack(ack, Duration::from_secs(20)).await {
                    Ok((true, d)) => Ok(Json(json!({"ok": true, "detail": d}))),
                    Ok((false, d)) => Ok(Json(json!({
                        "ok": false,
                        "detail": format!("satellite reported failure: {d}"),
                    }))),
                    Err(e) => Ok(Json(json!({
                        "ok": false,
                        "detail": format!("no playback ack from the satellite: {e}"),
                    }))),
                },
                Err(e) => Ok(Json(json!({
                    "ok": false,
                    "detail": format!("satellite is not connected: {e}"),
                }))),
            }
        }
        "browser" => {
            let channel = cfg.get("channel_id").and_then(|c| c.as_str()).unwrap_or("");
            let delivery = util::new_id();
            let cmd = AudioCommand::Play {
                delivery_id: delivery.clone(),
                url: format!("/media/{token}"),
                volume: 0.7,
                label: format!("Test — {name}"),
            };
            s.audio.send_to_channel(channel, &cmd)?;
            match s.audio.wait_ack(&delivery, Duration::from_secs(20)).await {
                Ok((_, result)) => Ok(Json(json!({"ok": true, "detail": result}))),
                Err(e) => Ok(Json(json!({
                    "ok": false,
                    "detail": format!("no playback ack from the browser: {e}"),
                }))),
            }
        }
        other => Ok(Json(json!({
            "ok": false,
            "detail": format!("endpoint kind '{other}' is not testable"),
        }))),
    }
}

// ---- browser audio channel ----------------------------------------------

#[derive(Deserialize)]
struct RegisterBody {
    name: String,
}

/// A logged-in browser opts in as an audio endpoint. Creates a durable
/// endpoint row + a live channel id; the tab then opens the SSE channel and
/// unlocks <audio> playback via a user gesture.
async fn register_browser(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<RegisterBody>,
) -> Result<Json<Value>, ApiError> {
    let name = if b.name.trim().is_empty() {
        format!("Browser audio ({})", user.username)
    } else {
        b.name.trim().to_string()
    };
    if name.len() > 80 {
        return Err(ApiError::BadRequest("name too long".into()));
    }
    // Channel id is stored in config; the SSE attach creates it lazily.
    let channel_id = util::new_id();
    let id = util::new_id();
    s.db.exec(
        "INSERT INTO endpoints(id,name,kind,config,enabled,created_at)
         VALUES(?1,?2,'browser',?3,1,?4)",
        vec![
            P::T(id.clone()),
            P::T(name),
            P::T(json!({"channel_id": channel_id}).to_string()),
            P::I(util::now_unix()),
        ],
    )
    .await?;
    Ok(Json(
        json!({"ok": true, "endpoint_id": id, "channel_id": channel_id}),
    ))
}

/// SSE stream of play/stop commands for one browser channel. Cookie auth
/// (GET). The channel id from registration reconnects to the same sender.
async fn audio_channel(
    user: AuthUser,
    State(s): State<AppState>,
    Path(channel_id): Path<String>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let _ = user;
    let rx = s.audio.attach_channel(&channel_id);
    let stream = async_stream::stream! {
        let mut rx = rx;
        yield Ok(Event::default().event("hello").data("{}"));
        loop {
            match rx.recv().await {
                Ok(cmd) => {
                    let data = serde_json::to_string(&cmd).unwrap_or_default();
                    yield Ok(Event::default().event("cmd").data(data));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
}

#[derive(Deserialize)]
struct AckBody {
    delivery_id: String,
    result: String,
}

async fn audio_ack(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<AckBody>,
) -> Result<Json<Value>, ApiError> {
    let _ = user;
    if b.delivery_id.len() > 80 || b.result.len() > 300 {
        return Err(ApiError::BadRequest("ack fields too long".into()));
    }
    s.audio.resolve_ack(&b.delivery_id, b.result);
    Ok(Json(json!({"ok": true})))
}
