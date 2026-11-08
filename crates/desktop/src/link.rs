//! The connection to the A.D.R.I.A.N. service: audio both ways, the window's
//! state, and PC actions. Reconnects by itself (1, 2, 5, then every 10 s,
//! starting over after a connection that held; every few minutes after a
//! refusal). Input only counts while the server is there: nothing typed or
//! pressed offline waits for the link to come back.

use crate::audio::{Shared, Up};
use adrian_pc_control::{Outcome, Pc};
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::{Value, json};
use std::fmt::Display;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// How long a welcomed connection has to last to count as working. One that
/// ends sooner keeps backing off: a server in trouble, or another PC on the
/// same token, since the server keeps one connection per device.
const HELD: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(30)
};
/// How long to wait after the server refuses this PC. Slowly: five refusals
/// shut the address out for a while, but a refusal isn't always final.
const REFUSED_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(1500)
} else {
    Duration::from_secs(5 * 60)
};
/// How often the server is pinged.
const PING: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(20)
};
/// Pings in a row that can go unanswered before the link counts as lost.
const UNANSWERED: u32 = 2;
/// How long one send may take. A send stuck on a full socket would otherwise
/// hold up everything here, the ping included.
const SEND_LIMIT: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(10)
};
/// What's said when input can't reach her.
pub const NOT_SENT: &str = "Not sent, she's offline.";

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

/// Hand `cmd` to the link if the server is there to take it. False when it
/// isn't: an order typed offline mustn't run when the link comes back.
pub fn offer(shared: &Shared, cmds: &mpsc::UnboundedSender<Cmd>, cmd: Cmd) -> bool {
    shared.online() && cmds.send(cmd).is_ok()
}

/// The talk button went down or up. False for a press nobody can hear.
pub fn talk_key(shared: &Shared, cmds: &mpsc::UnboundedSender<Cmd>, down: bool) -> bool {
    if down && !shared.online() {
        return false;
    }
    shared.ptt.store(down, Ordering::Relaxed);
    if down {
        let _ = cmds.send(Cmd::Ptt);
    }
    true
}

/// The talk hotkey went down or up. Nothing waits in a queue for it, but if it's
/// still down when she's back someone is still holding it, and what they say
/// from there on is for her. A reconnect takes a second, easy to miss from the tray.
pub fn talk_hotkey(shared: &Shared, cmds: &mpsc::UnboundedSender<Cmd>, down: bool) -> bool {
    shared.held.store(down, Ordering::Relaxed);
    talk_key(shared, cmds, down)
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
            r = session(&url, &token, &device, &mut up, &mut cmds, &shared, &pc, &events, &mut welcomed) => Some(r),
            _ = link.changed() => None,
        };
        // However it ended, the server is gone until a new welcome.
        shared.set_online(false);
        shared.stop_reply();
        shared.clear_state();
        let _ = events.send(Event::Thinking(false));
        let Some(outcome) = outcome else {
            backoff = 0;
            continue;
        };
        if welcomed && started.elapsed() >= HELD {
            backoff = 0;
        }
        let (text, wait) = match outcome {
            Ok(()) => (
                "Disconnected. Reconnecting…".to_string(),
                Duration::from_secs(next_wait(&mut backoff)),
            ),
            // Not the usual backoff: that would hammer the lockout.
            Err(Fail::Denied(r)) => (
                format!("The server refused this PC ({r}). Check the token in settings."),
                REFUSED_RETRY,
            ),
            Err(Fail::Other(e)) => (
                format!("Can't reach the server: {e}"),
                Duration::from_secs(next_wait(&mut backoff)),
            ),
        };
        let _ = events.send(Event::Status {
            connected: false,
            text,
            owner: None,
        });
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            r = link.changed() => {
                if r.is_err() {
                    return;
                }
                backoff = 0;
            }
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

/// The server's reason for turning this PC away. "no hello" is a server too busy
/// to read ours in time, which says nothing about this PC.
fn refused(reason: String) -> Fail {
    if reason == "no hello" {
        Fail::Other("it didn't answer in time".into())
    } else {
        Fail::Denied(reason)
    }
}

