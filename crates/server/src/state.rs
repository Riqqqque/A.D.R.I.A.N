//! Shared application state and helpers used across handlers/services.

use crate::auth::ratelimit::RateLimiter;
use crate::config::Config;
use crate::db::{Db, P};
use crate::error::ApiError;
use crate::util;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

/// Event pushed to authenticated UI clients over /api/events (SSE).
#[derive(Debug, Clone)]
pub struct ServerEvent {
    pub kind: &'static str,
    pub data: Value,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Db,
    pub rate_limit: Arc<RateLimiter>,
    pub http: reqwest::Client,
    pub events: broadcast::Sender<ServerEvent>,
    pub scheduler: crate::scheduler::SchedulerHandle,
    pub audio: Arc<crate::audio::AudioRegistry>,
    /// Live satellite websocket sessions for server → device pushes.
    pub satellites: Arc<crate::api::satellites::SatelliteHub>,
    pub ha: crate::ha::HaManager,
    pub started: Instant,
}

impl AppState {
    pub fn emit(&self, kind: &'static str, data: Value) {
        let _ = self.events.send(ServerEvent { kind, data });
    }

    // ---- settings / secrets -------------------------------------------

    pub async fn get_settings(&self) -> Result<HashMap<String, String>, ApiError> {
        let rows = self
            .db
            .q("SELECT key, value FROM settings", vec![], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>, ApiError> {
        self.db
            .q1(
                "SELECT value FROM settings WHERE key = ?1",
                vec![P::T(key.to_string())],
                |r| r.get(0),
            )
            .await
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), ApiError> {
        self.db
            .exec(
                "INSERT INTO settings(key,value) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                vec![P::T(key.to_string()), P::T(value.to_string())],
            )
            .await?;
        Ok(())
    }

    pub async fn set_secret(&self, name: &str, value: &str) -> Result<(), ApiError> {
        self.db
            .exec(
                "INSERT INTO secrets(name,value,created_at) VALUES(?1,?2,?3)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                vec![
                    P::T(name.to_string()),
                    P::T(value.to_string()),
                    P::I(util::now_unix()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_secret(&self, name: &str) -> Result<Option<String>, ApiError> {
        self.db
            .q1(
                "SELECT value FROM secrets WHERE name = ?1",
                vec![P::T(name.to_string())],
                |r| r.get(0),
            )
            .await
    }

    /// Which first-run pieces are configured — drives honest empty states.
    pub async fn setup_flags(&self) -> Result<Value, ApiError> {
        let provider: i64 = self
            .db
            .q1(
                "SELECT COUNT(*) FROM providers WHERE enabled = 1",
                vec![],
                |r| r.get(0),
            )
            .await?
            .unwrap_or(0);
        let ha: i64 = self
            .db
            .q1(
                "SELECT COUNT(*) FROM ha_connection WHERE enabled = 1",
                vec![],
                |r| r.get(0),
            )
            .await?
            .unwrap_or(0);
        let endpoint: i64 = self
            .db
            .q1(
                "SELECT COUNT(*) FROM endpoints WHERE enabled = 1",
                vec![],
                |r| r.get(0),
            )
            .await?
            .unwrap_or(0);
        let lights: i64 = self
            .db
            .q1(
                "SELECT COUNT(*) FROM entities WHERE allowed = 1 AND domain = 'light'",
                vec![],
                |r| r.get(0),
            )
            .await?
            .unwrap_or(0);
        Ok(json!({
            "provider_configured": provider > 0,
            "ha_configured": ha > 0,
            "audio_endpoint_configured": endpoint > 0,
            "lights_allowed": lights,
        }))
    }

    pub async fn timezone(&self) -> Result<jiff::tz::TimeZone, ApiError> {
        let name = self
            .get_setting("timezone")
            .await?
            .unwrap_or_else(|| "UTC".to_string());
        jiff::tz::TimeZone::get(&name)
            .map_err(|_| ApiError::BadRequest(format!("unknown timezone {name}")))
    }

    /// Record a policy-checked action with its outcome — "what did you
    /// change?" must be answerable.
    #[allow(clippy::too_many_arguments)]
    pub async fn audit(
        &self,
        user_id: Option<i64>,
        actor: &str,
        action: &str,
        target: &str,
        params: &Value,
        outcome: &str,
        detail: &str,
    ) {
        let _ = self
            .db
            .exec(
                "INSERT INTO action_ledger(user_id,actor,action,target,params,outcome,detail,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                vec![
                    user_id.map(P::I).unwrap_or(P::Null),
                    P::T(actor.to_string()),
                    P::T(action.to_string()),
                    P::T(target.to_string()),
                    P::T(params.to_string()),
                    P::T(outcome.to_string()),
                    P::T(detail.to_string()),
                    P::I(util::now_unix()),
                ],
            )
            .await;
    }
}
