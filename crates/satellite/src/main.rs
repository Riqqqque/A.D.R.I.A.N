//! Adrian native Windows voice satellite.
//!
//! Push-to-talk (and optional local wake-word) voice client. Audio streams to
//! the Adrian server over a persistent websocket session
//! (`GET /api/satellite/session`): JSON control envelopes per
//! `adrian-protocol`, binary frames carrying PCM s16le mono.
//!
//! Pairing is one-use-code based: `adrian-satellite pair --server <url>
//! --code <6 digits> --name "Desk PC"` redeems a code minted in the web UI
//! and stores a device token in %APPDATA%\adrian\satellite\config\config.json.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures_util::{SinkExt, StreamExt};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMsg;

mod replicas;
mod tray;
#[cfg(feature = "wake-word")]
mod wake;

const PTT_MODS: Modifiers = Modifiers::CONTROL.union(Modifiers::ALT);
const PTT_KEY: Code = Code::Space;
const MAX_RECORD_SECS: usize = 60;
/// Binary PCM chunk size streamed per websocket frame.
const PCM_CHUNK_BYTES: usize = 64 * 1024;
/// Heartbeat cadence for `Status` updates (also refreshes `last_seen`).
const HEARTBEAT_SECS: u64 = 25;
/// Wake-word endpointing: end the capture after this much trailing silence.
const WAKE_SILENCE_SECS: f32 = 0.9;
/// RMS below this counts as silence for wake endpointing.
const WAKE_SILENCE_RMS: f32 = 0.02;
/// Minimum wake-capture length before silence may endpoint it.
const WAKE_MIN_SPEECH_SECS: f32 = 0.4;

#[derive(Serialize, Deserialize, Default, Clone)]
struct Config {
    server_url: String,
    device_token: String,
    satellite_id: String,
    name: String,
    conversation_id: Option<String>,
    /// Optional wake-word config. Requires a build with `--features
    /// wake-word` and a trained rustpotter `.rpw` model file. Audio never
    /// leaves the device; spotting runs locally on a rolling buffer.
    wake_word: Option<WakeWordCfg>,
    /// Extra PEM CA certificate to trust, for a server using a self-signed
    /// or private-CA certificate (`ADRIAN_TLS_CERT`). System roots are
    /// always trusted too.
    #[serde(default)]
    ca_cert: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct WakeWordCfg {
    /// Path to a rustpotter wakeword model (.rpw).
    model_path: String,
    /// Optional score threshold override (0.0-1.0). Default: model's own.
    threshold: Option<f32>,
}

fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "adrian", "satellite")
        .context("cannot resolve config directory")?;
    let dir = dirs.config_dir();
    std::fs::create_dir_all(dir)?;
    Ok(dir.join("config.json"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "not paired — run `adrian-satellite pair --server <url> --code <code> --name <name>` first"
        );
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

// ---------- audio capture ----------

/// Mic stream died (device unplugged, driver error). Set by the cpal error
/// callback; the run loop re-initializes the input device.
struct MicDeath {
    dead: AtomicBool,
    notify: Notify,
}

struct Recorder {
    /// Samples for the in-progress capture (post-activation only).
    samples: Mutex<Vec<f32>>,
    /// Rolling pre-activation buffer for wake-word spotting (local only).
    ring: Mutex<VecDeque<f32>>,
    recording: AtomicBool,
    /// True while wake-word spotting is armed (ring buffer fills).
    wake_armed: AtomicBool,
    sample_rate: AtomicU32,
}

impl Recorder {
    fn rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }
}

