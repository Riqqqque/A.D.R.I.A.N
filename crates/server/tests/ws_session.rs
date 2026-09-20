//! WebSocket satellite session tests: bearer auth, protocol envelopes,
//! binary PCM capture, and alarm delivery pushed to a connected satellite.

use adrian_protocol::{Envelope, Message, PROTOCOL_VERSION, SessionEndReason};
use adrian_server::{Config, build_router, build_state, start_services};
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

struct Harness {
    state: Arc<adrian_server::AppState>,
    addr: SocketAddr,
    cookie: String,
    csrf: String,
    device_token: String,
    satellite_id: String,
    _dir: tempfile::TempDir,
}

async fn json_request(
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
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Boot a real TCP server, claim setup, pair a satellite.
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

    // Claim setup over the in-process router.
    let token = std::fs::read_to_string(state.config.data_dir.join("SETUP_TOKEN.txt")).unwrap();
    let (_, headers, body) = json_request(
        &router,
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

    // Mint + redeem a pairing code.
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/satellites/pairing-code",
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let code = res["code"].as_str().unwrap().to_string();
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/satellite/pair",
        Some(json!({"code": code, "name": "Test PC"})),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let device_token = res["device_token"].as_str().unwrap().to_string();
    let satellite_id = res["satellite_id"].as_str().unwrap().to_string();

    // Serve on a real socket for the websocket upgrade.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );

    Harness {
        state,
        addr,
        cookie,
        csrf,
        device_token,
        satellite_id,
        _dir: dir,
    }
}

async fn ws_connect(
    addr: SocketAddr,
    token: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut req = format!("ws://{addr}/api/satellite/session")
        .into_client_request()
        .unwrap();
    if let Some(t) = token {
        req.headers_mut()
            .insert("Authorization", format!("Bearer {t}").parse().unwrap());
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(ws)
}

async fn recv_env(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    timeout: Duration,
) -> Envelope {
    loop {
        let msg = tokio::time::timeout(timeout, ws.next())
            .await
            .expect("timed out waiting for ws message")
            .expect("ws closed")
            .expect("ws error");
        if let WsMsg::Text(t) = msg {
            return serde_json::from_str(&t).expect("envelope json");
        }
    }
}

async fn send_env<S>(
    ws: &mut S,
    mut env: Envelope,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: futures_util::Sink<WsMsg, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    env.device_id = Some("test-sat".into());
    ws.send(WsMsg::Text(serde_json::to_string(&env).unwrap().into()))
        .await
}

#[tokio::test]
async fn session_requires_device_token() {
    let h = harness().await;
    // No auth header at all.
    let err = ws_connect(h.addr, None).await.unwrap_err();
    assert!(
        matches!(err, tokio_tungstenite::tungstenite::Error::Http(_)),
        "{err:?}"
    );
    // Bogus token.
    let err = ws_connect(h.addr, Some("sat_bogus")).await.unwrap_err();
    assert!(
        matches!(err, tokio_tungstenite::tungstenite::Error::Http(_)),
        "{err:?}"
    );
    // Valid token upgrades.
    let _ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
}

#[tokio::test]
async fn session_protocol_envelopes() {
    let h = harness().await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();

    // Wrong protocol version → nack.
    let mut env = Envelope::new(Message::Status {
        state: adrian_protocol::SatelliteState::Idle,
        detail: None,
    });
    env.protocol_version = PROTOCOL_VERSION + 99;
    let rid = env.request_id.clone();
    send_env(&mut ws, env).await.unwrap();
    let ack = recv_env(&mut ws, Duration::from_secs(5)).await;
    match ack.message {
        Message::Ack { ok, detail } => {
            assert!(!ok);
            assert!(detail.unwrap().contains("protocol version"));
        }
        m => panic!("expected ack, got {m:?}"),
    }
    assert_eq!(ack.request_id, rid);

    // Audio session lifecycle: start → binary frames → end → acks. STT is
    // unconfigured in the test env, so the turn resolves to a "stt failed"
    // ack rather than a Transcript/Speak pair — the point is the session
    // channel works end to end.
    let start = Envelope::new(Message::AudioSessionStart {
        room: None,
        sample_rate: 16000,
    });
    send_env(&mut ws, start).await.unwrap();
    let ack = recv_env(&mut ws, Duration::from_secs(5)).await;
    assert!(matches!(ack.message, Message::Ack { ok: true, .. }));

    ws.send(WsMsg::Binary(vec![0u8; 3200].into()))
        .await
        .unwrap();
    let end = Envelope::new(Message::AudioSessionEnd {
        reason: SessionEndReason::Endpointed,
    });
    send_env(&mut ws, end).await.unwrap();
    let ack = recv_env(&mut ws, Duration::from_secs(5)).await;
    assert!(matches!(ack.message, Message::Ack { ok: true, .. }));
    // The STT result (success or failure ack) arrives asynchronously.
    let after = recv_env(&mut ws, Duration::from_secs(15)).await;
    assert!(matches!(
        after.message,
        Message::Ack { .. } | Message::Transcript { .. } | Message::Speak { .. }
    ));

    // Idempotency: the same key twice → "duplicate ignored".
    let mut e1 = Envelope::new(Message::Status {
        state: adrian_protocol::SatelliteState::Idle,
        detail: Some("a".into()),
    });
    e1.idempotency_key = Some("k-dup".into());
    send_env(&mut ws, e1).await.unwrap();
    let _ = recv_env(&mut ws, Duration::from_secs(5)).await;
    let mut e2 = Envelope::new(Message::Status {
        state: adrian_protocol::SatelliteState::Idle,
        detail: Some("a".into()),
    });
    e2.idempotency_key = Some("k-dup".into());
    send_env(&mut ws, e2).await.unwrap();
    let ack = recv_env(&mut ws, Duration::from_secs(5)).await;
    match ack.message {
        Message::Ack { ok, detail } => {
            assert!(ok);
            assert_eq!(detail.as_deref(), Some("duplicate ignored"));
        }
        m => panic!("expected ack, got {m:?}"),
    }
}

#[tokio::test]
async fn satellite_alarm_is_delivered_over_ws() {
    let h = harness().await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let now = adrian_server::util::now_unix();

    // Endpoint pointing at our connected satellite + a due alarm, inserted
    // directly (API-level endpoint creation is covered by request tests).
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/endpoints",
        Some(json!({
            "name": "Sat",
            "kind": "satellite",
            "satellite_id": h.satellite_id,
        })),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let ep_id = res["id"].as_str().unwrap().to_string();

    h.state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,timezone,recurrence,next_due_utc,endpoint_id,status,created_at,updated_at)
             VALUES('wsal',1,'test alarm','alarm','UTC','none',?1,?2,'scheduled',1,1)",
            vec![
                adrian_server::db::P::I(now - 1),
                adrian_server::db::P::T(ep_id),
            ],
        )
        .await
        .unwrap();
    h.state.scheduler.wake();

    // The scheduler pushes a Speak envelope addressed to the delivery id.
    let speak = recv_env(&mut ws, Duration::from_secs(15)).await;
    let Message::Speak { media_url, .. } = &speak.message else {
        panic!("expected speak, got {:?}", speak.message);
    };
    assert!(media_url.starts_with("/media/"));

    // Ack it like the real satellite would.
    let mut ack = Envelope::new(Message::Ack {
        ok: true,
        detail: Some("played".into()),
    });
    ack.request_id = speak.request_id.clone();
    send_env(&mut ws, ack).await.unwrap();

    // Delivery must be recorded as played.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row: Option<(String,)> = h
            .state
            .db
            .q1(
                "SELECT status FROM alarm_deliveries WHERE alarm_id='wsal'",
                vec![],
                |r| Ok((r.get::<_, String>(0)?,)),
            )
            .await
            .unwrap();
        if let Some((status,)) = row
            && status != "delivering"
        {
            assert_eq!(status, "played");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "delivery never finished"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn satellite_alarm_offline_is_failed_not_silent() {
    let h = harness().await;
    let now = adrian_server::util::now_unix();
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/endpoints",
        Some(json!({
            "name": "Sat",
            "kind": "satellite",
            "satellite_id": h.satellite_id,
        })),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let ep_id = res["id"].as_str().unwrap().to_string();

    // No websocket connected → delivery must fail honestly.
    h.state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,timezone,recurrence,next_due_utc,endpoint_id,status,created_at,updated_at)
             VALUES('offal',1,'t','alarm','UTC','none',?1,?2,'scheduled',1,1)",
            vec![
                adrian_server::db::P::I(now - 1),
                adrian_server::db::P::T(ep_id),
            ],
        )
        .await
        .unwrap();
    h.state.scheduler.wake();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let row: Option<(String, String)> = h
            .state
            .db
            .q1(
                "SELECT status, detail FROM alarm_deliveries WHERE alarm_id='offal'",
                vec![],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .await
            .unwrap();
        if let Some((status, detail)) = row
            && status != "delivering"
        {
            assert_eq!(status, "failed");
            assert!(detail.contains("not connected"), "{detail}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "delivery never finished"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
