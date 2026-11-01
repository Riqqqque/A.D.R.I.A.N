//! WebSocket satellite session tests: bearer auth, protocol envelopes,
//! binary PCM capture, and alarm delivery pushed to a connected satellite.

use adrian_protocol::{
    AlarmReplica, Envelope, LocalAlarmFiring, Message, PROTOCOL_VERSION, SessionEndReason,
};
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
        tls: None,
        trusted_proxy: None,
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

/// Next envelope, skipping alarm replica syncs (pushed on connect and after
/// every alarm change; see `recv_sync`).
async fn recv_env(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    timeout: Duration,
) -> Envelope {
    loop {
        let env = recv_any(ws, timeout).await;
        if !matches!(env.message, Message::AlarmReplicaSync { .. }) {
            return env;
        }
    }
}

/// Next alarm replica sync, skipping anything else.
async fn recv_sync(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    timeout: Duration,
) -> Vec<AlarmReplica> {
    loop {
        if let Message::AlarmReplicaSync { alarms } = recv_any(ws, timeout).await.message {
            return alarms;
        }
    }
}

async fn recv_any(
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

async fn revoke(h: &Harness) {
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "DELETE",
        &format!("/api/satellites/{}", h.satellite_id),
        None,
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
}

/// The server dropped this socket, and acked nothing on the way out.
async fn assert_session_ended(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => panic!("session still open 5 s after the revoke"),
            Ok(None | Some(Err(_)) | Some(Ok(WsMsg::Close(_)))) => return,
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                let env: Envelope = serde_json::from_str(&t).expect("envelope json");
                assert!(
                    !matches!(env.message, Message::Ack { .. }),
                    "acked after the revoke: {:?}",
                    env.message
                );
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

#[tokio::test]
async fn revoking_a_satellite_ends_its_live_session() {
    let h = harness().await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut ws, Duration::from_secs(10)).await;

    revoke(&h).await;

    // A device that carries on as if nothing happened gets nowhere.
    let status = Envelope::new(Message::Status {
        state: adrian_protocol::SatelliteState::Idle,
        detail: None,
    });
    let _ = send_env(&mut ws, status).await;
    assert_session_ended(&mut ws).await;
    assert!(!h.state.satellites.connected(&h.satellite_id));

    let err = ws_connect(h.addr, Some(&h.device_token)).await.unwrap_err();
    assert!(
        matches!(err, tokio_tungstenite::tungstenite::Error::Http(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn revoking_reaches_every_socket_on_the_token() {
    let h = harness().await;
    let mut first = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut first, Duration::from_secs(10)).await;
    let mut second = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut second, Duration::from_secs(10)).await;

    revoke(&h).await;

    assert_session_ended(&mut first).await;
    assert_session_ended(&mut second).await;
}

/// Send one short utterance and wait for the server to take it.
async fn send_utterance<S>(ws: &mut S)
where
    S: futures_util::Sink<WsMsg, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let start = Envelope::new(Message::AudioSessionStart {
        room: None,
        sample_rate: 16000,
    });
    send_env(ws, start).await.unwrap();
    let ack = recv_env(ws, Duration::from_secs(5)).await;
    assert!(matches!(ack.message, Message::Ack { ok: true, .. }));
    ws.send(WsMsg::Binary(vec![0u8; 3200].into()))
        .await
        .unwrap();
    let end = Envelope::new(Message::AudioSessionEnd {
        reason: SessionEndReason::Endpointed,
    });
    send_env(ws, end).await.unwrap();
    let ack = recv_env(ws, Duration::from_secs(5)).await;
    assert!(matches!(ack.message, Message::Ack { ok: true, .. }));
}

fn assert_turn_ack_refused(env: &Envelope) {
    assert_eq!(env.request_id, "turn", "{:?}", env.message);
    assert!(
        matches!(env.message, Message::Ack { ok: false, .. }),
        "{:?}",
        env.message
    );
}

/// Run an audio session on an open socket after `sql` takes the satellite's
/// rights away behind the hub's back, and expect no STT and no turn.
async fn assert_turn_refused(h: &Harness, sql: &str) {
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut ws, Duration::from_secs(10)).await;
    h.state.db.exec(sql, vec![]).await.unwrap();

    send_utterance(&mut ws).await;
    assert_turn_ack_refused(&recv_env(&mut ws, Duration::from_secs(15)).await);
}

#[tokio::test]
async fn revoked_satellite_cannot_run_a_turn_on_an_open_socket() {
    let h = harness().await;
    assert_turn_refused(&h, "UPDATE satellites SET revoked = 1").await;
}

#[tokio::test]
async fn disabled_owner_cannot_run_a_turn_on_an_open_socket() {
    let h = harness().await;
    assert_turn_refused(&h, "UPDATE users SET disabled_at = 1").await;
}

/// Start an STT that revokes the device while it runs; returns its address.
async fn revoking_stt(state: Arc<adrian_server::AppState>) -> SocketAddr {
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, stt).await.unwrap() });
    addr
}