fn start_mic(
    rec: Arc<Recorder>,
    death: Arc<MicDeath>,
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
) -> Result<cpal::Stream> {
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();
    let rate = config.sample_rate().0;
    rec.sample_rate.store(rate, Ordering::Relaxed);

    let push = move |data: &[f32], rec: &Arc<Recorder>| {
        // First channel only — good enough for speech.
        let mono: Vec<f32> = data.chunks(channels).map(|f| f[0]).collect();
        if rec.wake_armed.load(Ordering::Relaxed)
            && let Ok(mut ring) = rec.ring.try_lock()
        {
            ring.extend(mono.iter().copied());
            let cap = rate as usize * 2; // 2s rolling window
            while ring.len() > cap {
                ring.pop_front();
            }
        }
        if !rec.recording.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut buf) = rec.samples.try_lock() {
            buf.extend_from_slice(&mono);
            let cap = rate as usize * MAX_RECORD_SECS;
            if buf.len() > cap {
                buf.truncate(cap);
            }
        }
    };

    let d = death.clone();
    let err = move |e| {
        eprintln!("mic stream error: {e}");
        d.dead.store(true, Ordering::Relaxed);
        d.notify.notify_one();
    };
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let r = rec.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| push(data, &r),
                err,
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let r = rec.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                    push(&f, &r);
                },
                err,
                None,
            )?
        }
        fmt => bail!("unsupported mic sample format {fmt:?}"),
    };
    stream.play()?;
    Ok(stream)
}

/// (Re)open the default input device. Returns None when no mic exists —
/// the satellite keeps running and retries on the next death tick.
fn init_mic(rec: &Arc<Recorder>, death: &Arc<MicDeath>) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    match start_mic(rec.clone(), death.clone(), &device, &config) {
        Ok(stream) => {
            println!("mic: {} Hz", rec.rate());
            Some(stream)
        }
        Err(e) => {
            eprintln!("mic init failed: {e:#}");
            None
        }
    }
}

fn f32_to_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

// ---------- speech / audio output ----------

fn speak(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || match tts::Tts::default() {
        Ok(mut t) => {
            if let Err(e) = t.speak(text, true) {
                eprintln!("tts error: {e}");
            }
        }
        Err(e) => eprintln!("tts init error: {e}"),
    });
}

/// Speak and wait until speech finishes (bounded to 60 s).
fn speak_blocking(text: &str) {
    match tts::Tts::default() {
        Ok(mut t) => {
            if let Err(e) = t.speak(text, true) {
                eprintln!("tts error: {e}");
                return;
            }
            for _ in 0..600 {
                if !t.is_speaking().unwrap_or(false) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Err(e) => eprintln!("tts init error: {e}"),
    }
}

/// Play server audio (alarm tone etc.) on the default output device.
fn play_bytes(bytes: Vec<u8>) {
    std::thread::spawn(move || play_bytes_blocking(bytes));
}

fn play_bytes_blocking(bytes: Vec<u8>) {
    let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
        eprintln!("no default audio output device");
        return;
    };
    let Ok(sink) = rodio::Sink::try_new(&handle) else {
        eprintln!("could not create playback sink");
        return;
    };
    match rodio::Decoder::new(Cursor::new(bytes)) {
        Ok(decoder) => {
            sink.append(decoder);
            sink.sleep_until_end();
        }
        Err(e) => eprintln!("audio decode failed: {e}"),
    }
}

/// Ring a local alarm replica: tone + spoken label, three rounds.
fn ring_local(alarm: adrian_protocol::AlarmReplica) {
    std::thread::spawn(move || {
        let tone = replicas::tone_wav(alarm.volume);
        for _ in 0..3 {
            play_bytes_blocking(tone.clone());
            speak_blocking(&alarm.label);
        }
    });
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Shared local-alarm state: the replica store, whether a server session is
/// up, and a signal that something rang locally and should be reported.
struct AlarmCtx {
    store: replicas::ReplicaStore,
    connected: AtomicBool,
    fired: Notify,
}

/// Check replicas once a second and ring the ones the server has not
/// delivered.
fn spawn_replica_watchdog(alarms: Arc<AlarmCtx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let connected = alarms.connected.load(Ordering::Relaxed);
            let due = alarms.store.take_due(now_unix(), connected);
            if due.is_empty() {
                continue;
            }
            for a in due {
                println!(
                    "ringing local alarm '{}' ({})",
                    a.label,
                    if connected {
                        "server did not deliver it"
                    } else {
                        "server unreachable"
                    }
                );
                ring_local(a);
            }
            alarms.fired.notify_one();
        }
    });
}

