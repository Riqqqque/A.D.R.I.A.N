//! OpenAI Responses API adapter (`POST /v1/responses`, streaming SSE).

use super::{
    Capabilities, ChatEvent, ChatProvider, ChatRequest, EventStream, ToolCall, sse_data,
    upstream_error,
};
use crate::error::ApiError;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::pin::Pin;
use std::time::Duration;

pub struct OpenAiResponses {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
}

impl OpenAiResponses {
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

    /// Map internal messages to Responses `input` items.
    fn wire_input(req: &ChatRequest) -> Vec<Value> {
        let mut items = Vec::new();
        for m in &req.messages {
            match m.role.as_str() {
                "system" => items.push(json!({
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "input_text", "text": m.content }],
                })),
                "user" => items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": m.content }],
                })),
                "assistant" => {
                    if !m.content.is_empty() {
                        items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": m.content }],
                        }));
                    }
                    for c in &m.tool_calls {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": c.id,
                            "name": c.name,
                            "arguments": c.arguments.to_string(),
                        }));
                    }
                }
                "tool" => items.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id,
                    "output": m.content,
                })),
                _ => {}
            }
        }
        items
    }
}

impl ChatProvider for OpenAiResponses {
    fn kind(&self) -> &'static str {
        "openai_responses"
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
            "store": false,
            "input": Self::wire_input(&req),
            "tools": req.tools.iter().map(|t| json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "strict": false,
            })).collect::<Vec<_>>(),
        });
        let url = format!("{}/responses", self.base_url);
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
            while let Some(item) = data.next().await {
                let payload = match item {
                    Ok(p) => p,
                    Err(e) => {
                        yield ChatEvent::Failed(e.to_string());
                        return;
                    }
                };
                let Ok(v) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("response.output_text.delta") => {
                        if let Some(d) = v.get("delta").and_then(|d| d.as_str())
                            && !d.is_empty() {
                                yield ChatEvent::Text(d.to_string());
                            }
                    }
                    Some("response.output_item.done") => {
                        let item = v.get("item").cloned().unwrap_or_default();
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            let name = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args_raw = item
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("{}");
                            let arguments = serde_json::from_str::<Value>(args_raw)
                                .unwrap_or_else(|_| json!({"_unparseable_arguments": args_raw}));
                            let call_id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|i| i.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| format!("call_{}", crate::util::new_id()));
                            yield ChatEvent::ToolCall(ToolCall {
                                id: call_id,
                                name,
                                arguments,
                            });
                        }
                    }
                    Some("response.failed") | Some("error") => {
                        let msg = v
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .or_else(|| v.get("error"))
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("upstream response failed")
                            .to_string();
                        yield ChatEvent::Failed(msg);
                        return;
                    }
                    Some("response.completed") | Some("response.incomplete") => {}
                    _ => {}
                }
            }
            yield ChatEvent::Done;
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
                Ok("reachable; model list empty or unsupported".into())
            } else if models.iter().any(|m| m == &self.model) {
                Ok(format!("reachable; model '{}' present", self.model))
            } else {
                Err(ApiError::BadRequest(format!(
                    "reachable but model '{}' not advertised by the endpoint",
                    self.model
                )))
            }
        })
    }
}
