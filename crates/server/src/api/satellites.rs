//! Native satellite pairing + voice utterance transport.
//!
//! Pairing: the web UI mints a short-lived one-use code; the satellite
//! redeems it for a durable device token (sha256-hashed at rest). Voice:
//! the satellite POSTs a WAV utterance; the server transcribes it through
//! the configured provider's OpenAI-compatible `/audio/transcriptions`
//! endpoint, runs the normal assistant turn, and returns the reply text.

use crate::api::chat;
use crate::auth::{AuthUser, client_ip};
use crate::db::P;
use crate::error::ApiError;
use crate::providers;
use crate::state::AppState;
use crate::util;
use adrian_protocol as proto;
use axum::body::Body;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PAIRING_TTL_SECS: i64 = 300; // 5 minutes
const MAX_WAV_BYTES: usize = 25 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/satellites", get(list_satellites))
        .route("/api/satellites/pairing-code", post(mint_code))
        .route("/api/satellites/{id}", delete(revoke_satellite))
        .route("/api/satellite/pair", post(pair))
        .route("/api/satellite/status", get(status))
        .route("/api/satellite/session", get(session))
        .route("/api/satellite/utterance", post(utterance))
}

// ---------- session-authed (web UI) ----------

async fn list_satellites(
    user: AuthUser,
    State(s): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let rows =
        s.db.q(
            "SELECT id,name,paired_at,last_seen,revoked FROM satellites
             WHERE user_id = ?1 ORDER BY paired_at DESC LIMIT 100",
            vec![P::I(user.id)],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "paired_at": r.get::<_, i64>(2)?,
                    "last_seen": r.get::<_, Option<i64>>(3)?,
                    "revoked": r.get::<_, i64>(4)? != 0,
                }))
            },
        )
        .await?;
    Ok(Json(json!({"satellites": rows})))
}

async fn mint_code(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    if !user.is_owner() {
        return Err(ApiError::Forbidden(
            "only the owner can pair satellites".into(),
        ));
    }
    // Human-typeable code: 6 digits, 5-minute expiry, one use.
    // INSERT OR IGNORE + retry — never overwrite someone else's live code.
    let now = util::now_unix();
    let mut code = 0u32;
    for _ in 0..8 {
        let candidate: u32 = rand::random::<u32>() % 900_000 + 100_000;
        let n =
            s.db.exec(
                "INSERT OR IGNORE INTO pairing_codes(code,user_id,created_at,expires_at)
                 VALUES(?1,?2,?3,?4)",
                vec![
                    P::T(candidate.to_string()),
                    P::I(user.id),
                    P::I(now),
                    P::I(now + PAIRING_TTL_SECS),
                ],
            )
            .await?;
        if n == 1 {
            code = candidate;
            break;
        }
    }
    if code == 0 {
        return Err(ApiError::internal_msg("could not allocate a pairing code"));
    }
    // Best-effort sweep of expired codes.
    let _ =
        s.db.exec(
            "DELETE FROM pairing_codes WHERE expires_at < ?1 OR used = 1",
            vec![P::I(now)],
        )
        .await;
    s.audit(
        Some(user.id),
        "user",
        "satellite.pairing_code",
        "pairing_codes",
        &json!({}),
        "ok",
        "minted a satellite pairing code",
    )
    .await;
    Ok(Json(
        json!({"code": code.to_string(), "expires_at": now + PAIRING_TTL_SECS}),
    ))
}

async fn revoke_satellite(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_owner() {
        return Err(ApiError::Forbidden(
            "only the owner can revoke satellites".into(),
        ));
    }
    let n =
        s.db.exec(
            "UPDATE satellites SET revoked = 1 WHERE id = ?1 AND user_id = ?2",
            vec![P::T(id.clone()), P::I(user.id)],
        )
        .await?;
    if n == 0 {
        return Err(ApiError::NotFound("satellite".into()));
    }
    s.satellites.disconnect(&id);
    // Left enabled, it would stay a default alarm target nothing can answer.
    let retired =
        s.db.exec(
            "UPDATE endpoints SET enabled = 0
             WHERE kind = 'satellite' AND enabled = 1
               AND json_extract(config, '$.satellite_id') = ?1",
            vec![P::T(id.clone())],
        )
        .await?;
    if retired > 0 {
        s.emit("endpoint", json!({"event": "disabled", "satellite_id": id}));
    }
    s.audit(
        Some(user.id),
        "user",
        "satellite.revoke",
        &id,
        &json!({}),
        "ok",
        "revoked a satellite device token",
    )
    .await;
    Ok(Json(json!({"revoked": true})))
}

