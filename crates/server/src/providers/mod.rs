//! AI provider adapters behind a capability-aware internal contract.
//! The model proposes typed actions; Adrian validates and executes — the
//! model is never the source of authorization or of action truth.

pub mod openai_compat;
pub mod openai_responses;

use crate::db::{Db, P};
use crate::error::ApiError;
use crate::secrets::SecretBox;
use futures_util::Stream;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String, // system | user | assistant | tool
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(s: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: s.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: s.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn assistant(text: String, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: text,
            tool_calls: calls,
            tool_call_id: None,
        }
    }
    pub fn tool(call_id: String, output: String) -> Self {
        Self {
            role: "tool".into(),
            content: output,
            tool_calls: vec![],
            tool_call_id: Some(call_id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug)]
pub enum ChatEvent {
    Text(String),
    ToolCall(ToolCall),
    /// Token counts the provider reported for this call (sent before Done).
    Usage(crate::usage::Usage),
    Done,
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub streaming: bool,
    pub tools: bool,
}

pub type EventStream = Pin<Box<dyn Stream<Item = ChatEvent> + Send>>;

pub trait ChatProvider: Send + Sync {
    fn kind(&self) -> &'static str;
    fn model(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    /// Stream chat events. The stream is cold: work starts on poll.
    fn chat(&self, req: ChatRequest) -> EventStream;
    /// Bounded connectivity/credential/model check for setup + diagnostics.
    fn test(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ApiError>> + Send + '_>>;
}

pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub base_url: String,
    pub model: String,
    pub secret_name: Option<String>,
    pub enabled: bool,
    pub is_default: bool,
    pub timeout_secs: u64,
}

pub fn row_to_provider(
    row: &ProviderRow,
    http: reqwest::Client,
    api_key: Option<String>,
) -> Arc<dyn ChatProvider> {
    match row.kind.as_str() {
        "openai_responses" => Arc::new(openai_responses::OpenAiResponses::new(
            http,
            row.base_url.clone(),
            row.model.clone(),
            api_key,
            Duration::from_secs(row.timeout_secs),
        )),
        _ => Arc::new(openai_compat::OpenAiCompat::new(
            http,
            row.base_url.clone(),
            row.model.clone(),
            api_key,
            Duration::from_secs(row.timeout_secs),
        )),
    }
}

pub async fn load_provider_rows(db: &Db) -> Result<Vec<ProviderRow>, ApiError> {
    db.q(
        "SELECT id,kind,label,base_url,model,secret_name,enabled,is_default,
                COALESCE(json_extract(extra,'$.timeout_secs'),90)
         FROM providers ORDER BY created_at",
        vec![],
        |r| {
            Ok(ProviderRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                label: r.get(2)?,
                base_url: r.get(3)?,
                model: r.get(4)?,
                secret_name: r.get(5)?,
                enabled: r.get::<_, i64>(6)? != 0,
                is_default: r.get::<_, i64>(7)? != 0,
                timeout_secs: r.get::<_, i64>(8)?.max(5) as u64,
            })
        },
    )
    .await
}

/// True when a base URL points at a loopback/private/link-local host.
/// Used by strict-local privacy mode to decide whether egress is allowed.
pub fn base_is_local(base_url: &str) -> bool {
    let host = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    host == "localhost"
        || host == "::1"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || host.starts_with("172.16.")
        || host.starts_with("172.17.")
        || host.starts_with("172.18.")
        || host.starts_with("172.19.")
        || host.starts_with("172.2")
        || host.starts_with("172.30.")
        || host.starts_with("172.31.")
        || host.ends_with(".local")
        || !host.contains('.')
}

/// Speech-to-text against an OpenAI-compatible `/audio/transcriptions`
/// endpoint. Works for `openai_responses` rows (api.openai.com) and any
/// compatible local server (e.g. faster-whisper, whisper.cpp OpenAI shim).
/// Returns an honest error when the endpoint doesn't support it.
pub async fn transcribe_wav(
    state: &crate::state::AppState,
    user_id: Option<i64>,
    module: &str,
    wav: Vec<u8>,
) -> Result<String, ApiError> {
    let (db, sb, http) = (&state.db, &state.secret_box, state.http.clone());
    let rows = load_provider_rows(db).await?;
    let row = rows
        .iter()
        .find(|r| r.enabled && r.is_default)
        .or_else(|| rows.iter().find(|r| r.enabled))
        .cloned()
        .ok_or_else(|| {
            ApiError::NotConfigured("no AI provider configured for speech-to-text".into())
        })?;
    for w in crate::usage::check(state, &row, module).await? {
        tracing::info!(warning = %w, "budget");
    }
    let audio_seconds = wav_seconds(&wav);
    let key = secret_for(db, sb, &row.secret_name)
        .await?
        .unwrap_or_default();
    let stt_model = stt_model_name(&row);
    let url = format!(
        "{}/audio/transcriptions",
        row.base_url.trim_end_matches('/')
    );
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("utterance.wav")
        .mime_str("audio/wav")
        .map_err(|e| ApiError::internal_msg(format!("multipart: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", stt_model);
    let mut req = http
        .post(&url)
        .timeout(Duration::from_secs(row.timeout_secs.max(30)))
        .multipart(form);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let res = req
        .send()
        .await
        .map_err(|e| ApiError::internal_msg(format!("stt request failed: {e}")))?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(ApiError::internal_msg(format!(
            "stt endpoint returned {status}: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let v: serde_json::Value = res
        .json()
        .await
        .map_err(|e| ApiError::internal_msg(format!("stt response: {e}")))?;
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let metered = ProviderRow {
        model: stt_model_name(&row),
        ..row.clone()
    };
    crate::usage::record(
        state,
        &metered,
        "stt",
        module,
        user_id,
        &crate::usage::Usage::default(),
        audio_seconds,
    )
    .await;
    if text.is_empty() {
        return Err(ApiError::internal_msg(
            "stt endpoint returned an empty transcript",
        ));
    }
    Ok(text)
}

fn stt_model_name(row: &ProviderRow) -> String {
    match row.kind.as_str() {
        "openai_responses" => "whisper-1".to_string(),
        _ => row.model.clone(),
    }
}

/// Duration of a PCM WAV from its header (0 when it cannot be read).
fn wav_seconds(wav: &[u8]) -> f64 {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return 0.0;
    }
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut i = 12;
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let len = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= wav.len() {
            channels = u16::from_le_bytes([wav[body + 2], wav[body + 3]]);
            rate = u32::from_le_bytes([wav[body + 4], wav[body + 5], wav[body + 6], wav[body + 7]]);
            bits = u16::from_le_bytes([wav[body + 14], wav[body + 15]]);
        } else if id == b"data" {
            let bytes_per_sec = rate as f64 * channels as f64 * (bits as f64 / 8.0);
            let data = len.min(wav.len().saturating_sub(body));
            return if bytes_per_sec > 0.0 {
                data as f64 / bytes_per_sec
            } else {
                0.0
            };
        }
        i = body.saturating_add(len).saturating_add(len & 1);
    }
    0.0
}

async fn secret_for(
    db: &Db,
    sb: &SecretBox,
    name: &Option<String>,
) -> Result<Option<String>, ApiError> {
    let Some(n) = name else { return Ok(None) };
    let stored: Option<String> = db
        .q1(
            "SELECT value FROM secrets WHERE name = ?1",
            vec![P::T(n.clone())],
            |r| r.get(0),
        )
        .await?;
    stored.map(|v| sb.open(n, &v)).transpose()
}

/// Build the default enabled provider (or a specific one by id).
pub async fn load_provider(
    db: &Db,
    sb: &SecretBox,
    http: reqwest::Client,
    id: Option<&str>,
) -> Result<(Arc<dyn ChatProvider>, ProviderRow), ApiError> {
    let rows = load_provider_rows(db).await?;
    let row = match id {
        Some(id) => rows.into_iter().find(|r| r.id == id),
        None => rows
            .iter()
            .find(|r| r.enabled && r.is_default)
            .or_else(|| rows.iter().find(|r| r.enabled))
            .cloned(),
    };
    let Some(row) = row else {
        return Err(ApiError::NotConfigured(
            "no AI provider configured — add one under Integrations".into(),
        ));
    };
    if !row.enabled {
        return Err(ApiError::NotConfigured(format!(
            "provider '{}' is disabled",
            row.label
        )));
    }
    let key = secret_for(db, sb, &row.secret_name).await?;
    let needs_key = row.kind == "openai_responses" || row.base_url.contains("deepseek");
    if needs_key && key.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::NotConfigured(format!(
            "provider '{}' has no API key stored",
            row.label
        )));
    }
    Ok((row_to_provider(&row, http, key), row))
}

/// Split an HTTP SSE response into `data:` payload strings.
pub fn sse_data(resp: reqwest::Response) -> impl Stream<Item = Result<String, ApiError>> + Send {
    async_stream::stream! {
        use futures_util::StreamExt;
        let mut buf = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    buf.push_str(&String::from_utf8_lossy(&b));
                    while let Some(pos) = buf.find("\n\n") {
                        let block = buf[..pos].to_string();
                        buf.drain(..pos + 2);
                        for line in block.lines() {
                            if let Some(d) = line.strip_prefix("data:") {
                                yield Ok(d.trim().to_string());
                            }
                        }
                    }
                }
                Err(e) => {
                    yield Err(ApiError::Upstream(format!("provider stream error: {e}")));
                    break;
                }
            }
        }
    }
}

/// Read a bounded error body from a failed upstream response.
pub async fn upstream_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(300).collect();
    format!("HTTP {status}: {snippet}")
}
