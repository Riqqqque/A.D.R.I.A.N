//! The connection to the A.D.R.I.A.N. service: audio both ways, the window's
//! state, and PC actions. Reconnects by itself (1, 2, 5, then every 10 s,
//! starting over after a connection that held).

use crate::audio::{Shared, Up};
use adrian_pc_control::Pc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// How long a welcomed connection has to last to count as working. One that
/// ends sooner keeps backing off: a server in trouble, or another PC on the
/// same token, since the server keeps one connection per device.
const HELD: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(30)
};

/// What the window shows about the connection and her state.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Status {
        connected: bool,
        text: String,
        owner: Option<String>,
    },
    Thinking(bool),
    Transcript {
        who: String,
        text: String,
        last: bool,
    },
}

pub enum Cmd {
    Text(String),
    Stop,
    Ptt,
}

pub struct Link {
    pub url: String,
    pub token: String,
    pub device: String,
}

pub async fn run(
    link: watch::Receiver<Option<Link>>,
    mut up: mpsc::Receiver<Up>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    shared: Arc<Shared>,
    pc: Arc<Mutex<Arc<Pc>>>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut link = link;
    let mut backoff = 0usize;
    loop {
        let target = link
            .borrow_and_update()
            .as_ref()
            .map(|l| (l.url.clone(), l.token.clone(), l.device.clone()));
        let Some((url, token, device)) = target else {
            let _ = events.send(Event::Status {
                connected: false,
                text: "Not set up: open settings.".into(),
                owner: None,
            });
            if link.changed().await.is_err() {
                return;
            }
            continue;
        };
        let _ = events.send(Event::Status {
            connected: false,
            text: "Connecting…".into(),
            owner: None,
        });
        let mut welcomed = false;
        let started = Instant::now();
        let outcome = tokio::select! {
            r = session(&url, &token, &device, &mut up, &mut cmds, &shared, &pc, &events, &mut welcomed) => r,
            _ = link.changed() => { backoff = 0; continue; }
        };
        if welcomed && started.elapsed() >= HELD {
            backoff = 0;
        }
        let (text, retry) = match outcome {
            Ok(()) => ("Disconnected. Reconnecting…".to_string(), true),
            Err(Fail::Denied(r)) => (
                format!("The server refused this PC ({r}). Check the token in settings."),
                false,
            ),
            Err(Fail::Other(e)) => (format!("Can't reach the server: {e}"), true),
        };
        shared.stop_reply();
        shared.clear_state();
        let _ = events.send(Event::Thinking(false));
        let _ = events.send(Event::Status {
            connected: false,
            text,
            owner: None,
        });
        if !retry {
            // Wait for new settings rather than hammering the lockout.
            if link.changed().await.is_err() {
                return;
            }
            backoff = 0;
            continue;
        }
        let wait = next_wait(&mut backoff);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            _ = link.changed() => { backoff = 0; }
        }
    }
}

/// Seconds to wait before the next try.
fn next_wait(backoff: &mut usize) -> u64 {
    let wait = [1, 2, 5, 10][(*backoff).min(3)];
    *backoff += 1;
    wait
}

enum Fail {
    Denied(String),
    Other(String),
}