// ---------- device-token authed (satellite) ----------

#[derive(Clone)]
struct Satellite {
    id: String,
    name: String,
    user_id: i64,
}

fn token_hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

async fn satellite_from_headers(s: &AppState, headers: &HeaderMap) -> Result<Satellite, ApiError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Unauthorized)?;
    let row =
        s.db.q1(
            "SELECT s.id, s.name, s.user_id FROM satellites s
             JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.revoked = 0 AND u.disabled_at IS NULL",
            vec![P::T(token_hash(token))],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .await?;
    let Some((id, name, user_id)) = row else {
        return Err(ApiError::Unauthorized);
    };
    let _ =
        s.db.exec(
            "UPDATE satellites SET last_seen = ?1 WHERE id = ?2",
            vec![P::I(util::now_unix()), P::T(id.clone())],
        )
        .await;
    Ok(Satellite { id, name, user_id })
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    name: String,
}

async fn pair(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(b): Json<PairBody>,
) -> Result<Json<Value>, ApiError> {
    // Brute-force guard: 6-digit codes are guessable — throttle attempts
    // per source IP like login and setup claim.
    let ip = client_ip(&headers, addr, &s.config);
    if !s
        .rate_limit
        .check(&format!("pair:{ip}"), 10, Duration::from_secs(60))
    {
        return Err(ApiError::RateLimited);
    }
    let code = b.code.trim();
    let name = b.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(ApiError::BadRequest("name must be 1-80 chars".into()));
    }
    let now = util::now_unix();
    // Burn the code atomically — one use, even under concurrent redemptions.
    let row =
        s.db.q1(
            "UPDATE pairing_codes SET used = 1
             WHERE code = ?1 AND used = 0 AND expires_at > ?2
             RETURNING user_id",
            vec![P::T(code.to_string()), P::I(now)],
            |r| r.get::<_, i64>(0),
        )
        .await?;
    let Some(user_id) = row else {
        return Err(ApiError::Forbidden(
            "invalid or expired pairing code".into(),
        ));
    };

    let id = util::new_id();
    let token = format!("sat_{}", util::new_id());
    s.db.exec(
        "INSERT INTO satellites(id,user_id,name,token_hash,paired_at)
         VALUES(?1,?2,?3,?4,?5)",
        vec![
            P::T(id.clone()),
            P::I(user_id),
            P::T(name.to_string()),
            P::T(token_hash(&token)),
            P::I(now),
        ],
    )
    .await?;
    s.audit(
        Some(user_id),
        "satellite",
        "satellite.pair",
        &id,
        &json!({"name": name}),
        "ok",
        "paired a new satellite",
    )
    .await;
    Ok(Json(json!({
        "satellite_id": id,
        "device_token": token,
        "name": name,
    })))
}

async fn status(State(s): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let sat = satellite_from_headers(&s, &headers).await?;
    Ok(Json(json!({
        "ok": true,
        "satellite_id": sat.id,
        "name": sat.name,
    })))
}

#[derive(Deserialize)]
struct UtteranceQuery {
    conversation_id: Option<String>,
}

