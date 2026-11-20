//! The connection to the A.D.R.I.A.N. service: audio both ways, the window's
//! state, and PC actions. Reconnects by itself: a quarter second after a
//! connection that held ends, then about once a second while the server has
//! been gone under half a minute (a restart takes 20-25 s), then slowing to
//! every 10 s by the one-minute mark; every few minutes after a refusal.
//! Input only counts while the server is there: nothing typed or pressed
//! offline waits for the link to come back.

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
/// The first try after a connection that held: soon, since a dropped link to
/// a server that's still up comes straight back.
const FIRST_TRY: Duration = Duration::from_millis(250);
/// While the server has been gone this long, tries come about once a second.
/// A restart takes 20-25 s; this covers it with room to spare.
const QUICK_FOR: Duration = Duration::from_secs(30);
/// The wait between tries during the quick stretch.
const QUICK_WAIT: Duration = Duration::from_secs(1);
/// The longest wait between tries, reached once she's been gone a minute.
const LONGEST: Duration = Duration::from_secs(10);
/// How long one try may take to connect.
const CONNECT_LIMIT: Duration = Duration::from_secs(8);
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
    /// The server's alarm state changed (ringing, or the list).
    Alarms,
}

pub enum Cmd {
    Text(String),
    Stop,
    Ptt,
    /// Playing started or stopped: send `Shared::activity` as it is now.
    Activity,
    /// Stop or Snooze on an alarm the server is ringing.
    Alarm(&'static str),
    /// An alarm rung here was answered here: tell the server now.
    AlarmDone,
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

/// Stop or Snooze on an alarm, from the window or the tray. False when an order
/// meant for the server can't reach it.
pub fn answer_alarm(
    shared: &Shared,
    cmds: &mpsc::UnboundedSender<Cmd>,
    action: &'static str,
    now: f64,
) -> bool {
    match shared.alarms.answer(action, now) {
        // Rung here by itself: quiet now. The server hears now if it's there,
        // or after the next welcome (which sends whatever is still pending).
        crate::alarm::Answer::Here => {
            shared.stop_reply();
            offer(shared, cmds, Cmd::AlarmDone);
            true
        }
        crate::alarm::Answer::Server => offer(shared, cmds, Cmd::Alarm(action)),
        crate::alarm::Answer::Nothing => true,
    }
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
    let mut retry = Retry::new();
    let mut away = Away::default();
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
        let mut trace = Trace::default();
        let started = Instant::now();
        let outcome = tokio::select! {
            r = session(&url, &token, &device, &mut up, &mut cmds, &shared, &pc, &events, &mut trace, &mut away) => Some(r),
            _ = link.changed() => None,
        };
        // However it ended, the server is gone until a new welcome.
        shared.set_online(false);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        shared.alarms.link_lost(now);
        let _ = events.send(Event::Alarms);
        shared.stop_reply();
        shared.clear_state();
        let _ = events.send(Event::Thinking(false));
        let held = trace.welcomed && started.elapsed() >= HELD;
        if trace.welcomed {
            shared.log.line(&trace.ended(started.elapsed(), &shared, outcome.as_ref()));
            away.down = Some(Instant::now());
        } else if let Some(o) = &outcome {
            away.failed(o, started.elapsed(), &shared);
        }
        let Some(outcome) = outcome else {
            retry = Retry::new();
            away = Away::default();
            continue;
        };
        let (text, wait) = match outcome {
            Ok(()) => (
                "Disconnected. Reconnecting…".to_string(),
                retry.next(trace.answered, held),
            ),
            // Not the usual backoff: that would hammer the lockout.
            Err(Fail::Denied(r)) => (
                format!("The server refused this PC ({r}). Check the token in settings."),
                REFUSED_RETRY,
            ),
            Err(Fail::Other(e)) => (
                format!("Can't reach the server: {e}"),
                retry.next(trace.answered, held),
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
                retry = Retry::new();
                away = Away::default();
            }
        }
    }
}

/// When to try again. The schedule follows how long the server has been out
/// of reach, so tries come quickly through a restart and slow down for a long
/// outage. A server that answers and then doesn't keep the connection (it
/// drops it soon after the welcome, or never welcomes it) is backed off on its
/// own count as well: another PC on the same token, or a server too busy to
/// read the hello (which counts toward its lockout), mustn't get a new
/// connection every second.
struct Retry {
    /// When the server was last known to be fine: the end of the last
    /// connection that held, or the start.
    since: Instant,
    /// Tries that failed since then.
    tries: u32,
    /// Connections the server answered and didn't keep, in a row.
    short: usize,
}

impl Retry {
    fn new() -> Self {
        Retry {
            since: Instant::now(),
            tries: 0,
            short: 0,
        }
    }

