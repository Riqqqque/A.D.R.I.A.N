//! Local alarm replicas.
//!
//! The server pushes the alarms routed to this satellite
//! (`AlarmReplicaSync`); they are kept in `replicas.json` next to the
//! config so they survive restarts. The server still owns delivery. A
//! replica rings here only when the server has not delivered it: at due
//! time while disconnected, or after [`CONNECTED_GRACE_SECS`] while
//! connected. Local firings are kept until the server acknowledges a
//! report of them.

use adrian_protocol::{AlarmRef, AlarmReplica, LocalAlarmFiring};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// While connected the server delivers; ring locally only if it has not
/// done so this long after the due time.
pub const CONNECTED_GRACE_SECS: i64 = 60;
/// Replicas older than this are dropped without ringing (the satellite was
/// off, and a stale alarm would only surprise).
pub const STALE_SECS: i64 = 6 * 3600;
/// Bound on remembered local firings awaiting a report ack.
const MAX_FIRED: usize = 200;

#[derive(Serialize, Deserialize, Default)]
struct StoreData {
    alarms: Vec<AlarmReplica>,
    fired: Vec<LocalAlarmFiring>,
}

pub struct ReplicaStore {
    path: PathBuf,
    data: Mutex<StoreData>,
}

fn same(a: &AlarmReplica, alarm_id: &str, occurrence: i64) -> bool {
    a.alarm_id == alarm_id && a.occurrence_unix == occurrence
}

impl ReplicaStore {
    pub fn open(path: PathBuf) -> Self {
        let data = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                eprintln!("ignoring unreadable {}: {e}", path.display());
                StoreData::default()
            }),
            Err(_) => StoreData::default(),
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut StoreData) -> (R, bool)) -> R {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let (r, changed) = f(&mut d);
        if changed {
            self.save(&d);
        }
        r
    }

    fn save(&self, d: &StoreData) {
        let Ok(text) = serde_json::to_string_pretty(d) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            eprintln!("cannot save {}: {e}", self.path.display());
        }
    }

    /// Replace the replica set with the server's authoritative list.
    /// Occurrences already rung locally are not re-added.
    pub fn replace_all(&self, alarms: Vec<AlarmReplica>) {
        self.with(|d| {
            d.alarms = alarms
                .into_iter()
                .filter(|a| {
                    !d.fired
                        .iter()
                        .any(|f| f.alarm_id == a.alarm_id && f.occurrence_unix == a.occurrence_unix)
                })
                .collect();
            ((), true)
        })
    }

    pub fn upsert(&self, alarm: AlarmReplica) {
        self.with(|d| {
            d.alarms.retain(|a| a.alarm_id != alarm.alarm_id);
            d.alarms.push(alarm);
            ((), true)
        })
    }

    pub fn cancel(&self, alarm_id: &str, occurrence: i64) {
        self.with(|d| {
            let before = d.alarms.len();
            d.alarms.retain(|a| !same(a, alarm_id, occurrence));
            ((), d.alarms.len() != before)
        })
    }

    /// The server is delivering this occurrence itself: drop the replica so
    /// it cannot also ring after the grace period.
    pub fn delivered_by_server(&self, r: &AlarmRef) {
        self.cancel(&r.alarm_id, r.occurrence_unix);
    }

    /// Whether this occurrence already rang locally.
    pub fn already_fired(&self, r: &AlarmRef) -> bool {
        self.with(|d| {
            let hit = d
                .fired
                .iter()
                .any(|f| f.alarm_id == r.alarm_id && f.occurrence_unix == r.occurrence_unix);
            (hit, false)
        })
    }

    /// Local firings not yet acknowledged by the server.
    pub fn pending_report(&self) -> Vec<LocalAlarmFiring> {
        self.with(|d| (d.fired.clone(), false))
    }

    /// The server acknowledged a report: forget those firings.
    pub fn settle_report(&self, reported: &[LocalAlarmFiring]) {
        self.with(|d| {
            let before = d.fired.len();
            d.fired.retain(|f| !reported.contains(f));
            ((), d.fired.len() != before)
        })
    }

    /// Remove and return replicas that should ring now, recording each as a
    /// local firing. Stale replicas are dropped silently.
    pub fn take_due(&self, now: i64, connected: bool) -> Vec<AlarmReplica> {
        self.with(|d| {
            let grace = if connected { CONNECTED_GRACE_SECS } else { 0 };
            let mut due = Vec::new();
            let before = d.alarms.len();
            d.alarms.retain(|a| {
                if now - a.occurrence_unix > STALE_SECS {
                    return false;
                }
                if now >= a.occurrence_unix + grace {
                    due.push(a.clone());
                    return false;
                }
                true
            });
            for a in &due {
                d.fired.push(LocalAlarmFiring {
                    alarm_id: a.alarm_id.clone(),
                    occurrence_unix: a.occurrence_unix,
                    fired_at_unix: now,
                });
            }
            if d.fired.len() > MAX_FIRED {
                let excess = d.fired.len() - MAX_FIRED;
                d.fired.drain(..excess);
            }
            let changed = d.alarms.len() != before;
            (due, changed)
        })
    }
}