/// What a close from the server means. Before the welcome a policy close is a
/// refusal too: the address lockout closes that way and sends no `denied`.
fn closed(welcomed: bool, frame: Option<CloseFrame>) -> Result<(), Fail> {
    if welcomed {
        return Ok(());
    }
    Err(match frame {
        Some(f) if f.code == CloseCode::Policy => refused(f.reason.to_string()),
        Some(f) if !f.reason.is_empty() => Fail::Other(f.reason.to_string()),
        _ => Fail::Other("closed".into()),
    })
}

/// Send one message, within `SEND_LIMIT`.
async fn put<S, E>(tx: &mut S, m: Message) -> Result<(), Fail>
where
    S: Sink<Message, Error = E> + Unpin,
    E: Display,
{
    match tokio::time::timeout(SEND_LIMIT, tx.send(m)).await {
        Ok(sent) => sent.map_err(|e| Fail::Other(e.to_string())),
        Err(_) => Err(Fail::Other("sending timed out".into())),
    }
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
    put(&mut tx, Message::Text(hello.to_string().into())).await?;
    // Results of PC actions come back from blocking threads.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<String>();
    // Drop anything left from before: the mic's audio from while offline, and
    // any order that got in just as the last connection fell.
    while up.try_recv().is_ok() {}
    while cmds.try_recv().is_ok() {}
    let mut ping = tokio::time::interval(PING);
    // After a stall one late tick, not a burst of them: those would read as
    // pings going unanswered.
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut unanswered = 0;
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(msg) = msg else { return Ok(()) };
                let msg = msg.map_err(|e| Fail::Other(e.to_string()))?;
                unanswered = 0;
                match msg {
                    Message::Binary(b) => shared.push_reply(&b),
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<Value>(&t) else { continue };
                        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        match s("type").as_str() {
                            "welcome" => {
                                *welcomed = true;
                                shared.set_online(true);
                                let owner = s("owner");
                                let _ = events.send(Event::Status { connected: true, text: format!("Connected as {}", s("name")), owner: (!owner.is_empty()).then_some(owner) });
                                // The hotkey was down all along.
                                if shared.ptt.load(Ordering::Relaxed) {
                                    put(&mut tx, Message::Text(json!({ "type": "ptt", "down": true }).to_string().into())).await?;
                                }
                            }
                            "denied" => return Err(refused(s("reason"))),
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
                            // Nothing runs for a server that hasn't accepted this PC's token yet:
                            // whatever answers at the address could send it.
                            "action" if !*welcomed => {
                                let _ = done_tx.send(json!({ "type": "action_result", "id": s("id"), "ok": false, "result": "Not connected yet." }).to_string());
                            }
                            "action" => {
                                let (id, name, args) = (s("id"), s("name"), m.get("args").cloned().unwrap_or(json!({})));
                                // The settings may have changed what's allowed: use the current policy.
                                let pc = pc.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                let done = done_tx.clone();
                                tokio::task::spawn_blocking(move || {
                                    let o = pc.run(&name, &args);
                                    let _ = done.send(action_result(&id, &o));
                                });
                            }
                            _ => {}
                        }
                    }
                    Message::Close(f) => return closed(*welcomed, f),
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
                    put(&mut tx, m).await?;
                }
            }
            Some(c) = cmds.recv() => {
                // Input is only taken once welcomed; this is one that slipped in.
                if !*welcomed {
                    continue;
                }
                let m = match c {
                    Cmd::Text(t) => json!({ "type": "text", "text": t }),
                    Cmd::Stop => { shared.stop_reply(); json!({ "type": "stop" }) }
                    Cmd::Ptt => json!({ "type": "ptt", "down": true }),
                };
                put(&mut tx, Message::Text(m.to_string().into())).await?;
            }
            Some(r) = done_rx.recv() => {
                put(&mut tx, Message::Text(r.into())).await?;
            }
            _ = ping.tick() => {
                // Anything from the server counts as an answer. A process that
                // hangs leaves the socket open, so nothing else would notice.
                if unanswered >= UNANSWERED {
                    return Err(Fail::Other("it stopped answering".into()));
                }
                unanswered += 1;
                put(&mut tx, Message::Text(json!({ "type": "ping" }).to_string().into())).await?;
            }
        }
    }
}