    /// The wait after a connection ended (`answered`: the server took it;
    /// `held`: it lasted long enough after the welcome to count as working).
    fn next(&mut self, answered: bool, held: bool) -> Duration {
        if held {
            *self = Retry::new();
        }
        let mut wait = retry_wait(self.tries, self.since.elapsed());
        if answered && !held {
            wait = wait.max(Duration::from_secs(short_wait(self.short)));
            self.short += 1;
        }
        self.tries += 1;
        wait
    }
}

/// The wait before the next try, `tries` failed tries into an outage that
/// began `gone` ago.
fn retry_wait(tries: u32, gone: Duration) -> Duration {
    if tries == 0 {
        FIRST_TRY
    } else if gone < QUICK_FOR {
        QUICK_WAIT
    } else {
        // From once a second up to `LONGEST` over the next half minute.
        let ramp = Duration::from_secs_f64(((gone - QUICK_FOR).as_secs_f64() * 0.3).min(60.0));
        (QUICK_WAIT + ramp).min(LONGEST)
    }
}

/// Seconds to wait after the `n`-th connection in a row that the server
/// answered and didn't keep.
fn short_wait(n: usize) -> u64 {
    [1, 2, 5, 10][n.min(3)]
}

/// What's known about one connection, for the log.
#[derive(Default)]
struct Trace {
    /// The server took the connection (the WebSocket opened).
    answered: bool,
    /// The server accepted this PC.
    welcomed: bool,
    /// How it ended, when that isn't in the error.
    how: String,
    /// When the server was last heard from.
    heard: Option<Instant>,
    /// Pings unanswered when it ended.
    unanswered: u32,
    /// The latest a ping went out after it was due: a sign the app was held up.
    late: Duration,
}

impl Trace {
    /// The log line for a connection that was welcomed, once it's over.
    fn ended(&self, lasted: Duration, shared: &Shared, outcome: Option<&Result<(), Fail>>) -> String {
        let how = match outcome {
            None => "new settings".to_string(),
            Some(Err(Fail::Other(e))) => e.clone(),
            Some(Err(Fail::Denied(r))) => format!("refused: {r}"),
            Some(Ok(())) if self.how.is_empty() => "closed".to_string(),
            Some(Ok(())) => self.how.clone(),
        };
        let heard = self
            .heard
            .map_or_else(|| "-".to_string(), |h| format!("{:.1}", h.elapsed().as_secs_f64()));
        format!(
            "link ended: {how}; lasted {:.0} s, heard from {heard} s before, {} ping(s) unanswered, pings up to {} ms late; {}",
            lasted.as_secs_f64(),
            self.unanswered,
            self.late.as_millis(),
            here(shared)
        )
    }
}

/// What this PC was doing, for the log.
fn here(shared: &Shared) -> String {
    let gaming = shared.activity.lock().unwrap_or_else(|e| e.into_inner()).gaming;
    format!(
        "quiet mode {}, game {}",
        if shared.quiet.load(Ordering::Relaxed) { "on" } else { "off" },
        if gaming { "in front" } else { "no" }
    )
}

/// The tries made while the server was out of reach, for one log line when
/// she's back (and one at the first failure, in case she never is).
#[derive(Default)]
struct Away {
    /// When the last link ended, or the first try failed.
    down: Option<Instant>,
    tries: u32,
    refused: u32,
    timed_out: u32,
    other: u32,
    last: String,
    /// The longest one try took to fail.
    slowest: Duration,
}

impl Away {
    fn failed(&mut self, outcome: &Result<(), Fail>, took: Duration, shared: &Shared) {
        let what = match outcome {
            Ok(()) => "closed before the welcome".to_string(),
            Err(Fail::Denied(r)) => format!("refused: {r}"),
            Err(Fail::Other(e)) => e.clone(),
        };
        self.down.get_or_insert_with(Instant::now);
        if self.tries == 0 {
            shared.log.line(&format!(
                "can't reach the server: {what} (after {} ms); {}",
                took.as_millis(),
                here(shared)
            ));
        }
        self.tries += 1;
        let lower = what.to_lowercase();
        if lower.contains("refused") {
            self.refused += 1;
        } else if lower.contains("timed out") {
            self.timed_out += 1;
        } else {
            self.other += 1;
        }
        self.slowest = self.slowest.max(took);
        self.last = what;
    }

