//! OpenAI Chat Completions-compatible adapter. Used for DeepSeek (its
//! supported interface), configurable local endpoints (e.g. llama.cpp
//! llama-server `/v1`), and any other compatible endpoint the user adds.

use super::{
    Capabilities, ChatEvent, ChatProvider, ChatRequest, EventStream, ToolCall, sse_data,
    upstream_error,
};
use crate::error::ApiError;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

pub struct OpenAiCompat {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
}

impl OpenAiCompat {
    pub fn new(
        http: reqwest::Client,
        base_url: String,
        model: String,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            timeout,
        }
    }

    fn wire_messages(req: &ChatRequest) -> Vec<Value> {
        req.messages
            .iter()
            .map(|m| {
                if m.role == "assistant" && !m.tool_calls.is_empty() {
                    json!({
                        "role": "assistant",
                        "content": if m.content.is_empty() { Value::Null } else { json!(m.content) },
                        "tool_calls": m.tool_calls.iter().map(|c| json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": c.arguments.to_string(),
                            }
                        })).collect::<Vec<_>>(),
                    })
                } else if m.role == "tool" {
                    json!({
                        "role": "tool",
                        "tool_call_id": m.tool_call_id,
                        "content": m.content,
                    })
                } else {
                    json!({ "role": m.role, "content": m.content })
                }
            })
            .collect()
    }
}

impl ChatProvider for OpenAiCompat {
    fn kind(&self) -> &'static str {
        "openai_compatible"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            tools: true,
        }
    }

    fn chat(&self, req: ChatRequest) -> EventStream {
        let body = json!({
            "model": self.model,
            "stream": true,
            "messages": Self::wire_messages(&req),
            "tools": req.tools.iter().map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })).collect::<Vec<_>>(),
            "tool_choice": "auto",
        });
        let url = format!("{}/chat/completions", self.base_url);
        let http = self.http.clone();
        let key = self.api_key.clone();
        let timeout = self.timeout;
        Box::pin(async_stream::stream! {
            let mut rb = http.post(&url).timeout(timeout).json(&body);
            if let Some(k) = key.as_deref().filter(|k| !k.is_empty()) {
                rb = rb.bearer_auth(k);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    yield ChatEvent::Failed(format!("provider request failed: {e}"));
                    return;
                }
            };
            if !resp.status().is_success() {
                yield ChatEvent::Failed(upstream_error(resp).await);
                return;
            }
            let mut data = Box::pin(sse_data(resp));
            // Accumulate streamed tool-call fragments keyed by index.
            let mut calls: HashMap<i64, (String, String, String)> = HashMap::new();
            let mut finish: Option<String> = None;
            while let Some(item) = data.next().await {
                let payload = match item {
                    Ok(p) => p,
                    Err(e) => {
                        yield ChatEvent::Failed(e.to_string());
                        return;
                    }
                };
                if payload == "[DONE]" {
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                if let Some(err) = v.get("error") {
                    yield ChatEvent::Failed(format!(
                        "provider error: {}",
                        err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
                    ));
                    return;
                }
                let Some(choice) = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                else {
                    continue;
                };
                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    finish = Some(fr.to_string());
                }
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|c| c.as_str())
                        && !text.is_empty() {
                            yield ChatEvent::Text(text.to_string());
                        }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                            let entry = calls.entry(idx).or_default();
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                entry.0 = id.to_string();
                            }
                            if let Some(f) = tc.get("function") {
                                if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                                    entry.1 = n.to_string();
                                }
                                if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                                    entry.2.push_str(a);
                                }
                            }
                        }
                    }
                }
            }
            let mut ordered: Vec<_> = calls.into_iter().collect();
            ordered.sort_by_key(|(i, _)| *i);
            for (_, (id, name, args)) in ordered {
                if name.is_empty() {
                    continue;
                }
                let arguments = serde_json::from_str::<Value>(&args)
                    .unwrap_or_else(|_| json!({"_unparseable_arguments": args}));
                yield ChatEvent::ToolCall(ToolCall {
                    id: if id.is_empty() { format!("call_{}", crate::util::new_id()) } else { id },
                    name,
                    arguments,
                });
            }
            yield ChatEvent::Done;
            let _ = finish;
        })
    }

    fn test(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ApiError>> + Send + '_>> {
        Box::pin(async move {
            let url = format!("{}/models", self.base_url);
            let mut rb = self.http.get(&url).timeout(Duration::from_secs(15));
            if let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
                rb = rb.bearer_auth(k);
            }
            let resp = rb
                .send()
                .await
                .map_err(|e| ApiError::Upstream(format!("models request failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(ApiError::Upstream(upstream_error(resp).await));
            }
            let v: Value = resp.json().await.unwrap_or_else(|_| json!({}));
            let models: Vec<String> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if models.is_empty() {
                Ok(
                    "reachable; /models returned no list (endpoint may not support model listing)"
                        .into(),
                )
            } else if models.iter().any(|m| m == &self.model) {
                Ok(format!(
                    "reachable; model '{}' present among {} models",
                    self.model,
                    models.len()
                ))
            } else {
                Err(ApiError::BadRequest(format!(
                    "endpoint reachable but model '{}' not in advertised list ({} models)",
                    self.model,
                    models.len()
                )))
            }
        })
    }
}

/// Ensure `Stream` stays in scope for callers using `.next()` on data.
#[allow(dead_code)]
fn _assert_stream(s: impl Stream) -> impl Stream {
    s
}