/// Report unacknowledged local firings; the ack settles them.
fn send_local_report(
    out: &mpsc::UnboundedSender<WsMsg>,
    cfg: &Config,
    alarms: &AlarmCtx,
    in_flight: &mut HashMap<String, Vec<adrian_protocol::LocalAlarmFiring>>,
) {
    let fired = alarms.store.pending_report();
    let env = adrian_protocol::Envelope::new(adrian_protocol::Message::AlarmLocalReport {
        fired: fired.clone(),
    });
    in_flight.insert(env.request_id.clone(), fired);
    env_msg(out, env, &cfg.satellite_id);
}

// ---------- server calls ----------

fn read_ca(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read CA certificate {path}"))
}

/// HTTP client trusting system roots plus the configured CA, if any.
fn http_client(ca_cert: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder();
    if let Some(path) = ca_cert {
        let cert = reqwest::Certificate::from_pem(&read_ca(path)?)
            .with_context(|| format!("bad CA certificate {path}"))?;
        b = b.add_root_certificate(cert);
    }
    b.build().context("cannot build HTTP client")
}

/// Websocket TLS connector for a configured CA; `None` keeps the default
/// (system roots).
fn ws_connector(ca_cert: Option<&str>) -> Result<Option<tokio_tungstenite::Connector>> {
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;
    let Some(path) = ca_cert else { return Ok(None) };
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let pem = read_ca(path)?;
    let mut added = 0;
    for cert in CertificateDer::pem_slice_iter(&pem) {
        roots
            .add(cert.with_context(|| format!("bad CA certificate {path}"))?)
            .with_context(|| format!("bad CA certificate {path}"))?;
        added += 1;
    }
    if added == 0 {
        bail!("no certificates found in {path}");
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS setup")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls))))
}

async fn pair(server: &str, code: &str, name: &str, ca_cert: Option<String>) -> Result<()> {
    let client = http_client(ca_cert.as_deref())?;
    let res = client
        .post(format!(
            "{}/api/satellite/pair",
            server.trim_end_matches('/')
        ))
        .json(&serde_json::json!({"code": code, "name": name}))
        .send()
        .await
        .context("pair request failed")?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("pairing rejected ({status}): {body}");
    }
    let v: serde_json::Value = res.json().await?;
    let cfg = Config {
        server_url: server.trim_end_matches('/').to_string(),
        device_token: v["device_token"].as_str().unwrap_or("").to_string(),
        satellite_id: v["satellite_id"].as_str().unwrap_or("").to_string(),
        name: v["name"].as_str().unwrap_or(name).to_string(),
        conversation_id: None,
        wake_word: None,
        ca_cert,
    };
    if cfg.device_token.is_empty() {
        bail!("server did not return a device token");
    }
    save_config(&cfg)?;
    println!(
        "paired as '{}' ({}). Token saved — run `adrian-satellite run`.",
        cfg.name, cfg.satellite_id
    );
    Ok(())
}

async fn status(cfg: &Config) -> Result<()> {
    let client = http_client(cfg.ca_cert.as_deref())?;
    let res = client
        .get(format!("{}/api/satellite/status", cfg.server_url))
        .bearer_auth(&cfg.device_token)
        .send()
        .await
        .context("status request failed")?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    println!("{status} {body}");
    Ok(())
}

// ---------- websocket session ----------

/// What the session loop asks the outer run loop to do.
enum SessionOutcome {
    /// Connection lost or server closed — backoff and reconnect.
    Reconnect,
    /// Quit requested (tray menu, Ctrl+C, or revoked token).
    Shutdown,
}

