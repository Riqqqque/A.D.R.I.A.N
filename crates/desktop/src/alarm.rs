//! Alarms on this PC. While one rings (the server streams its sound) the
//! window and tray offer Stop and Snooze, and muted or nearly silent speakers
//! are turned up, then put back. The server also sends the alarms coming up:
//! if it can't be reached when one is due (or doesn't ring it), the app rings
//! it itself and tells the server how it was answered: right away when the
//! link is up, or once it's back.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// The server gets this long to ring an alarm before the PC does it itself.
pub const GRACE_MS: f64 = 20_000.0;
/// Speakers below this (or muted) are turned up while an alarm rings.
pub const FLOOR: f32 = 0.3;
/// Kept at most, so a bad message can't grow the file.
const MAX_KEPT: usize = 64;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Replica {
    pub id: String,
    pub kind: String,
    pub label: String,
    /// When it rings next, epoch ms.
    pub at: f64,
    /// The occurrence it's for, epoch ms.
    pub base: f64,
}

/// An alarm the PC rang itself and someone answered here.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Done {
    pub id: String,
    pub base: f64,
    pub action: String,
    pub at: f64,
}

/// An occurrence answered here. Until the server's list shows it moved on, a
/// list that still has it at or before `answered` was made before the server
/// heard the answer (the hourly resync, one that crossed the PC's
/// `alarm_done`, or the list sent at connect, before the answers go out).
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
struct Answered {
    id: String,
    base: f64,
    /// When it was answered (the server's clock).
    answered: f64,
    /// Snoozed: when it rings again. None: stopped.
    until: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    list: Vec<Replica>,
    done: Vec<Done>,
    /// Files from before 0.17.2 don't have it.
    #[serde(default)]
    answered: Vec<Answered>,
    snooze_min: f64,
    ring_min: f64,
    /// The speakers' level and mute before an alarm raised them.
    restore: Option<(f32, bool)>,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            list: vec![],
            done: vec![],
            answered: vec![],
            snooze_min: 9.0,
            ring_min: 10.0,
            restore: None,
        }
    }
}

#[derive(Clone, Serialize, Debug, PartialEq, Default)]
pub struct View {
    pub ringing: bool,
    /// "7:00 alarm: gym", "Pasta timer".
    pub label: String,
    pub timer: bool,
    /// Rung by the PC itself (the server is out of reach).
    pub local: bool,
}

#[derive(Default)]
struct State {
    saved: Saved,
    /// What the server says is ringing now.
    server: Vec<Replica>,
    /// When each of those was first seen ringing (the server's clock).
    server_seen: Vec<(String, f64)>,
    /// Ringing here by itself, since then.
    local: Option<(Vec<Replica>, f64)>,
    /// Occurrences already rung here (id, at), so one is never rung twice.
    rung: HashSet<(String, u64)>,
    /// The server's clock minus this PC's, ms: due times are the server's.
    skew: f64,
}

/// What Stop or Snooze did.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// Handled here (it was ringing by itself); the server hears as soon as it can.
    Here,
    /// The server rings it: send it the order.
    Server,
    Nothing,
}

#[derive(Default)]
pub struct Alarms {
    s: Mutex<State>,
    file: Mutex<Option<PathBuf>>,
}

fn read(m: &Value) -> Option<Replica> {
    let s = |k: &str| m.get(k).and_then(Value::as_str).map(|x| x.chars().take(80).collect::<String>());
    let n = |k: &str| m.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
    Some(Replica {
        id: s("id")?,
        kind: s("kind").unwrap_or_else(|| "alarm".into()),
        label: s("label").unwrap_or_default(),
        at: n("at").or_else(|| n("base"))?,
        base: n("base")?,
    })
}

