//! Local wake-word spotting via rustpotter (feature `wake-word`).
//!
//! All audio stays on-device: the mic callback feeds a short rolling buffer
//! and this thread runs the detector on it. A detection fires a signal that
//! the session loop treats like a PTT press. Requires a trained rustpotter
//! wakeword file (`.rpw`, built with `rustpotter-cli` from a few recordings
//! of the phrase) — the feature is disabled unless
//! `config.wake_word.model_path` points at a readable file.

use crate::{Recorder, WakeWordCfg};
use anyhow::{Context, Result, anyhow, bail};
use rustpotter::{Rustpotter, RustpotterConfig};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawn the detector thread. Returns a receiver that yields `()` on each
/// detection (debounced — one activation per wake event).
pub fn spawn_detector(
    rec: Arc<Recorder>,
    cfg: &WakeWordCfg,
) -> Result<mpsc::UnboundedReceiver<()>> {
    if !std::path::Path::new(&cfg.model_path).is_file() {
        bail!("wake-word model not found: {}", cfg.model_path);
    }
    let sample_rate = rec.rate();
    let mut config = RustpotterConfig::default();
    config.fmt.sample_rate = sample_rate as usize;
    config.fmt.channels = 1;
    if let Some(t) = cfg.threshold {
        config.detector.threshold = t;
    }
    let mut detector = Rustpotter::new(&config).map_err(|e| anyhow!("rustpotter init: {e}"))?;
    detector
        .add_wakeword_from_file("wake", &cfg.model_path)
        .map_err(|e| anyhow!("load wakeword model: {e}"))?;

    let (tx, rx) = mpsc::unbounded_channel::<()>();
    std::thread::Builder::new()
        .name("adrian-wake".into())
        .spawn(move || {
            // Drain the recorder's rolling ring in ~30ms chunks.
            let chunk = (sample_rate as usize / 33).max(160);
            loop {
                let mut buf = Vec::with_capacity(chunk);
                {
                    let mut ring = rec.ring.lock().unwrap_or_else(|e| e.into_inner());
                    let n = chunk.min(ring.len());
                    buf.extend(ring.drain(..n));
                }
                if buf.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    continue;
                }
                let pcm: Vec<i16> = buf
                    .iter()
                    .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                if let Some(det) = detector.process_samples(pcm) {
                    eprintln!("wake word '{}' (score {:.2})", det.name, det.score);
                    if tx.send(()).is_err() {
                        return;
                    }
                    // Cool-down: flush the ring so the tail of the wake
                    // phrase can't immediately retrigger.
                    rec.ring.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        })
        .context("spawn wake detector")?;
    Ok(rx)
}
