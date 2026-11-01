//! End-to-end HTTP flow tests against the real router + real SQLite
//! (tempdir), driven through tower::ServiceExt — no sockets required.

use adrian_server::audio::AudioCommand;
use adrian_server::{Config, build_router, build_state, start_services};
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Harness {
    state: Arc<adrian_server::AppState>,
    router: axum::Router,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    harness_with(None).await
}

async fn harness_with(trusted_proxy: Option<IpAddr>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        web_dir: dir.path().join("web"),
        tls: None,
        trusted_proxy,
    };
    let state = build_state(config).await.unwrap();
    start_services(&state).await.unwrap();
    let router = build_router(state.clone());
    Harness {
        state,
        router,
        _dir: dir,
    }
}

async fn request(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    request_with(router, method, path, body, cookie, csrf, &[]).await
}

async fn request_with(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
    csrf: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
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
    for (k, v) in extra_headers {
        b = b.header(*k, *v);
    }
    let req = b
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

fn setup_token(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("SETUP_TOKEN.txt")).unwrap()
}

#[tokio::test]
async fn setup_is_one_use_and_sessions_work() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);

    // Unauthenticated API access is denied.
    let (status, _, _) = request(&h.router, "GET", "/api/memory", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A bad token cannot claim setup.
    let (status, _, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": "wrong", "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The real token claims exactly once.
    let (status, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(
            json!({"token": token, "username": "owner", "password": "password123",
                    "timezone": "UTC", "privacy_mode": "hybrid"}),
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
    let csrf = body["csrf"].as_str().unwrap().to_string();

    // Replaying the token is rejected.
    let token2 = setup_token(&h.state.config.data_dir);
    let (status, _, _) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token2, "username": "second", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Session cookie authenticates.
    let (status, _, me) =
        request(&h.router, "GET", "/api/auth/me", None, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user"]["username"], "owner");

    // Mutations require CSRF.
    let (status, _, _) = request(
        &h.router,
        "POST",
        "/api/memory",
        Some(json!({"text": "secret fact"})),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(
        &h.router,
        "POST",
        "/api/memory",
        Some(json!({"text": "secret fact"})),
        Some(&cookie),
        Some("wrong-csrf"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // With CSRF, the write succeeds.
    let (status, _, _) = request(
        &h.router,
        "POST",
        "/api/memory",
        Some(json!({"text": "secret fact", "scope": "private"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn data_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_path_buf();
    let (cookie, csrf);
    {
        let config = Config {
            data_dir: data_dir.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().join("web"),
            tls: None,
            trusted_proxy: None,
        };
        let state = build_state(config).await.unwrap();
        start_services(&state).await.unwrap();
        let router = build_router(state.clone());
        let token = setup_token(&data_dir);
        let (_, headers, body) = request(
            &router,
            "POST",
            "/api/setup/claim",
            Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
            None,
            None,
        )
        .await;
        cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        csrf = body["csrf"].as_str().unwrap().to_string();
        let (s, _, _) = request(
            &router,
            "POST",
            "/api/memory",
            Some(json!({"text": "the boiler room key hangs by the door"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    } // first server instance dropped

    // Second instance over the same data dir: session + fact persist.
    let config = Config {
        data_dir: data_dir.clone(),
        bind: "127.0.0.1:0".parse().unwrap(),
        web_dir: dir.path().join("web"),
        tls: None,
        trusted_proxy: None,
    };
    let state = build_state(config).await.unwrap();
    start_services(&state).await.unwrap();
    let router = build_router(state);
    let (s, _, me) = request(&router, "GET", "/api/auth/me", None, Some(&cookie), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["username"], "owner");
    let (s, _, mem) = request(
        &router,
        "GET",
        "/api/memory?q=boiler",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        mem["facts"][0]["text"],
        "the boiler room key hangs by the door"
    );
}

#[tokio::test]
async fn alarms_refuse_without_endpoint_but_persist_with_one() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = body["csrf"].as_str().unwrap().to_string();

    // No endpoint configured -> honest refusal, nothing committed.
    let (s, _, e) = request(
        &h.router,
        "POST",
        "/api/alarms",
        Some(json!({"label": "x", "in_minutes": 60})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{e}");

    // Register a browser endpoint, then scheduling succeeds and is durable.
    let (s, _, ep) = request(
        &h.router,
        "POST",
        "/api/audio/register",
        Some(json!({"name": "test browser"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ep}");
    let endpoint_id = ep["endpoint_id"].as_str().unwrap();

    let (s, _, a) = request(
        &h.router,
        "POST",
        "/api/alarms",
        Some(json!({"label": "tea", "in_minutes": 30, "endpoint_id": endpoint_id})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["alarm"]["persisted"], true);
    let alarm_id = a["alarm"]["id"].as_str().unwrap().to_string();

    // It shows in the list and survives a router-level reload of state.
    let (s, _, list) = request(&h.router, "GET", "/api/alarms", None, Some(&cookie), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["alarms"].as_array().unwrap().len(), 1);

    // Cancel is durable.
    let (s, _, _) = request(
        &h.router,
        "DELETE",
        &format!("/api/alarms/{alarm_id}"),
        None,
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn backup_file_is_a_valid_database() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = body["csrf"].as_str().unwrap().to_string();

    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/backup",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let path = res["backup"].as_str().unwrap();

    // The backup must be a real, readable SQLite database with our schema.
    let conn = rusqlite::Connection::open(path).unwrap();
    let users: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(users, 1);
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('alarms','facts','sessions')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 3);
}

#[tokio::test]
async fn satellite_pair_status_and_revoke() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = body["csrf"].as_str().unwrap().to_string();

    // Mint a pairing code (owner + CSRF).
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellites/pairing-code",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let code = res["code"].as_str().unwrap().to_string();

    // Redeem it — no session needed, the code is the credential.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellite/pair",
        Some(json!({"code": code, "name": "Test PC"})),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let device_token = res["device_token"].as_str().unwrap().to_string();
    let sat_id = res["satellite_id"].as_str().unwrap().to_string();
    assert!(device_token.starts_with("sat_"));

    // One-use: redeeming again is forbidden.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellite/pair",
        Some(json!({"code": code, "name": "Second PC"})),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{res}");

    // Bearer-authed status check.
    let bearer_req = |path: &str| {
        Request::builder()
            .method("GET")
            .uri(path)
            .extension(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))))
            .header(header::AUTHORIZATION, format!("Bearer {device_token}"))
            .body(Body::empty())
            .unwrap()
    };
    let res = h
        .router
        .clone()
        .oneshot(bearer_req("/api/satellite/status"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Device list shows the paired satellite.
    let (s, _, res) = request(
        &h.router,
        "GET",
        "/api/satellites",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(res["satellites"][0]["id"].as_str().unwrap(), sat_id);

    // Utterance with a wrong token is rejected before touching providers.
    let bad = Request::builder()
        .method("POST")
        .uri("/api/satellite/utterance")
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))))
        .header(header::AUTHORIZATION, "Bearer bogus")
        .header(header::CONTENT_TYPE, "audio/wav")
        .body(Body::from(vec![0u8; 1024]))
        .unwrap();
    let res = h.router.clone().oneshot(bad).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Revoke, then the token must be dead.
    let (s, _, res) = request(
        &h.router,
        "DELETE",
        &format!("/api/satellites/{sat_id}"),
        None,
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let res = h
        .router
        .clone()
        .oneshot(bearer_req("/api/satellite/status"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pair_is_rate_limited_and_one_use_is_atomic() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = body["csrf"].as_str().unwrap().to_string();

    // Redeeming a minted code is a race — exactly one concurrent attempt wins.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellites/pairing-code",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let code = res["code"].as_str().unwrap().to_string();

    let results = futures_util::future::join_all((0..6).map(|_| {
        request(
            &h.router,
            "POST",
            "/api/satellite/pair",
            Some(json!({"code": code, "name": "racer"})),
            None,
            None,
        )
    }))
    .await;
    let oks = results
        .iter()
        .filter(|(s, _, _)| *s == StatusCode::OK)
        .count();
    assert_eq!(oks, 1, "exactly one redemption may succeed: {results:?}");
    for (s, _, res) in results.iter().filter(|(s, _, _)| *s != StatusCode::OK) {
        assert_eq!(*s, StatusCode::FORBIDDEN, "{res}");
    }
    let winner = results
        .iter()
        .find(|(s, _, _)| *s == StatusCode::OK)
        .unwrap();
    let device_token = winner.2["device_token"].as_str().unwrap().to_string();

    // The utterance route lifts axum's 2MB default body limit for WAVs:
    // a >2MB body reaches the handler (fails later on STT config, not 413).
    let big = Request::builder()
        .method("POST")
        .uri("/api/satellite/utterance")
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))))
        .header(header::AUTHORIZATION, format!("Bearer {device_token}"))
        .header(header::CONTENT_TYPE, "audio/wav")
        .body(Body::from(vec![0u8; 3 * 1024 * 1024]))
        .unwrap();
    let res = h.router.clone().oneshot(big).await.unwrap();
    assert_eq!(res.status(), StatusCode::PRECONDITION_FAILED);

    // Bad-code attempts are throttled per IP: 10/min, then 429.
    for _ in 0..4 {
        let (s, _, _) = request(
            &h.router,
            "POST",
            "/api/satellite/pair",
            Some(json!({"code": "000000", "name": "x"})),
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellite/pair",
        Some(json!({"code": "000000", "name": "x"})),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{res}");
}

/// Pair a satellite; returns its device token and id.
async fn pair_satellite(h: &Harness, cookie: &str, csrf: &str) -> (String, String) {
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellites/pairing-code",
        Some(json!({})),
        Some(cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let code = res["code"].as_str().unwrap().to_string();
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/satellite/pair",
        Some(json!({"code": code, "name": "Test PC"})),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    (
        res["device_token"].as_str().unwrap().to_string(),
        res["satellite_id"].as_str().unwrap().to_string(),
    )
}

fn utterance_request(token: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/satellite/utterance")
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "audio/wav")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn utterance_rejects_a_bad_token_without_reading_the_body() {
    let h = harness().await;
    let stalled = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
    let call = h
        .router
        .clone()
        .oneshot(utterance_request("bogus", Body::from_stream(stalled)));
    let res = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("waited for the body of an unauthenticated caller")
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn utterance_body_stays_limited() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (token, _) = pair_satellite(&h, &cookie, &csrf).await;
    let oversized = Body::from(vec![0u8; 25 * 1024 * 1024 + 1]);
    let res = h
        .router
        .clone()
        .oneshot(utterance_request(&token, oversized))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn utterance_revoked_during_upload_never_reaches_stt() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (token, sat_id) = pair_satellite(&h, &cookie, &csrf).await;

    // The token is good when the request starts; the audio arrives later.
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    let body = futures_util::stream::once(async move {
        let _ = gate.await;
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![0u8; 1024]))
    });
    let call = tokio::spawn(
        h.router
            .clone()
            .oneshot(utterance_request(&token, Body::from_stream(body))),
    );

    // The handler stamps last_seen once the token has passed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen: Option<i64> = h
            .state
            .db
            .q1(
                "SELECT last_seen FROM satellites WHERE id = ?1",
                vec![adrian_server::db::P::T(sat_id.clone())],
                |r| r.get(0),
            )
            .await
            .unwrap()
            .flatten();
        if seen.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the token was not checked before the body"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    h.state
        .db
        .exec("UPDATE satellites SET revoked = 1", vec![])
        .await
        .unwrap();
    release.send(()).unwrap();

    // No provider is configured, so getting as far as STT would give 412.
    let res = call.await.unwrap().unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ha_status_is_flat_and_configured_comes_from_the_saved_row() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;

    let (s, _, ha) = request(
        &h.router,
        "GET",
        "/api/integrations/ha",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ha}");
    assert_eq!(ha["configured"], false, "{ha}");
    assert_eq!(ha["connected"], false, "{ha}");

    // Nothing listens on this port: saved, but not connected.
    let (s, _, res) = request(
        &h.router,
        "PUT",
        "/api/integrations/ha",
        Some(json!({"url": "http://127.0.0.1:1", "token": "0123456789abcdef"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let (s, _, ha) = request(
        &h.router,
        "GET",
        "/api/integrations/ha",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ha}");
    assert_eq!(ha["configured"], true, "{ha}");
    assert_eq!(ha["url"], "http://127.0.0.1:1", "{ha}");
    assert_eq!(ha["has_token"], true, "{ha}");
    assert_eq!(ha["enabled"], true, "{ha}");
    assert_eq!(ha["connected"], false, "{ha}");
    assert!(ha.get("connection").is_none(), "{ha}");
    assert!(ha.get("live").is_none(), "{ha}");
}

#[tokio::test]
async fn utterance_revoked_during_stt_gets_no_turn() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (token, _) = pair_satellite(&h, &cookie, &csrf).await;
    let state = h.state.clone();
    let stt = axum::Router::new().route(
        "/v1/audio/transcriptions",
        axum::routing::post(move |_: axum::body::Bytes| {
            let state = state.clone();
            async move {
                state
                    .db
                    .exec("UPDATE satellites SET revoked = 1", vec![])
                    .await
                    .unwrap();
                axum::Json(json!({"text": "turn off the lights"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, stt).await.unwrap() });
    add_provider(&h, &cookie, &csrf, &format!("http://{addr}/v1")).await;

    let wav = utterance_request(&token, Body::from(vec![0u8; 1024]));
    let res = h.router.clone().oneshot(wav).await.unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(count(&h, "SELECT COUNT(*) FROM conversations").await, 0);
}

async fn set_strict_local(h: &Harness, cookie: &str, csrf: &str) {
    let (s, _, res) = request(
        &h.router,
        "PUT",
        "/api/settings",
        Some(json!({"privacy_mode": "strict_local"})),
        Some(cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
}

async fn add_provider(h: &Harness, cookie: &str, csrf: &str, base_url: &str) -> String {
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/integrations/providers",
        Some(json!({"kind": "openai_compatible", "label": "Test",
                    "base_url": base_url, "model": "m"})),
        Some(cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    res["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn strict_local_refuses_to_test_a_cloud_provider() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    set_strict_local(&h, &cookie, &csrf).await;

    let cloud = add_provider(&h, &cookie, &csrf, "https://203.0.113.5/v1").await;
    let (s, _, res) = request(
        &h.router,
        "POST",
        &format!("/api/integrations/providers/{cloud}/test"),
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{res}");

    // A local endpoint is still tested; nothing listens, so it just fails.
    let local = add_provider(&h, &cookie, &csrf, "http://127.0.0.1:1/v1").await;
    let (s, _, res) = request(
        &h.router,
        "POST",
        &format!("/api/integrations/providers/{local}/test"),
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(res["ok"], false, "{res}");
}

#[tokio::test]
async fn strict_local_voice_blocks_public_hosts_but_reports_a_missing_provider() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (token, _) = pair_satellite(&h, &cookie, &csrf).await;
    set_strict_local(&h, &cookie, &csrf).await;

    // No provider yet: that is the problem to report, not a privacy refusal.
    let wav = || utterance_request(&token, Body::from(vec![0u8; 1024]));
    let res = h.router.clone().oneshot(wav()).await.unwrap();
    assert_eq!(res.status(), StatusCode::PRECONDITION_FAILED);

    add_provider(&h, &cookie, &csrf, "https://[2001:db8::1]/v1").await;
    let res = h.router.clone().oneshot(wav()).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// The address `request` connects from.
const PEER: &str = "127.0.0.1";

fn set_cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

async fn bad_login_from(h: &Harness, forwarded_for: &str) -> StatusCode {
    let (s, _, _) = request_with(
        &h.router,
        "POST",
        "/api/auth/login",
        Some(json!({"username": "nobody", "password": "wrong-password"})),
        None,
        None,
        &[("x-forwarded-for", forwarded_for)],
    )
    .await;
    s
}

#[tokio::test]
async fn login_limit_is_per_client_behind_the_trusted_proxy() {
    let h = harness_with(Some(PEER.parse().unwrap())).await;
    for _ in 0..10 {
        assert_eq!(
            bad_login_from(&h, "10.0.0.5").await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        bad_login_from(&h, "10.0.0.5").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // One noisy client no longer locks out everyone else...
    assert_eq!(
        bad_login_from(&h, "10.0.0.6").await,
        StatusCode::UNAUTHORIZED
    );
    // ...and the proxy's own entry is the one that counts, so a client
    // cannot dodge its limit by sending a list of its own.
    assert_eq!(
        bad_login_from(&h, "198.51.100.7, 10.0.0.5").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn claim_and_pair_limits_are_per_client_behind_the_trusted_proxy() {
    let h = harness_with(Some(PEER.parse().unwrap())).await;
    // A one-letter username fails claim before the slow password hash.
    let claim = json!({"token": "x", "username": "o", "password": "x"});
    let pair = json!({"code": "000000", "name": "x"});
    for (path, body, limit, refused) in [
        ("/api/setup/claim", claim, 8, StatusCode::BAD_REQUEST),
        ("/api/satellite/pair", pair, 10, StatusCode::FORBIDDEN),
    ] {
        let from = |ip: &'static str| {
            let body = body.clone();
            let router = h.router.clone();
            async move {
                let forwarded = [("x-forwarded-for", ip)];
                let (s, _, _) =
                    request_with(&router, "POST", path, Some(body), None, None, &forwarded).await;
                s
            }
        };
        for _ in 0..limit {
            assert_eq!(from("10.0.0.5").await, refused, "{path}");
        }
        assert_eq!(
            from("10.0.0.5").await,
            StatusCode::TOO_MANY_REQUESTS,
            "{path}"
        );
        assert_eq!(from("10.0.0.6").await, refused, "{path}");
    }
}

#[tokio::test]
async fn forwarded_headers_are_ignored_from_anyone_but_the_trusted_proxy() {
    for trusted in [None, Some("10.9.9.9".parse().unwrap())] {
        let h = harness_with(trusted).await;
        for i in 0..10 {
            assert_eq!(
                bad_login_from(&h, &format!("10.0.0.{i}")).await,
                StatusCode::UNAUTHORIZED,
                "{trusted:?}"
            );
        }
        assert_eq!(
            bad_login_from(&h, "10.0.1.1").await,
            StatusCode::TOO_MANY_REQUESTS,
            "a rotating header must not reset the limit: {trusted:?}"
        );

        let token = setup_token(&h.state.config.data_dir);
        let (s, headers, body) = request_with(
            &h.router,
            "POST",
            "/api/setup/claim",
            Some(json!({"token": token, "username": "owner", "password": "password123"})),
            None,
            None,
            &[("x-forwarded-proto", "https")],
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(
            !set_cookie(&headers).contains("; Secure"),
            "{trusted:?}: a Secure cookie from a plain-http login is dropped by the browser"
        );
    }
}

#[tokio::test]
async fn session_cookie_is_secure_when_the_trusted_proxy_saw_https() {
    let h = harness_with(Some(PEER.parse().unwrap())).await;
    let https = [("x-forwarded-proto", "https")];
    let login = json!({"username": "owner", "password": "password123"});

    let token = setup_token(&h.state.config.data_dir);
    let (s, headers, body) = request_with(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123"})),
        None,
        None,
        &https,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(set_cookie(&headers).contains("; Secure"));

    let (s, headers, body) = request_with(
        &h.router,
        "POST",
        "/api/auth/login",
        Some(login.clone()),
        None,
        None,
        &https,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let cookie = set_cookie(&headers);
    assert!(cookie.contains("; Secure"), "{cookie}");
    let session = cookie.split(';').next().unwrap().to_string();
    let csrf = body["csrf"].as_str().unwrap();

    // Plain http through the same proxy: a Secure cookie would be dropped.
    let (s, headers, body) = request(
        &h.router,
        "POST",
        "/api/auth/login",
        Some(login),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(!set_cookie(&headers).contains("; Secure"));

    let (s, headers, body) = request_with(
        &h.router,
        "POST",
        "/api/auth/logout",
        None,
        Some(&session),
        Some(csrf),
        &https,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(set_cookie(&headers).contains("; Secure"));
}

#[tokio::test]
async fn backup_restore_roundtrip() {
    let h = harness().await;
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = body["csrf"].as_str().unwrap().to_string();

    async fn marker_is(state: &adrian_server::AppState, v: &str) -> bool {
        state
            .db
            .q1(
                "SELECT value FROM settings WHERE key = 'restore_marker'",
                vec![],
                |r| r.get::<_, String>(0),
            )
            .await
            .unwrap()
            .map(|s| s == v)
            .unwrap_or(false)
    }

    h.state
        .db
        .exec(
            "INSERT OR REPLACE INTO settings(key, value) VALUES('restore_marker','original')",
            vec![],
        )
        .await
        .unwrap();

    // Owner-only backup.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/backup",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let backup_path = res["backup"].as_str().unwrap().to_string();
    let file_name = std::path::Path::new(&backup_path)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Mutate the db after the backup.
    h.state
        .db
        .exec(
            "UPDATE settings SET value='changed' WHERE key='restore_marker'",
            vec![],
        )
        .await
        .unwrap();
    assert!(marker_is(&h.state, "changed").await);

    // Path traversal is rejected before touching the filesystem.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/restore",
        Some(json!({"file": "../../adrian.db"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{res}");
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/restore",
        Some(json!({"file": "nope.db"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{res}");

    // Real restore swaps the live db on the worker thread.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/restore",
        Some(json!({"file": file_name})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert!(marker_is(&h.state, "original").await);

    // The worker keeps serving queries on the restored connection.
    let (s, _, res) = request(
        &h.router,
        "GET",
        "/api/diagnostics",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
}

/// Claim setup; returns the owner's session cookie and CSRF token.
async fn claim_owner(h: &Harness) -> (String, String) {
    let token = setup_token(&h.state.config.data_dir);
    let (_, headers, body) = request(
        &h.router,
        "POST",
        "/api/setup/claim",
        Some(json!({"token": token, "username": "owner", "password": "password123", "timezone": "UTC"})),
        None,
        None,
    )
    .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    (cookie, body["csrf"].as_str().unwrap().to_string())
}

/// Register a browser speaker; returns its endpoint and channel ids.
async fn browser_endpoint(h: &Harness, cookie: &str, csrf: &str) -> (String, String) {
    let (s, _, ep) = request(
        &h.router,
        "POST",
        "/api/audio/register",
        Some(json!({"name": "test browser"})),
        Some(cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ep}");
    (
        ep["endpoint_id"].as_str().unwrap().to_string(),
        ep["channel_id"].as_str().unwrap().to_string(),
    )
}

async fn insert_alarm(h: &Harness, id: &str, status: &str, due: i64, endpoint_id: &str) {
    h.state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,timezone,recurrence,next_due_utc,endpoint_id,status,created_at,updated_at)
             VALUES(?1,1,'tea','alarm','UTC','none',?2,?3,?4,1,1)",
            vec![
                adrian_server::db::P::T(id.into()),
                adrian_server::db::P::I(due),
                adrian_server::db::P::T(endpoint_id.into()),
                adrian_server::db::P::T(status.into()),
            ],
        )
        .await
        .unwrap();
}

async fn count(h: &Harness, sql: &str) -> i64 {
    h.state
        .db
        .q1(sql, vec![], |r| r.get(0))
        .await
        .unwrap()
        .unwrap()
}

/// Wait for the next Play sent to a browser channel; returns its delivery id.
async fn next_play(rx: &mut tokio::sync::broadcast::Receiver<AudioCommand>) -> String {
    let cmd = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("timed out waiting for the play command")
        .unwrap();
    let AudioCommand::Play { delivery_id, .. } = cmd else {
        panic!("expected play, got {cmd:?}");
    };
    delivery_id
}

/// Ring an alarm on a browser endpoint, ack it with `ack_body` (the delivery
/// id is added), and return the delivery outcome once recorded.
async fn browser_delivery_outcome(h: &Harness, ack_body: Value) -> (String, String) {
    let (cookie, csrf) = claim_owner(h).await;
    let (endpoint_id, channel_id) = browser_endpoint(h, &cookie, &csrf).await;
    let mut rx = h.state.audio.attach_channel(&channel_id);
    let due = adrian_server::util::now_unix() - 1;
    insert_alarm(h, "brw", "scheduled", due, &endpoint_id).await;
    h.state.scheduler.wake();
    let delivery_id = next_play(&mut rx).await;

    let mut body = ack_body;
    body["delivery_id"] = json!(delivery_id);
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/audio/ack",
        Some(body),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row = h
            .state
            .db
            .q1(
                "SELECT status, detail FROM alarm_deliveries WHERE alarm_id='brw'",
                vec![],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .await
            .unwrap();
        if let Some(row) = row
            && row.0 != "delivering"
        {
            return row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "delivery never finished"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn browser_playback_failure_is_recorded_as_failed() {
    let h = harness().await;
    let (status, detail) = browser_delivery_outcome(
        &h,
        json!({"ok": false, "result": "playback blocked: NotAllowedError"}),
    )
    .await;
    assert_eq!(status, "failed");
    assert!(detail.contains("NotAllowedError"), "{detail}");

    let alarms = adrian_server::scheduler::list_for(&h.state.db, 1, true)
        .await
        .unwrap();
    assert_eq!(alarms[0]["last_delivery_status"], "failed");
}

/// A tab still running the bundle from before `ok` existed reports success.
#[tokio::test]
async fn browser_ack_without_ok_counts_as_played() {
    let h = harness().await;
    let (status, _) = browser_delivery_outcome(&h, json!({"result": "played"})).await;
    assert_eq!(status, "played");
}

#[tokio::test]
async fn browser_test_button_reports_playback_failure() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (endpoint_id, channel_id) = browser_endpoint(&h, &cookie, &csrf).await;
    let mut rx = h.state.audio.attach_channel(&channel_id);

    let test = tokio::spawn({
        let (router, cookie, csrf) = (h.router.clone(), cookie.clone(), csrf.clone());
        async move {
            request(
                &router,
                "POST",
                &format!("/api/endpoints/{endpoint_id}/test"),
                Some(json!({})),
                Some(&cookie),
                Some(&csrf),
            )
            .await
        }
    });
    let delivery_id = next_play(&mut rx).await;
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/audio/ack",
        Some(json!({"delivery_id": delivery_id, "ok": false, "result": "playback blocked"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let (s, _, res) = test.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(res["ok"], false, "{res}");
    assert!(
        res["detail"].as_str().unwrap().contains("playback blocked"),
        "{res}"
    );
}

#[tokio::test]
async fn used_endpoint_is_removable_once_no_alarm_is_waiting_on_it() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (endpoint_id, _) = browser_endpoint(&h, &cookie, &csrf).await;
    let far = adrian_server::util::now_unix() + 3600;
    insert_alarm(&h, "old", "done", far, &endpoint_id).await;
    insert_alarm(&h, "next", "scheduled", far, &endpoint_id).await;
    let path = format!("/api/endpoints/{endpoint_id}");

    // A pending alarm still needs it: refused, and nothing changes.
    let (s, _, res) = request(&h.router, "DELETE", &path, None, Some(&cookie), Some(&csrf)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{res}");
    assert!(
        res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 pending"),
        "{res}"
    );
    assert_eq!(count(&h, "SELECT COUNT(*) FROM endpoints").await, 1);

    // With only finished alarms left, it goes and they let go of it.
    let (s, _, res) = request(
        &h.router,
        "DELETE",
        "/api/alarms/next",
        None,
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let (s, _, res) = request(&h.router, "DELETE", &path, None, Some(&cookie), Some(&csrf)).await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(count(&h, "SELECT COUNT(*) FROM endpoints").await, 0);
    assert_eq!(
        count(
            &h,
            "SELECT COUNT(*) FROM alarms WHERE endpoint_id IS NOT NULL"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn unregistered_browser_stops_being_the_default_alarm_target() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (old_id, old_channel) = browser_endpoint(&h, &cookie, &csrf).await;
    let (new_id, _) = browser_endpoint(&h, &cookie, &csrf).await;
    // Oldest enabled endpoint is the default target.
    h.state
        .db
        .exec(
            "UPDATE endpoints SET created_at = 1 WHERE id = ?1",
            vec![adrian_server::db::P::T(old_id.clone())],
        )
        .await
        .unwrap();
    let scheduled_on = async |cookie: &str, csrf: &str| {
        let (s, _, res) = request(
            &h.router,
            "POST",
            "/api/alarms",
            Some(json!({"in_minutes": 30})),
            Some(cookie),
            Some(csrf),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{res}");
        let (_, _, list) = request(&h.router, "GET", "/api/alarms", None, Some(cookie), None).await;
        let id = res["alarm"]["id"].clone();
        let alarms = list["alarms"].as_array().unwrap();
        let alarm = alarms.iter().find(|a| a["id"] == id).unwrap();
        alarm["endpoint_id"].as_str().unwrap().to_string()
    };
    assert_eq!(scheduled_on(&cookie, &csrf).await, old_id);

    // Without the tab's own channel id nothing happens.
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/audio/unregister",
        Some(json!({"endpoint_id": old_id, "channel_id": "not-the-channel"})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(scheduled_on(&cookie, &csrf).await, old_id);

    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/audio/unregister",
        Some(json!({"endpoint_id": old_id, "channel_id": old_channel})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(scheduled_on(&cookie, &csrf).await, new_id);
}

#[tokio::test]
async fn alarm_list_reports_the_snooze_length() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    browser_endpoint(&h, &cookie, &csrf).await;
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/alarms",
        Some(json!({"in_minutes": 30})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let (s, _, list) = request(&h.router, "GET", "/api/alarms", None, Some(&cookie), None).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["alarms"][0]["snooze_minutes"], 9, "{list}");
}

/// A restore rewinds the alarms to when the backup was taken. Nothing is
/// delivering them any more, and the executor is asleep on the old schedule.
#[tokio::test]
async fn restore_ends_a_ring_from_the_backup_and_wakes_the_executor() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (endpoint_id, _) = browser_endpoint(&h, &cookie, &csrf).await;
    let due = adrian_server::util::now_unix() - 1;

    // Nothing has woken the executor since boot, so these stay untouched.
    insert_alarm(&h, "due", "scheduled", due, &endpoint_id).await;
    insert_alarm(&h, "mid-ring", "ringing", due, &endpoint_id).await;
    h.state
        .db
        .exec(
            "INSERT INTO alarm_deliveries(id,alarm_id,occurrence_utc,execution_id,status,started_at)
             VALUES('d','mid-ring',?1,'e','delivering',?1)",
            vec![adrian_server::db::P::I(due)],
        )
        .await
        .unwrap();
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/backup",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let backup = std::path::Path::new(res["backup"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    h.state
        .db
        .exec(
            "UPDATE alarms SET status='canceled', next_due_utc=NULL",
            vec![],
        )
        .await
        .unwrap();

    let mut events = h.state.events.subscribe();
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/diagnostics/restore",
        Some(json!({"file": backup})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let row = h
        .state
        .db
        .q1(
            "SELECT a.status, a.next_due_utc, d.status, d.detail
             FROM alarms a JOIN alarm_deliveries d ON d.alarm_id = a.id
             WHERE a.id = 'mid-ring'",
            vec![],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.0.as_str(), row.1), ("done", None));
    assert_eq!(row.2, "failed");
    assert!(row.3.contains("database restore"), "{}", row.3);

    // No wake and this would wait out the executor's sleep.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while count(
        &h,
        "SELECT COUNT(*) FROM alarm_deliveries WHERE alarm_id = 'due'",
    )
    .await
        == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the restored alarm was not delivered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Satellites resync on alarm events; their replicas are from before.
    let restored = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = events.recv().await.unwrap();
            if ev.kind == "alarm" && ev.data["event"] == "restored" {
                return;
            }
        }
    });
    restored.await.expect("no alarm event after the restore");
}

#[tokio::test]
async fn clearing_public_url_falls_back_to_the_server_address() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let put = async |url: &str| {
        let (s, _, res) = request(
            &h.router,
            "PUT",
            "/api/settings",
            Some(json!({"public_url": url})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{res}");
    };

    put("http://speakers.test/").await;
    assert_eq!(h.state.public_base().await.unwrap(), "http://speakers.test");

    put("").await;
    let (_, _, res) = request(&h.router, "GET", "/api/settings", None, Some(&cookie), None).await;
    assert_eq!(res["settings"]["public_url"], "", "{res}");
    assert_eq!(
        h.state.public_base().await.unwrap(),
        h.state.config.base_url()
    );
}

/// The channel id is what proves a browser owns its endpoint.
#[tokio::test]
async fn endpoint_list_keeps_browser_channels_private() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (endpoint_id, channel_id) = browser_endpoint(&h, &cookie, &csrf).await;

    let (s, _, res) = request(
        &h.router,
        "GET",
        "/api/endpoints",
        None,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    assert_eq!(res["endpoints"][0]["id"], endpoint_id.as_str(), "{res}");
    assert!(!res.to_string().contains(&channel_id), "{res}");
}

/// Left enabled, the dead satellite would stay the default alarm target.
#[tokio::test]
async fn revoking_a_satellite_retires_its_endpoint() {
    let h = harness().await;
    let (cookie, csrf) = claim_owner(&h).await;
    let (_, sat_id) = pair_satellite(&h, &cookie, &csrf).await;
    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/endpoints",
        Some(json!({"name": "Desk", "kind": "satellite", "satellite_id": sat_id})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let (s, _, res) = request(
        &h.router,
        "DELETE",
        &format!("/api/satellites/{sat_id}"),
        None,
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");

    let (s, _, res) = request(
        &h.router,
        "POST",
        "/api/alarms",
        Some(json!({"in_minutes": 30})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{res}");
}