impl Alarms {
    /// Use this file for the copy (the settings folder); loads what's there.
    pub fn load(&self, file: PathBuf) {
        if let Some(saved) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Saved>(&t).ok()) {
            self.lock().saved = saved;
        }
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.s.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, st: &State) {
        let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string(&st.saved) {
            let tmp = f.with_extension("tmp");
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &f);
            }
        }
    }

    /// The server's `alarms` message. The server is back: what it says wins,
    /// and anything ringing here by itself stops once the server rings it or
    /// has moved on from it (that occurrence is gone from its list). One the
    /// server still lists as due but isn't ringing keeps ringing here: the
    /// server resends the list every hour and on every change, and that alone
    /// mustn't silence an alarm nobody answered.
    ///
    /// An occurrence answered here that the list still shows unanswered keeps
    /// the answer (see `Answered`): a snoozed one rings at its new time, a
    /// stopped one is left out.
    pub fn server_said(&self, m: &Value) {
        let here = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        self.said(m, here);
    }

    fn said(&self, m: &Value, here: f64) {
        let mut st = self.lock();
        let list = |k: &str| -> Vec<Replica> { m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(read).take(MAX_KEPT).collect()).unwrap_or_default() };
        let mut fresh = list("list");
        st.server = list("ringing");
        if let Some(t) = m.get("now").and_then(Value::as_f64).filter(|t| t.is_finite()) {
            st.skew = t - here;
        }
        let now = here + st.skew;
        // Answers the list doesn't know about yet. The server moved it on (a
        // later time, another occurrence) or dropped it: then it knows.
        let answered = std::mem::take(&mut st.saved.answered);
        st.saved.answered = answered
            .into_iter()
            .filter(|w| fresh.iter().any(|r| r.id == w.id && r.base == w.base && r.at <= w.answered))
            .collect();
        fresh.retain_mut(|r| match st.saved.answered.iter().find(|w| w.id == r.id && w.base == r.base) {
            Some(w) => w.until.map(|t| r.at = t).is_some(),
            None => true,
        });
        st.saved.list = fresh;
        // When each ringing one was first seen, so one carried over when the
        // link goes rings only for what's left of its ring time.
        let seen = std::mem::take(&mut st.server_seen);
        st.server_seen = st.server.iter().map(|r| (r.id.clone(), seen.iter().find(|s| s.0 == r.id).map_or(now, |s| s.1))).collect();
        if let Some(v) = m.get("snoozeMin").and_then(Value::as_f64).filter(|v| (1.0..=60.0).contains(v)) {
            st.saved.snooze_min = v;
        }
        if let Some(v) = m.get("ringMin").and_then(Value::as_f64).filter(|v| (1.0..=30.0).contains(v)) {
            st.saved.ring_min = v;
        }
        if let Some((list, since)) = st.local.take() {
            // Not even due yet by the server's clock: this PC's clock jumped
            // ahead since the last list. It rings at its real time instead.
            let (keep, early): (Vec<Replica>, Vec<Replica>) = list
                .into_iter()
                .filter(|a| !st.server.iter().any(|r| r.id == a.id) && st.saved.list.iter().any(|r| r.id == a.id && r.at == a.at))
                .partition(|a| a.at + GRACE_MS <= now);
            for a in early {
                st.rung.remove(&(a.id, a.at as u64));
            }
            if !keep.is_empty() {
                st.local = Some((keep, since));
            }
        }
        self.save(&st);
    }

    /// The link to the server is gone. Whatever it was ringing keeps ringing,
    /// here now: the server may be what died.
    pub fn link_lost(&self, now: f64) {
        let mut st = self.lock();
        let was = std::mem::take(&mut st.server);
        let seen = std::mem::take(&mut st.server_seen);
        for a in &was {
            st.rung.insert((a.id.clone(), a.at as u64));
        }
        // It has been ringing since the server started it (a PC that slept
        // through the rest of its ring time doesn't start it over on waking).
        let now = now + st.skew;
        let since = was.iter().filter_map(|a| seen.iter().find(|s| s.0 == a.id).map(|s| s.1)).fold(now, f64::min);
        match &mut st.local {
            // Already ringing one here: the server's join it.
            Some((list, _)) => {
                for a in was {
                    if !list.iter().any(|l| l.id == a.id) {
                        list.push(a);
                    }
                }
            }
            None if !was.is_empty() => st.local = Some((was, since)),
            None => {}
        }
    }

    /// Once a tick. An alarm the server hasn't rung within the grace rings
    /// here (the server may be down, or up but not ringing it); one ringing
    /// here stops after the ring time. True: ringing here.
    pub fn tick(&self, now: f64) -> bool {
        let mut st = self.lock();
        let now = now + st.skew;
        let ring_ms = st.saved.ring_min * 60_000.0;
        if st.local.as_ref().is_some_and(|(_, since)| now - since > ring_ms) {
            st.local = None;
        }
        // A ringing alarm is the server's (it says which): only one it never started counts.
        if st.local.is_none() {
            let due: Vec<Replica> = st
                .saved
                .list
                .iter()
                .filter(|a| a.at + GRACE_MS <= now && now - a.at < ring_ms && !st.rung.contains(&(a.id.clone(), a.at as u64)) && !st.server.iter().any(|r| r.id == a.id))
                .cloned()
                .collect();
            if !due.is_empty() {
                for a in &due {
                    st.rung.insert((a.id.clone(), a.at as u64));
                }
                st.local = Some((due, now));
            }
        }
        st.local.is_some()
    }

    /// Stop or Snooze from the window or the tray.
    pub fn answer(&self, action: &str, now: f64) -> Answer {
        let mut st = self.lock();
        let now = now + st.skew;
        if let Some((list, _)) = st.local.take() {
            let snooze = st.saved.snooze_min * 60_000.0;
            for a in &list {
                st.saved.done.push(Done { id: a.id.clone(), base: a.base, action: action.into(), at: now });
                st.saved.answered.retain(|w| w.id != a.id);
                let until = (action == "snooze").then_some(now + snooze);
                if st.saved.answered.len() < MAX_KEPT {
                    st.saved.answered.push(Answered { id: a.id.clone(), base: a.base, answered: now, until });
                }
                if action == "snooze" {
                    // Still offline then: it rings here again. One the server was
                    // ringing when the link went isn't in the copy (the server leaves
                    // out what's ringing), so it goes back in.
                    if let Some(r) = st.saved.list.iter_mut().find(|r| r.id == a.id) {
                        r.at = now + snooze;
                    } else if st.saved.list.len() < MAX_KEPT {
                        st.saved.list.push(Replica { at: now + snooze, ..a.clone() });
                    }
                } else {
                    st.saved.list.retain(|r| r.id != a.id);
                }
            }
            let n = st.saved.done.len();
            if n > MAX_KEPT {
                st.saved.done.drain(..n - MAX_KEPT);
            }
            self.save(&st);
            return Answer::Here;
        }
        if st.server.is_empty() { Answer::Nothing } else { Answer::Server }
    }

    /// What was answered here, for the server (sent after the welcome). They
    /// stay until `sent_done` says they went out.
    pub fn pending_done(&self) -> Vec<Value> {
        let st = self.lock();
        st.saved.done.iter().map(|d| json!({ "type": "alarm_done", "id": d.id, "base": d.base, "action": d.action, "at": d.at })).collect()
    }

    /// The first `n` of `pending_done` reached the server.
    pub fn sent_done(&self, n: usize) {
        let mut st = self.lock();
        let n = n.min(st.saved.done.len());
        if n > 0 {
            st.saved.done.drain(..n);
            self.save(&st);
        }
    }

    pub fn ringing(&self) -> bool {
        let st = self.lock();
        st.local.is_some() || !st.server.is_empty()
    }

    pub fn view(&self) -> View {
        let st = self.lock();
        let (list, local) = match &st.local {
            Some((l, _)) => (l.clone(), true),
            None => (st.server.clone(), false),
        };
        let Some(first) = list.first() else { return View::default() };
        let timer = list.iter().all(|a| a.kind == "timer");
        let label = if timer {
            if first.label.is_empty() { "Timer".into() } else { format!("{} timer", first.label) }
        } else if first.label.is_empty() {
            "Alarm".into()
        } else {
            format!("Alarm: {}", first.label)
        };
        View { ringing: true, label, timer, local }
    }

    /// Before raising the speakers: remember how they were (saved first, so a
    /// crash still puts them back). None: nothing to change.
    pub fn plan_raise(&self, now: (f32, bool)) -> Option<f32> {
        let mut st = self.lock();
        if st.saved.restore.is_some() || (!now.1 && now.0 >= FLOOR) {
            return None;
        }
        st.saved.restore = Some(now);
        self.save(&st);
        Some(now.0.max(FLOOR))
    }

    /// After: how they were, once.
    pub fn take_restore(&self) -> Option<(f32, bool)> {
        let mut st = self.lock();
        let r = st.saved.restore.take();
        if r.is_some() {
            self.save(&st);
        }
        r
    }
}