/// The answer to an `action`. A picture (pc_screen) goes along as
/// `image: { mime, data }` in base64; servers that don't know it ignore it.
fn action_result(id: &str, o: &Outcome) -> String {
    use base64::Engine;
    let mut m = json!({ "type": "action_result", "id": id, "ok": o.ok, "result": o.text });
    if let Some(img) = &o.image {
        m["image"] = json!({
            "mime": img.mime,
            "data": base64::engine::general_purpose::STANDARD.encode(&img.bytes),
        });
    }
    m.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::UP_QUEUE;
    use adrian_pc_control::Policy;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    const WELCOME: &str = r#"{"type":"welcome","name":"test"}"#;

    #[test]
    fn a_picture_rides_along_with_its_answer() {
        let plain: Value = serde_json::from_str(&action_result("7", &Outcome::ok("Done."))).unwrap();
        assert_eq!(plain, json!({ "type": "action_result", "id": "7", "ok": true, "result": "Done." }));
        let img = adrian_pc_control::Image { mime: "image/jpeg", bytes: vec![0xFF, 0xD8, 0xFF, 0xD9], width: 2, height: 1 };
        let with: Value = serde_json::from_str(&action_result("8", &Outcome::ok("Here.").with_image(img))).unwrap();
        assert_eq!(with["result"], "Here.");
        assert_eq!(with["image"], json!({ "mime": "image/jpeg", "data": "/9j/2Q==" }));
    }

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
        // Holding on, answering pings the way the server does.
        if !hold.is_zero() {
            let end = tokio::time::Instant::now() + hold;
            while let Ok(Some(Ok(m))) = tokio::time::timeout_at(end, ws.next()).await {
                if matches!(&m, Message::Text(t) if t.contains("\"ping\"")) {
                    ws.send(Message::Text(r#"{"type":"pong"}"#.into()))
                        .await
                        .unwrap();
                }
            }
        }
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

    /// Be the server for one connection and note every order it gets until
    /// `last`, leaving out pings. `after_hello` runs before anything is said.
    async fn orders(
        listener: &TcpListener,
        after_hello: impl FnOnce(),
        say: &[&str],
        last: &str,
    ) -> Vec<String> {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await;
        after_hello();
        // Long enough for the client to pick up whatever that left it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for m in say {
            ws.send(Message::Text((*m).into())).await.unwrap();
        }
        heard_until(ws, last).await
    }

    /// Every order the server gets until `last`, leaving out pings, then close.
    async fn heard_until(mut ws: WebSocketStream<TcpStream>, last: &str) -> Vec<String> {
        let mut heard = Vec::new();
        while let Some(Ok(m)) = ws.next().await {
            let Message::Text(t) = m else { continue };
            if t.contains("\"ping\"") {
                continue;
            }
            heard.push(t.to_string());
            if t.contains(last) {
                break;
            }
        }
        let _ = ws.close(None).await;
        while let Some(Ok(_)) = ws.next().await {}
        heard
    }

    /// A server that took the hello and answered it, then hung with the socket
    /// open: it reads nothing more and says nothing more.
    async fn hang_after_welcome(listener: &TcpListener) {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await;
        ws.send(Message::Text(WELCOME.into())).await.unwrap();
        std::future::pending::<()>().await;
    }

    fn pc() -> Arc<Mutex<Arc<Pc>>> {
        Arc::new(Mutex::new(Arc::new(Pc::new(Policy::default()))))
    }

    /// One session with the rest of the app stubbed out: how it ended, and
    /// whether the server welcomed it.
    async fn attempt(
        url: &str,
        up: &mut mpsc::Receiver<Up>,
        cmds: &mut mpsc::UnboundedReceiver<Cmd>,
        events: &mpsc::UnboundedSender<Event>,
    ) -> (Result<(), Fail>, bool) {
        attempt_on(&Shared::new(0.0), url, up, cmds, events).await
    }

    /// The same, for a test that looks at `shared` too.
    async fn attempt_on(
        shared: &Arc<Shared>,
        url: &str,
        up: &mut mpsc::Receiver<Up>,
        cmds: &mut mpsc::UnboundedReceiver<Cmd>,
        events: &mpsc::UnboundedSender<Event>,
    ) -> (Result<(), Fail>, bool) {
        let mut welcomed = false;
        let ended = session(
            url,
            "token",
            "test",
            up,
            cmds,
            shared,
            &pc(),
            events,
            &mut welcomed,
        )
        .await;
        (ended, welcomed)
    }

    async fn connect(
        url: &str,
        up: &mut mpsc::Receiver<Up>,
        events: &mpsc::UnboundedSender<Event>,
    ) -> bool {
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        attempt(url, up, &mut cmds, events).await.1
    }

    /// Wait for a status with this `connected`.
    async fn until_status(seen: &mut mpsc::UnboundedReceiver<Event>, want: bool) {
        while let Some(e) = seen.recv().await {
            if matches!(e, Event::Status { connected, .. } if connected == want) {
                return;
            }
        }
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

    #[test]
    fn input_is_turned_down_offline_and_nothing_waits() {
        let shared = Shared::new(0.0);
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(!offer(&shared, &tx, Cmd::Text("lock the PC".into())));
        assert!(!talk_key(&shared, &tx, true));
        assert!(!shared.ptt.load(Ordering::Relaxed));
        assert!(rx.try_recv().is_err(), "nothing may stay queued");
        // Letting go always works.
        assert!(talk_key(&shared, &tx, false));

        shared.set_online(true);
        assert!(offer(&shared, &tx, Cmd::Text("lock the PC".into())));
        assert!(talk_key(&shared, &tx, true));
        assert!(shared.ptt.load(Ordering::Relaxed));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Text(_))));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Ptt)));

        // The key doesn't stay down past the link.
        shared.set_online(false);
        assert!(!shared.ptt.load(Ordering::Relaxed));
    }

    #[test]
    fn the_hotkey_outlasts_the_link_and_the_button_does_not() {
        let shared = Shared::new(0.0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let ptt = || shared.ptt.load(Ordering::Relaxed);
        shared.set_online(true);

        // Held through a drop: gone with the link, back with it.
        assert!(talk_hotkey(&shared, &tx, true));
        assert!(ptt());
        shared.set_online(false);
        assert!(!ptt());
        shared.set_online(true);
        assert!(ptt());

        // Let go during a drop: that's the end of it.
        shared.set_online(false);
        assert!(talk_hotkey(&shared, &tx, false));
        shared.set_online(true);
        assert!(!ptt());

        // The page lets go of its button when the link falls. If that never comes
        // (the window went with it), the next welcome mustn't open the mic by itself.
        assert!(talk_key(&shared, &tx, true));
        shared.set_online(false);
        shared.set_online(true);
        assert!(!ptt());
    }

    #[tokio::test]
    async fn a_hotkey_still_down_when_she_comes_back_counts_from_there() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        // Pressed during a reconnect: nobody to hear it yet, and nothing queued.
        assert!(!talk_hotkey(&shared, &cmds_tx, true));
        assert!(!shared.ptt.load(Ordering::Relaxed));
        let (listener, url) = listen().await;
        let (heard, _) = tokio::join!(
            orders(&listener, || {}, &[WELCOME], "\"ptt\""),
            attempt_on(&shared, &url, &mut up, &mut cmds, &events)
        );
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].contains("\"ptt\""));
        assert!(shared.ptt.load(Ordering::Relaxed), "the mic has to follow");
    }

    #[tokio::test]
    async fn a_hotkey_let_go_before_she_comes_back_is_forgotten() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        assert!(!talk_hotkey(&shared, &cmds_tx, true));
        assert!(talk_hotkey(&shared, &cmds_tx, false));
        let (listener, url) = listen().await;
        let say_now = async {
            until_status(&mut seen, true).await;
            cmds_tx.send(Cmd::Text("new order".into())).unwrap();
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = attempt_on(&shared, &url, &mut up, &mut cmds, &events) => {}
                _ = say_now => {}
            }
        };
        let (heard, _) = tokio::join!(orders(&listener, || {}, &[WELCOME], "new order"), client);
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].contains("new order"));
        assert!(!shared.ptt.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn orders_left_from_before_the_connection_are_never_sent() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        // Enough that the client can't get through them before it reads the welcome.
        for _ in 0..20 {
            cmds_tx.send(Cmd::Text("old order".into())).unwrap();
        }
        cmds_tx.send(Cmd::Ptt).unwrap();
        cmds_tx.send(Cmd::Stop).unwrap();
        let (listener, url) = listen().await;
        // The welcome is there from the client's first look, so waiting for it
        // can't be what holds these back: they have to be gone by then.
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(WELCOME.into())).await.unwrap();
            ws.next().await;
            heard_until(ws, "new order").await
        };
        let say_now = async {
            until_status(&mut seen, true).await;
            cmds_tx.send(Cmd::Text("new order".into())).unwrap();
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = attempt(&url, &mut up, &mut cmds, &events) => {}
                _ = say_now => {}
            }
        };
        let (heard, _) = tokio::join!(server, client);
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].contains("new order"));
    }

    #[tokio::test]
    async fn an_order_before_the_welcome_is_never_sent() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let early = cmds_tx.clone();
        let say_now = async {
            until_status(&mut seen, true).await;
            cmds_tx.send(Cmd::Text("new order".into())).unwrap();
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = attempt(&url, &mut up, &mut cmds, &events) => {}
                _ = say_now => {}
            }
        };
        let (heard, _) = tokio::join!(
            orders(
                &listener,
                move || early.send(Cmd::Text("early order".into())).unwrap(),
                &[WELCOME],
                "new order"
            ),
            client
        );
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].contains("new order"));
    }

    #[tokio::test]
    async fn input_is_only_taken_while_the_link_is_up() {
        let (listener, url) = listen().await;
        let (link_tx, link) = watch::channel(Some(Link {
            url,
            token: "token".into(),
            device: "test".into(),
        }));
        let (_up_tx, up) = mpsc::channel(4);
        let (_cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        let check = async {
            assert!(!shared.online());
            until_status(&mut seen, true).await;
            assert!(shared.online());
            shared.ptt.store(true, Ordering::Relaxed);
            // New settings end a connection that was fine, on the way to a dead address.
            link_tx
                .send(Some(Link {
                    url: "ws://127.0.0.1:1".into(),
                    token: "token".into(),
                    device: "test".into(),
                }))
                .unwrap();
            until_status(&mut seen, false).await;
            assert!(!shared.online());
            assert!(!shared.ptt.load(Ordering::Relaxed));
        };
        // It ends when the client leaves, which is what `check` is waiting to see.
        let server = async {
            serve_for(&listener, &[WELCOME], false, Duration::from_secs(60)).await;
            std::future::pending::<()>().await
        };
        tokio::select! {
            _ = run(link, up, cmds, shared.clone(), pc(), events) => panic!("the link gave up"),
            _ = server => unreachable!(),
            r = tokio::time::timeout(Duration::from_secs(10), check) => r.expect("no status came"),
        }
    }

    #[test]
    fn no_hello_is_a_slow_server_not_a_refusal() {
        assert!(matches!(refused("bad token".into()), Fail::Denied(_)));
        assert!(matches!(refused("no hello".into()), Fail::Other(_)));
    }

    #[test]
    fn reads_what_a_close_means() {
        let frame = |code, reason: &str| {
            Some(CloseFrame {
                code,
                reason: reason.into(),
            })
        };
        let lockout = frame(CloseCode::Policy, "too many failed attempts");
        assert!(
            matches!(closed(false, lockout.clone()), Err(Fail::Denied(r)) if r == "too many failed attempts")
        );
        assert!(
            matches!(closed(false, frame(CloseCode::Restart, "restarting")), Err(Fail::Other(r)) if r == "restarting")
        );
        assert!(matches!(closed(false, None), Err(Fail::Other(r)) if r == "closed"));
        // Once welcomed, a close is just the end of a connection that worked.
        assert!(closed(true, lockout).is_ok());
    }

    #[tokio::test]
    async fn a_lockout_close_counts_as_a_refusal() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let shut_out = CloseFrame {
                code: CloseCode::Policy,
                reason: "too many failed attempts".into(),
            };
            ws.close(Some(shut_out)).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        };
        let (_, (ended, _)) = tokio::join!(server, attempt(&url, &mut up, &mut cmds, &events));
        assert!(matches!(ended, Err(Fail::Denied(r)) if r == "too many failed attempts"));
    }

    #[tokio::test]
    async fn a_refusal_is_tried_again_without_new_settings_but_slowly() {
        let (listener, url) = listen().await;
        let server = async {
            let denied = r#"{"type":"denied","reason":"bad token"}"#;
            serve(&listener, &[denied], false).await;
            let refused_at = Instant::now();
            let _ = listener.accept().await;
            refused_at.elapsed()
        };
        let waited = tokio::select! {
            _ = reconnect_loop(url) => panic!("the link gave up"),
            r = tokio::time::timeout(REFUSED_RETRY * 4, server) => r.expect("it never tried again"),
        };
        // The usual first wait would be 1 s.
        assert!(waited > Duration::from_millis(1200), "{waited:?}");
    }

    #[tokio::test]
    async fn a_server_that_stops_answering_is_a_lost_link() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let ended = tokio::select! {
            _ = hang_after_welcome(&listener) => unreachable!(),
            r = tokio::time::timeout(PING * 10, attempt(&url, &mut up, &mut cmds, &events)) => r,
        };
        let (ended, welcomed) = ended.expect("it still thinks it's connected");
        assert!(welcomed);
        assert!(matches!(ended, Err(Fail::Other(r)) if r == "it stopped answering"));
    }

    #[tokio::test]
    async fn answered_pings_keep_a_quiet_link_up() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let (_, (ended, _)) = tokio::join!(
            serve_for(&listener, &[WELCOME], false, PING * 6),
            attempt(&url, &mut up, &mut cmds, &events)
        );
        assert!(ended.is_ok(), "the link fell with the server answering");
    }

    #[tokio::test]
    async fn the_servers_own_keep_alive_counts_as_an_answer() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.next().await;
            ws.send(Message::Text(WELCOME.into())).await.unwrap();
            // Never a pong, only pings of its own, one every 1.5 of ours: 30 s
            // against 20 s on the real server.
            for _ in 0..6 {
                ws.send(Message::Ping(Default::default())).await.unwrap();
                let beat = tokio::time::Instant::now() + PING * 3 / 2;
                while let Ok(Some(Ok(_))) = tokio::time::timeout_at(beat, ws.next()).await {}
            }
            let _ = ws.close(None).await;
            while let Some(Ok(_)) = ws.next().await {}
        };
        let (_, (ended, _)) = tokio::join!(server, attempt(&url, &mut up, &mut cmds, &events));
        assert!(ended.is_ok(), "the link fell with the server alive");
    }

    #[tokio::test]
    async fn a_stall_counts_as_one_unanswered_ping() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        // All of this runs on one thread, so this stops the client and the server
        // alike, the way a PC going to sleep stops the client: every ping that
        // came due meanwhile is late, and none of them has had a chance to be
        // answered. Back from it, that mustn't read as a server gone quiet.
        let stall = async {
            until_status(&mut seen, true).await;
            std::thread::sleep(PING * 6);
        };
        let (_, (ended, _), _) = tokio::join!(
            serve_for(&listener, &[WELCOME], false, PING * 10),
            attempt(&url, &mut up, &mut cmds, &events),
            stall
        );
        assert!(ended.is_ok(), "the link fell after a stall");
    }

    #[tokio::test]
    async fn a_send_stuck_on_a_full_socket_is_a_lost_link() {
        let (up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let (listener, url) = listen().await;
        // More audio than the sockets hold, with nobody reading it. The wait is
        // inside one send here, so the ping's check can't be what ends it.
        let talk = async {
            until_status(&mut seen, true).await;
            while up_tx.send(Up::Pcm(vec![0; 1 << 16])).await.is_ok() {}
            std::future::pending::<()>().await
        };
        let ended = tokio::select! {
            _ = hang_after_welcome(&listener) => unreachable!(),
            _ = talk => unreachable!(),
            r = tokio::time::timeout(PING * 20, attempt(&url, &mut up, &mut cmds, &events)) => r,
        };
        let (ended, _) = ended.expect("it never noticed");
        assert!(matches!(ended, Err(Fail::Other(r)) if r == "sending timed out"));
    }
}
