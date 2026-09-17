//! Audio endpoint plumbing: browser playback channels (SSE), short-lived
//! media grants for tokenized alarm-sound URLs, delivery acknowledgments,
//! and a built-in alarm tone generated in-process (no external assets, no
//! licensing concerns).

use crate::error::ApiError;
use crate::util;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AudioCommand {
    Play {
        delivery_id: String,
        url: String,
        volume: f64,
        label: String,
    },
    Stop {
        delivery_id: String,
    },
}

struct MediaGrant {
    kind: String,
    expires_unix: i64,
}

pub struct AudioRegistry {
    channels: Mutex<HashMap<String, broadcast::Sender<AudioCommand>>>,
    pending_acks: Mutex<HashMap<String, oneshot::Sender<String>>>,
    media: Mutex<HashMap<String, MediaGrant>>,
    tone: Mutex<Option<Vec<u8>>>,
}

impl Default for AudioRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRegistry {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            pending_acks: Mutex::new(HashMap::new()),
            media: Mutex::new(HashMap::new()),
            tone: Mutex::new(None),
        }
    }

    /// Attach a receiver to a browser audio channel, creating the channel
    /// lazily on first connect.
    pub fn attach_channel(&self, channel_id: &str) -> broadcast::Receiver<AudioCommand> {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(channel_id.to_string())
            .or_insert_with(|| broadcast::channel(16).0)
            .subscribe()
    }

    pub fn unregister_channel(&self, id: &str) {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    pub fn send_to_channel(&self, channel_id: &str, cmd: &AudioCommand) -> Result<(), ApiError> {
        let tx = {
            self.channels
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(channel_id)
                .cloned()
        };
        let Some(tx) = tx else {
            return Err(ApiError::NotConfigured(
                "browser audio channel is not connected".into(),
            ));
        };
        tx.send(cmd.clone())
            .map_err(|_| ApiError::NotConfigured("browser audio channel closed".into()))?;
        Ok(())
    }

    /// Stop playback on every connected browser channel for a delivery.
    pub fn broadcast_stop(&self, alarm_id: &str) {
        let txs: Vec<broadcast::Sender<AudioCommand>> = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for tx in txs {
            let _ = tx.send(AudioCommand::Stop {
                delivery_id: alarm_id.to_string(),
            });
        }
    }

    /// Mint a short-lived token that authorizes one media fetch without a
    /// session (HA media players cannot carry cookies).
    pub fn grant_media(&self, kind: &str) -> String {
        let token = util::new_id();
        self.media.lock().unwrap_or_else(|e| e.into_inner()).insert(
            token.clone(),
            MediaGrant {
                kind: kind.to_string(),
                expires_unix: util::now_unix() + 600,
            },
        );
        token
    }

    /// Validate + consume nothing (grants may be fetched more than once —
    /// e.g. retry — until expiry).
    pub fn check_media(&self, token: &str) -> Option<String> {
        let mut map = self.media.lock().unwrap_or_else(|e| e.into_inner());
        let grant = map.get(token)?;
        if grant.expires_unix < util::now_unix() {
            map.remove(token);
            return None;
        }
        Some(grant.kind.clone())
    }

    /// Arm a oneshot the ack endpoint resolves for a delivery.
    pub async fn wait_ack(&self, delivery_id: &str, timeout: Duration) -> Result<String, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.pending_acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(delivery_id.to_string(), tx);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(_)) => Err(ApiError::internal_msg("ack channel dropped")),
            Err(_) => Err(ApiError::Timeout("browser playback ack".into())),
        }
    }

    pub fn resolve_ack(&self, delivery_id: &str, result: String) {
        if let Some(tx) = self
            .pending_acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(delivery_id)
        {
            let _ = tx.send(result);
        }
    }

    /// 16-bit PCM mono WAV, 22050 Hz: a soft two-note chime (~2.4 s) with a
    /// raised-cosine envelope. Generated, so no external file/license is
    /// required for the baseline alarm sound.
    pub fn builtin_tone_wav(&self) -> Vec<u8> {
        if let Some(v) = self.tone.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return v;
        }
        let wav = generate_tone_wav();
        *self.tone.lock().unwrap_or_else(|e| e.into_inner()) = Some(wav.clone());
        wav
    }
}

fn generate_tone_wav() -> Vec<u8> {
    let rate = 22050u32;
    // note sequence: (freq Hz, ms)
    let seq = [
        (880.0f32, 300u32),
        (660.0, 300),
        (880.0, 300),
        (1108.7, 500),
    ];
    let mut pcm: Vec<i16> = Vec::new();
    for (freq, ms) in seq {
        let n = (rate as u64 * ms as u64 / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let env = (std::f32::consts::PI * i as f32 / n as f32).sin().powi(2);
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.6;
            pcm.push((s * i16::MAX as f32) as i16);
        }
        // short gap
        pcm.extend(std::iter::repeat_n(0, (rate / 20) as usize));
    }
    // repeat once for a fuller alarm
    let cycle = pcm.clone();
    pcm.extend_from_slice(&cycle);

    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}