/// The PC's own alarm sound: classic beeps, 48 kHz mono s16le like her voice.
#[derive(Default)]
pub struct Beeper {
    pos: usize,
}

impl Beeper {
    const RATE: usize = 48_000;
    /// One cycle: four beeps, then a pause.
    const CYCLE: usize = Self::RATE * 16 / 10;

    pub fn next(&mut self, samples: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for _ in 0..samples {
            let c = self.pos % Self::CYCLE;
            let t = c as f32 / Self::RATE as f32;
            let beep = (t / 0.125) as usize;
            let within = t - beep as f32 * 0.125;
            let mut v = 0.0;
            if beep < 4 && within < 0.07 {
                let ph = std::f32::consts::TAU * 1000.0 * t;
                // A soft square, with a few ms of fade at each edge.
                let edge = (within / 0.003).min((0.07 - within) / 0.006).min(1.0);
                v = (ph.sin() + (3.0 * ph).sin() / 3.0 + (5.0 * ph).sin() / 5.0) * 0.3 * edge;
            }
            out.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
            self.pos += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(at: f64) -> Value {
        json!({ "type": "alarms", "ringing": [], "list": [{ "id": "a1", "kind": "alarm", "label": "gym", "at": at, "base": at }], "snoozeMin": 5, "ringMin": 10 })
    }

    #[test]
    fn the_server_dying_mid_ring_leaves_it_ringing_here() {
        let a = Alarms::default();
        a.server_said(&json!({ "type": "alarms", "ringing": [{ "id": "a1", "kind": "alarm", "label": "gym", "base": 0.0 }], "list": [] }));
        assert!(!a.tick(1_000.0), "the server rings it");
        a.link_lost(2_000.0);
        assert!(a.tick(2_200.0));
        assert!(a.view().local);
        assert_eq!(a.answer("stop", 3_000.0), Answer::Here);
        assert_eq!(a.pending_done()[0]["action"], "stop");
    }

    #[test]
    fn online_but_never_rung_by_the_server_it_rings_here() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(!a.tick(GRACE_MS - 1.0));
        assert!(a.tick(GRACE_MS + 1.0), "the server is up but didn't ring it");
    }

    #[test]
    fn offline_it_rings_by_itself_after_the_grace_and_only_once() {
        let a = Alarms::default();
        a.server_said(&state(1_000_000.0));
        assert!(!a.tick(1_000_000.0 + 5_000.0), "the server still has time");
        assert!(a.tick(1_000_000.0 + GRACE_MS + 1.0));
        assert_eq!(a.view().label, "Alarm: gym");
        assert!(a.view().local);
        // The ring time runs out: it stops, and doesn't start again.
        assert!(!a.tick(1_000_000.0 + 11.0 * 60_000.0));
        assert!(!a.tick(1_000_000.0 + 11.0 * 60_000.0 + 1.0));
    }

    #[test]
    fn answered_here_is_told_to_the_server_later_and_a_snooze_rings_again() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        assert!(!a.tick(31_000.0));
        assert!(a.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0), "rings again after the snooze");
        assert_eq!(a.answer("stop", 400_000.0), Answer::Here);
        let done = a.pending_done();
        assert_eq!(done.len(), 2);
        assert_eq!(done[0]["action"], "snooze");
        assert_eq!(done[1]["action"], "stop");
        a.sent_done(1);
        assert_eq!(a.pending_done().len(), 1, "only what went out is dropped");
        a.sent_done(1);
        assert!(a.pending_done().is_empty());
        assert!(!a.tick(10_000_000.0), "a stopped one is gone");
    }

    #[test]
    fn the_server_coming_back_wins() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        a.server_said(&json!({ "type": "alarms", "ringing": [{ "id": "a1", "kind": "alarm", "label": "gym", "base": 0.0 }], "list": [] }));
        assert!(a.ringing());
        assert!(!a.view().local);
        assert_eq!(a.answer("stop", 50_000.0), Answer::Server);
        a.server_said(&json!({ "type": "alarms", "ringing": [], "list": [] }));
        assert_eq!(a.answer("stop", 60_000.0), Answer::Nothing);
    }

    #[test]
    fn muted_speakers_go_up_and_back_once() {
        let a = Alarms::default();
        assert_eq!(a.plan_raise((0.6, false)), None, "loud enough already");
        assert_eq!(a.plan_raise((0.1, false)), Some(FLOOR));
        assert_eq!(a.plan_raise((0.3, false)), None, "already raised");
        assert_eq!(a.take_restore(), Some((0.1, false)));
        assert_eq!(a.take_restore(), None);
        assert_eq!(a.plan_raise((0.8, true)), Some(0.8), "muted: unmuted at its own level");
    }

    #[test]
    fn a_bad_message_changes_nothing_dangerous() {
        let a = Alarms::default();
        a.server_said(&json!({ "type": "alarms", "list": [{ "id": 5 }, { "id": "x", "base": "soon" }, { "id": "ok", "base": 1.0 }] }));
        assert!(a.tick(GRACE_MS + 2.0));
    }

    fn epoch_now() -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64
    }

    #[test]
    fn a_resync_from_the_server_doesnt_silence_one_ringing_here() {
        // The server is up but never rang it; it resends the list every hour and on every change.
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        a.server_said(&state(0.0));
        assert!(a.ringing(), "it stopped without anyone answering it");
        assert!(a.view().local);
        assert_eq!(a.answer("stop", 60_000.0), Answer::Here);
        assert_eq!(a.pending_done().len(), 1, "and the server hears how it was answered");
    }

    #[test]
    fn snoozed_here_while_linked_a_stale_resync_doesnt_lose_the_snooze() {
        // The server is up but didn't ring it; the PC did, and it's snoozed here.
        // The server's next list (hourly, or one that crossed the PC's alarm_done)
        // still shows the occurrence at the time it already rang.
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        a.server_said(&state(0.0));
        assert!(!a.tick(31_000.0), "not before the snooze is up");
        assert!(a.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0), "the snooze was lost");
    }

    #[test]
    fn once_the_server_has_the_snooze_its_time_wins() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        // The server applied it with its own snooze length (this alarm's is 7 min).
        let theirs = 30_000.0 + 7.0 * 60_000.0;
        a.server_said(&json!({ "type": "alarms", "ringing": [], "snoozeMin": 5,
            "list": [{ "id": "a1", "kind": "alarm", "label": "gym", "at": theirs, "base": 0.0 }] }));
        assert!(!a.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0), "rang at the PC's time, not the server's");
        assert!(a.tick(theirs + GRACE_MS + 1.0));
    }

    #[test]
    fn snoozed_here_then_stopped_or_moved_on_elsewhere_it_stays_quiet() {
        // Stopped from Discord after the PC's snooze: the server lists the next occurrence.
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        a.server_said(&state(86_400_000.0));
        assert!(!a.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0));
        // Deleted there: gone from the list altogether.
        let b = Alarms::default();
        b.server_said(&state(0.0));
        assert!(b.tick(GRACE_MS + 1.0));
        assert_eq!(b.answer("snooze", 30_000.0), Answer::Here);
        b.server_said(&json!({ "type": "alarms", "ringing": [], "list": [] }));
        assert!(!b.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0));
    }

    #[test]
    fn stopped_here_while_linked_a_stale_resync_doesnt_ring_it_again() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        assert!(a.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0));
        assert_eq!(a.answer("stop", 400_000.0), Answer::Here);
        // The server hasn't heard either answer yet.
        a.server_said(&state(0.0));
        assert!(!a.tick(401_000.0));
        assert!(!a.tick(30_000.0 + 10.0 * 60_000.0 + GRACE_MS + 1.0), "a stopped one rang again");
    }

    #[test]
    fn a_stale_resync_while_the_snooze_rings_doesnt_silence_it() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
        let again = 30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0;
        assert!(a.tick(again));
        a.server_said(&state(0.0));
        assert!(a.ringing(), "the snoozed ring stopped without anyone answering it");
        assert!(a.tick(again + 1_000.0));
    }

    #[test]
    fn a_snooze_survives_a_restart_before_the_server_hears_it() {
        let dir = std::env::temp_dir().join(format!("adrian-alarm-snooze-{}", std::process::id()));
        let file = dir.join("alarms.json");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let a = Alarms::default();
            a.load(file.clone());
            a.server_said(&state(0.0));
            assert!(a.tick(GRACE_MS + 1.0));
            assert_eq!(a.answer("snooze", 30_000.0), Answer::Here);
            // ...an update restarts the app.
        }
        let b = Alarms::default();
        b.load(file.clone());
        // The list sent at connect comes before the answers go out.
        b.server_said(&state(0.0));
        assert!(!b.tick(31_000.0), "it rang again straight away");
        assert!(b.tick(30_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0), "the snooze was lost");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_before_answers_were_kept_still_loads() {
        let dir = std::env::temp_dir().join(format!("adrian-alarm-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("alarms.json");
        std::fs::write(
            &file,
            r#"{"list":[{"id":"a1","kind":"alarm","label":"gym","at":5000.0,"base":5000.0}],"done":[{"id":"a0","base":1.0,"action":"stop","at":2.0}],"snooze_min":9.0,"ring_min":10.0,"restore":[0.1,false]}"#,
        )
        .unwrap();
        let a = Alarms::default();
        a.load(file);
        assert_eq!(a.pending_done().len(), 1);
        assert_eq!(a.take_restore(), Some((0.1, false)));
        assert!(a.tick(5_000.0 + GRACE_MS + 1.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rung_early_because_this_pcs_clock_jumped_ahead_it_waits_for_its_real_time() {
        let s = 1_000_000_000.0;
        let hour = 3_600_000.0;
        let list = |now: f64| json!({ "type": "alarms", "ringing": [], "now": now,
            "list": [{ "id": "a1", "kind": "alarm", "label": "", "at": s + hour, "base": s + hour }] });
        let a = Alarms::default();
        a.said(&list(s), s);
        // The PC's clock is set an hour and a minute ahead: by its old skew the
        // alarm looks a minute overdue.
        let jumped = s + hour + 60_000.0;
        assert!(a.tick(jumped + 1_000.0));
        // The next list says what time it really is (a few seconds later).
        a.said(&list(s + 5_000.0), jumped + 5_000.0);
        assert!(!a.ringing(), "still ringing an alarm an hour early");
        assert!(!a.tick(jumped + 10_000.0));
        // And it rings at its time on the server's clock.
        assert!(a.tick(jumped + 5_000.0 + (hour - 5_000.0) + GRACE_MS + 1.0), "it never rang");
    }

    #[test]
    fn waking_from_sleep_long_after_the_server_rang_it_doesnt_start_it_over() {
        // The server rang it, the PC slept mid-ring, and the dead link is only
        // noticed after waking, hours later.
        let ringing = json!({ "type": "alarms", "ringing": [{ "id": "a1", "kind": "alarm", "label": "gym", "base": 0.0 }], "list": [] });
        let a = Alarms::default();
        a.said(&ringing, 1_000.0);
        a.link_lost(5.0 * 3_600_000.0);
        assert!(!a.tick(5.0 * 3_600_000.0 + 200.0), "rang for ten minutes, five hours late");
        // A link lost two minutes in rings here for the rest of its ring time.
        let b = Alarms::default();
        b.said(&ringing, 1_000.0);
        b.link_lost(121_000.0);
        assert!(b.tick(121_200.0));
        assert!(b.tick(1_000.0 + 9.0 * 60_000.0));
        assert!(!b.tick(1_000.0 + 10.0 * 60_000.0 + 1_000.0));
    }

    #[test]
    fn asleep_through_its_whole_ring_time_it_stays_quiet_on_waking() {
        // Rung here offline; the PC sleeps longer than the ring time.
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        assert!(!a.tick(GRACE_MS + 30.0 * 60_000.0), "the ring time ran out while it slept");
        assert!(!a.tick(GRACE_MS + 30.0 * 60_000.0 + 200.0), "and it doesn't start over");
        assert_eq!(a.answer("stop", GRACE_MS + 31.0 * 60_000.0), Answer::Nothing);
    }

    #[test]
    fn a_resync_still_stops_it_once_the_server_rings_it_or_moved_on() {
        let a = Alarms::default();
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        // Answered somewhere else: the server's list has the next occurrence.
        a.server_said(&state(86_400_000.0));
        assert!(!a.ringing());
        assert!(!a.tick(GRACE_MS + 2.0), "and it doesn't start again");
    }

    #[test]
    fn snoozed_here_after_the_link_dropped_mid_ring_it_rings_again() {
        // The server's list leaves out what it's ringing, so the copy has no entry for it.
        let a = Alarms::default();
        a.server_said(&json!({ "type": "alarms", "ringing": [{ "id": "a1", "kind": "alarm", "label": "gym", "base": 0.0 }], "list": [], "snoozeMin": 5 }));
        a.link_lost(1_000.0);
        assert!(a.tick(1_200.0));
        assert_eq!(a.answer("snooze", 2_000.0), Answer::Here);
        assert!(!a.tick(3_000.0));
        assert!(a.tick(2_000.0 + 5.0 * 60_000.0 + GRACE_MS + 1.0), "the snooze never came back");
        assert_eq!(a.view().label, "Alarm: gym");
    }

    #[test]
    fn the_link_dropping_while_one_rings_here_keeps_the_servers_too() {
        let a = Alarms::default();
        // a1 rings here (the server never did); then the server starts ringing a2.
        a.server_said(&state(0.0));
        assert!(a.tick(GRACE_MS + 1.0));
        a.server_said(&json!({ "type": "alarms",
            "ringing": [{ "id": "a2", "kind": "timer", "label": "pasta", "base": 30_000.0 }],
            "list": [{ "id": "a1", "kind": "alarm", "label": "gym", "at": 0.0, "base": 0.0 }] }));
        a.link_lost(40_000.0);
        assert!(a.tick(40_200.0));
        assert_eq!(a.answer("stop", 41_000.0), Answer::Here);
        let done: Vec<String> = a.pending_done().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(done, ["a1", "a2"], "the server's alarm was dropped when the link went");
    }

    #[test]
    fn due_times_follow_the_servers_clock() {
        // This PC's clock is an hour behind the server's.
        let here = epoch_now();
        let server_now = here + 3_600_000.0;
        let a = Alarms::default();
        a.server_said(&json!({ "type": "alarms", "ringing": [], "now": server_now,
            "list": [{ "id": "a1", "kind": "alarm", "label": "", "at": server_now + 60_000.0, "base": server_now + 60_000.0 }] }));
        assert!(!a.tick(here + 60_000.0 + GRACE_MS - 1_000.0), "not yet by the server's clock");
        assert!(a.tick(here + 60_000.0 + GRACE_MS + 1_000.0));
        // Its ring time runs on the same clock: ten minutes from when it started here.
        assert!(a.tick(here + 60_000.0 + 9.0 * 60_000.0));
        assert!(!a.tick(here + 60_000.0 + GRACE_MS + 10.0 * 60_000.0 + 2_000.0));
    }

    #[test]
    fn speakers_raised_before_a_crash_are_put_back_at_the_next_start() {
        let dir = std::env::temp_dir().join(format!("adrian-alarm-test-{}", std::process::id()));
        let file = dir.join("alarms.json");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let a = Alarms::default();
            a.load(file.clone());
            a.server_said(&state(5_000.0));
            assert_eq!(a.plan_raise((0.05, true)), Some(FLOOR));
            // ...and the app dies here, with the speakers still up.
        }
        let b = Alarms::default();
        b.load(file.clone());
        assert_eq!(b.take_restore(), Some((0.05, true)));
        assert_eq!(b.take_restore(), None, "only once");
        // The copy came back too, and the restore is gone from the file.
        assert!(b.tick(5_000.0 + GRACE_MS + 1.0));
        let c = Alarms::default();
        c.load(file);
        assert_eq!(c.take_restore(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_starts_empty() {
        let dir = std::env::temp_dir().join(format!("adrian-alarm-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("alarms.json");
        std::fs::write(&file, b"{ not json").unwrap();
        let a = Alarms::default();
        a.load(file);
        assert_eq!(a.take_restore(), None);
        assert!(!a.tick(1e13));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn beeps_then_quiet() {
        let mut b = Beeper::default();
        let one = b.next(Beeper::CYCLE);
        let at = |s: f32| i16::from_le_bytes([one[(s * 48_000.0) as usize * 2], one[(s * 48_000.0) as usize * 2 + 1]]);
        assert!(one.chunks(2).take(3360).any(|x| i16::from_le_bytes([x[0], x[1]]).abs() > 5000));
        assert_eq!(at(1.2), 0);
        assert_eq!(one.len(), Beeper::CYCLE * 2);
    }
}
