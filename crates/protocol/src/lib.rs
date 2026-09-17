//! Adrian wire protocol: versioned, typed messages exchanged between the
//! Adrian server and native satellites (and, later, other endpoints).
//!
//! Every message carries a [`Envelope`] with a request id, protocol version,
//! device context, deadline, and optional idempotency key so the server can
//! deduplicate retried deliveries.

use serde::{Deserialize, Serialize};

/// Current protocol version. Bump when `Message` changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum bytes for a single protocol frame (defense in depth; also enforced
/// by transport-level limits).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum seconds of audio a satellite may send per activated session.
pub const MAX_AUDIO_SECONDS: u32 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Unique id for this request/message (uuid v4 text).
    pub request_id: String,
    pub protocol_version: u32,
    /// Device the message originates from or is addressed to.
    pub device_id: Option<String>,
    /// Unix-seconds deadline after which the receiver may drop the work.
    pub deadline_unix: Option<i64>,
    /// Idempotency key; receivers must not apply the same key twice.
    pub idempotency_key: Option<String>,
    #[serde(flatten)]
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    // ---- pairing ----
    PairRequest {
        pairing_code: String,
        device_name: String,
    },
    PairResponse {
        device_id: String,
        device_token: String,
        room: Option<String>,
    },
    PairRejected {
        reason: String,
    },

    // ---- sessions / audio ----
    /// Satellite -> server: an activated command session (post wake or PTT).
    AudioSessionStart {
        room: Option<String>,
        sample_rate: u32,
    },
    /// Satellite -> server: raw PCM s16le frames (base64 in JSON framing is
    /// avoided; binary frames are used on the wire — this variant is for the
    /// JSON control channel only).
    AudioSessionEnd {
        reason: SessionEndReason,
    },
    /// Server -> satellite: transcribed command text (STT result).
    Transcript {
        text: String,
        is_final: bool,
    },
    /// Server -> satellite: speak/play this audio. `media_url` is relative to
    /// the server and fetched over the authenticated channel.
    Speak {
        media_url: String,
        text: Option<String>,
    },
    /// Satellite -> server: lifecycle status update.
    Status {
        state: SatelliteState,
        detail: Option<String>,
    },
    /// Generic acknowledgement tied to `request_id` in the envelope.
    Ack {
        ok: bool,
        detail: Option<String>,
    },

    // ---- alarms ----
    /// Server -> satellite: persist a local alarm replica (later gate).
    AlarmReplicaUpsert {
        alarm: AlarmReplica,
    },
    AlarmReplicaCancel {
        alarm_id: String,
        occurrence_unix: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    Endpointed,
    UserStopped,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteState {
    Idle,
    Listening,
    Activated,
    Speaking,
    Muted,
    Disconnected,
}

/// A durable alarm replica pushed to a capable satellite so a critical alarm
/// can fire locally even if the server is unreachable (Gate C deliverable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmReplica {
    pub alarm_id: String,
    pub occurrence_unix: i64,
    /// Content-addressed cached sound id the satellite already holds.
    pub sound_id: String,
    pub volume: f32,
    pub label: String,
    pub schedule_version: u64,
}

impl Envelope {
    pub fn new(message: Message) -> Self {
        Self {
            request_id: uuid_v4(),
            protocol_version: PROTOCOL_VERSION,
            device_id: None,
            deadline_unix: None,
            idempotency_key: None,
            message,
        }
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Small dependency-free random id: time + counter + process entropy.
    // Sufficient for request correlation; cryptographic tokens live in the
    // server crate which has a CSPRNG.
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{:016x}-{:016x}",
        nanos,
        c ^ (std::process::id() as u64) << 32
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips_with_flattened_message() {
        let mut env = Envelope::new(Message::Status {
            state: SatelliteState::Listening,
            detail: Some("mic ok".into()),
        });
        env.device_id = Some("sat-1".into());
        env.idempotency_key = Some("k1".into());
        let json = serde_json::to_string(&env).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "status");
        assert_eq!(v["state"], "listening");
        assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, env.request_id);
        assert_eq!(back.device_id.as_deref(), Some("sat-1"));
        assert!(matches!(back.message, Message::Status { .. }));
    }

    #[test]
    fn request_ids_are_unique() {
        let a = Envelope::new(Message::Ack { ok: true, detail: None });
        let b = Envelope::new(Message::Ack { ok: true, detail: None });
        assert_ne!(a.request_id, b.request_id);
    }
}