#[tokio::test]
async fn satellite_revoked_during_stt_gets_no_turn() {
    let h = harness().await;
    let stt = revoking_stt(h.state.clone()).await;
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/integrations/providers",
        Some(json!({"kind": "openai_compatible", "label": "Test",
                    "base_url": format!("http://{stt}/v1"), "model": "m"})),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut ws, Duration::from_secs(10)).await;

    send_utterance(&mut ws).await;

    let heard = recv_env(&mut ws, Duration::from_secs(15)).await;
    assert!(
        matches!(heard.message, Message::Transcript { .. }),
        "{:?}",
        heard.message
    );
    assert_turn_ack_refused(&recv_env(&mut ws, Duration::from_secs(15)).await);
}

/// A provider that hears "set a timer", sets it, then fails the next round.
async fn timer_then_failing_provider() -> SocketAddr {
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/v1/audio/transcriptions",
            axum::routing::post(|_: axum::body::Bytes| async {
                axum::Json(json!({"text": "set a timer for ten minutes"}))
            }),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(move |_: axum::body::Bytes| {
                let rounds = rounds.clone();
                async move {
                    use axum::response::IntoResponse;
                    if rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                        return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
                    }
                    let args = json!({"in_minutes": 10}).to_string();
                    let call = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
                        "index":0,"id":"call_0",
                        "function":{"name":"set_alarm","arguments":args}}]}}]});
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        format!("data: {call}\n\ndata: [DONE]\n\n"),
                    )
                        .into_response()
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn a_failed_later_round_is_spoken_with_what_was_done() {
    let h = harness().await;
    let provider = timer_then_failing_provider().await;
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/integrations/providers",
        Some(json!({"kind": "openai_compatible", "label": "Test",
                    "base_url": format!("http://{provider}/v1"), "model": "m"})),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    satellite_endpoint(&h).await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    recv_sync(&mut ws, Duration::from_secs(10)).await;

    send_utterance(&mut ws).await;

    let spoken = loop {
        let env = recv_env(&mut ws, Duration::from_secs(15)).await;
        if let Message::Speak { text, .. } = env.message {
            break text.unwrap_or_default();
        }
    };
    assert!(spoken.contains("Timer set"), "{spoken}");
    assert!(!spoken.starts_with("error:"), "{spoken}");
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
    let Message::Speak {
        media_url, alarm, ..
    } = &speak.message
    else {
        panic!("expected speak, got {:?}", speak.message);
    };
    assert!(media_url.starts_with("/media/"));
    let alarm = alarm
        .as_ref()
        .expect("alarm deliveries carry the alarm ref");
    assert_eq!(alarm.alarm_id, "wsal");
    assert_eq!(alarm.occurrence_unix, now - 1);

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

/// Create a satellite endpoint for the harness satellite; returns its id.
async fn satellite_endpoint(h: &Harness) -> String {
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
    res["id"].as_str().unwrap().to_string()
}

async fn insert_alarm(h: &Harness, id: &str, due: i64, endpoint_id: &str) {
    h.state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,timezone,recurrence,next_due_utc,endpoint_id,status,created_at,updated_at)
             VALUES(?1,1,'wake up','alarm','UTC','none',?2,?3,'scheduled',1,1)",
            vec![
                adrian_server::db::P::T(id.into()),
                adrian_server::db::P::I(due),
                adrian_server::db::P::T(endpoint_id.into()),
            ],
        )
        .await
        .unwrap();
}

