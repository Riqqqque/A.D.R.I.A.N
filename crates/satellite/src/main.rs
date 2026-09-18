//! Adrian native Windows voice satellite.
//!
//! Push-to-talk voice client: hold Ctrl+Alt+Space, speak, release. The WAV
//! utterance is POSTed to the Adrian server, which transcribes it, runs the
//! normal assistant turn, and returns reply text that this client speaks
//! through the local speech synthesizer (SAPI on Windows).
//!
//! Pairing is one-use-code based: `adrian-satellite pair --server <url>
//! --code <6 digits> --name "Desk PC"` redeems a code minted in the web UI
//! and stores a device token in %APPDATA%\adrian\satellite\config.json.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const PTT_MODS: Modifiers = Modifiers::CONTROL.union(Modifiers::ALT);
const PTT_KEY: Code = Code::Space;
const MAX_RECORD_SECS: usize = 60;

#[derive(Serialize, Deserialize, Default)]
struct Config {
    server_url: String,
    device_token: String,
    satellite_id: String,
    name: String,
    conversation_id: Option<String>,
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

struct Recorder {
    samples: Mutex<Vec<f32>>,
    recording: AtomicBool,
    sample_rate: u32,
    channels: u16,
}

fn start_mic(
    rec: Arc<Recorder>,
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
) -> Result<cpal::Stream> {
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();

    let push = move |data: &[f32], rec: &Arc<Recorder>| {
        if !rec.recording.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut buf) = rec.samples.lock() {
            // First channel only — good enough for speech.
            for frame in data.chunks(channels) {
                buf.push(frame[0]);
            }
            let cap = rec.sample_rate as usize * MAX_RECORD_SECS;
            if buf.len() > cap {
                buf.truncate(cap);
            }
        }
    };

    let err = |e| eprintln!("mic stream error: {e}");
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

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec)?;
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
        w.finalize()?;
    }
    Ok(cursor.into_inner())
}

// ---------- speech output ----------

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

// ---------- server calls ----------

async fn pair(server: &str, code: &str, name: &str) -> Result<()> {
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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

async fn send_utterance(cfg: &Config, wav: Vec<u8>) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut url = format!("{}/api/satellite/utterance", cfg.server_url);
    if let Some(cid) = &cfg.conversation_id {
        url = format!("{url}?conversation_id={cid}");
    }
    let res = client
        .post(&url)
        .bearer_auth(&cfg.device_token)
        .header("Content-Type", "audio/wav")
        .body(wav)
        .send()
        .await
        .context("utterance request failed")?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("server rejected utterance ({status}): {body}");
    }
    Ok(res.json().await?)
}

// ---------- run loop ----------

async fn run(mut cfg: Config) -> Result<()> {
    println!("adrian-satellite '{}' → {}", cfg.name, cfg.server_url);
    println!("hold Ctrl+Alt+Space to talk; Ctrl+C to quit");

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default audio input device")?;
    let mic_config = device.default_input_config()?;
    let rec = Arc::new(Recorder {
        samples: Mutex::new(Vec::new()),
        recording: AtomicBool::new(false),
        sample_rate: mic_config.sample_rate().0,
        channels: mic_config.channels(),
    });
    let _stream = start_mic(rec.clone(), &device, &mic_config)?;
    println!("mic: {} Hz, {} ch", rec.sample_rate, rec.channels);

    let manager = GlobalHotKeyManager::new().context("hotkey manager")?;
    let hotkey = HotKey::new(Some(PTT_MODS), PTT_KEY);
    manager.register(hotkey).context("register PTT hotkey")?;

    loop {
        match GlobalHotKeyEvent::receiver().recv() {
            Ok(ev) if ev.id == hotkey.id() => match ev.state {
                HotKeyState::Pressed => {
                    rec.samples.lock().unwrap().clear();
                    rec.recording.store(true, Ordering::Relaxed);
                    print!("listening… ");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                HotKeyState::Released => {
                    rec.recording.store(false, Ordering::Relaxed);
                    let samples: Vec<f32> = rec.samples.lock().unwrap().drain(..).collect();
                    let secs = samples.len() as f32 / rec.sample_rate.max(1) as f32;
                    if secs < 0.4 {
                        println!("(too short, ignored)");
                        continue;
                    }
                    println!("{secs:.1}s captured, sending…");
                    match encode_wav(&samples, rec.sample_rate) {
                        Ok(wav) => match send_utterance(&cfg, wav).await {
                            Ok(v) => {
                                if let Some(cid) = v["conversation_id"].as_str() {
                                    cfg.conversation_id = Some(cid.to_string());
                                    let _ = save_config(&cfg);
                                }
                                let transcript = v["transcript"].as_str().unwrap_or("");
                                let response = v["response"].as_str().unwrap_or("");
                                let error = v["error"].as_str().unwrap_or("");
                                println!("you: {transcript}");
                                if !response.is_empty() {
                                    println!("adrian: {response}");
                                    speak(response);
                                }
                                if !error.is_empty() {
                                    println!("error: {error}");
                                }
                            }
                            Err(e) => println!("send failed: {e:#}"),
                        },
                        Err(e) => println!("wav encode failed: {e:#}"),
                    }
                }
            },
            Ok(_) => {}
            Err(e) => {
                eprintln!("hotkey channel error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
            pair(&server, &code, &name).await
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
            speak(&text);
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            Ok(())
        }
        _ => {
            eprintln!(
                "adrian-satellite {}\n\
                 usage:\n\
                 \x20 pair --server <url> --code <6 digits> [--name <name>]\n\
                 \x20 run [--server <url>]     push-to-talk voice client\n\
                 \x20 status                   check device token\n\
                 \x20 say \"<text>\"             test speech output",
                env!("CARGO_PKG_VERSION")
            );
            std::process::exit(2);
        }
    }
}
