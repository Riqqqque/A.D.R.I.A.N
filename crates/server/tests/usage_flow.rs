//! Usage metering and budgets end to end: a mock OpenAI-compatible provider
//! streams a reply with a usage chunk, the chat turn records it, and an
//! exhausted budget refuses paid calls while local ones keep working.

use adrian_server::{Config, build_router, build_state, start_services};
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    harness_with(None).await
}

/// `cloud` is the address `CLOUD_HOST` resolves to. The name has to be a
/// public-looking one: metering treats any local address as free, so a
/// budget can only bite on a provider that looks like a cloud.
async fn harness_with(cloud: Option<SocketAddr>) -> H {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        web_dir: dir.path().join("web"),
        tls: None,
        trusted_proxy: None,
    };
    let mut state = build_state(config).await.unwrap();
    if let Some(addr) = cloud {
        Arc::get_mut(&mut state).unwrap().http = reqwest::Client::builder()
            .no_proxy()
            .resolve(CLOUD_HOST, addr)
            .build()
            .unwrap();
    }
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

const CLOUD_HOST: &str = "cloud.mock.test";

fn sse_reply(chunks: Vec<Value>) -> Response {
    let mut sse = String::new();
    for c in chunks {
        sse.push_str(&format!("data: {c}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
}

fn text_reply(text: &str) -> Response {
    sse_reply(vec![
        json!({"choices":[{"index":0,"delta":{"content": text},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":100}}),
    ])
}

fn tool_reply(calls: &[(&str, Value)]) -> Response {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({"index": i, "id": format!("call_{i}"),
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    sse_reply(vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls": tool_calls}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":100}}),
    ])
}

fn rate_limited() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response()
}

type Scripted = (SocketAddr, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>);

async fn scripted_provider(
    script: impl Fn(usize) -> Response + Clone + Send + Sync + 'static,
) -> Scripted {
    provider_after(Duration::ZERO, script).await
}

/// Mock `/v1/chat/completions` answering by request number, from 0, after
/// `delay`; the body of each request is kept. Returns its address, hit count
/// and bodies.
async fn provider_after(
    delay: Duration,
    script: impl Fn(usize) -> Response + Clone + Send + Sync + 'static,
) -> Scripted {
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let (h, b) = (hits.clone(), bodies.clone());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (h, b, script) = (h.clone(), b.clone(), script.clone());
            async move {
                b.lock().unwrap().push(body);
                let n = h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                script(n)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    (addr, hits, bodies)
}

impl H {
    /// Add a provider and make it the default; returns its id.
    async fn use_provider(&self, base_url: &str) -> String {
        let (s, p) = self
            .call(
                "POST",
                "/api/integrations/providers",
                Some(
                    json!({"kind":"openai_compatible","label":"Mock","base_url": base_url,
                            "model":"mock-1","api_key":"sk-test","enabled":true}),
                ),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{p}");
        let id = p["id"].as_str().unwrap().to_string();
        let (s, r) = self
            .call(
                "POST",
                &format!("/api/integrations/providers/{id}/default"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        id
    }

    /// Price the provider at $2 per million tokens either way, with a budget.
    async fn set_budget(&self, provider_id: &str, monthly_limit: f64) {
        let (s, r) = self
            .call(
                "PUT",
                "/api/usage/prices",
                Some(json!({"provider_id": provider_id, "model":"mock-1",
                            "input_per_mtok": 2.0, "output_per_mtok": 2.0})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let (s, r) = self
            .call(
                "POST",
                "/api/usage/budgets",
                Some(json!({"scope":"global","monthly_limit": monthly_limit})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
    }

    async fn add_speaker(&self) {
        let (s, r) = self
            .call(
                "POST",
                "/api/audio/register",
                Some(json!({"name":"speaker"})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
    }

    async fn assistant_lines(&self) -> Vec<String> {
        self.state
            .db
            .q(
                "SELECT content FROM messages WHERE role = 'assistant' ORDER BY id",
                vec![],
                |r| r.get(0),
            )
            .await
            .unwrap()
    }

    async fn alarms(&self) -> i64 {
        self.state
            .db
            .q1("SELECT COUNT(*) FROM alarms", vec![], |r| r.get(0))
            .await
            .unwrap()
            .unwrap()
    }
}

fn set_timer() -> Response {
    tool_reply(&[("set_alarm", json!({"in_minutes": 10}))])
}

/// The action must be told before the error, and be in the stored chat.
async fn assert_timer_reported(h: &H, sse: &str) {
    let told = sse.find("Timer set").unwrap_or_else(|| panic!("{sse}"));
    let failed = sse.find("event: error").unwrap_or_else(|| panic!("{sse}"));
    assert!(told < failed, "{sse}");
    let saved = h.assistant_lines().await;
    assert!(saved.iter().any(|l| l.contains("Timer set")), "{saved:?}");
    assert_eq!(h.alarms().await, 1, "the timer itself must stand");
}

#[tokio::test]
async fn a_failed_later_round_still_reports_what_was_done() {
    let (addr, hits, _) = scripted_provider(|n| match n {
        0 => set_timer(),
        _ => rate_limited(),
    })
    .await;
    let h = harness().await;
    h.use_provider(&format!("http://{addr}/v1")).await;
    h.add_speaker().await;

    let sse = h.chat("set a timer for 10 minutes").await;

    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_timer_reported(&h, &sse).await;
}

#[tokio::test]
async fn a_budget_used_up_between_rounds_still_reports_what_was_done() {
    let (addr, hits, _) = scripted_provider(|_| set_timer()).await;
    let h = harness_with(Some(addr)).await;
    let id = h
        .use_provider(&format!("http://{CLOUD_HOST}:{}/v1", addr.port()))
        .await;
    // The first round costs $0.0022.
    h.set_budget(&id, 0.001).await;
    h.add_speaker().await;

    let sse = h.chat("set a timer for 10 minutes").await;

    assert_eq!(hits.load(Ordering::SeqCst), 1, "{sse}");
    assert!(sse.contains("used up"), "{sse}");
    assert_timer_reported(&h, &sse).await;
}

#[tokio::test]
async fn running_out_of_rounds_is_said_and_saved() {
    let (addr, hits, _) = scripted_provider(|_| tool_reply(&[("list_lights", json!({}))])).await;
    let h = harness().await;
    h.use_provider(&format!("http://{addr}/v1")).await;

    let sse = h.chat("what lights are there").await;

    assert_eq!(hits.load(Ordering::SeqCst), 8);
    let saved = h.assistant_lines().await;
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(!saved[0].is_empty());
    assert!(sse.contains(&saved[0]), "{sse}");
}

#[tokio::test]
async fn calls_past_the_round_cap_are_answered_but_not_run() {
    let calls: Vec<(&str, Value)> = (0..10).map(|_| ("get_current_time", json!({}))).collect();
    let (addr, _, bodies) = scripted_provider(move |n| match n {
        0 => tool_reply(&calls),
        _ => text_reply("It is late."),
    })
    .await;
    let h = harness().await;
    h.use_provider(&format!("http://{addr}/v1")).await;

    let sse = h.chat("what time is it now").await;

    assert_eq!(sse.matches(r#""status":"executing""#).count(), 8, "{sse}");
    // Every call needs a result, or the provider rejects the next round.
    let bodies = bodies.lock().unwrap();
    let results: Vec<&str> = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results.len(), 10, "{results:?}");
    assert!(results[..8].iter().all(|r| r.contains(r#""ok":true"#)));
    assert!(results[8..].iter().all(|r| r.contains(r#""ok":false"#)));
}

#[tokio::test]
async fn overlapping_turns_cannot_both_spend_the_last_of_a_budget() {
    // Slow enough that both turns are in flight together.
    let (addr, hits, _) = provider_after(Duration::from_millis(300), |_| text_reply("Hello")).await;
    let h = harness_with(Some(addr)).await;
    let id = h
        .use_provider(&format!("http://{CLOUD_HOST}:{}/v1", addr.port()))
        .await;
    // Room for one turn: it costs $0.0022.
    h.set_budget(&id, 0.002).await;

    let (a, b) = tokio::join!(h.chat("first"), h.chat("second"));

    assert_eq!(hits.load(Ordering::SeqCst), 1, "{a}\n{b}");
    assert!(a.contains("used up") || b.contains("used up"), "{a}\n{b}");
}

#[tokio::test]
async fn an_unpriced_cloud_model_is_refused_under_a_budget() {
    let h = harness().await;
    h.use_provider("https://api.example.invalid/v1").await;
    let (s, r) = h
        .call(
            "POST",
            "/api/usage/budgets",
            Some(json!({"scope":"global","monthly_limit": 5.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    let sse = h.chat("what's the weather").await;

    assert!(sse.contains("no price"), "{sse}");
    assert!(!sse.contains("provider error"), "{sse}");
}

#[tokio::test]
async fn speech_is_refused_when_the_budget_cannot_price_its_model() {
    let h = harness().await;
    let (s, p) = h
        .call(
            "POST",
            "/api/integrations/providers",
            Some(json!({"kind":"openai_responses","label":"Cloud","base_url":
                        "https://api.example.invalid/v1","model":"big-1","api_key":"sk-test",
                        "enabled":true})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let id = p["id"].as_str().unwrap().to_string();
    // The chat model has a price; the speech model it runs on does not.
    let (s, r) = h
        .call(
            "PUT",
            "/api/usage/prices",
            Some(json!({"provider_id": id, "model":"big-1",
                        "input_per_mtok": 2.0, "output_per_mtok": 2.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = h
        .call(
            "POST",
            "/api/usage/budgets",
            Some(json!({"scope":"global","monthly_limit": 5.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    let err =
        adrian_server::providers::transcribe_wav(&h.state, Some(1), "satellite", vec![0; 200])
            .await
            .unwrap_err();

    let err = err.to_string();
    assert!(
        err.contains("no price") && err.contains("whisper-1"),
        "{err}"
    );
}

/// A refused call never runs, so the speech model can't surface as something
/// that ran unpriced. The owner has to learn it from the Usage page, but only
/// once something on this server actually sends speech.
#[tokio::test]
async fn the_speech_model_is_named_as_unpriced_once_voice_is_in_use() {
    let h = harness().await;
    let (s, p) = h
        .call(
            "POST",
            "/api/integrations/providers",
            Some(json!({"kind":"openai_responses","label":"Cloud","base_url":
                        "https://api.example.invalid/v1","model":"big-1","api_key":"sk-test",
                        "enabled":true})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let (s, r) = h
        .call(
            "PUT",
            "/api/usage/prices",
            Some(json!({"provider_id": p["id"], "model":"big-1",
                        "input_per_mtok": 2.0, "output_per_mtok": 2.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    let (_, summary) = h.call("GET", "/api/usage", None).await;
    assert_eq!(summary["unpriced"], json!([]), "no satellite, no speech");

    h.state
        .db
        .exec(
            "INSERT INTO satellites(id,user_id,name,token_hash,paired_at)
             VALUES('sat1',1,'PC','hash',1)",
            vec![],
        )
        .await
        .unwrap();

    let (_, summary) = h.call("GET", "/api/usage", None).await;
    assert_eq!(summary["unpriced"], json!(["Cloud (whisper-1)"]));
}

/// The refusal needs a budget that covers the call: nothing changes for a
/// cloud model on a setup that has none.
#[tokio::test]
async fn an_unpriced_cloud_model_runs_when_no_budget_covers_it() {
    let (addr, hits, _) = scripted_provider(|_| text_reply("Sure")).await;
    let h = harness_with(Some(addr)).await;
    h.use_provider(&format!("http://{CLOUD_HOST}:{}/v1", addr.port()))
        .await;
    let (s, r) = h
        .call(
            "POST",
            "/api/usage/budgets",
            Some(json!({"scope":"module","scope_id":"satellite","monthly_limit": 5.0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    let sse = h.chat("what's the weather").await;

    assert!(sse.contains("Sure"), "{sse}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// What the fast path turns down isn't lost: the model gets to read it.
#[tokio::test]
async fn a_sentence_that_is_not_a_whole_command_reaches_the_model() {
    let (addr, hits, _) = scripted_provider(|_| text_reply("Which ones?")).await;
    let h = harness().await;
    h.use_provider(&format!("http://{addr}/v1")).await;
    h.state
        .db
        .exec(
            "INSERT INTO entities(entity_id,name,domain,state,allowed,updated_at)
             VALUES('light.hall','Hall Light','light','on',1,1),
                   ('light.kitchen','Kitchen Light','light','on',1,1)",
            vec![],
        )
        .await
        .unwrap();

    for (n, text) in [
        "Turn off all the lights.",
        "Is the hall light on?",
        "Turn off the kitchen light, please.",
    ]
    .into_iter()
    .enumerate()
    {
        let sse = h.chat(text).await;
        assert!(sse.contains("Which ones?"), "{text}: {sse}");
        assert_eq!(hits.load(Ordering::SeqCst), n + 1, "{text}");
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