/// A short two-tone beep as an in-memory WAV, scaled by `volume`.
pub fn tone_wav(volume: f32) -> Vec<u8> {
    const RATE: u32 = 22_050;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let amp = volume.clamp(0.05, 1.0) * 0.6 * i16::MAX as f32;
    let mut cursor = std::io::Cursor::new(Vec::new());
    if let Ok(mut w) = hound::WavWriter::new(&mut cursor, spec) {
        // Three beeps (880, 660, 880 Hz), 0.25 s each with 0.1 s gaps.
        for freq in [880.0f32, 660.0, 880.0] {
            for n in 0..(RATE as f32 * 0.25) as u32 {
                let t = n as f32 / RATE as f32;
                let s = (t * freq * std::f32::consts::TAU).sin() * amp;
                let _ = w.write_sample(s as i16);
            }
            for _ in 0..(RATE as f32 * 0.1) as u32 {
                let _ = w.write_sample(0i16);
            }
        }
        let _ = w.finalize();
    }
    cursor.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replica(id: &str, at: i64) -> AlarmReplica {
        AlarmReplica {
            alarm_id: id.into(),
            occurrence_unix: at,
            sound_id: "builtin:tone".into(),
            volume: 0.7,
            label: "Alarm".into(),
            schedule_version: 1,
        }
    }

    fn store() -> (tempfile::TempDir, ReplicaStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = ReplicaStore::open(dir.path().join("replicas.json"));
        (dir, s)
    }

    #[test]
    fn disconnected_rings_at_due_connected_waits_for_grace() {
        let (_d, s) = store();
        s.replace_all(vec![replica("a", 1000), replica("b", 2000)]);
        assert!(s.take_due(999, false).is_empty());
        assert!(s.take_due(1000 + CONNECTED_GRACE_SECS - 1, true).is_empty());
        let due = s.take_due(1000, false);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].alarm_id, "a");
        // Rung once only, and remembered for the report.
        assert!(s.take_due(1001, false).is_empty());
        assert_eq!(s.pending_report().len(), 1);
    }

    #[test]
    fn server_delivery_and_resync_prevent_double_rings() {
        let (_d, s) = store();
        s.replace_all(vec![replica("a", 1000), replica("b", 1000)]);
        s.delivered_by_server(&AlarmRef {
            alarm_id: "a".into(),
            occurrence_unix: 1000,
        });
        let due = s.take_due(1000, false);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].alarm_id, "b");
        let b = AlarmRef {
            alarm_id: "b".into(),
            occurrence_unix: 1000,
        };
        assert!(s.already_fired(&b));
        // A resync still listing b (server has not seen the report) does
        // not bring it back.
        s.replace_all(vec![replica("b", 1000)]);
        assert!(s.take_due(1100, false).is_empty());
        // Once acknowledged, the firing is forgotten.
        let pending = s.pending_report();
        s.settle_report(&pending);
        assert!(s.pending_report().is_empty());
    }

    #[test]
    fn stale_replicas_are_dropped_and_store_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replicas.json");
        {
            let s = ReplicaStore::open(path.clone());
            s.replace_all(vec![replica("old", 0), replica("new", 100_000)]);
            assert!(s.take_due(STALE_SECS + 1, false).is_empty());
        }
        let s = ReplicaStore::open(path);
        let due = s.take_due(100_000, false);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].alarm_id, "new");
    }

    #[test]
    fn tone_is_a_valid_wav() {
        let wav = tone_wav(0.7);
        let r = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        assert_eq!(r.spec().sample_rate, 22_050);
        assert!(r.len() > 10_000);
    }
}
