//! AI provider adapters behind a capability-aware internal contract.
//! The model proposes typed actions; Adrian validates and executes — the
//! model is never the source of authorization or of action truth.

pub mod openai_compat;
pub mod openai_responses;

use crate::db::{Db, P};
use crate::error::ApiError;
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

async fn secret_for(db: &Db, name: &Option<String>) -> Result<Option<String>, ApiError> {
    match name {
        Some(n) => {
            db.q1(
                "SELECT value FROM secrets WHERE name = ?1",
                vec![P::T(n.clone())],
                |r| r.get(0),
            )
            .await
        }
        None => Ok(None),
    }
}

/// Build the default enabled provider (or a specific one by id).
pub async fn load_provider(
    db: &Db,
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
    let key = secret_for(db, &row.secret_name).await?;
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
