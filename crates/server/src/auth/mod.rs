//! Authentication: one-use local setup token, Argon2id passwords, DB-backed
//! sessions in HttpOnly SameSite=Lax cookies, and a per-session CSRF token
//! required on every mutating request.

pub mod password;
pub mod ratelimit;

use crate::AppState;
use crate::db::P;
use crate::error::ApiError;
use crate::util;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;

pub const SESSION_COOKIE: &str = "adrian_session";
pub const CSRF_HEADER: &str = "x-adrian-csrf";
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub session_id: String,
    pub csrf: String,
}

impl AuthUser {
    pub fn is_owner(&self) -> bool {
        self.role == "owner"
    }
    pub fn require_owner(&self) -> Result<(), ApiError> {
        if self.is_owner() {
            Ok(())
        } else {
            Err(ApiError::Forbidden("owner role required".into()))
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let sid = cookie_value(&parts.headers, SESSION_COOKIE).ok_or(ApiError::Unauthorized)?;
        let row = state
            .db
            .q1(
                "SELECT s.user_id, s.csrf, s.expires_at, s.revoked_at,
                        u.username, u.role, u.disabled_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.id = ?1",
                vec![P::T(sid.clone())],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .await?
            .ok_or(ApiError::Unauthorized)?;
        let (user_id, csrf, expires, revoked, username, role, disabled) = row;
        if revoked.is_some() || disabled.is_some() || expires <= util::now_unix() {
            return Err(ApiError::Unauthorized);
        }
        // CSRF: mutating API calls must present the per-session token.
        if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let presented = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if presented != csrf {
                return Err(ApiError::Forbidden("missing or invalid csrf token".into()));
            }
        }
        Ok(Self {
            id: user_id,
            username,
            role,
            session_id: sid,
            csrf,
        })
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn session_cookie(value: &str, max_age_secs: i64) -> String {
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}")
}

/// First-boot: mint the one-use setup token if the system is unclaimed.
/// Shown on the local console and written next to the database; never served
/// over HTTP.
pub async fn bootstrap(db: &crate::db::Db, data_dir: &std::path::Path) -> Result<(), ApiError> {
    let users: i64 = db
        .q1("SELECT COUNT(*) FROM users", vec![], |r| r.get(0))
        .await?
        .unwrap_or(0);
    let pending = db
        .q1(
            "SELECT used_at FROM setup_state WHERE id = 1",
            vec![],
            |r| r.get::<_, Option<i64>>(0),
        )
        .await?;
    match (users, pending) {
        (0, None) => {
            let token = util::new_token();
            db.exec(
                "INSERT INTO setup_state(id, token_hash, used_at) VALUES(1,?1,NULL)",
                vec![P::T(util::sha256_hex(&token))],
            )
            .await?;
            let msg = format!(
                "\n=========================================================\n\
                 Adrian first-run setup token (one use, keep private):\n\n    {token}\n\n\
                 Enter it on the setup page. Also saved to {}\n\
                 =========================================================",
                data_dir.join("SETUP_TOKEN.txt").display()
            );
            eprintln!("{msg}");
            tracing::info!("wrote one-use setup token to {}", data_dir.display());
            std::fs::write(data_dir.join("SETUP_TOKEN.txt"), &token).ok();
        }
        (0, Some(Some(_))) => {
            // Token consumed but no user exists — inconsistent; remint.
            let token = util::new_token();
            db.exec(
                "UPDATE setup_state SET token_hash = ?1, used_at = NULL WHERE id = 1",
                vec![P::T(util::sha256_hex(&token))],
            )
            .await?;
            eprintln!("Adrian setup token reminted: {token}");
            std::fs::write(data_dir.join("SETUP_TOKEN.txt"), &token).ok();
        }
        _ => {}
    }
    Ok(())
}

/// Delete sessions that are expired or revoked — keeps the table bounded.
pub async fn sweep_expired_sessions(db: &crate::db::Db) {
    let now = util::now_unix();
    let _ = db
        .exec(
            "DELETE FROM sessions WHERE expires_at < ?1 OR revoked_at IS NOT NULL",
            vec![P::I(now)],
        )
        .await;
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/setup/status", get(setup_status))
        .route("/api/setup/claim", post(setup_claim))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
}

async fn setup_status(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let users: i64 =
        s.db.q1("SELECT COUNT(*) FROM users", vec![], |r| r.get(0))
            .await?
            .unwrap_or(0);
    let pending =
        s.db.q1(
            "SELECT used_at IS NULL FROM setup_state WHERE id = 1",
            vec![],
            |r| r.get::<_, bool>(0),
        )
        .await?
        .unwrap_or(false);
    Ok(Json(json!({
        "initialized": users > 0,
        "setup_pending": users == 0 && pending,
    })))
}

#[derive(Deserialize)]
struct ClaimBody {
    token: String,
    username: String,
    display_name: Option<String>,
    password: String,
    timezone: Option<String>,
    assistant_name: Option<String>,
    privacy_mode: Option<String>,
}

async fn setup_claim(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(b): Json<ClaimBody>,
) -> Result<Response, ApiError> {
    if !s
        .rate_limit
        .check(&format!("claim:{}", addr.ip()), 8, Duration::from_secs(60))
    {
        return Err(ApiError::RateLimited);
    }
    let username = b.username.trim().to_string();
    if username.len() < 2 || username.len() > 64 || b.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "username must be 2-64 chars and password at least 8 chars".into(),
        ));
    }
    let tz = b.timezone.clone().unwrap_or_else(|| "UTC".into());
    validate_timezone(&tz)?;
    let privacy = b.privacy_mode.clone().unwrap_or_else(|| "hybrid".into());
    if !matches!(privacy.as_str(), "hybrid" | "strict_local") {
        return Err(ApiError::BadRequest(
            "privacy_mode must be hybrid|strict_local".into(),
        ));
    }
    let display = b
        .display_name
        .clone()
        .unwrap_or_else(|| username.clone())
        .trim()
        .to_string();
    let hash = password::hash_password(&b.password)?;
    let token_hash = util::sha256_hex(b.token.trim());
    let session_id = util::new_id();
    let csrf = util::new_id();
    let now = util::now_unix();
    let assistant_name = b
        .assistant_name
        .clone()
        .unwrap_or_else(|| "Adrian".into())
        .trim()
        .to_string();

    let session_id_c = session_id.clone();
    let csrf_c = csrf.clone();
    let created =
        s.db.call(move |c| {
            let tx = c.transaction()?;
            let claimed = tx.execute(
                "UPDATE setup_state SET used_at = ?1
                 WHERE id = 1 AND used_at IS NULL AND token_hash = ?2",
                rusqlite::params![now, token_hash],
            )?;
            if claimed == 0 {
                return Err(ApiError::Forbidden(
                    "setup token invalid or already used".into(),
                ));
            }
            tx.execute(
                "INSERT INTO users(username, display_name, password_hash, role, created_at)
                 VALUES(?1,?2,?3,'owner',?4)",
                rusqlite::params![username, display, hash, now],
            )?;
            let uid = tx.last_insert_rowid();
            for (k, v) in [
                ("timezone", tz.as_str()),
                ("assistant_name", assistant_name.as_str()),
                ("privacy_mode", privacy.as_str()),
            ] {
                tx.execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    rusqlite::params![k, v],
                )?;
            }
            tx.execute(
                "INSERT INTO sessions(id,user_id,csrf,created_at,expires_at)
                 VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![session_id_c, uid, csrf_c, now, now + SESSION_TTL_SECS],
            )?;
            tx.commit()?;
            Ok(uid)
        })
        .await;

    let uid = created?;
    Ok((
        [(SET_COOKIE, session_cookie(&session_id, SESSION_TTL_SECS))],
        Json(json!({
            "ok": true,
            "csrf": csrf,
            "user": { "id": uid, "username": b.username, "role": "owner" }
        })),
    )
        .into_response())
}