fn ws_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    let base = base
        .strip_prefix("http://")
        .map(|r| format!("ws://{r}"))
        .or_else(|| base.strip_prefix("https://").map(|r| format!("wss://{r}")))
        .unwrap_or_else(|| format!("ws://{base}"));
    format!("{base}/api/satellite/session")
}

fn env_msg(out: &mpsc::UnboundedSender<WsMsg>, mut env: adrian_protocol::Envelope, sat_id: &str) {
    env.device_id = Some(sat_id.to_string());
    if let Ok(text) = serde_json::to_string(&env) {
        let _ = out.send(WsMsg::Text(text.into()));
    }
}

fn send_status(
    out: &mpsc::UnboundedSender<WsMsg>,
    sat_id: &str,
    state: adrian_protocol::SatelliteState,
    detail: Option<String>,
) {
    env_msg(
        out,
        adrian_protocol::Envelope::new(adrian_protocol::Message::Status { state, detail }),
        sat_id,
    );
}

fn send_ack(
    out: &mpsc::UnboundedSender<WsMsg>,
    sat_id: &str,
    request_id: &str,
    ok: bool,
    detail: impl Into<String>,
) {
    let mut env = adrian_protocol::Envelope::new(adrian_protocol::Message::Ack {
        ok,
        detail: Some(detail.into()),
    });
    env.request_id = request_id.to_string();
    env_msg(out, env, sat_id);
}

/// Handle one server→satellite `Speak`: play the media clip (alarms) and/or
/// synthesize the text, then ack with the envelope's request_id.
fn handle_speak(
    cfg: Config,
    media_url: String,
    text: Option<String>,
    request_id: String,
    out: mpsc::UnboundedSender<WsMsg>,
) {
    tokio::spawn(async move {
        let mut ok = true;
        let mut detail = String::new();
        if !media_url.is_empty() {
            let url = format!("{}{}", cfg.server_url, media_url);
            let client = match http_client(cfg.ca_cert.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    send_ack(
                        &out,
                        &cfg.satellite_id,
                        &request_id,
                        false,
                        format!("{e:#}"),
                    );
                    return;
                }
            };
            match client.get(url).bearer_auth(&cfg.device_token).send().await {
                Ok(res) if res.status().is_success() => match res.bytes().await {
                    Ok(b) => play_bytes(b.to_vec()),
                    Err(e) => {
                        ok = false;
                        detail = format!("media body failed: {e}");
                    }
                },
                Ok(res) => {
                    ok = false;
                    detail = format!("media fetch {}", res.status());
                }
                Err(e) => {
                    ok = false;
                    detail = format!("media fetch failed: {e}");
                }
            }
        }
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            println!("adrian: {t}");
            speak(&t);
        }
        send_ack(&out, &cfg.satellite_id, &request_id, ok, detail);
    });
}

struct Capture {
    kind: CaptureKind,
    started: Instant,
    last_voice: Instant,
}

#[derive(PartialEq)]
enum CaptureKind {
    Ptt,
    Wake,
}

/// Drain the recorder into binary frames while capturing.
fn flush_capture(rec: &Recorder, out: &mpsc::UnboundedSender<WsMsg>) -> Vec<f32> {
    let drained: Vec<f32> = rec
        .samples
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    if !drained.is_empty() {
        let pcm = f32_to_pcm16(&drained);
        for chunk in pcm.chunks(PCM_CHUNK_BYTES) {
            let _ = out.send(WsMsg::Binary(chunk.to_vec().into()));
        }
    }
    drained
}

/// Borrowed context for one websocket session — keeps `session` readable.
struct SessionCtx<'a> {
    cfg: &'a Config,
    rec: &'a Arc<Recorder>,
    death: &'a Arc<MicDeath>,
    hk_rx: &'a mut mpsc::UnboundedReceiver<GlobalHotKeyEvent>,
    hotkey_id: u32,
    mic: &'a mut Option<cpal::Stream>,
    status_tx: &'a tokio::sync::watch::Sender<String>,
    quit: &'a Notify,
    wake_rx: &'a mut Option<mpsc::UnboundedReceiver<()>>,
    alarms: &'a Arc<AlarmCtx>,
}

