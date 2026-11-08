//! Volume: the master level of the default speakers, one app's own level in
//! the Windows volume mixer ("turn Spotify down", "mute Discord"), or the level
//! of whatever is making sound right now ("turn my music down").

use super::{Com, Index, exe_stem};
use crate::Outcome;
use crate::matching::{normalize, score};
use serde_json::Value;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::Interface;

/// How far "up" and "down" move, in percent, when no amount is given.
const STEP: f32 = 10.0;

fn endpoint() -> windows::core::Result<windows::Win32::Media::Audio::IMMDevice> {
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        e.GetDefaultAudioEndpoint(eRender, eConsole)
    }
}

/// Programs whose sound is never "the music": voice chat, the assistant
/// itself, and the parts of Windows and launchers that make sound.
const NOT_MUSIC: [&str; 16] = [
    "discord",
    "discordptb",
    "discordcanary",
    "vesktop",
    "teamspeak",
    "ts3client_win64",
    "mumble",
    "zoom",
    "ms-teams",
    "teams",
    "slack",
    "steam",
    "adrian",
    "adrian-desktop",
    "audiodg",
    "explorer",
];

/// How long to listen for sound before deciding what's playing: a quiet
/// moment in a song shouldn't hide it.
const LISTEN: std::time::Duration = std::time::Duration::from_millis(250);

/// One app's audio sessions (an app can have several).
struct AppVolume {
    name: String,
    path: String,
    controls: Vec<ISimpleAudioVolume>,
    /// The level meters of its sessions with a stream open now.
    meters: Vec<IAudioMeterInformation>,
}

fn app_sessions() -> windows::core::Result<Vec<AppVolume>> {
    let mut out: Vec<AppVolume> = Vec::new();
    unsafe {
        let mgr: IAudioSessionManager2 = endpoint()?.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        for i in 0..list.GetCount()? {
            let ctl = list.GetSession(i)?;
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = ctl2.GetProcessId().unwrap_or(0);
            if pid == 0 {
                continue; // system sounds
            }
            let Some(path) = super::wnd::exe_path(pid) else {
                continue;
            };
            let name = exe_stem(&path);
            let Ok(vol) = ctl.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let meter = (ctl.GetState().ok() == Some(AudioSessionStateActive))
                .then(|| ctl.cast::<IAudioMeterInformation>().ok())
                .flatten();
            let i = match out.iter().position(|a| a.name == name) {
                Some(i) => i,
                None => {
                    out.push(AppVolume {
                        name,
                        path,
                        controls: Vec::new(),
                        meters: Vec::new(),
                    });
                    out.len() - 1
                }
            };
            out[i].controls.push(vol);
            out[i].meters.extend(meter);
        }
    }
    Ok(out)
}

fn pct(x: f32) -> u32 {
    (x * 100.0).round() as u32
}

/// The target level for set/up/down, from 0 to 1.
fn new_level(action: &str, now: f32, args: &Value) -> Option<f32> {
    let by = args.get("level").and_then(Value::as_f64).map(|v| v as f32);
    let x = match action {
        "set" => by? / 100.0,
        "up" => now + by.unwrap_or(STEP) / 100.0,
        "down" => now - by.unwrap_or(STEP) / 100.0,
        _ => return None,
    };
    Some(x.clamp(0.0, 1.0))
}