async fn delivery(h: &Harness, alarm_id: &str) -> Option<(String, String)> {
    h.state
        .db
        .q1(
            "SELECT status, detail FROM alarm_deliveries WHERE alarm_id = ?1",
            vec![adrian_server::db::P::T(alarm_id.into())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .await
        .unwrap()
}

async fn report_ack<S>(ws: &mut S, fired: Vec<LocalAlarmFiring>) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<WsMsg, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let env = Envelope::new(Message::AlarmLocalReport { fired });
    let rid = env.request_id.clone();
    send_env(ws, env).await.unwrap();
    loop {
        let e = recv_env(ws, Duration::from_secs(10)).await;
        if e.request_id == rid {
            let Message::Ack { ok, detail } = e.message else {
                panic!("expected ack, got {:?}", e.message);
            };
            assert!(ok, "{detail:?}");
            return detail.unwrap_or_default();
        }
    }
}

#[tokio::test]
async fn replicas_sync_on_connect_and_on_cancel() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    let due = adrian_server::util::now_unix() + 3600;
    insert_alarm(&h, "rep1", due, &ep).await;

    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let alarms = recv_sync(&mut ws, Duration::from_secs(10)).await;
    assert_eq!(alarms.len(), 1, "{alarms:?}");
    assert_eq!(alarms[0].alarm_id, "rep1");
    assert_eq!(alarms[0].occurrence_unix, due);
    assert_eq!(alarms[0].label, "wake up");

    // Canceling through the API pushes a new, empty set.
    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "DELETE",
        "/api/alarms/rep1",
        None,
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    let alarms = recv_sync(&mut ws, Duration::from_secs(10)).await;
    assert!(alarms.is_empty(), "{alarms:?}");
}

#[tokio::test]
async fn local_firing_corrects_a_failed_offline_delivery() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    let due = adrian_server::util::now_unix() - 1;
    insert_alarm(&h, "off1", due, &ep).await;
    h.state.scheduler.wake();

    // Satellite offline: the server records a failure.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some((status, _)) = delivery(&h, "off1").await
            && status != "delivering"
        {
            assert_eq!(status, "failed");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "delivery never finished"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // It rang locally; on reconnect it reports and the ledger is corrected.
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let detail = report_ack(
        &mut ws,
        vec![LocalAlarmFiring {
            alarm_id: "off1".into(),
            occurrence_unix: due,
            fired_at_unix: due,
        }],
    )
    .await;
    assert_eq!(detail, "1 recorded");
    let (status, detail) = delivery(&h, "off1").await.unwrap();
    assert_eq!(status, "played");
    assert!(detail.contains("locally"), "{detail}");
}

#[tokio::test]
async fn local_firing_before_server_delivery_advances_the_alarm() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    // Future due time: the server has not delivered it yet (as when it was
    // down at due time and the satellite reports first).
    let due = adrian_server::util::now_unix() + 3600;
    insert_alarm(&h, "early1", due, &ep).await;
    // An alarm routed elsewhere must not be touchable by this satellite.
    h.state
        .db
        .exec(
            "INSERT INTO endpoints(id,name,kind,config,created_at)
             VALUES('ep-other','Other','browser','{}',1)",
            vec![],
        )
        .await
        .unwrap();
    insert_alarm(&h, "other1", due, "ep-other").await;

    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let detail = report_ack(
        &mut ws,
        vec![
            LocalAlarmFiring {
                alarm_id: "early1".into(),
                occurrence_unix: due,
                fired_at_unix: due,
            },
            LocalAlarmFiring {
                alarm_id: "other1".into(),
                occurrence_unix: due,
                fired_at_unix: due,
            },
            // Unknown occurrence of a routed alarm: ignored.
            LocalAlarmFiring {
                alarm_id: "early1".into(),
                occurrence_unix: due + 60,
                fired_at_unix: due,
            },
        ],
    )
    .await;
    assert_eq!(detail, "1 recorded");
    let (status, _) = delivery(&h, "early1").await.unwrap();
    assert_eq!(status, "played");
    let alarm_status: String = h
        .state
        .db
        .q1("SELECT status FROM alarms WHERE id='early1'", vec![], |r| {
            r.get(0)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alarm_status, "done");
    assert!(delivery(&h, "other1").await.is_none());
}