async fn session(ctx: &mut SessionCtx<'_>) -> Result<SessionOutcome> {
    let SessionCtx {
        cfg,
        rec,
        death,
        hk_rx,
        hotkey_id,
        mic,
        status_tx,
        quit,
        wake_rx,
        alarms,
    } = ctx;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = ws_url(&cfg.server_url)
        .into_client_request()
        .context("bad websocket url")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", cfg.device_token)
            .parse()
            .context("bad token")?,
    );
    let connector = ws_connector(cfg.ca_cert.as_deref())?;
    let (sock, _resp) =
        match tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector).await {
            Ok(r) => r,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let code = resp.status();
                if code == 401 || code == 403 {
                    bail!("device token rejected ({code}) — re-pair this satellite");
                }
                eprintln!("session connect failed: http {code}");
                return Ok(SessionOutcome::Reconnect);
            }
            Err(e) => {
                eprintln!("session connect failed: {e}");
                return Ok(SessionOutcome::Reconnect);
            }
        };
    println!("connected — hold Ctrl+Alt+Space to talk");
    let _ = status_tx.send("connected".into());

    let (mut sink, mut stream) = sock.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    send_status(
        &out_tx,
        &cfg.satellite_id,
        adrian_protocol::SatelliteState::Listening,
        None,
    );
    // Always report on connect (possibly empty) so the server settles any
    // alarm that rang here while it was unreachable.
    alarms.connected.store(true, Ordering::Relaxed);
    let mut reports = HashMap::new();
    send_local_report(&out_tx, cfg, alarms, &mut reports);

    let mut capture: Option<Capture> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut beats = 0u32;

    let outcome = loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { break SessionOutcome::Reconnect };
                match msg {
                    Ok(WsMsg::Text(t)) => {
                        let env: adrian_protocol::Envelope = match serde_json::from_str(&t) {
                            Ok(e) => e,
                            Err(e) => {
                                eprintln!("bad control frame: {e}");
                                continue;
                            }
                        };
                        match env.message {
                            adrian_protocol::Message::Speak { media_url, text, alarm } => {
                                if let Some(a) = &alarm {
                                    if alarms.store.already_fired(a) {
                                        send_ack(&out_tx, &cfg.satellite_id, &env.request_id, true,
                                            "already rang locally from the alarm replica (audibility not confirmed)");
                                        continue;
                                    }
                                    alarms.store.delivered_by_server(a);
                                }
                                handle_speak(cfg.clone(), media_url, text, env.request_id, out_tx.clone());
                            }
                            adrian_protocol::Message::Transcript { text, is_final } => {
                                if is_final && !text.is_empty() {
                                    println!("you: {text}");
                                }
                            }
                            adrian_protocol::Message::Ack { ok, detail } => {
                                if let Some(fired) = reports.remove(&env.request_id)
                                    && ok
                                {
                                    alarms.store.settle_report(&fired);
                                }
                                if !ok {
                                    eprintln!("server nack: {}", detail.unwrap_or_default());
                                }
                            }
                            adrian_protocol::Message::PairRejected { reason } => {
                                eprintln!("pair rejected: {reason}");
                            }
                            adrian_protocol::Message::AlarmReplicaSync { alarms: list } => {
                                alarms.store.replace_all(list);
                            }
                            adrian_protocol::Message::AlarmReplicaUpsert { alarm } => {
                                alarms.store.upsert(alarm);
                                send_ack(&out_tx, &cfg.satellite_id, &env.request_id, true, "stored");
                            }
                            adrian_protocol::Message::AlarmReplicaCancel { alarm_id, occurrence_unix } => {
                                alarms.store.cancel(&alarm_id, occurrence_unix);
                                send_ack(&out_tx, &cfg.satellite_id, &env.request_id, true, "removed");
                            }
                            _ => {}
                        }
                    }
                    Ok(WsMsg::Binary(_)) => {} // server never sends binary today
                    Ok(WsMsg::Close(_)) => break SessionOutcome::Reconnect,
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("session error: {e}");
                        break SessionOutcome::Reconnect;
                    }
                }
            }
            Some(ev) = hk_rx.recv() => {
                if ev.id != *hotkey_id { continue; }
                match ev.state {
                    HotKeyState::Pressed => {
                        if capture.is_none() {
                            begin_capture(rec, &out_tx, cfg, CaptureKind::Ptt, &mut capture);
                            print!("listening… ");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                    }
                    HotKeyState::Released => {
                        if let Some(c) = capture.take() {
                            if c.kind == CaptureKind::Ptt {
                                rec.recording.store(false, Ordering::Relaxed);
                                end_capture(rec, &out_tx, cfg, c.started.elapsed().as_secs_f32());
                            } else {
                                capture = Some(c); // a wake capture is in flight; ignore
                            }
                        }
                    }
                }
            }
            Some(()) = async { wake_rx.as_mut().unwrap().recv().await }, if wake_rx.is_some() => {
                if capture.is_none() {
                    begin_capture(rec, &out_tx, cfg, CaptureKind::Wake, &mut capture);
                    println!("wake word heard — listening…");
                }
            }
            _ = tick.tick() => {
                // Stream buffered audio while a capture is active.
                if let Some(c) = &mut capture {
                    let drained = flush_capture(rec, &out_tx);
                    if rms(&drained) > WAKE_SILENCE_RMS {
                        c.last_voice = Instant::now();
                    }
                    // Wake captures end themselves on trailing silence or
                    // the max-duration cap; PTT caps at the max duration.
                    let secs = c.started.elapsed().as_secs_f32();
                    let done = if c.kind == CaptureKind::Wake {
                        (secs >= WAKE_MIN_SPEECH_SECS
                            && c.last_voice.elapsed().as_secs_f32() >= WAKE_SILENCE_SECS)
                            || secs >= MAX_RECORD_SECS as f32
                    } else {
                        secs >= MAX_RECORD_SECS as f32
                    };
                    if done {
                        capture.take();
                        rec.recording.store(false, Ordering::Relaxed);
                        end_capture(rec, &out_tx, cfg, secs);
                    }
                }
                beats += 1;
                if beats.is_multiple_of(HEARTBEAT_SECS as u32 * 10) {
                    send_status(&out_tx, &cfg.satellite_id,
                        adrian_protocol::SatelliteState::Idle, None);
                }
            }
            _ = death.notify.notified() => {
                death.dead.store(false, Ordering::Relaxed);
                eprintln!("mic lost — re-initializing…");
                let _ = status_tx.send("mic lost — retrying".into());
                **mic = None; // drop the dead stream
                // Retry a few times quickly, then keep the session alive;
                // a device appearing later is picked up on the next press.
                for _ in 0..5 {
                    if let Some(s) = init_mic(rec, death) {
                        **mic = Some(s);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if mic.is_none() {
                    eprintln!("no mic — will retry on next session");
                    let _ = status_tx.send("no microphone".into());
                }
            }
            _ = alarms.fired.notified() => {
                send_local_report(&out_tx, cfg, alarms, &mut reports);
            }
            _ = quit.notified() => {
                let _ = out_tx.send(WsMsg::Close(None));
                break SessionOutcome::Shutdown;
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = out_tx.send(WsMsg::Close(None));
                break SessionOutcome::Shutdown;
            }
        }
    };

    alarms.connected.store(false, Ordering::Relaxed);
    writer.abort();
    Ok(outcome)
}

