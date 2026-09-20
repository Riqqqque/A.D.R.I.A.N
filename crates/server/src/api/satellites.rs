//! Native satellite pairing + voice utterance transport.
//!
//! Pairing: the web UI mints a short-lived one-use code; the satellite
//! redeems it for a durable device token (sha256-hashed at rest). Voice:
//! the satellite POSTs a WAV utterance; the server transcribes it through
//! the configured provider's OpenAI-compatible `/audio/transcriptions`
//! endpoint, runs the normal assistant turn, and returns the reply text.

use crate::api::chat;
use crate::auth::AuthUser;
use crate::db::P;
use crate::error::ApiError;
use crate::providers;
use crate::state::AppState;
use crate::util;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const PAIRING_TTL_SECS: i64 = 300; // 5 minutes
const MAX_WAV_BYTES: usize = 25 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    // The utterance route carries WAV bodies up to MAX_WAV_BYTES; the rest
    // stay under axum's default 2MB request-body limit.
    let voice = Router::new()
        .route("/api/satellite/utterance", post(utterance))
        .layer(DefaultBodyLimit::max(MAX_WAV_BYTES));
    Router::new()
        .route("/api/satellites", get(list_satellites))
        .route("/api/satellites/pairing-code", post(mint_code))
        .route("/api/satellites/{id}", delete(revoke_satellite))
        .route("/api/satellite/pair", post(pair))
        .route("/api/satellite/status", get(status))
        .merge(voice)
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
        let n = s
            .db
            .exec(
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
    Json(b): Json<PairBody>,
) -> Result<Json<Value>, ApiError> {
    // Brute-force guard: 6-digit codes are guessable — throttle attempts
    // per source IP like login and setup claim.
    if !s
        .rate_limit
        .check(&format!("pair:{}", addr.ip()), 10, Duration::from_secs(60))
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
    let row = s
        .db
        .q1(
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
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sat = satellite_from_headers(&s, &headers).await?;
    if body.len() < 100 || body.len() > MAX_WAV_BYTES {
        return Err(ApiError::BadRequest(format!(
            "wav body must be 100B-{}MB",
            MAX_WAV_BYTES / 1024 / 1024
        )));
    }
    let state = Arc::new(s);

    // Speech-to-text through the default provider's transcription endpoint.
    // Strict-local still applies: a cloud STT endpoint is egress.
    let strict = state.get_setting("privacy_mode").await?.as_deref() == Some("strict_local");
    if strict {
        let rows = providers::load_provider_rows(&state.db).await?;
        let base = rows
            .iter()
            .find(|r| r.enabled && r.is_default)
            .or_else(|| rows.iter().find(|r| r.enabled))
            .map(|r| r.base_url.clone())
            .unwrap_or_default();
        if !providers::base_is_local(&base) {
            return Err(ApiError::Forbidden(
                "strict-local mode blocks cloud egress — voice transcription needs a local STT endpoint".into(),
            ));
        }
    }
    let transcript =
        providers::transcribe_wav(&state.db, state.http.clone(), body.to_vec()).await?;

    let user_row = state
        .db
        .q1(
            "SELECT username, role FROM users WHERE id = ?1",
            vec![P::I(sat.user_id)],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .await?
        .ok_or_else(|| ApiError::internal_msg("satellite owner missing"))?;
    let user = AuthUser {
        id: sat.user_id,
        username: user_row.0,
        role: user_row.1,
        session_id: format!("satellite:{}", sat.id),
        csrf: String::new(),
    };

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
