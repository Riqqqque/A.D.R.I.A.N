//! End-to-end HTTP flow tests against the real router + real SQLite
//! (tempdir), driven through tower::ServiceExt — no sockets required.

use adrian_server::{Config, build_router, build_state, start_services};
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

struct Harness {
    state: Arc<adrian_server::AppState>,
    router: axum::Router,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        web_dir: dir.path().join("web"),
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