/// Start streaming audio: clear the buffer, arm the recorder, and open a
/// capture session on the control channel.
fn begin_capture(
    rec: &Recorder,
    out: &mpsc::UnboundedSender<WsMsg>,
    cfg: &Config,
    kind: CaptureKind,
    capture: &mut Option<Capture>,
) {
    rec.samples
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    rec.recording.store(true, Ordering::Relaxed);
    env_msg(
        out,
        adrian_protocol::Envelope::new(adrian_protocol::Message::AudioSessionStart {
            room: None,
            sample_rate: rec.rate(),
        }),
        &cfg.satellite_id,
    );
    *capture = Some(Capture {
        kind,
        started: Instant::now(),
        last_voice: Instant::now(),
    });
}

/// Flush any remainder and close the capture session.
fn end_capture(rec: &Recorder, out: &mpsc::UnboundedSender<WsMsg>, cfg: &Config, secs: f32) {
    let drained = flush_capture(rec, out);
    let total_secs = secs.max(drained.len() as f32 / rec.rate().max(1) as f32);
    if total_secs < 0.4 {
        println!("(too short, ignored)");
        env_msg(
            out,
            adrian_protocol::Envelope::new(adrian_protocol::Message::AudioSessionEnd {
                reason: adrian_protocol::SessionEndReason::UserStopped,
            }),
            &cfg.satellite_id,
        );
        return;
    }
    println!("{total_secs:.1}s sent");
    env_msg(
        out,
        adrian_protocol::Envelope::new(adrian_protocol::Message::AudioSessionEnd {
            reason: adrian_protocol::SessionEndReason::Endpointed,
        }),
        &cfg.satellite_id,
    );
}

