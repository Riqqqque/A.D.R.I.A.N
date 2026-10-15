//! Home Assistant integration: REST commands + a supervised WebSocket
//! subscription for real state updates. Adrian never exposes an
//! unrestricted service-call tool — callers go through typed helpers.

use crate::db::{Db, P};
use crate::error::ApiError;
use crate::state::ServerEvent;
use crate::util;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

#[derive(Debug, Clone, serde::Serialize)]
pub struct HaStatus {
    pub configured: bool,
    pub connected: bool,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct HaManager {
    db: Db,
    http: reqwest::Client,
    events: broadcast::Sender<ServerEvent>,
    secret_box: Arc<crate::secrets::SecretBox>,
    reload: Arc<Notify>,
    status: Arc<RwLock<HaStatus>>,
}

pub struct HaRest {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl HaRest {
    fn api(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }
    pub async fn get(&self, path: &str) -> Result<Value, ApiError> {
        let resp = self
            .http
            .get(self.api(path))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ApiError::Upstream(format!("home assistant unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ApiError::Upstream(format!(
                "home assistant HTTP {} for {path}",
                resp.status()
            )));
        }
        resp.json().await.map_err(ApiError::internal)
    }
    pub async fn post_service(
        &self,
        domain: &str,
        service: &str,
        body: Value,
    ) -> Result<Value, ApiError> {
        let resp = self
            .http
            .post(self.api(&format!("/api/services/{domain}/{service}")))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(15))
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Upstream(format!("home assistant unreachable: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Upstream(format!(
                "home assistant service call failed HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        resp.json().await.map_err(ApiError::internal)
    }
}

#[derive(Debug, serde::Serialize)]
pub struct LightOutcome {
    pub accepted: bool,
    pub confirmed: bool,
    pub observed_state: Option<String>,
    pub detail: String,
}

impl HaManager {
    pub fn new(
        db: Db,
        http: reqwest::Client,
        events: broadcast::Sender<ServerEvent>,
        secret_box: Arc<crate::secrets::SecretBox>,
    ) -> Self {
        Self {
            db,
            http,
            events,
            secret_box,
            reload: Arc::new(Notify::new()),
            status: Arc::new(RwLock::new(HaStatus {
                configured: false,
                connected: false,
                last_ok_at: None,
                last_error: None,
            })),
        }
    }

    /// Supervisor: reconnects the WS subscription whenever config changes or
    /// the connection drops, with bounded backoff.
    pub fn spawn(self: &Arc<Self>) {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(2);
            loop {
                let cfg = match mgr.load_config().await {
                    Ok(c) => c,
                    Err(e) => {
                        mgr.set_status(false, false, Some(e.to_string()));
                        tokio::select! {
                            _ = mgr.reload.notified() => continue,
                            _ = tokio::time::sleep(Duration::from_secs(30)) => continue,
                        }
                    }
                };
                let Some((url, token)) = cfg else {
                    mgr.set_status(false, false, None);
                    mgr.reload.notified().await;
                    continue;
                };
                match mgr.ws_session(&url, &token).await {
                    Ok(()) => {
                        mgr.set_status(true, false, Some("websocket closed".into()));
                        backoff = Duration::from_secs(2);
                    }
                    Err(e) => {
                        mgr.set_status(true, false, Some(e.to_string()));
                        let wait = backoff.min(Duration::from_secs(60));
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        tokio::select! {
                            _ = mgr.reload.notified() => { backoff = Duration::from_secs(2); }
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                }
            }
        });
    }

    fn set_status(&self, configured: bool, connected: bool, err: Option<String>) {
        let mut s = self.status.write().unwrap_or_else(|e| e.into_inner());
        s.configured = configured;
        s.connected = connected;
        if connected {
            s.last_ok_at = Some(util::now_unix());
            s.last_error = None;
        } else if let Some(e) = err {
            s.last_error = Some(e);
        }
        drop(s);
        let _ = self.events.send(ServerEvent {
            kind: "ha_status",
            data: self.status_json_now(),
        });
    }

    pub fn status_json_now(&self) -> Value {
        let s = self
            .status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        serde_json::to_value(s).unwrap_or_default()
    }

    pub fn notify_reload(&self) {
        self.reload.notify_one();
    }

