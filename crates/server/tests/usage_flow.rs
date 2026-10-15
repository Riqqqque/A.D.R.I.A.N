//! Usage metering and budgets end to end: a mock OpenAI-compatible provider
//! streams a reply with a usage chunk, the chat turn records it, and an
//! exhausted budget refuses paid calls while local ones keep working.

use adrian_server::{Config, build_router, build_state, start_services};
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

struct H {
    state: Arc<adrian_server::AppState>,
    router: axum::Router,
    cookie: String,
    csrf: String,
    _dir: tempfile::TempDir,
}

async fn raw(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))));
    if body.is_some() {
        b = b.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    if let Some(t) = csrf {
        b = b.header("x-adrian-csrf", t);
    }
    let req = b
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

impl H {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, _, text) = raw(
            &self.router,
            method,
            path,
            body,
            Some(&self.cookie),
            Some(&self.csrf),
        )
        .await;
        (s, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Run one chat message and return the raw SSE text.
    async fn chat(&self, message: &str) -> String {
        let (s, _, text) = raw(
            &self.router,
            "POST",
            "/api/chat",
            Some(json!({"message": message})),
            Some(&self.cookie),
            Some(&self.csrf),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{text}");
        text
    }
}

async fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        web_dir: dir.path().join("web"),
        tls: None,
    };
    let state = build_state(config).await.unwrap();
    start_services(&state).await.unwrap();
    let router = build_router(state.clone());
    let token = std::fs::read_to_string(dir.path().join("SETUP_TOKEN.txt")).unwrap();
    let (status, headers, body) = raw(
        &router,
        "POST",
        "/api/setup/claim",
        Some(
            json!({"token": token, "username": "owner", "password": "password123",
                    "timezone": "America/Denver", "privacy_mode": "hybrid"}),
        ),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = serde_json::from_str::<Value>(&body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    H {
        state,
        router,
        cookie,
        csrf,
        _dir: dir,
    }
}

/// Mock `/v1/chat/completions`: streams "Hello there" then a usage chunk.
/// Counts requests and whether `stream_options.include_usage` was sent.
async fn mock_provider() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let asked_usage = Arc::new(AtomicUsize::new(0));
    let (h, a) = (hits.clone(), asked_usage.clone());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (h, a) = (h.clone(), a.clone());
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                if body["stream_options"]["include_usage"] == json!(true) {
                    a.fetch_add(1, Ordering::SeqCst);
                }
                let chunks = [
                    json!({"choices":[{"index":0,"delta":{"content":"Hello "}}]}),
                    json!({"choices":[{"index":0,"delta":{"content":"there"},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":7,
                        "prompt_tokens_details":{"cached_tokens":20}}}),
                ];
                let mut sse = String::new();
                for c in chunks {
                    sse.push_str(&format!("data: {c}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    (format!("http://{addr}/v1"), hits, asked_usage)
}

async fn usage_rows(h: &H) -> Vec<(String, i64, i64, i64, Option<i64>, i64, i64)> {
    h.state
        .db
        .q(
            "SELECT provider_label, input_tokens, cached_tokens, output_tokens, cost_micros,
                local, estimated FROM usage_events ORDER BY id",
            vec![],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn chat_usage_is_metered_and_budgets_gate_paid_calls() {
    let h = harness().await;
    let (base, hits, asked_usage) = mock_provider().await;

    // Local mock provider.
    let (s, local) = h
        .call(
            "POST",
            "/api/integrations/providers",
            Some(
                json!({"kind":"openai_compatible","label":"Local mock","base_url": base,
                        "model":"mock-1","enabled":true}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{local}");
    let local_id = local["id"].as_str().unwrap().to_string();
    h.call(
        "POST",
        &format!("/api/integrations/providers/{local_id}/default"),
        None,
    )
    .await;

    // 1. A chat turn streams the reply and records the reported usage.
    let sse = h.chat("say hello").await;
    assert!(sse.contains("Hello ") && sse.contains("there"), "{sse}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        asked_usage.load(Ordering::SeqCst),
        1,
        "include_usage requested"
    );
    let rows = usage_rows(&h).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0], ("Local mock".into(), 120, 20, 7, Some(0), 1, 0));

    // 2. A cloud provider (never reached here) without a price is reported
    // as unpriced; with a price, recorded usage gets a cost.
    let (s, cloud) = h
        .call(
            "POST",
            "/api/integrations/providers",
            Some(
                json!({"kind":"openai_compatible","label":"Cloud","base_url":
                        "https://api.example.invalid/v1","model":"big-1","api_key":"sk-test",
                        "enabled":true}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{cloud}");
    let cloud_id = cloud["id"].as_str().unwrap().to_string();
    let (_, summary) = h.call("GET", "/api/usage", None).await;
    assert!(
        summary["unpriced"].to_string().contains("Cloud (big-1)"),
        "{summary}"
    );
    let (s, r) = h
        .call(
            "PUT",
            "/api/usage/prices",
            Some(json!({"provider_id": cloud_id, "model":"big-1",
                        "input_per_mtok": 2.0, "cached_per_mtok": 0.5, "output_per_mtok": 8.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let rows_db = adrian_server::providers::load_provider_rows(&h.state.db)
        .await
        .unwrap();
    let cloud_row = rows_db.iter().find(|r| r.id == cloud_id).unwrap().clone();
    let u = adrian_server::usage::Usage {
        input: 1_000,
        cached: 400,
        output: 250,
        estimated: false,
    };
    adrian_server::usage::record(&h.state, &cloud_row, "chat", "web", Some(1), &u, 0.0).await;
    let rows = usage_rows(&h).await;
    assert_eq!(rows[1].4, Some(3_400), "600*2 + 400*0.5 + 250*8 micro-USD");

    // 3. A $0.004 household budget is now 85% used: one soft warning, once.
    let (s, b) = h
        .call(
            "POST",
            "/api/usage/budgets",
            Some(json!({"scope":"global","monthly_limit": 0.004, "soft_pct": 80})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let w = adrian_server::usage::check(&h.state, &cloud_row, "web")
        .await
        .unwrap();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("85%"), "{}", w[0]);
    assert!(
        adrian_server::usage::check(&h.state, &cloud_row, "web")
            .await
            .unwrap()
            .is_empty(),
        "soft warning is sent once per month"
    );

    // 4. Over the limit: paid chat is refused before any network call.
    adrian_server::usage::record(&h.state, &cloud_row, "chat", "web", Some(1), &u, 0.0).await;
    h.call(
        "POST",
        &format!("/api/integrations/providers/{cloud_id}/default"),
        None,
    )
    .await;
    let sse = h.chat("what's the weather").await;
    assert!(sse.contains("budget") && sse.contains("used up"), "{sse}");
    assert_eq!(
        usage_rows(&h).await.len(),
        3,
        "refused turn records nothing"
    );

    // 5. A local provider is free and keeps working with the budget spent.
    h.call(
        "POST",
        &format!("/api/integrations/providers/{local_id}/default"),
        None,
    )
    .await;
    let sse = h.chat("say hello again").await;
    assert!(sse.contains("there"), "{sse}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // 6. The summary reports totals and the budget standing.
    let (_, summary) = h.call("GET", "/api/usage", None).await;
    assert_eq!(summary["total_cost_micros"], json!(6_800), "{summary}");
    assert_eq!(summary["budgets"][0]["spent_micros"], json!(6_800));
    assert_eq!(summary["unpriced"], json!([]));

    // Bad month labels are rejected, not panicked on.
    let (s, _) = h.call("GET", "/api/usage?month=2026-13", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