// ---------- run loop ----------

async fn run(cfg: Config) -> Result<()> {
    println!("adrian-satellite '{}' → {}", cfg.name, cfg.server_url);

    let rec = Arc::new(Recorder {
        samples: Mutex::new(Vec::new()),
        ring: Mutex::new(VecDeque::new()),
        recording: AtomicBool::new(false),
        wake_armed: AtomicBool::new(false),
        sample_rate: AtomicU32::new(16000),
    });
    let death = Arc::new(MicDeath {
        dead: AtomicBool::new(false),
        notify: Notify::new(),
    });
    let mut mic = init_mic(&rec, &death);
    if mic.is_none() {
        println!("no microphone yet — will retry when one appears");
    }

    let manager = GlobalHotKeyManager::new().context("hotkey manager")?;
    let hotkey = HotKey::new(Some(PTT_MODS), PTT_KEY);
    manager.register(hotkey).context("register PTT hotkey")?;

    // global_hotkey's receiver is blocking — pump it on a dedicated
    // thread and forward events into the async loop.
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel::<GlobalHotKeyEvent>();
    std::thread::spawn(move || {
        while let Ok(ev) = GlobalHotKeyEvent::receiver().recv() {
            if hk_tx.send(ev).is_err() {
                return;
            }
        }
    });

    // Tray icon (Windows): status line + Quit. Shares a quit Notify with
    // the session loop.
    let (status_tx, status_rx) = tokio::sync::watch::channel("starting".to_string());
    let quit = Arc::new(Notify::new());
    let _tray = tray::start(status_rx, quit.clone());

    // Optional local wake-word spotting.
    let mut wake_rx: Option<mpsc::UnboundedReceiver<()>> = None;
    #[cfg(feature = "wake-word")]
    {
        if let Some(wc) = &cfg.wake_word {
            match wake::spawn_detector(rec.clone(), wc) {
                Ok(rx) => {
                    rec.wake_armed.store(true, Ordering::Relaxed);
                    wake_rx = Some(rx);
                    println!("wake word armed (local spotting)");
                }
                Err(e) => eprintln!("wake word disabled: {e:#}"),
            }
        }
    }
    #[cfg(not(feature = "wake-word"))]
    if cfg.wake_word.is_some() {
        eprintln!("wake_word configured but binary lacks `--features wake-word` — ignoring");
    }

    let replica_path = config_path()?.with_file_name("replicas.json");
    let alarms = Arc::new(AlarmCtx {
        store: replicas::ReplicaStore::open(replica_path),
        connected: AtomicBool::new(false),
        fired: Notify::new(),
    });
    spawn_replica_watchdog(alarms.clone());

    let mut backoff = Duration::from_secs(1);
    loop {
        // Wake detections only need to inject a synthetic "press" — the
        // simplest correct hook is a hotkey-shaped signal into the session.
        let mut ctx = SessionCtx {
            cfg: &cfg,
            rec: &rec,
            death: &death,
            hk_rx: &mut hk_rx,
            hotkey_id: hotkey.id(),
            mic: &mut mic,
            status_tx: &status_tx,
            quit: &quit,
            wake_rx: &mut wake_rx,
            alarms: &alarms,
        };
        match session(&mut ctx).await? {
            SessionOutcome::Shutdown => {
                let _ = status_tx.send("quit".into());
                println!("bye");
                return Ok(());
            }
            SessionOutcome::Reconnect => {
                let _ = status_tx.send("reconnecting…".into());
                eprintln!("disconnected — retrying in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = quit.notified() => return Ok(()),
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let arg = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    match args.get(1).map(|s| s.as_str()) {
        Some("pair") => {
            let server = arg("--server").context("missing --server <url>")?;
            let code = arg("--code").context("missing --code <6 digits>")?;
            let name = arg("--name").unwrap_or_else(|| {
                std::env::var("COMPUTERNAME").unwrap_or_else(|_| "satellite".into())
            });
            let ca = arg("--ca").map(|p| {
                std::fs::canonicalize(&p)
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or(p)
            });
            pair(&server, &code, &name, ca).await
        }
        Some("run") => {
            let mut cfg = load_config()?;
            if let Some(s) = arg("--server") {
                cfg.server_url = s.trim_end_matches('/').to_string();
            }
            run(cfg).await
        }
        Some("status") => status(&load_config()?).await,
        Some("say") => {
            let text = args.get(2).cloned().unwrap_or_default();
            let mut t = tts::Tts::default()?;
            t.speak(&text, true)?;
            for _ in 0..600 {
                if !t.is_speaking().unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(())
        }
        _ => {
            eprintln!(
                "adrian-satellite {}\n\
                 usage:\n\
                 \x20 pair --server <url> --code <6 digits> [--name <name>] [--ca <cert.pem>]\n\
                 \x20 run [--server <url>]     voice client (PTT + optional wake word)\n\
                 \x20 status                   check device token\n\
                 \x20 say \"<text>\"             test speech output",
                env!("CARGO_PKG_VERSION")
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;

    /// Live check against a running TLS server with a private CA:
    /// `ADRIAN_TEST_TLS_URL=https://127.0.0.1:18787 ADRIAN_TEST_TLS_CA=cert.pem
    /// cargo test -p adrian-satellite -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn trusts_configured_ca_over_https_and_wss() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = std::env::var("ADRIAN_TEST_TLS_URL").unwrap();
        let ca = std::env::var("ADRIAN_TEST_TLS_CA").unwrap();

        // Without the CA the self-signed server must be rejected.
        assert!(
            http_client(None)
                .unwrap()
                .get(format!("{url}/api/setup/status"))
                .send()
                .await
                .is_err()
        );
        let res = http_client(Some(&ca))
            .unwrap()
            .get(format!("{url}/api/setup/status"))
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success());

        // The TLS handshake succeeds; the server then refuses the missing
        // device token at the HTTP layer.
        let req = ws_url(&url).into_client_request().unwrap();
        let err = tokio_tungstenite::connect_async_tls_with_config(
            req,
            None,
            false,
            ws_connector(Some(&ca)).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 401),
            "{err}"
        );
    }
}
