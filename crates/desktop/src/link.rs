//! The connection to the A.D.R.I.A.N. service: audio both ways, the window's
//! state, and PC actions. Reconnects by itself (1, 2, 5, then every 10 s).

use crate::audio::{Shared, Up};
use adrian_pc_control::Pc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

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
    mut up: mpsc::UnboundedReceiver<Up>,
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
        let outcome = tokio::select! {
            r = session(&url, &token, &device, &mut up, &mut cmds, &shared, &pc, &events) => r,
            _ = link.changed() => { backoff = 0; continue; }
        };
        let (text, retry) = match outcome {
            Ok(()) => ("Disconnected. Reconnecting…".to_string(), true),
            Err(Fail::Denied(r)) => (
                format!("The server refused this PC ({r}). Check the token in settings."),
                false,
            ),
            Err(Fail::Other(e)) => (format!("Can't reach the server: {e}"), true),
        };
        shared.stop_reply();
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
        let wait = [1, 2, 5, 10][backoff.min(3)];
        backoff += 1;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            _ = link.changed() => { backoff = 0; }
        }
    }
}

enum Fail {
    Denied(String),
    Other(String),
}

#[allow(clippy::too_many_arguments)]
async fn session(
    url: &str,
    token: &str,
    device: &str,
    up: &mut mpsc::UnboundedReceiver<Up>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    shared: &Arc<Shared>,
    pc: &Arc<Mutex<Arc<Pc>>>,
    events: &mpsc::UnboundedSender<Event>,
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
    let mut welcomed = false;
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
                                welcomed = true;
                                let owner = s("owner");
                                let _ = events.send(Event::Status { connected: true, text: format!("Connected as {}", s("name")), owner: (!owner.is_empty()).then_some(owner) });
                            }
                            "denied" => return Err(Fail::Denied(s("reason"))),
                            "mode" => { let _ = events.send(Event::Thinking(s("mode") == "thinking")); }
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
                    Message::Close(_) => return if welcomed { Ok(()) } else { Err(Fail::Other("closed".into())) },
                    _ => {}
                }
            }
            Some(u) = up.recv() => {
                let m = match u {
                    Up::Start => Message::Text(json!({ "type": "speech_start" }).to_string().into()),
                    Up::Pcm(b) => Message::Binary(b.into()),
                    Up::End => Message::Text(json!({ "type": "speech_end" }).to_string().into()),
                };
                if welcomed {
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