fn validate_timezone(tz: &str) -> Result<(), ApiError> {
    jiff::tz::TimeZone::get(tz)
        .map(|_| ())
        .map_err(|_| ApiError::BadRequest(format!("unknown IANA timezone: {tz}")))
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(b): Json<LoginBody>,
) -> Result<Response, ApiError> {
    if !s
        .rate_limit
        .check(&format!("login:{}", addr.ip()), 10, Duration::from_secs(60))
    {
        return Err(ApiError::RateLimited);
    }
    let row =
        s.db.q1(
            "SELECT id, password_hash, role, disabled_at FROM users WHERE username = ?1",
            vec![P::T(b.username.trim().to_string())],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .await?;
    let Some((uid, hash, role, disabled)) = row else {
        return Err(ApiError::Unauthorized);
    };
    if disabled.is_some() || !password::verify_password(&hash, &b.password) {
        return Err(ApiError::Unauthorized);
    }
    let session_id = util::new_id();
    let csrf = util::new_id();
    let now = util::now_unix();
    s.db.exec(
        "INSERT INTO sessions(id,user_id,csrf,created_at,expires_at)
         VALUES(?1,?2,?3,?4,?5)",
        vec![
            P::T(session_id.clone()),
            P::I(uid),
            P::T(csrf.clone()),
            P::I(now),
            P::I(now + SESSION_TTL_SECS),
        ],
    )
    .await?;
    Ok((
        [(SET_COOKIE, session_cookie(&session_id, SESSION_TTL_SECS))],
        Json(json!({
            "ok": true,
            "csrf": csrf,
            "user": { "id": uid, "username": b.username, "role": role }
        })),
    )
        .into_response())
}

async fn logout(user: AuthUser, State(s): State<AppState>) -> Result<Response, ApiError> {
    s.db.exec(
        "UPDATE sessions SET revoked_at = ?1 WHERE id = ?2",
        vec![P::I(util::now_unix()), P::T(user.session_id)],
    )
    .await?;
    Ok((
        [(SET_COOKIE, session_cookie("", 0))],
        Json(json!({"ok": true})),
    )
        .into_response())
}

async fn me(user: AuthUser, State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let settings = s.get_settings().await?;
    let flags = s.setup_flags().await?;
    Ok(Json(json!({
        "user": {
            "id": user.id,
            "username": user.username,
            "role": user.role,
        },
        "csrf": user.csrf,
        "settings": settings,
        "setup": flags,
    })))
}