/// Ring an alarm on the connected satellite and leave its ack outstanding:
/// the window in which someone can snooze, stop or cancel it.
async fn ring(
    h: &Harness,
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    id: &str,
    recurrence: &str,
    endpoint_id: &str,
) -> Envelope {
    h.state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,timezone,local_time,recurrence,next_due_utc,endpoint_id,status,created_at,updated_at)
             VALUES(?1,1,'wake up','alarm','UTC','07:00',?2,?3,?4,'scheduled',1,1)",
            vec![
                adrian_server::db::P::T(id.into()),
                adrian_server::db::P::T(recurrence.into()),
                adrian_server::db::P::I(adrian_server::util::now_unix() - 1),
                adrian_server::db::P::T(endpoint_id.into()),
            ],
        )
        .await
        .unwrap();
    h.state.scheduler.wake();
    let speak = recv_env(ws, Duration::from_secs(15)).await;
    assert!(
        matches!(speak.message, Message::Speak { .. }),
        "expected speak, got {:?}",
        speak.message
    );
    speak
}

/// The satellite finishes playing after the user already acted.
async fn late_ack<S>(ws: &mut S, speak: &Envelope)
where
    S: futures_util::Sink<WsMsg, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut ack = Envelope::new(Message::Ack {
        ok: true,
        detail: Some("played".into()),
    });
    ack.request_id = speak.request_id.clone();
    send_env(ws, ack).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn alarm_row(h: &Harness, id: &str) -> (String, Option<i64>) {
    h.state
        .db
        .q1(
            "SELECT status, next_due_utc FROM alarms WHERE id = ?1",
            vec![adrian_server::db::P::T(id.into())],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn snooze_while_ringing_survives_the_late_ack() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let speak = ring(&h, &mut ws, "snz", "none", &ep).await;

    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/alarms/snz/snooze",
        Some(json!({})),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    late_ack(&mut ws, &speak).await;

    let snoozed_to = res["alarm"]["next_due_utc"].as_i64();
    assert!(snoozed_to.is_some(), "{res}");
    assert_eq!(
        alarm_row(&h, "snz").await,
        ("scheduled".to_string(), snoozed_to)
    );
    assert_eq!(delivery(&h, "snz").await.unwrap().0, "canceled");
}

#[tokio::test]
async fn cancel_while_ringing_survives_the_late_ack() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let speak = ring(&h, &mut ws, "cnl", "daily", &ep).await;

    let router = build_router(h.state.clone());
    let (s, _, res) = json_request(
        &router,
        "DELETE",
        "/api/alarms/cnl",
        None,
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    late_ack(&mut ws, &speak).await;

    assert_eq!(alarm_row(&h, "cnl").await, ("canceled".to_string(), None));
    assert_eq!(delivery(&h, "cnl").await.unwrap().0, "canceled");
}

#[tokio::test]
async fn stop_ends_the_ringing_but_keeps_a_daily_schedule() {
    let h = harness().await;
    let ep = satellite_endpoint(&h).await;
    let mut ws = ws_connect(h.addr, Some(&h.device_token)).await.unwrap();
    let router = build_router(h.state.clone());
    let now = adrian_server::util::now_unix();

    let speak = ring(&h, &mut ws, "daily", "daily", &ep).await;
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/alarms/daily/stop",
        Some(json!({})),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    late_ack(&mut ws, &speak).await;
    let (status, due) = alarm_row(&h, "daily").await;
    assert_eq!(status, "scheduled");
    assert!(due.unwrap() > now);

    // A one-shot has nothing left to come back to.
    let speak = ring(&h, &mut ws, "once", "none", &ep).await;
    let (s, _, res) = json_request(
        &router,
        "POST",
        "/api/alarms/once/stop",
        Some(json!({})),
        Some(&h.cookie),
        Some(&h.csrf),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{res}");
    late_ack(&mut ws, &speak).await;
    assert_eq!(alarm_row(&h, "once").await, ("done".to_string(), None));
}