async fn utterance(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UtteranceQuery>,
    body: Body,
) -> Result<impl IntoResponse, ApiError> {
    // The token comes before the body: only a paired device gets to make the
    // server buffer MAX_WAV_BYTES.
    let sat = satellite_from_headers(&s, &headers).await?;
    let bad_size = || {
        ApiError::BadRequest(format!(
            "wav body must be 100B-{}MB",
            MAX_WAV_BYTES / 1024 / 1024
        ))
    };
    let body = axum::body::to_bytes(body, MAX_WAV_BYTES)
        .await
        .map_err(|_| bad_size())?;
    if body.len() < 100 {
        return Err(bad_size());
    }
    let state = Arc::new(s);

    // Uploads and STT both take time; the device may be revoked meanwhile.
    sat_auth_user(&state, &sat).await?;
    ensure_stt_allowed(&state).await?;
    let transcript =
        providers::transcribe_wav(&state, Some(sat.user_id), "satellite", body.to_vec()).await?;
    let user = sat_auth_user(&state, &sat).await?;

    let conv_id =
        chat::ensure_conversation(&state, &user, q.conversation_id.as_deref(), &transcript).await?;
    chat::save_message(&state, &conv_id, "user", &transcript).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let st = state.clone();
    let us = user.clone();
    let cid = conv_id.clone();
    let msg = transcript.clone();
    tokio::spawn(async move {
        chat::run_turn(&st, &us, &cid, &msg, &tx).await;
    });

    let mut response = String::new();
    let mut tools = Vec::new();
    let mut error = String::new();
    while let Some((kind, data)) = rx.recv().await {
        match kind.as_str() {
            "delta" => response.push_str(data.get("text").and_then(|t| t.as_str()).unwrap_or("")),
            "tool" => tools.push(data),
            "error" => {
                error = data
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string()
            }
            _ => {}
        }
    }

    state
        .audit(
            Some(sat.user_id),
            "satellite",
            "satellite.utterance",
            &sat.id,
            &json!({"transcript_len": transcript.chars().count()}),
            if error.is_empty() { "ok" } else { "failed" },
            &error,
        )
        .await;

    Ok((
        StatusCode::OK,
        Json(json!({
            "conversation_id": conv_id,
            "transcript": transcript,
            "response": response,
            "tools": tools,
            "error": if error.is_empty() { Value::Null } else { json!(error) },
        })),
    ))
}

/// Speech-to-text egress policy: strict-local mode forbids cloud STT.
async fn ensure_stt_allowed(state: &AppState) -> Result<(), ApiError> {
    let strict = state.get_setting("privacy_mode").await?.as_deref() == Some("strict_local");
    if !strict {
        return Ok(());
    }
    let rows = providers::load_provider_rows(&state.db).await?;
    let row = rows
        .iter()
        .find(|r| r.enabled && r.is_default)
        .or_else(|| rows.iter().find(|r| r.enabled));
    // No provider means no egress; transcribe_wav reports what is missing.
    let Some(row) = row else {
        return Ok(());
    };
    if !providers::base_is_local(&row.base_url) {
        return Err(ApiError::Forbidden(
            "strict-local mode blocks cloud egress — voice transcription needs a local STT endpoint"
                .into(),
        ));
    }
    Ok(())
}

/// The satellite's owner as an `AuthUser` for the assistant pipeline.
/// Unauthorized once the satellite is revoked or its owner disabled, so a
/// `Satellite` authenticated earlier cannot keep acting.
async fn sat_auth_user(state: &AppState, sat: &Satellite) -> Result<AuthUser, ApiError> {
    let (username, role) = state
        .db
        .q1(
            "SELECT u.username, u.role FROM satellites s
             JOIN users u ON u.id = s.user_id
             WHERE s.id = ?1 AND s.revoked = 0 AND u.disabled_at IS NULL",
            vec![P::T(sat.id.clone())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .await?
        .ok_or(ApiError::Unauthorized)?;
    Ok(AuthUser {
        id: sat.user_id,
        username,
        role,
        session_id: format!("satellite:{}", sat.id),
        csrf: String::new(),
    })
}

// ---------- websocket session (persistent bidirectional channel) ----------

/// Outbound sink for one connected satellite.
type SatTx = tokio::sync::mpsc::UnboundedSender<WsMessage>;

/// Connected satellites, keyed by satellite id — lets the scheduler and
/// other services push (alarm playback, future notify/wake) to a live
/// device. A second connection for the same satellite replaces the first,
/// which is told to end; each registration carries a conn id so a dying
/// session cannot unregister a newer one.
#[derive(Default)]
pub struct SatelliteHub {
    sessions: Mutex<HashMap<String, (u64, SatTx, CancellationToken)>>,
    next_conn: AtomicU64,
}

impl SatelliteHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, satellite_id: &str, tx: SatTx) -> (u64, CancellationToken) {
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
        let end = CancellationToken::new();
        let replaced = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(satellite_id.to_string(), (conn, tx, end.clone()));
        // Only the newest socket is tracked, so a later revoke could not
        // reach this one.
        if let Some((_, _, old)) = replaced {
            old.cancel();
        }
        (conn, end)
    }

    fn unregister(&self, satellite_id: &str, conn: u64) {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(satellite_id).is_some_and(|(c, _, _)| *c == conn) {
            map.remove(satellite_id);
        }
    }

    /// End a satellite's live session. Its read loop stops at once, so
    /// nothing the device still sends is handled, whether or not it would
    /// have honored a Close frame.
    pub fn disconnect(&self, satellite_id: &str) {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(satellite_id);
        if let Some((_, _, end)) = session {
            end.cancel();
        }
    }

    /// Ids of all connected satellites.
    pub fn connected_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// True while a session for this satellite is connected.
    pub fn connected(&self, satellite_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(satellite_id)
    }

    /// Push a protocol envelope to a connected satellite.
    pub fn send(&self, satellite_id: &str, env: &proto::Envelope) -> Result<(), ApiError> {
        let tx = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(satellite_id)
            .map(|(_, tx, _)| tx.clone());
        let Some(tx) = tx else {
            return Err(ApiError::NotConfigured(format!(
                "satellite '{satellite_id}' is not connected"
            )));
        };
        let text = serde_json::to_string(env).map_err(ApiError::internal)?;
        tx.send(WsMessage::Text(text.into()))
            .map_err(|_| ApiError::NotConfigured("satellite channel closed".into()))
    }
}