    async fn load_config(&self) -> Result<Option<(String, String)>, ApiError> {
        let row = self
            .db
            .q1(
                "SELECT url, secret_name FROM ha_connection WHERE id = 1 AND enabled = 1",
                vec![],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .await?;
        let Some((url, secret)) = row else {
            return Ok(None);
        };
        let token = self
            .db
            .q1(
                "SELECT value FROM secrets WHERE name = ?1",
                vec![P::T(secret.clone())],
                |r| r.get::<_, String>(0),
            )
            .await?
            .ok_or_else(|| ApiError::NotConfigured("home assistant token missing".into()))?;
        let token = self.secret_box.open(&secret, &token)?;
        Ok(Some((url, token)))
    }

    pub async fn rest(&self) -> Result<HaRest, ApiError> {
        let (url, token) = self
            .load_config()
            .await?
            .ok_or_else(|| ApiError::NotConfigured("home assistant not configured".into()))?;
        Ok(HaRest {
            http: self.http.clone(),
            base: url,
            token,
        })
    }

    /// GET /api/ — cheap liveness + credential check.
    pub async fn test_connection(&self) -> Result<String, ApiError> {
        let rest = self.rest().await?;
        let v = rest.get("/api/").await?;
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("reachable")
            .to_string();
        self.db
            .exec(
                "UPDATE ha_connection SET last_ok_at = ?1, last_error = NULL WHERE id = 1",
                vec![P::I(util::now_unix())],
            )
            .await?;
        Ok(msg)
    }

    /// Synchronize the entity inventory: REST /api/states for
    /// entity/state/name plus WS registries for area labels. The `allowed`
    /// flag is preserved across imports.
    pub async fn import_entities(&self) -> Result<usize, ApiError> {
        let (url, token) = self
            .load_config()
            .await?
            .ok_or_else(|| ApiError::NotConfigured("home assistant not configured".into()))?;
        let rest = HaRest {
            http: self.http.clone(),
            base: url.clone(),
            token: token.clone(),
        };
        let states = rest.get("/api/states").await?;
        let states = states
            .as_array()
            .ok_or_else(|| ApiError::Upstream("unexpected /api/states shape".into()))?
            .clone();
        let areas = self.fetch_areas(&url, &token).await.unwrap_or_default();
        let now = util::now_unix();
        let count = states.len();
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                for st in &states {
                    let Some(entity_id) = st.get("entity_id").and_then(|e| e.as_str()) else {
                        continue;
                    };
                    let domain = entity_id.split('.').next().unwrap_or("").to_string();
                    let attrs = st.get("attributes").cloned().unwrap_or(json!({}));
                    let name = attrs
                        .get("friendly_name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(entity_id)
                        .to_string();
                    let area = areas.get(entity_id).cloned();
                    let state = st.get("state").and_then(|s| s.as_str()).unwrap_or("");
                    tx.execute(
                        "INSERT INTO entities(entity_id,area,name,domain,state,attributes,allowed,updated_at)
                         VALUES(?1,?2,?3,?4,?5,?6,0,?7)
                         ON CONFLICT(entity_id) DO UPDATE SET
                            area=excluded.area, name=excluded.name, domain=excluded.domain,
                            state=excluded.state, attributes=excluded.attributes,
                            updated_at=excluded.updated_at",
                        rusqlite::params![
                            entity_id,
                            area,
                            name,
                            domain,
                            state,
                            attrs.to_string(),
                            now
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(count)
            })
            .await
    }

    /// entity_id -> area name via the WS config registries (best effort).
    async fn fetch_areas(
        &self,
        url: &str,
        token: &str,
    ) -> Result<std::collections::HashMap<String, String>, ApiError> {
        use std::collections::HashMap;
        let ws_url = ws_url(url);
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(&ws_url),
        )
        .await
        .map_err(|_| ApiError::Timeout("home assistant websocket".into()))?
        .map_err(|e| ApiError::Upstream(format!("ha websocket: {e}")))?;
        ws_auth(&mut ws, token).await?;
        ws.send(tungstenite_text(
            &json!({"id":1,"type":"config/area_registry/list"}),
        ))
        .await
        .map_err(ApiError::internal)?;
        ws.send(tungstenite_text(
            &json!({"id":2,"type":"config/entity_registry/list"}),
        ))
        .await
        .map_err(ApiError::internal)?;
        let mut area_names: HashMap<String, String> = HashMap::new();
        let mut entity_area: HashMap<String, String> = HashMap::new();
        let mut got = 0;
        while got < 2 {
            let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
                .await
                .map_err(|_| ApiError::Timeout("ha registry reply".into()))?
                .ok_or_else(|| ApiError::Upstream("ha websocket closed".into()))?
                .map_err(ApiError::internal)?;
            let Ok(v) = serde_json::from_str::<Value>(msg.to_text().unwrap_or_default()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("result") {
                continue;
            }
            let id = v.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
            let result = v.get("result").cloned().unwrap_or(json!([]));
            if id == 1 {
                for a in result.as_array().cloned().unwrap_or_default() {
                    if let (Some(id), Some(name)) = (
                        a.get("area_id").and_then(|x| x.as_str()),
                        a.get("name").and_then(|x| x.as_str()),
                    ) {
                        area_names.insert(id.to_string(), name.to_string());
                    }
                }
                got += 1;
            } else if id == 2 {
                for e in result.as_array().cloned().unwrap_or_default() {
                    if let (Some(eid), Some(aid)) = (
                        e.get("entity_id").and_then(|x| x.as_str()),
                        e.get("area_id").and_then(|x| x.as_str()),
                    ) {
                        entity_area.insert(eid.to_string(), aid.to_string());
                    }
                }
                got += 1;
            }
        }
        Ok(entity_area
            .into_iter()
            .filter_map(|(eid, aid)| area_names.get(&aid).map(|n| (eid, n.clone())))
            .collect())
    }

    /// Live state subscription: upserts changed entities and broadcasts to UI.
    async fn ws_session(&self, url: &str, token: &str) -> Result<(), ApiError> {
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(ws_url(url)),
        )
        .await
        .map_err(|_| ApiError::Timeout("home assistant websocket".into()))?
        .map_err(|e| ApiError::Upstream(format!("ha websocket: {e}")))?;
        ws_auth(&mut ws, token).await?;
        ws.send(tungstenite_text(&json!({
            "id": 1,
            "type": "subscribe_events",
            "event_type": "state_changed"
        })))
        .await
        .map_err(ApiError::internal)?;
        // Expect subscription result.
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
                .await
                .map_err(|_| ApiError::Timeout("ha subscribe reply".into()))?
                .ok_or_else(|| ApiError::Upstream("ha websocket closed".into()))?
                .map_err(ApiError::internal)?;
            let v: Value = serde_json::from_str(msg.to_text().unwrap_or_default())
                .map_err(|_| ApiError::Upstream("ha protocol error".into()))?;
            match v.get("type").and_then(|t| t.as_str()) {
                Some("result") => {
                    if v.get("success").and_then(|s| s.as_bool()) == Some(true) {
                        break;
                    }
                    return Err(ApiError::Upstream("ha event subscription refused".into()));
                }
                _ => continue,
            }
        }
        self.set_status(true, true, None);
        tracing::info!("home assistant websocket connected");
        while let Some(msg) = ws.next().await {
            let msg = msg.map_err(ApiError::internal)?;
            let Ok(v) = serde_json::from_str::<Value>(msg.to_text().unwrap_or_default()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("event") {
                continue;
            }
            let Some(data) = v.pointer("/event/data") else {
                continue;
            };
            let Some(entity_id) = data.get("entity_id").and_then(|e| e.as_str()) else {
                continue;
            };
            let Some(new_state) = data.get("new_state") else {
                continue;
            };
            let state = new_state
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let attrs = new_state.get("attributes").cloned().unwrap_or(json!({}));
            let eid = entity_id.to_string();
            let st = state.clone();
            let attrs_s = attrs.to_string();
            let now = util::now_unix();
            let _ = self
                .db
                .exec(
                    "UPDATE entities SET state = ?1, attributes = ?2, updated_at = ?3
                     WHERE entity_id = ?4",
                    vec![
                        P::T(st.clone()),
                        P::T(attrs_s),
                        P::I(now),
                        P::T(eid.clone()),
                    ],
                )
                .await;
            let _ = self.events.send(ServerEvent {
                kind: "entity_state",
                data: json!({"entity_id": eid, "state": st}),
            });
        }
        Ok(())
    }

    /// Execute an allowed light service then verify observed state — an
    /// accepted API call is not proof the device changed.
    pub async fn light_command(
        &self,
        entity_id: &str,
        action: &str,
        brightness_pct: Option<i64>,
    ) -> Result<LightOutcome, ApiError> {
        let rest = self.rest().await?;
        let before = rest
            .get(&format!("/api/states/{entity_id}"))
            .await
            .ok()
            .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(String::from));
        let mut body = json!({"entity_id": entity_id});
        if let Some(b) = brightness_pct {
            body["brightness_pct"] = json!(b.clamp(1, 100));
        }
        rest.post_service("light", action, body).await?;
        let want = match action {
            "turn_on" => Some("on"),
            "turn_off" => Some("off"),
            _ => None, // toggle: any state change counts
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut observed = before.clone();
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if let Ok(v) = rest.get(&format!("/api/states/{entity_id}")).await {
                let cur = v.get("state").and_then(|s| s.as_str()).map(String::from);
                if cur != before && cur.is_some() {
                    observed = cur;
                    break;
                }
                observed = cur.or(observed);
            }
        }
        let confirmed = match (&want, &observed, &before) {
            (Some(w), Some(o), _) => o == w,
            (None, Some(o), Some(b)) => o != b,
            _ => false,
        };
        let detail = if confirmed {
            format!("observed state '{}'", observed.as_deref().unwrap_or("?"))
        } else {
            "service call accepted but the state change was not confirmed within 5s".to_string()
        };
        Ok(LightOutcome {
            accepted: true,
            confirmed,
            observed_state: observed,
            detail,
        })
    }

    /// Play a URL on a media_player entity (used for alarm delivery).
    pub async fn play_media(
        &self,
        entity_id: &str,
        media_url: &str,
        volume: Option<f64>,
    ) -> Result<(), ApiError> {
        let rest = self.rest().await?;
        if let Some(v) = volume {
            let _ = rest
                .post_service(
                    "media_player",
                    "volume_set",
                    json!({"entity_id": entity_id, "volume_level": v.clamp(0.0, 1.0)}),
                )
                .await; // volume_set unsupported on some players — non-fatal
        }
        rest.post_service(
            "media_player",
            "play_media",
            json!({
                "entity_id": entity_id,
                "media_content_id": media_url,
                "media_content_type": "music",
            }),
        )
        .await?;
        Ok(())
    }
}

fn ws_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if let Some(rest) = b.strip_prefix("https://") {
        format!("wss://{rest}/api/websocket")
    } else if let Some(rest) = b.strip_prefix("http://") {
        format!("ws://{rest}/api/websocket")
    } else {
        format!("ws://{b}/api/websocket")
    }
}

fn tungstenite_text(v: &Value) -> tokio_tungstenite::tungstenite::Message {
    tokio_tungstenite::tungstenite::Message::Text(v.to_string().into())
}

async fn ws_auth(
    ws: &mut (
             impl SinkExt<
        tokio_tungstenite::tungstenite::Message,
        Error = tokio_tungstenite::tungstenite::Error,
    > + StreamExt<
        Item = Result<
            tokio_tungstenite::tungstenite::Message,
            tokio_tungstenite::tungstenite::Error,
        >,
    > + Unpin
         ),
    token: &str,
) -> Result<(), ApiError> {
    // Expect auth_required, send token, expect auth_ok.
    let first = tokio::time::timeout(Duration::from_secs(15), ws.next())
        .await
        .map_err(|_| ApiError::Timeout("ha auth greeting".into()))?
        .ok_or_else(|| ApiError::Upstream("ha websocket closed".into()))?
        .map_err(ApiError::internal)?;
    let v: Value = serde_json::from_str(first.to_text().unwrap_or_default())
        .map_err(|_| ApiError::Upstream("ha protocol error".into()))?;
    if v.get("type").and_then(|t| t.as_str()) != Some("auth_required") {
        return Err(ApiError::Upstream(
            "unexpected ha websocket greeting".into(),
        ));
    }
    ws.send(tungstenite_text(
        &json!({"type":"auth","access_token":token}),
    ))
    .await
    .map_err(ApiError::internal)?;
    let reply = tokio::time::timeout(Duration::from_secs(15), ws.next())
        .await
        .map_err(|_| ApiError::Timeout("ha auth reply".into()))?
        .ok_or_else(|| ApiError::Upstream("ha websocket closed".into()))?
        .map_err(ApiError::internal)?;
    let v: Value = serde_json::from_str(reply.to_text().unwrap_or_default())
        .map_err(|_| ApiError::Upstream("ha protocol error".into()))?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("auth_ok") => Ok(()),
        _ => Err(ApiError::Forbidden(
            "home assistant rejected the access token".into(),
        )),
    }
}