fn pretty(stem: &str) -> String {
    let mut c = stem.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Whether an app's sound can be what they mean by "my music": not a game, not
/// voice chat, not her.
fn could_be_music(index: &Index, a: &AppVolume) -> bool {
    !NOT_MUSIC.contains(&a.name.as_str()) && !index.is_protected(&a.path)
}

/// Of the apps that could be music, the ones making sound, and the names of
/// the rest. Meters are read a few times over `LISTEN`, so a pause between
/// songs isn't taken for silence.
fn playing(index: &Index, apps: Vec<AppVolume>) -> (Vec<AppVolume>, Vec<String>) {
    let (candidates, others): (Vec<_>, Vec<_>) =
        apps.into_iter().partition(|a| could_be_music(index, a));
    let mut heard = vec![false; candidates.len()];
    let start = std::time::Instant::now();
    loop {
        for (h, a) in heard.iter_mut().zip(&candidates) {
            *h |= a
                .meters
                .iter()
                .any(|m| unsafe { m.GetPeakValue() }.unwrap_or(0.0) > 0.001);
        }
        if heard.iter().all(|h| *h) || start.elapsed() >= LISTEN {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut rest: Vec<String> = others.iter().map(|a| pretty(&a.name)).collect();
    let mut found = Vec::new();
    for (a, h) in candidates.into_iter().zip(heard) {
        if h {
            found.push(a);
        } else {
            rest.push(pretty(&a.name));
        }
    }
    (found, rest)
}

/// `index` is only built for `playing`, which needs to know where games live.
pub fn run(action: &str, args: &Value, index: impl FnOnce() -> Index) -> Outcome {
    let _com = Com::init();
    let app = args.get("app").and_then(Value::as_str).unwrap_or("").trim();
    let result = if args.get("playing").and_then(Value::as_bool) == Some(true) {
        what_plays(action, args, &index())
    } else if app.is_empty() {
        master(action, args)
    } else {
        one_app(action, app, args)
    };
    result.unwrap_or_else(|e| Outcome::no(format!("Windows audio error: {}", e.message())))
}

fn master(action: &str, args: &Value) -> windows::core::Result<Outcome> {
    unsafe {
        let v: IAudioEndpointVolume = endpoint()?.Activate(CLSCTX_ALL, None)?;
        let now = v.GetMasterVolumeLevelScalar()?;
        let muted = v.GetMute()?.as_bool();
        Ok(match action {
            "get" => Outcome::ok(format!(
                "Volume is {}%{}.",
                pct(now),
                if muted { " and muted" } else { "" }
            )),
            "mute" => {
                v.SetMute(true, std::ptr::null())?;
                Outcome::ok("Muted.")
            }
            "unmute" => {
                v.SetMute(false, std::ptr::null())?;
                Outcome::ok(format!("Unmuted, at {}%.", pct(now)))
            }
            _ => match new_level(action, now, args) {
                Some(x) => {
                    v.SetMasterVolumeLevelScalar(x, std::ptr::null())?;
                    if muted && x > 0.0 {
                        v.SetMute(false, std::ptr::null())?;
                    }
                    Outcome::ok(format!(
                        "Volume {}%{}.",
                        pct(x),
                        if pct(x) == pct(now) {
                            " (it was already there)"
                        } else {
                            ""
                        }
                    ))
                }
                None => Outcome::no(
                    "Volume action must be get, set (with level 0-100), up, down, mute or unmute.",
                ),
            },
        })
    }
}

fn one_app(action: &str, app: &str, args: &Value) -> windows::core::Result<Outcome> {
    let apps = app_sessions()?;
    let q = normalize(app);
    let best = apps
        .iter()
        .map(|a| (score(&q, &normalize(&a.name)), a))
        .filter(|(s, _)| *s > 0)
        .max_by_key(|(s, _)| *s);
    let Some((_, a)) = best else {
        let playing: Vec<String> = apps.iter().map(|a| pretty(&a.name)).collect();
        return Ok(Outcome::no(format!(
            "{app} isn't playing any sound right now.{}",
            if playing.is_empty() {
                String::new()
            } else {
                format!(" Apps with sound: {}.", playing.join(", "))
            }
        )));
    };
    let name = pretty(&a.name);
    unsafe {
        let now = a.controls[0].GetMasterVolume()?;
        match action {
            "get" => Ok(Outcome::ok(format!(
                "{name} is at {}%{}.",
                pct(now),
                if a.controls[0].GetMute()?.as_bool() {
                    ", muted"
                } else {
                    ""
                }
            ))),
            "mute" | "unmute" => {
                for c in &a.controls {
                    c.SetMute(action == "mute", std::ptr::null())?;
                }
                Ok(Outcome::ok(format!(
                    "{name} {}.",
                    if action == "mute" { "muted" } else { "unmuted" }
                )))
            }
            _ => match new_level(action, now, args) {
                Some(x) => {
                    for c in &a.controls {
                        c.SetMasterVolume(x, std::ptr::null())?;
                    }
                    Ok(Outcome::ok(format!(
                        "{name} at {}% (of the main volume).",
                        pct(x)
                    )))
                }
                None => Ok(Outcome::no(
                    "Volume action must be get, set (with level 0-100), up, down, mute or unmute.",
                )),
            },
        }
    }
}

/// "Turn my music down": every app making sound that could be music, each from
/// its own level. The master volume, games and voice chat stay where they are.
fn what_plays(action: &str, args: &Value, index: &Index) -> windows::core::Result<Outcome> {
    let (found, rest) = playing(index, app_sessions()?);
    if found.is_empty() {
        return Ok(Outcome::no(if rest.is_empty() {
            "Nothing is playing sound right now.".to_string()
        } else {
            format!(
                "No music or video is playing. Making sound, but left alone: {}.",
                rest.join(", ")
            )
        }));
    }
    let mut said = Vec::new();
    for a in &found {
        let name = pretty(&a.name);
        unsafe {
            let now = a.controls[0].GetMasterVolume()?;
            let muted = a.controls[0].GetMute()?.as_bool();
            match action {
                "get" => said.push(format!(
                    "{name} is at {}%{}",
                    pct(now),
                    if muted { ", muted" } else { "" }
                )),
                "mute" | "unmute" => {
                    for c in &a.controls {
                        c.SetMute(action == "mute", std::ptr::null())?;
                    }
                    said.push(format!(
                        "{name} {}",
                        if action == "mute" { "muted" } else { "unmuted" }
                    ));
                }
                _ => {
                    let Some(x) = new_level(action, now, args) else {
                        return Ok(Outcome::no(
                            "Volume action must be get, set (with level 0-100), up, down, mute or unmute.",
                        ));
                    };
                    for c in &a.controls {
                        c.SetMasterVolume(x, std::ptr::null())?;
                        if muted && x > 0.0 {
                            c.SetMute(false, std::ptr::null())?;
                        }
                    }
                    let how = match action {
                        "down" => "down to",
                        "up" => "up to",
                        _ => "at",
                    };
                    said.push(format!("{name} {how} {}%", pct(x)));
                }
            }
        }
    }
    Ok(Outcome::ok(format!("{}.", said.join(", "))))
}