/// One connection, until it ends. `welcomed` is set when the server accepts this PC.
#[allow(clippy::too_many_arguments)]
async fn session(
    url: &str,
    token: &str,
    device: &str,
    up: &mut mpsc::Receiver<Up>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    shared: &Arc<Shared>,
    pc: &Arc<Mutex<Arc<Pc>>>,
    events: &mpsc::UnboundedSender<Event>,
    welcomed: &mut bool,
) -> Result<(), Fail> {
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .map_err(|_| Fail::Other("timed out".into()))?
    .map_err(|e| Fail::Other(e.to_string()))?;
    let (mut tx, mut rx) = ws.split();
    let hello = json!({ "type": "hello", "token": token, "device": device, "version": env!("CARGO_PKG_VERSION") });
    tx.send(Message::Text(hello.to_string().into()))
        .await
        .map_err(|e| Fail::Other(e.to_string()))?;
    // Results of PC actions come back from blocking threads.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<String>();
    // Drop anything the mic queued while offline.
    while up.try_recv().is_ok() {}
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg.map_err(|e| Fail::Other(e.to_string()))? {
                    Message::Binary(b) => shared.push_reply(&b),
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<Value>(&t) else { continue };
                        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        match s("type").as_str() {
                            "welcome" => {
                                *welcomed = true;
                                let owner = s("owner");
                                let _ = events.send(Event::Status { connected: true, text: format!("Connected as {}", s("name")), owner: (!owner.is_empty()).then_some(owner) });
                            }
                            "denied" => return Err(Fail::Denied(s("reason"))),
                            "mode" => { let _ = events.send(Event::Thinking(s("mode") == "thinking")); }
                            "state" => {
                                if let Some(v) = m.get("speaking").and_then(Value::as_bool) { shared.set_voice(v); }
                                if let Some(v) = m.get("listenMs").and_then(Value::as_f64) { shared.set_listening(v.max(0.0) as u64); }
                            }
                            "transcript" => {
                                let last = m.get("final").and_then(Value::as_bool).unwrap_or(true);
                                let _ = events.send(Event::Transcript { who: s("who"), text: s("text"), last });
                            }
                            "audio_stop" => shared.stop_reply(),
                            "action" => {
                                let (id, name, args) = (s("id"), s("name"), m.get("args").cloned().unwrap_or(json!({})));
                                // The settings may have changed what's allowed: use the current policy.
                                let pc = pc.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                let done = done_tx.clone();
                                tokio::task::spawn_blocking(move || {
                                    let o = pc.run(&name, &args);
                                    let _ = done.send(json!({ "type": "action_result", "id": id, "ok": o.ok, "result": o.text }).to_string());
                                });
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => return if *welcomed { Ok(()) } else { Err(Fail::Other("closed".into())) },
                    _ => {}
                }
            }
            Some(u) = up.recv() => {
                let m = match u {
                    Up::Start => Message::Text(json!({ "type": "speech_start" }).to_string().into()),
                    Up::Pcm(b) => Message::Binary(b.into()),
                    Up::End => Message::Text(json!({ "type": "speech_end" }).to_string().into()),
                };
                if *welcomed {
                    tx.send(m).await.map_err(|e| Fail::Other(e.to_string()))?;
                }
            }
            Some(c) = cmds.recv() => {
                let m = match c {
                    Cmd::Text(t) => json!({ "type": "text", "text": t }),
                    Cmd::Stop => { shared.stop_reply(); json!({ "type": "stop" }) }
                    Cmd::Ptt => json!({ "type": "ptt", "down": true }),
                };
                tx.send(Message::Text(m.to_string().into())).await.map_err(|e| Fail::Other(e.to_string()))?;
            }
            Some(r) = done_rx.recv() => {
                tx.send(Message::Text(r.into())).await.map_err(|e| Fail::Other(e.to_string()))?;
            }
            _ = ping.tick() => {
                tx.send(Message::Text(json!({ "type": "ping" }).to_string().into())).await.map_err(|e| Fail::Other(e.to_string()))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::UP_QUEUE;
    use adrian_pc_control::Policy;
    use tokio::net::TcpListener;

    const WELCOME: &str = r#"{"type":"welcome","name":"test"}"#;

    #[test]
    fn waits_1_2_5_then_10_and_starts_over_after_a_reset() {
        let mut backoff = 0;
        let waits: Vec<u64> = (0..5).map(|_| next_wait(&mut backoff)).collect();
        assert_eq!(waits, [1, 2, 5, 10, 10]);
        backoff = 0;
        assert_eq!(next_wait(&mut backoff), 1);
    }

    async fn listen() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    /// Be the server for one connection: take the hello, say `say`, wait for
    /// the first audio frame (if `audio`) and return it, then close.
    async fn serve(listener: &TcpListener, say: &[&str], audio: bool) -> Option<Vec<u8>> {
        serve_for(listener, say, audio, Duration::ZERO).await
    }

    /// The same, staying connected for `hold` first.
    async fn serve_for(
        listener: &TcpListener,
        say: &[&str],
        audio: bool,
        hold: Duration,
    ) -> Option<Vec<u8>> {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await;
        for m in say {
            ws.send(Message::Text((*m).into())).await.unwrap();
        }
        tokio::time::sleep(hold).await;
        let mut heard = None;
        while audio && heard.is_none() {
            match ws.next().await {
                Some(Ok(Message::Binary(b))) => heard = Some(b.to_vec()),
                Some(Ok(_)) => {}
                _ => break,
            }
        }
        // Close properly: dropping the socket with her ping unread resets the
        // connection, and that loses what was said on the way.
        let _ = ws.close(None).await;
        while let Some(Ok(_)) = ws.next().await {}
        heard
    }

    fn pc() -> Arc<Mutex<Arc<Pc>>> {
        Arc::new(Mutex::new(Arc::new(Pc::new(Policy::default()))))
    }

    /// One session with the rest of the app stubbed out.
    async fn connect(
        url: &str,
        up: &mut mpsc::Receiver<Up>,
        events: &mpsc::UnboundedSender<Event>,
    ) -> bool {
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let mut welcomed = false;
        let _ = session(
            url,
            "token",
            "test",
            up,
            &mut cmds,
            &Shared::new(0.0),
            &pc(),
            events,
            &mut welcomed,
        )
        .await;
        welcomed
    }

    #[tokio::test]
    async fn a_welcome_is_what_counts_as_connected() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (events, _seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let (_, welcomed) = tokio::join!(
            serve(&listener, &[WELCOME], false),
            connect(&url, &mut up, &events)
        );
        assert!(welcomed);
        // Accepted and dropped without one: the backoff has to keep growing.
        let (_, welcomed) = tokio::join!(
            serve(&listener, &[], false),
            connect(&url, &mut up, &events)
        );
        assert!(!welcomed);
    }

    /// The real reconnect loop, with the rest of the app stubbed out.
    async fn reconnect_loop(url: String) {
        let (_link_tx, link) = watch::channel(Some(Link {
            url,
            token: "token".into(),
            device: "test".into(),
        }));
        let (_up_tx, up) = mpsc::channel(4);
        let (_cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        run(link, up, cmds, Shared::new(0.0), pc(), events).await;
    }

    #[tokio::test]
    async fn after_a_connection_that_held_the_next_try_comes_after_one_second() {
        let (listener, url) = listen().await;
        let server = async {
            // Turned away twice: waits of 1 s and 2 s, with 5 s next.
            serve(&listener, &[], false).await;
            serve(&listener, &[], false).await;
            // A connection that worked, then a restart: back to 1 s.
            serve_for(&listener, &[WELCOME], false, HELD * 2).await;
            let closed = Instant::now();
            let _ = listener.accept().await;
            closed.elapsed()
        };
        tokio::select! {
            _ = reconnect_loop(url) => panic!("the link gave up"),
            waited = server => assert!(waited < Duration::from_secs(3), "{waited:?}"),
        }
    }

    #[tokio::test]
    async fn a_connection_dropped_right_after_its_welcome_keeps_backing_off() {
        let (listener, url) = listen().await;
        let server = async {
            // What two PCs on one token do to each other: the server keeps one
            // connection per device. Waits of 1 s and then 2 s, not 1 s forever.
            serve(&listener, &[WELCOME], false).await;
            serve(&listener, &[WELCOME], false).await;
            let closed = Instant::now();
            let _ = listener.accept().await;
            closed.elapsed()
        };
        tokio::select! {
            _ = reconnect_loop(url) => panic!("the link gave up"),
            waited = server => assert!(waited > Duration::from_millis(1500), "{waited:?}"),
        }
    }

    #[tokio::test]
    async fn audio_from_while_offline_is_never_sent() {
        let (up_tx, mut up) = mpsc::channel(UP_QUEUE);
        let (events, mut seen) = mpsc::unbounded_channel();
        // A full queue: audio read before the welcome is dropped anyway, so a
        // few frames would prove nothing about what's left behind once it comes.
        while up_tx.try_send(Up::Pcm(vec![1, 1])).is_ok() {}
        let (listener, url) = listen().await;
        // Talk only once the link says it's connected, like the real gate would.
        let talk = async {
            while let Some(e) = seen.recv().await {
                if matches!(
                    e,
                    Event::Status {
                        connected: true,
                        ..
                    }
                ) {
                    up_tx.send(Up::Pcm(vec![3, 3])).await.unwrap();
                    break;
                }
            }
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = connect(&url, &mut up, &events) => {}
                _ = talk => {}
            }
        };
        let (heard, _) = tokio::join!(serve(&listener, &[WELCOME], true), client);
        assert_eq!(heard, Some(vec![3, 3]));
    }
}