fn send_env(tx: &SatTx, env: proto::Envelope) {
    if let Ok(text) = serde_json::to_string(&env) {
        let _ = tx.send(WsMessage::Text(text.into()));
    }
}

fn send_ack(tx: &SatTx, request_id: &str, ok: bool, detail: impl Into<String>) {
    let mut env = proto::Envelope::new(proto::Message::Ack {
        ok,
        detail: Some(detail.into()),
    });
    env.request_id = request_id.to_string();
    send_env(tx, env);
}

/// PCM s16le mono → RIFF/WAV container (44-byte canonical header).
fn pcm_s16le_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk len
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

struct Capture {
    sample_rate: u32,
    pcm: Vec<u8>,
}

/// `GET /api/satellite/session` — persistent satellite channel. JSON text
/// frames carry `Envelope`s; binary frames carry PCM s16le mono while an
/// `AudioSessionStart` capture is active.
async fn session(
    State(s): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let sat = satellite_from_headers(&s, &headers).await?;
    Ok(ws
        .max_message_size(proto::MAX_FRAME_BYTES)
        .on_upgrade(move |sock| session_loop(s, sat, sock)))
}

/// Register a session, unless the satellite lost its rights after the
/// upgrade request was checked: a revoke in that gap found nothing to
/// disconnect. One that lands after this check finds the registration.
async fn admit(s: &AppState, sat: &Satellite, tx: SatTx) -> Option<(u64, CancellationToken)> {
    let (conn, end) = s.satellites.register(&sat.id, tx);
    if sat_auth_user(s, sat).await.is_err() {
        s.satellites.unregister(&sat.id, conn);
        return None;
    }
    Some((conn, end))
}