    /// Welcomed: the line for the outage, if there was one.
    fn back(&mut self, shared: &Shared) {
        match self.down {
            None => shared.log.line(&format!("link up; {}", here(shared))),
            Some(down) if self.tries == 0 => shared.log.line(&format!(
                "link back after {:.1} s, at the first try; {}",
                down.elapsed().as_secs_f64(),
                here(shared)
            )),
            Some(down) => shared.log.line(&format!(
                "link back after {:.1} s: {} failed tries ({} refused, {} timed out, {} other; slowest {} ms), last: {}; {}",
                down.elapsed().as_secs_f64(),
                self.tries,
                self.refused,
                self.timed_out,
                self.other,
                self.slowest.as_millis(),
                self.last,
                here(shared)
            )),
        }
        *self = Away::default();
    }
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

/// Tell the server how the alarms rung here were answered. Each is forgotten
/// only once it has gone out.
async fn send_done<S, E>(tx: &mut S, alarms: &crate::alarm::Alarms) -> Result<(), Fail>
where
    S: Sink<Message, Error = E> + Unpin,
    E: Display,
{
    for d in alarms.pending_done() {
        put(tx, Message::Text(d.to_string().into())).await?;
        alarms.sent_done(1);
    }
    Ok(())
}

/// One connection, until it ends. `trace` notes how it went (`welcomed` once
/// the server accepts this PC); `away` hears about the welcome.
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
    trace: &mut Trace,
    away: &mut Away,
) -> Result<(), Fail> {
    let (ws, _) = tokio::time::timeout(CONNECT_LIMIT, tokio_tungstenite::connect_async(url))
    .await
    .map_err(|_| Fail::Other("timed out".into()))?
    .map_err(|e| Fail::Other(e.to_string()))?;
    trace.answered = true;
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
                let Some(msg) = msg else {
                    trace.how = "the connection ended without a close".into();
                    return Ok(());
                };
                let msg = msg.map_err(|e| Fail::Other(e.to_string()))?;
                trace.heard = Some(Instant::now());
                unanswered = 0;
                trace.unanswered = 0;
                match msg {
                    Message::Binary(b) => shared.push_reply(&b),
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<Value>(&t) else { continue };
                        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        match s("type").as_str() {
                            "welcome" => {
                                trace.welcomed = true;
                                away.back(shared);
                                shared.set_online(true);
                                let owner = s("owner");
                                let _ = events.send(Event::Status { connected: true, text: format!("Connected as {}", s("name")), owner: (!owner.is_empty()).then_some(owner) });
                                // The hotkey was down all along.
                                if shared.ptt.load(Ordering::Relaxed) {
                                    put(&mut tx, Message::Text(json!({ "type": "ptt", "down": true }).to_string().into())).await?;
                                }
                                // In a game all along: the server starts every connection as not playing.
                                let playing = shared.activity.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                if playing.gaming {
                                    put(&mut tx, Message::Text(playing.message().to_string().into())).await?;
                                }
                                // Alarms rung here while the server was out of reach, and how they were answered.
                                send_done(&mut tx, &shared.alarms).await?;
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
                            "alarms" if trace.welcomed => {
                                shared.alarms.server_said(&m);
                                let _ = events.send(Event::Alarms);
                            }
                            // Nothing runs for a server that hasn't accepted this PC's token yet:
                            // whatever answers at the address could send it.
                            "action" if !trace.welcomed => {
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
                    Message::Close(f) => {
                        trace.how = match &f {
                            Some(f) => format!("the server closed it ({} {})", u16::from(f.code), f.reason),
                            None => "the server closed it".into(),
                        };
                        return closed(trace.welcomed, f);
                    }
                    _ => {}
                }
            }
            Some(u) = up.recv() => {
                let m = match u {
                    Up::Start => Message::Text(json!({ "type": "speech_start" }).to_string().into()),
                    Up::Pcm(b) => Message::Binary(b.into()),
                    Up::End => Message::Text(json!({ "type": "speech_end" }).to_string().into()),
                };
                if trace.welcomed {
                    put(&mut tx, m).await?;
                }
            }
            Some(c) = cmds.recv() => {
                // Input is only taken once welcomed; this is one that slipped in.
                if !trace.welcomed {
                    continue;
                }
                let m = match c {
                    Cmd::Text(t) => json!({ "type": "text", "text": t }),
                    Cmd::Stop => { shared.stop_reply(); json!({ "type": "stop" }) }
                    Cmd::Ptt => json!({ "type": "ptt", "down": true }),
                    Cmd::Alarm(action) => json!({ "type": "alarm_action", "action": action }),
                    Cmd::Activity => shared.activity.lock().unwrap_or_else(|e| e.into_inner()).message(),
                    Cmd::AlarmDone => {
                        send_done(&mut tx, &shared.alarms).await?;
                        continue;
                    }
                };
                put(&mut tx, Message::Text(m.to_string().into())).await?;
            }
            Some(r) = done_rx.recv() => {
                put(&mut tx, Message::Text(r.into())).await?;
            }
            due = ping.tick() => {
                // Late by more than a moment: this app was held up (asleep, or
                // starved of the CPU), which the log should show.
                trace.late = trace.late.max(tokio::time::Instant::now().saturating_duration_since(due));
                // Anything from the server counts as an answer. A process that
                // hangs leaves the socket open, so nothing else would notice.
                if unanswered >= UNANSWERED {
                    return Err(Fail::Other("it stopped answering".into()));
                }
                unanswered += 1;
                trace.unanswered = unanswered;
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
        let plain: Value =
            serde_json::from_str(&action_result("7", &Outcome::ok("Done."))).unwrap();
        assert_eq!(
            plain,
            json!({ "type": "action_result", "id": "7", "ok": true, "result": "Done." })
        );
        let img = adrian_pc_control::Image {
            mime: "image/jpeg",
            bytes: vec![0xFF, 0xD8, 0xFF, 0xD9],
            width: 2,
            height: 1,
        };
        let with: Value =
            serde_json::from_str(&action_result("8", &Outcome::ok("Here.").with_image(img)))
                .unwrap();
        assert_eq!(with["result"], "Here.");
        assert_eq!(
            with["image"],
            json!({ "mime": "image/jpeg", "data": "/9j/2Q==" })
        );
    }

    #[test]
    fn tries_quickly_through_a_restart_and_slowly_through_a_long_outage() {
        let ms = |d: Duration| d.as_millis() as u64;
        let s = Duration::from_secs;
        assert_eq!(ms(retry_wait(0, Duration::ZERO)), 250);
        assert_eq!(ms(retry_wait(1, s(1))), 1000);
        assert_eq!(ms(retry_wait(9, s(29))), 1000);
        assert_eq!(ms(retry_wait(12, s(40))), 4000);
        assert_eq!(ms(retry_wait(15, s(60))), 10_000);
        assert_eq!(ms(retry_wait(40, s(3600))), 10_000);
        // Waits never shrink as an outage goes on.
        let mut last = Duration::ZERO;
        for t in 1..600 {
            let w = retry_wait(t, Duration::from_millis(u64::from(t) * 250));
            assert!(w >= last, "{t}");
            last = w;
        }
    }

    #[test]
    fn a_server_that_answers_and_doesnt_keep_it_is_backed_off_1_2_5_then_10() {
        let mut r = Retry::new();
        let waits: Vec<u64> = (0..5).map(|_| r.next(true, false).as_secs()).collect();
        assert_eq!(waits, [1, 2, 5, 10, 10]);
        // One that held starts it over.
        assert_eq!(r.next(true, true), FIRST_TRY);
        assert_eq!(r.next(false, false), QUICK_WAIT);
    }

    /// The tries made in an outage of `secs` under the schedule, with each
    /// failed try costing `cost` s (a refused connect takes 2 s on Windows, one
    /// that hangs takes the 8 s connect limit),
    /// and how long after the server is back the next try comes.
    fn model(policy: &dyn Fn(u32, f64) -> f64, secs: f64, cost: f64) -> (f64, u32) {
        let (mut t, mut tries) = (0.0, 0);
        loop {
            t += policy(tries, t);
            tries += 1;
            if t >= secs {
                return (t - secs, tries);
            }
            t += cost;
        }
    }

    /// Not a check: the schedule before and after, over outages of 0.1 to 120 s.
    /// `cargo test -p adrian-desktop reconnect_schedule_sweep -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn reconnect_schedule_sweep() {
        let old = |tries: u32, _: f64| [1.0, 2.0, 5.0, 10.0][(tries as usize).min(3)];
        let new = |tries: u32, t: f64| retry_wait(tries, Duration::from_secs_f64(t)).as_secs_f64();
        for cost in [0.02, 2.0, 8.0] {
            for (name, p) in [("before", &old as &dyn Fn(u32, f64) -> f64), ("after", &new)] {
                let stats = |lo: f64, hi: f64| {
                    let mut late: Vec<f64> = (1..=1200)
                        .map(|k| k as f64 / 10.0)
                        .filter(|s| (lo..=hi).contains(s))
                        .map(|s| model(p, s, cost).0)
                        .collect();
                    late.sort_by(f64::total_cmp);
                    (late[late.len() / 2], late[late.len() - 1])
                };
                let (m, x) = stats(0.0, 120.0);
                let (rm, rx) = stats(15.0, 30.0);
                let at: Vec<String> = [3.0, 10.0, 21.0, 60.0]
                    .iter()
                    .map(|&s| format!("{:.2}", model(p, s, cost).0))
                    .collect();
                let tries = |s: f64| model(p, s, cost).1;
                println!(
                    "{name:6} try cost {cost:4} s: 0-120 s outages late median {m:.2} max {x:.2}; 15-30 s median {rm:.2} max {rx:.2}; at 3/10/21/60 s {}; tries by 60 s {}, 60-120 s {}, 120-300 s {}",
                    at.join("/"),
                    tries(60.0),
                    tries(120.0) - tries(60.0),
                    tries(300.0) - tries(120.0)
                );
            }
        }
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
        let mut trace = Trace::default();
        let ended = session(
            url,
            "token",
            "test",
            up,
            cmds,
            shared,
            &pc(),
            events,
            &mut trace,
            &mut Away::default(),
        )
        .await;
        (ended, trace.welcomed)
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
    async fn after_a_connection_that_held_the_next_try_comes_within_a_second() {
        let (listener, url) = listen().await;
        let server = async {
            // Turned away twice: waits of 1 s and 2 s, with 5 s next.
            serve(&listener, &[], false).await;
            serve(&listener, &[], false).await;
            // A connection that worked, then a restart: a quarter second.
            serve_for(&listener, &[WELCOME], false, HELD * 2).await;
            let closed = Instant::now();
            let _ = listener.accept().await;
            closed.elapsed()
        };
        tokio::select! {
            _ = reconnect_loop(url) => panic!("the link gave up"),
            waited = server => assert!(waited < Duration::from_millis(900), "{waited:?}"),
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
    async fn a_game_already_going_when_she_comes_back_is_said_after_the_welcome() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (_cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, _seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        *shared.activity.lock().unwrap() = adrian_pc_control::activity::Activity {
            gaming: true,
            game: Some("VALORANT".into()),
        };
        let (listener, url) = listen().await;
        let (heard, _) = tokio::join!(
            orders(&listener, || {}, &[WELCOME], "\"activity\""),
            attempt_on(&shared, &url, &mut up, &mut cmds, &events)
        );
        assert_eq!(heard.len(), 1, "{heard:?}");
        let m: Value = serde_json::from_str(&heard[0]).unwrap();
        assert_eq!(
            m,
            json!({ "type": "activity", "gaming": true, "game": "VALORANT" })
        );
    }

    #[tokio::test]
    async fn a_game_starting_or_ending_is_said_once_connected() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        let (listener, url) = listen().await;
        let change = async {
            until_status(&mut seen, true).await;
            *shared.activity.lock().unwrap() = adrian_pc_control::activity::Activity {
                gaming: true,
                game: None,
            };
            assert!(offer(&shared, &cmds_tx, Cmd::Activity));
            *shared.activity.lock().unwrap() = Default::default();
            // Both readings go: whatever the first one sent, the last one is the state.
            assert!(offer(&shared, &cmds_tx, Cmd::Activity));
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = attempt_on(&shared, &url, &mut up, &mut cmds, &events) => {}
                _ = change => {}
            }
        };
        let (heard, _) = tokio::join!(
            orders(&listener, || {}, &[WELCOME], "\"gaming\":false"),
            client
        );
        assert!(!heard.is_empty(), "{heard:?}");
        assert!(heard.last().unwrap().contains("\"gaming\":false"));
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

    fn epoch_now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64
    }

    /// An alarm ringing here by itself: the server listed it and never rang it.
    fn ringing_here(shared: &Shared) -> f64 {
        let now = epoch_now();
        let due = now - 30_000.0;
        shared.alarms.server_said(&json!({
            "type": "alarms",
            "list": [{ "id": "a1", "kind": "alarm", "label": "", "at": due, "base": due }],
            "ringing": [],
            "now": now,
        }));
        assert!(shared.alarms.tick(now), "it should ring here");
        now
    }

    #[tokio::test]
    async fn an_alarm_answered_here_reaches_the_server_on_the_same_link() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        let now = ringing_here(&shared);
        let (listener, url) = listen().await;
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            accepted.fetch_add(1, Ordering::SeqCst);
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.next().await;
            ws.send(Message::Text(WELCOME.into())).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), heard_until(ws, "alarm_done")).await
        };
        let answer = async {
            until_status(&mut seen, true).await;
            assert!(answer_alarm(&shared, &cmds_tx, "stop", now));
            std::future::pending::<()>().await
        };
        let client = async {
            tokio::select! {
                _ = attempt_on(&shared, &url, &mut up, &mut cmds, &events) => {}
                _ = answer => {}
            }
        };
        let (heard, _) = tokio::join!(server, client);
        let heard = heard.expect("the server never heard how it was answered");
        let done: Vec<Value> = heard
            .iter()
            .filter_map(|t| serde_json::from_str::<Value>(t).ok())
            .filter(|m| m["type"] == "alarm_done")
            .collect();
        assert_eq!(done.len(), 1, "{heard:?}");
        assert_eq!(done[0]["id"], "a1");
        assert_eq!(done[0]["action"], "stop");
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "on the same link");
        assert!(shared.alarms.pending_done().is_empty(), "and it's forgotten");
        assert!(!shared.alarms.ringing());
    }

    #[tokio::test]
    async fn an_alarm_answered_offline_goes_out_once_at_the_welcome() {
        let (_up_tx, mut up) = mpsc::channel(4);
        let (cmds_tx, mut cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let shared = Shared::new(0.0);
        let now = ringing_here(&shared);
        assert!(answer_alarm(&shared, &cmds_tx, "snooze", now));
        assert_eq!(shared.alarms.pending_done().len(), 1);
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
        let done = heard.iter().filter(|t| t.contains("\"alarm_done\"")).count();
        assert_eq!(done, 1, "{heard:?}");
        assert!(shared.alarms.pending_done().is_empty());
    }

    /// A log file of its own under the temp folder, for one test.
    fn temp_log(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("adrian-link-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("link.log")
    }

    /// The number before `after` in the first line holding `line`.
    fn number_before(log: &str, line: &str, after: &str) -> Option<u64> {
        let l = log.lines().find(|l| l.contains(line))?;
        let head = &l[..l.find(after)?];
        head.rsplit(' ').next()?.parse().ok()
    }

    /// A PC held up past the server's keep-alive (the server drops it after a
    /// missed ping, like server.ts) is back soon after it wakes, and the log
    /// says it was held up.
    #[test]
    fn a_pc_held_up_past_the_servers_keep_alive_is_back_soon_after_it_wakes() {
        const BEAT: Duration = Duration::from_millis(300);
        let server_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let listener = server_rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (welcomed_tx, welcomed) = std::sync::mpsc::channel::<Instant>();
        server_rt.spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                ws.next().await;
                ws.send(Message::Text(WELCOME.into())).await.unwrap();
                let _ = welcomed_tx.send(Instant::now());
                let mut alive = true;
                let mut beat = tokio::time::interval(BEAT);
                beat.tick().await;
                loop {
                    tokio::select! {
                        m = ws.next() => match m {
                            Some(Ok(Message::Pong(_))) => alive = true,
                            Some(Ok(Message::Text(t))) if t.contains("\"ping\"") => {
                                let _ = ws.send(Message::Text(r#"{"type":"pong"}"#.into())).await;
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        _ = beat.tick() => {
                            // A missed ping: dropped without a close, as `terminate()` does.
                            if !alive {
                                break;
                            }
                            alive = false;
                            let _ = ws.send(Message::Ping(Default::default())).await;
                        }
                    }
                }
            }
        });
        let file = temp_log("held-up");
        let log = file.clone();
        let woke = Arc::new(Mutex::new(None::<Instant>));
        let woke_at = woke.clone();
        let (stop_tx, stop) = tokio::sync::oneshot::channel::<()>();
        let client = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let (_link_tx, link) = watch::channel(Some(Link {
                    url,
                    token: "token".into(),
                    device: "test".into(),
                }));
                let (_up_tx, up) = mpsc::channel(4);
                let (_cmds_tx, cmds) = mpsc::unbounded_channel();
                let (events, mut seen) = mpsc::unbounded_channel();
                let shared = Shared::new(0.0);
                shared.log.to(log);
                let stall = async {
                    until_status(&mut seen, true).await;
                    // A link that held, then everything here stops.
                    tokio::time::sleep(HELD * 2).await;
                    std::thread::sleep(BEAT * 5);
                    *woke_at.lock().unwrap() = Some(Instant::now());
                    std::future::pending::<()>().await
                };
                tokio::select! {
                    _ = run(link, up, cmds, shared.clone(), pc(), events) => {}
                    _ = stall => {}
                    _ = stop => {}
                }
            });
        });
        welcomed.recv_timeout(Duration::from_secs(10)).expect("never connected");
        // The first welcome after it woke (a busy machine may have cost it a
        // link before the stall too; that one doesn't count).
        let (again, woke) = loop {
            let at = welcomed.recv_timeout(Duration::from_secs(10)).expect("never came back");
            let w = *woke.lock().unwrap();
            if let Some(w) = w
                && at > w
            {
                break (at, w);
            }
        };
        let _ = stop_tx.send(());
        client.join().unwrap();
        drop(server_rt);
        let back = again.saturating_duration_since(woke);
        println!("back {} ms after waking", back.as_millis());
        assert!(back < Duration::from_millis(700), "back {back:?} after waking");
        let text = std::fs::read_to_string(&file).unwrap();
        let held_up = text
            .lines()
            .filter(|l| l.contains("link ended"))
            .filter_map(|l| number_before(l, "link ended", " ms late"))
            .max()
            .expect(&text);
        assert!(held_up >= (BEAT * 3).as_millis() as u64, "{text}");
        assert!(text.contains("at the first try"), "{text}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn an_outage_is_logged_once_with_its_tries_and_nothing_secret() {
        let file = temp_log("outage");
        let shared = Shared::new(0.0);
        shared.log.to(file.clone());
        let (listener, url) = listen().await;
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (_link_tx, link) = watch::channel(Some(Link {
            url,
            token: "sat_secret".into(),
            device: "test".into(),
        }));
        let (_up_tx, up) = mpsc::channel(4);
        let (_cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let server = async {
            // Out of reach for two tries, then back.
            let mut fails = 0;
            while fails < 2 {
                if let Some(Event::Status { text, .. }) = seen.recv().await
                    && text.starts_with("Can't reach")
                {
                    fails += 1;
                }
            }
            let listener = TcpListener::bind(addr).await.unwrap();
            serve(&listener, &[WELCOME], false).await;
            until_status(&mut seen, false).await;
        };
        tokio::select! {
            _ = run(link, up, cmds, shared.clone(), pc(), events) => panic!("the link gave up"),
            r = tokio::time::timeout(Duration::from_secs(20), server) => r.expect("never came back"),
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.matches("can't reach the server").count(), 1, "{text}");
        assert_eq!(text.matches("link back after").count(), 1, "{text}");
        assert!(number_before(&text, "link back after", " failed tries").unwrap() >= 2, "{text}");
        assert!(text.contains("quiet mode off, game no"), "{text}");
        assert!(!text.contains("sat_secret") && !text.contains(&addr.port().to_string()), "{text}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn an_answer_that_fails_to_go_out_stays_pending() {
        let shared = Shared::new(0.0);
        let now = ringing_here(&shared);
        let (cmds_tx, _cmds) = mpsc::unbounded_channel();
        assert!(answer_alarm(&shared, &cmds_tx, "stop", now));
        let broken = futures_util::sink::unfold((), |_, _m: Message| async {
            Err::<(), _>("the socket is gone")
        });
        let mut broken = std::pin::pin!(broken);
        assert!(send_done(&mut broken, &shared.alarms).await.is_err());
        assert_eq!(shared.alarms.pending_done().len(), 1, "kept for the next welcome");
        let mut sent = Vec::new();
        {
            let fine = futures_util::sink::unfold(&mut sent, |sent, m: Message| async move {
                sent.push(m);
                Ok::<_, std::convert::Infallible>(sent)
            });
            let mut fine = std::pin::pin!(fine);
            assert!(send_done(&mut fine, &shared.alarms).await.is_ok());
        }
        assert_eq!(sent.len(), 1);
        assert!(shared.alarms.pending_done().is_empty());
    }

    /// What one outage cost: from the server listening again to its welcome
    /// going out, and the tries made while it was gone.
    struct Outage {
        back: Duration,
        tries: usize,
    }

    /// The real reconnect loop against a server that welcomes it, holds the
    /// link past `HELD`, closes it the way the real one shuts down (1000 "bye")
    /// and is then gone for `secs`: not listening at all (`hole` false), or
    /// taking connections and never answering them, like a dead path (`hole`).
    async fn outage(secs: f64, hole: bool) -> Outage {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_link_tx, link) = watch::channel(Some(Link {
            url: format!("ws://{addr}"),
            token: "token".into(),
            device: "test".into(),
        }));
        let (_up_tx, up) = mpsc::channel(4);
        let (_cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events, mut seen) = mpsc::unbounded_channel();
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = tries.clone();
        let count = tokio::spawn(async move {
            while let Some(e) = seen.recv().await {
                if matches!(&e, Event::Status { text, .. } if text == "Connecting…") {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let client = tokio::spawn(run(link, up, cmds, Shared::new(0.0), pc(), events));
        // The link that worked.
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await;
        ws.send(Message::Text(WELCOME.into())).await.unwrap();
        let end = tokio::time::Instant::now() + HELD * 2;
        while let Ok(Some(Ok(m))) = tokio::time::timeout_at(end, ws.next()).await {
            if matches!(&m, Message::Text(t) if t.contains("\"ping\"")) {
                ws.send(Message::Text(r#"{"type":"pong"}"#.into())).await.unwrap();
            }
        }
        let bye = CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        };
        let _ = ws.close(Some(bye)).await;
        while let Some(Ok(_)) = ws.next().await {}
        let before = tries.load(Ordering::SeqCst);
        let gone = Duration::from_secs_f64(secs);
        // Sockets the dead path took, kept open so tries on them time out.
        let mut held = Vec::new();
        let listener = if hole {
            let until = tokio::time::Instant::now() + gone;
            while let Ok(Ok((tcp, _))) = tokio::time::timeout_at(until, listener.accept()).await {
                held.push(tcp);
            }
            listener
        } else {
            drop(listener);
            tokio::time::sleep(gone).await;
            TcpListener::bind(addr).await.expect("the port came back")
        };
        let back = Instant::now();
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await;
        ws.send(Message::Text(WELCOME.into())).await.unwrap();
        let back = back.elapsed();
        let tries = tries.load(Ordering::SeqCst) - before - 1;
        client.abort();
        count.abort();
        drop(held);
        Outage { back, tries }
    }

    /// Not a check: prints how long the link takes to come back after outages
    /// of a few lengths, and how often it tried meanwhile.
    /// `cargo test -p adrian-desktop reconnect_after_outages -- --ignored --nocapture --test-threads 1`
    /// (`ADRIAN_OUTAGES=3,10` for other lengths, `ADRIAN_OUTAGE_RUNS` for repeats).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn reconnect_after_outages() {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let t = Instant::now();
        let r = tokio_tungstenite::connect_async(format!("ws://{addr}")).await;
        println!("one refused connect: {:?} (failed: {})", t.elapsed(), r.is_err());
        let lengths: Vec<f64> = std::env::var("ADRIAN_OUTAGES")
            .ok()
            .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![3.0, 10.0, 21.0, 60.0]);
        let runs: usize = std::env::var("ADRIAN_OUTAGE_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        for hole in [false, true] {
            for &secs in &lengths {
                for _ in 0..runs {
                    let o = outage(secs, hole).await;
                    println!(
                        "outage {} {secs:>4} s: back {:>6} ms after, {} tries meanwhile",
                        if hole { "dead path" } else { "refused  " },
                        o.back.as_millis(),
                        o.tries
                    );
                }
            }
        }
    }
}