async fn session_loop(s: AppState, sat: Satellite, sock: WebSocket) {
    let (mut sink, mut stream) = sock.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();
    let Some((conn, end)) = admit(&s, &sat, tx.clone()).await else {
        return;
    };
    let _ =
        s.db.exec(
            "UPDATE satellites SET last_seen = ?1 WHERE id = ?2",
            vec![P::I(util::now_unix()), P::T(sat.id.clone())],
        )
        .await;
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(s);
    crate::scheduler::replicas::sync(&state, &sat.id).await;
    let conv_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut capture: Option<Capture> = None;
    let mut seen_keys: HashSet<String> = HashSet::new();

    loop {
        let msg = tokio::select! {
            biased;
            _ = end.cancelled() => break,
            msg = stream.next() => msg,
        };
        let Some(Ok(msg)) = msg else { break };
        match msg {
            WsMessage::Binary(b) => {
                if let Some(c) = &mut capture {
                    let cap = c.sample_rate as usize * 2 * proto::MAX_AUDIO_SECONDS as usize;
                    let take = cap.saturating_sub(c.pcm.len()).min(b.len());
                    c.pcm.extend_from_slice(&b[..take]);
                }
            }
            WsMessage::Text(t) => {
                let env: proto::Envelope = match serde_json::from_str(&t) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if env.protocol_version != proto::PROTOCOL_VERSION {
                    send_ack(&tx, &env.request_id, false, "unsupported protocol version");
                    continue;
                }
                // Past-deadline work is dropped rather than executed late.
                if let Some(d) = env.deadline_unix
                    && util::now_unix() > d
                {
                    send_ack(&tx, &env.request_id, false, "deadline expired");
                    continue;
                }
                if let Some(k) = &env.idempotency_key {
                    if seen_keys.len() > 1024 {
                        seen_keys.clear();
                    }
                    if !seen_keys.insert(k.clone()) {
                        send_ack(&tx, &env.request_id, true, "duplicate ignored");
                        continue;
                    }
                }
                match env.message {
                    proto::Message::AudioSessionStart { sample_rate, .. } => {
                        capture = Some(Capture {
                            sample_rate: sample_rate.max(1),
                            pcm: Vec::new(),
                        });
                        send_ack(&tx, &env.request_id, true, "capturing");
                    }
                    proto::Message::AudioSessionEnd { .. } => {
                        let Some(c) = capture.take() else {
                            send_ack(&tx, &env.request_id, false, "no active session");
                            continue;
                        };
                        send_ack(&tx, &env.request_id, true, "processing");
                        let st = state.clone();
                        let sat_c = sat.clone();
                        let tx_c = tx.clone();
                        let conv = conv_id.clone();
                        tokio::spawn(async move {
                            finish_utterance(&st, &sat_c, c, conv, &tx_c).await;
                        });
                    }
                    proto::Message::Status { state: st, detail } => {
                        tracing::debug!(sat = %sat.id, ?st, ?detail, "satellite status");
                        let _ = state
                            .db
                            .exec(
                                "UPDATE satellites SET last_seen = ?1 WHERE id = ?2",
                                vec![P::I(util::now_unix()), P::T(sat.id.clone())],
                            )
                            .await;
                        send_ack(&tx, &env.request_id, true, "ok");
                    }
                    proto::Message::Ack { ok, detail } => {
                        // Alarm playback confirmations correlate on
                        // request_id = delivery_id (see scheduler deliver).
                        state.audio.resolve_ack_status(
                            &env.request_id,
                            ok,
                            detail.unwrap_or_else(|| {
                                if ok {
                                    "ok".into()
                                } else {
                                    "playback failed".into()
                                }
                            }),
                        );
                    }
                    proto::Message::AlarmLocalReport { fired } => {
                        match crate::scheduler::replicas::record_local_firings(
                            &state, &sat.id, &sat.name, &fired,
                        )
                        .await
                        {
                            Ok(n) => {
                                // The satellite clears its pending report on
                                // this ack; recorded firings (and ones the
                                // server rejected) are both settled.
                                send_ack(&tx, &env.request_id, true, format!("{n} recorded"));
                            }
                            Err(e) => send_ack(&tx, &env.request_id, false, e.to_string()),
                        }
                    }
                    proto::Message::PairRequest { .. } => {
                        let mut e = proto::Envelope::new(proto::Message::PairRejected {
                            reason: "pairing uses POST /api/satellite/pair".into(),
                        });
                        e.request_id = env.request_id;
                        send_env(&tx, e);
                    }
                    _ => send_ack(&tx, &env.request_id, false, "unsupported message"),
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    state.satellites.unregister(&sat.id, conn);
    writer.abort();
}

/// What a satellite says for a finished turn. Whatever she got to say stands,
/// because it may report actions that really happened.
fn spoken_reply(reply: String, error: &str) -> String {
    if error.is_empty() {
        reply
    } else if reply.trim().is_empty() {
        format!("error: {error}")
    } else {
        format!("{reply} Then something went wrong.")
    }
}

/// Buffered PCM → WAV → STT → assistant turn → `Speak` reply, pushed back
/// over the session channel.
async fn finish_utterance(
    state: &Arc<AppState>,
    sat: &Satellite,
    cap: Capture,
    conv: Arc<Mutex<Option<String>>>,
    tx: &SatTx,
) {
    // Checked here and again after STT: a revoked device spends nothing, and
    // one revoked while STT runs gets no turn.
    if let Err(e) = sat_auth_user(state, sat).await {
        send_ack(tx, "turn", false, e.to_string());
        return;
    }
    if let Err(e) = ensure_stt_allowed(state).await {
        send_ack(tx, "stt", false, e.to_string());
        return;
    }
    let wav = pcm_s16le_to_wav(&cap.pcm, cap.sample_rate);
    let transcript =
        match providers::transcribe_wav(state, Some(sat.user_id), "satellite", wav).await {
            Ok(t) => t,
            Err(e) => {
                send_ack(tx, "stt", false, format!("stt failed: {e}"));
                return;
            }
        };
    {
        let mut env = proto::Envelope::new(proto::Message::Transcript {
            text: transcript.clone(),
            is_final: true,
        });
        env.device_id = Some(sat.id.clone());
        send_env(tx, env);
    }
    let user = match sat_auth_user(state, sat).await {
        Ok(u) => u,
        Err(e) => {
            send_ack(tx, "turn", false, e.to_string());
            return;
        }
    };
    let existing = conv.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let cid = match chat::ensure_conversation(state, &user, existing.as_deref(), &transcript).await
    {
        Ok(c) => c,
        Err(e) => {
            send_ack(tx, "turn", false, e.to_string());
            return;
        }
    };
    *conv.lock().unwrap_or_else(|e| e.into_inner()) = Some(cid.clone());
    chat::save_message(state, &cid, "user", &transcript).await;

    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let st = state.clone();
    let cid_c = cid.clone();
    let msg = transcript.clone();
    tokio::spawn(async move {
        chat::run_turn(&st, &user, &cid_c, &msg, &etx).await;
    });
    let mut reply = String::new();
    let mut error = String::new();
    while let Some((kind, data)) = erx.recv().await {
        match kind.as_str() {
            "delta" => reply.push_str(data.get("text").and_then(|t| t.as_str()).unwrap_or("")),
            "error" => {
                error = data
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
            }
            _ => {}
        }
    }
    let mut env = proto::Envelope::new(proto::Message::Speak {
        media_url: String::new(),
        text: Some(spoken_reply(reply, &error)),
        alarm: None,
    });
    env.device_id = Some(sat.id.clone());
    send_env(tx, env);
    state
        .audit(
            Some(sat.user_id),
            "satellite",
            "satellite.utterance",
            &sat.id,
            &json!({"transcript_len": transcript.chars().count(), "channel": "ws"}),
            if error.is_empty() { "ok" } else { "failed" },
            &error,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state_with_satellite(revoked: i64) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().to_path_buf(),
            tls: None,
            trusted_proxy: None,
        };
        let state = crate::build_state(config).await.unwrap();
        state
            .db
            .exec(
                "INSERT INTO users(id,username,display_name,password_hash,role,created_at)
                 VALUES(1,'owner','Owner','x','owner',1)",
                vec![],
            )
            .await
            .unwrap();
        state
            .db
            .exec(
                "INSERT INTO satellites(id,user_id,name,token_hash,paired_at,revoked)
                 VALUES('sat1',1,'PC','hash',1,?1)",
                vec![P::I(revoked)],
            )
            .await
            .unwrap();
        (state, dir)
    }

    fn sat() -> Satellite {
        Satellite {
            id: "sat1".into(),
            name: "PC".into(),
            user_id: 1,
        }
    }

    #[test]
    fn a_failed_turn_keeps_what_she_already_said() {
        assert_eq!(spoken_reply("Done.".into(), ""), "Done.");
        assert_eq!(
            spoken_reply(" ".into(), "no provider"),
            "error: no provider"
        );
        let said = spoken_reply("Timer set.".into(), "no provider");
        assert!(said.starts_with("Timer set."), "{said}");
    }

    #[tokio::test]
    async fn a_session_revoked_during_its_upgrade_is_not_admitted() {
        let (state, _dir) = state_with_satellite(1).await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(admit(&state, &sat(), tx).await.is_none());
        assert!(!state.satellites.connected("sat1"));
    }

    #[tokio::test]
    async fn a_live_satellite_is_admitted() {
        let (state, _dir) = state_with_satellite(0).await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(admit(&state, &sat(), tx).await.is_some());
        assert!(state.satellites.connected("sat1"));
    }
}
