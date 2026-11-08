//! A.D.R.I.A.N. on Windows: a tray app with the orb window. It listens all
//! the time (audio only leaves the PC while someone talks), talks back with
//! her real voice, and does PC actions the server asks for, within what this
//! PC's settings allow.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod link;
mod quiet;
mod single;
mod tray;
mod update;

use adrian_pc_control::{Pc, Policy};
use audio::Shared;
use config::Settings;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use link::{Cmd, Event, Link};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewWindowBuilder};
use tokio::sync::{mpsc, watch};

struct AppState {
    settings: Mutex<Settings>,
    link: watch::Sender<Option<Link>>,
    cmds: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
    pc: Arc<Mutex<Arc<Pc>>>,
    status: Mutex<StatusView>,
    /// What the tray shows now.
    tray: Mutex<Option<tray::View>>,
}

#[derive(Serialize, Clone, Default)]
struct StatusView {
    connected: bool,
    text: String,
    owner: Option<String>,
}

#[derive(Serialize)]
struct SettingsView {
    server_url: String,
    paired: bool,
    mic_threshold: f32,
    pc: Policy,
    /// The shortcuts as lines for the settings box.
    shortcuts: String,
    settings_file: String,
    auto_update: bool,
    /// The update line for each position of the switch: it can flip before a Save.
    update_on: String,
    update_off: String,
    quiet_while_playing: bool,
}

#[derive(Serialize, Clone)]
struct Frame {
    mode: &'static str,
    level: f32,
    /// The mic's level whatever she's doing, for the settings meter.
    mic: f32,
    mic_error: Option<String>,
}

/// How long the window may sit minimized before it's closed to the tray.
const MINIMIZED_TO_TRAY: Duration = Duration::from_secs(120);
/// How often to look at what's in front, for "playing a game now".
const GAME_CHECK: Duration = Duration::from_secs(2);

/// Her state right now, as the window shows it.
fn frame(shared: &Shared) -> Frame {
    let out = f32::from_bits(shared.out_level.load(Ordering::Relaxed));
    let mic = f32::from_bits(shared.mic_level.load(Ordering::Relaxed));
    // Her own music isn't her talking: the orb only speaks with her voice.
    let (mode, level) = if (shared.playing() || out > 0.03) && shared.her_voice() {
        ("speaking", out)
    } else if shared.online()
        && (shared.ptt.load(Ordering::Relaxed)
            || (shared.talking.load(Ordering::Relaxed) && shared.listening()))
    {
        // Only when she's really listening to you: push-to-talk, or talking
        // right after her answer. Any loud sound opens the mic gate (music, a
        // cough); the server ignores those, so the orb shouldn't light up for
        // them either. And offline nobody is listening at all.
        ("listening", mic)
    } else if shared.answering.load(Ordering::Relaxed) {
        ("thinking", 0.0)
    } else {
        ("idle", 0.0)
    };
    let mic_error = shared.mic_error.lock().ok().and_then(|e| e.clone());
    Frame {
        mode,
        level,
        mic,
        mic_error,
    }
}

/// Decides which frames reach the page. With the orb drawing, all of them.
/// With it paused (say behind a full-screen game) the chip and the Stop button
/// still have to be right, so on slow ticks it looks, and only sends a change.
#[derive(Default)]
struct Pacer {
    sent: Option<(&'static str, Option<String>)>,
}

impl Pacer {
    fn next(&mut self, shared: &Shared, drawing: bool, slow_tick: bool) -> Option<Frame> {
        if drawing {
            self.sent = None;
            return Some(frame(shared));
        }
        if !slow_tick {
            return None;
        }
        let f = frame(shared);
        let state = (f.mode, f.mic_error.clone());
        if self.sent.as_ref() == Some(&state) {
            return None;
        }
        self.sent = Some(state);
        Some(f)
    }
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

fn link_for(s: &Settings) -> Option<Link> {
    Some(Link {
        url: s.url()?,
        token: s.token()?,
        device: device_name(),
    })
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> SettingsView {
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let update = update::status();
    SettingsView {
        server_url: s.server_url.clone(),
        paired: s.token().is_some(),
        mic_threshold: s.mic_threshold,
        pc: s.pc.clone(),
        shortcuts: config::shortcut_lines(&s.pc.shortcuts),
        settings_file: config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        auto_update: !s.updates_off,
        update_on: update.line(true),
        update_off: update.line(false),
        quiet_while_playing: !s.quiet_off,
    }
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> StatusView {
    state
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[tauri::command]
fn save_settings(
    state: tauri::State<'_, AppState>,
    server_url: String,
    token: Option<String>,
    mic_threshold: f32,
    pc: Policy,
    shortcuts: String,
    auto_update: Option<bool>,
    quiet_while_playing: Option<bool>,
) -> Result<(), String> {
    let form = config::Form {
        server_url,
        token,
        mic_threshold,
        pc,
        shortcuts,
        auto_update,
        quiet_while_playing,
    };
    let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let saved = config::commit(&mut s, form, config::save)?;
    state
        .shared
        .threshold
        .store(s.mic_threshold.to_bits(), Ordering::Relaxed);
    let new_pc = Arc::new(Pc::new(s.pc.clone()));
    *state.pc.lock().unwrap_or_else(|e| e.into_inner()) = new_pc;
    let connected = state
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected;
    if saved.reconnect(connected) {
        let _ = state.link.send(link_for(&s));
    }
    if saved.updates_on {
        update::check_now();
    }
    Ok(())
}

#[tauri::command]
fn send_text(state: tauri::State<'_, AppState>, text: String) -> Result<(), String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(());
    }
    let cmd = Cmd::Text(t.chars().take(2000).collect());
    if link::offer(&state.shared, &state.cmds, cmd) {
        Ok(())
    } else {
        Err(link::NOT_SENT.into())
    }
}

#[tauri::command]
fn stop_talking(state: tauri::State<'_, AppState>) {
    // Offline there's nothing to stop: losing the link already silenced her.
    link::offer(&state.shared, &state.cmds, Cmd::Stop);
}

/// The talk button: held down, the mic is open and what's said is for her.
#[tauri::command]
fn push_to_talk(app: AppHandle, state: tauri::State<'_, AppState>, down: bool) {
    if !link::talk_key(&state.shared, &state.cmds, down) {
        let _ = app.emit("not_sent", link::NOT_SENT);
    }
}

/// The page asks once at load, in case frames are paused then; after that it
/// follows "frame" events.
#[tauri::command]
fn her_state(state: tauri::State<'_, AppState>) -> Frame {
    frame(&state.shared)
}

#[derive(Serialize, Clone, PartialEq)]
struct Devices {
    mic: String,
    speakers: String,
}

fn devices(shared: &Shared) -> Devices {
    let (mic, speakers) = shared
        .devices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Devices { mic, speakers }
}

/// The mic and speakers in use; the page follows "devices" events after this.
#[tauri::command]
fn audio_devices(state: tauri::State<'_, AppState>) -> Devices {
    devices(&state.shared)
}

/// The page asks once at load; after that it follows "draw" events.
#[tauri::command]
fn orb_drawing(app: AppHandle) -> bool {
    orb_on_screen(&app)
}

/// Bring the window up. In the tray there is no window at all (its web view
/// costs ~300 MB and some CPU), so it's built again here when needed.
fn show(app: &AppHandle) {
    let w = match app.get_webview_window("main") {
        Some(w) => w,
        None => {
            let Some(cfg) = app.config().app.windows.first() else {
                return;
            };
            match WebviewWindowBuilder::from_config(app, cfg).and_then(|b| b.build()) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("couldn't open the window: {e}");
                    return;
                }
            }
        }
    };
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_focus();
}

/// Back to the tray: the window and its web view go away completely.
fn close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.destroy();
    }
}

/// Have the main thread bring the tray up to date with the link and the mic.
fn refresh_tray(app: &AppHandle) {
    let h = app.clone();
    let _ = app.run_on_main_thread(move || {
        let state = h.state::<AppState>();
        let view = {
            let status = state.status.lock().unwrap_or_else(|e| e.into_inner());
            let error = state
                .shared
                .mic_error
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            tray::view(status.connected, &status.text, error.as_deref())
        };
        tray::show(&h, view, &state.tray);
    });
}

/// A full-screen app (a game, a full-screen video) is in front.
#[cfg(windows)]
fn fullscreen_app_in_front() -> bool {
    use windows::Win32::UI::Shell::{
        QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
        SHQueryUserNotificationState,
    };
    // SAFETY: no arguments; it only reads shell state.
    matches!(
        unsafe { SHQueryUserNotificationState() },
        Ok(s) if s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE
    )
}

#[cfg(not(windows))]
fn fullscreen_app_in_front() -> bool {
    false
}

/// Whether the orb is worth drawing: the window is up and nothing full-screen covers it.
/// WebView2 keeps the page "visible" when the window is hidden to the tray, so the page
/// can't tell on its own.
fn orb_on_screen(app: &AppHandle) -> bool {
    let Some(w) = app.get_webview_window("main") else {
        return false;
    };
    w.is_visible().unwrap_or(false)
        && !w.is_minimized().unwrap_or(false)
        && (w.is_focused().unwrap_or(false) || !fullscreen_app_in_front())
}

fn toggle(app: &AppHandle) {
    let up = app
        .get_webview_window("main")
        .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false));
    if up {
        close(app);
    } else {
        show(app);
    }
}

/// `adrian-desktop --pair SERVER TOKEN`: save the connection without the window.
fn pair(server: Option<&String>, token: Option<&String>) -> Result<String, String> {
    let (Some(server), Some(token)) = (server, token) else {
        return Err("usage: adrian-desktop --pair SERVER TOKEN".into());
    };
    if config::normalize_url(server).is_none() || !token.starts_with("sat_") {
        return Err("give the server address (like homeserver:8791) and the sat_ token".into());
    }
    let mut s = config::load();
    s.server_url = server.trim().to_string();
    s.set_token(token.trim())?;
    config::save(&s)?;
    Ok(format!("Paired with {}.", s.url().unwrap_or_default()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--pair") {
        match pair(args.get(2), args.get(3)) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--update-now") {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        match update::now() {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("update failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let context = tauri::generate_context!();
    // The copy before this one runs this ahead of swapping to it: this build
    // starts, and here's its version. No claim, window, tray or audio, so it
    // works next to that copy, and it comes before anything that touches files.
    if args.get(1).map(String::as_str) == Some("--self-check") {
        match tauri::Builder::default().build(context) {
            Ok(_app) => {
                println!("{}", update::VERSION);
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
    // Already running: bring that window up instead of starting a second copy.
    // Right after an update the old copy is still closing: give it a moment.
    let after_update = args.iter().any(|a| a == "--after-update");
    let mut show_signal = single::claim(!after_update);
    for _ in 0..40 {
        if show_signal.is_some() || !after_update {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        show_signal = single::claim(!after_update);
    }
    let Some(show_signal) = show_signal else {
        return;
    };
    // Only now: a launch that just raised the window mustn't delete the update
    // the running copy is waiting to install.
    update::clean_up();
    // At login it starts in the tray; opened by hand, the window shows.
    let background = args.iter().any(|a| a == "--background");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let settings = config::load();
    let shared = Shared::new(settings.mic_threshold);
    let pc = Arc::new(Mutex::new(Arc::new(Pc::new(settings.pc.clone()))));
    let (link_tx, link_rx) = watch::channel(link_for(&settings));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (up_tx, up_rx) = mpsc::channel(audio::UP_QUEUE);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    let needs_setup = link_for(&settings).is_none();

    tauri::Builder::default()
        .manage(AppState {
            settings: Mutex::new(settings),
            link: link_tx,
            cmds: cmd_tx.clone(),
            shared: shared.clone(),
            pc: pc.clone(),
            status: Mutex::new(StatusView::default()),
            tray: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_status,
            save_settings,
            send_text,
            stop_talking,
            push_to_talk,
            her_state,
            orb_drawing,
            audio_devices
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Tray icon: click to show or hide, right-click for the menu.
            let show_item =
                MenuItem::with_id(app, "show", "Show A.D.R.I.A.N.", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("A.D.R.I.A.N.")
                .menu(&menu)
                .show_menu_on_left_click(false);
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.on_menu_event(|app, e| match e.id.as_ref() {
                "show" => show(app),
                "quit" => app.exit(0),
                _ => {}
            })
            .on_tray_icon_event(|t, e| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = e
                {
                    toggle(t.app_handle());
                }
            })
            .build(app)?;

            // Push-to-talk: Ctrl+Alt+Space held down.
            match GlobalHotKeyManager::new() {
                Ok(manager) => {
                    let key = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
                    if manager.register(key).is_ok() {
                        // Registration lasts as long as the manager: keep it for the app's life.
                        Box::leak(Box::new(manager));
                        let (handle, shared, cmds) =
                            (handle.clone(), shared.clone(), cmd_tx.clone());
                        std::thread::spawn(move || {
                            while let Ok(ev) = GlobalHotKeyEvent::receiver().recv() {
                                if ev.id != key.id() {
                                    continue;
                                }
                                let down = ev.state == HotKeyState::Pressed;
                                if !link::talk_hotkey(&shared, &cmds, down) {
                                    let _ = handle.emit("not_sent", link::NOT_SENT);
                                }
                            }
                        });
                    }
                }
                Err(e) => eprintln!("push-to-talk hotkey unavailable: {e}"),
            }

            // Audio and the server link.
            audio::start(shared.clone(), up_tx.clone());
            tauri::async_runtime::spawn(link::run(
                link_rx,
                up_rx,
                cmd_rx,
                shared.clone(),
                pc.clone(),
                ev_tx,
            ));

            // Server events → window and tray, and her state ~20 times a second.
            {
                let (handle, shared) = (handle.clone(), shared.clone());
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        match ev {
                            Event::Status {
                                connected,
                                text,
                                owner,
                            } => {
                                // Only a welcome from the server says connected.
                                if connected {
                                    update::welcomed();
                                }
                                let v = StatusView {
                                    connected,
                                    text,
                                    owner,
                                };
                                *handle
                                    .state::<AppState>()
                                    .status
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) = v.clone();
                                let _ = handle.emit("status", v);
                                refresh_tray(&handle);
                            }
                            Event::Thinking(on) => {
                                shared.answering.store(on, Ordering::Relaxed);
                            }
                            Event::Transcript { who, text, last } => {
                                let _ = handle.emit(
                                    "transcript",
                                    serde_json::json!({ "who": who, "text": text, "last": last }),
                                );
                            }
                        }
                    }
                });
            }
            {
                let (handle, shared) = (handle.clone(), shared.clone());
                std::thread::spawn(move || {
                    let mut drawing = None;
                    let mut pacer = Pacer::default();
                    let mut shown_devices: Option<Devices> = None;
                    let mut mic_error: Option<String> = None;
                    let mut quiet = None;
                    let mut minimized_since: Option<std::time::Instant> = None;
                    let started = std::time::Instant::now();
                    let mut game_checked: Option<std::time::Instant> = None;
                    let mut playing = adrian_pc_control::activity::Debounce::default();
                    for tick in 0u64.. {
                        // No window to draw for: check twice a second and nothing else.
                        let window = handle.get_webview_window("main");
                        std::thread::sleep(Duration::from_millis(if window.is_some() {
                            50
                        } else {
                            500
                        }));
                        if window.is_none() || tick % 10 == 0 {
                            // Idle in the tray: efficiency mode until she's needed.
                            // Passing what's said on to the server is light work; only
                            // her answers and the window need full speed.
                            let busy = window.is_some()
                                || shared.playing()
                                || shared.ptt.load(Ordering::Relaxed)
                                || shared.answering.load(Ordering::Relaxed);
                            if quiet != Some(!busy) {
                                quiet = Some(!busy);
                                quiet::background(!busy);
                                if !busy {
                                    quiet::trim();
                                }
                            }
                            // Left minimized for a while: tuck it into the tray for real.
                            let minimized = window
                                .as_ref()
                                .is_some_and(|w| w.is_minimized().unwrap_or(false));
                            match (minimized, minimized_since) {
                                (true, None) => minimized_since = Some(std::time::Instant::now()),
                                (true, Some(t)) if t.elapsed() > MINIMIZED_TO_TRAY => {
                                    minimized_since = None;
                                    let h = handle.clone();
                                    let _ = handle.run_on_main_thread(move || close(&h));
                                }
                                (false, _) => minimized_since = None,
                                _ => {}
                            }
                            // Speakers or mic switched: tell the settings page.
                            if window.is_some() {
                                let d = devices(&shared);
                                if shown_devices.as_ref() != Some(&d) {
                                    let _ = handle.emit("devices", d.clone());
                                    shown_devices = Some(d);
                                }
                            } else {
                                shown_devices = None;
                            }
                            // The mic failed or came back: nothing else tells the tray.
                            let error = shared.mic_error.lock().ok().and_then(|e| e.clone());
                            if error != mic_error {
                                mic_error = error;
                                refresh_tray(&handle);
                            }
                            let on = window.is_some() && orb_on_screen(&handle);
                            if drawing != Some(on) {
                                drawing = Some(on);
                                let _ = handle.emit("draw", on);
                            }
                            // Playing a game now: the server keeps her quiet unless named.
                            if game_checked.is_none_or(|t| t.elapsed() >= GAME_CHECK) {
                                game_checked = Some(std::time::Instant::now());
                                let state = handle.state::<AppState>();
                                let wanted = !state
                                    .settings
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .quiet_off;
                                let change = if wanted {
                                    let pc =
                                        state.pc.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                    let seen = pc.game_in_front(fullscreen_app_in_front());
                                    playing.observe(
                                        started.elapsed().as_millis() as u64,
                                        seen.as_ref(),
                                    )
                                } else {
                                    playing.reset()
                                };
                                if let Some(a) = change {
                                    *shared.activity.lock().unwrap_or_else(|e| e.into_inner()) = a;
                                    // Offline it waits for the next welcome, which sends it.
                                    link::offer(&shared, &state.cmds, Cmd::Activity);
                                }
                            }
                        }
                        if window.is_none() {
                            pacer = Pacer::default();
                            continue;
                        }
                        let f = pacer.next(&shared, drawing == Some(true), tick % 10 == 0);
                        if let Some(f) = f {
                            let _ = handle.emit("frame", f);
                        }
                    }
                });
            }
            if needs_setup || !background {
                show(&handle);
            }
            // New versions from GitHub, installed while she's idle in the tray.
            {
                let (h1, h2, h3, shared) = (
                    handle.clone(),
                    handle.clone(),
                    handle.clone(),
                    shared.clone(),
                );
                std::thread::spawn(move || {
                    update::run(
                        move || {
                            !h1.state::<AppState>()
                                .settings
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .updates_off
                        },
                        move || {
                            h2.get_webview_window("main").is_none()
                                && !shared.playing()
                                && !shared.talking.load(Ordering::Relaxed)
                                && !shared.ptt.load(Ordering::Relaxed)
                                && !shared.answering.load(Ordering::Relaxed)
                                // A restart mid-match would take the window focus.
                                && !fullscreen_app_in_front()
                        },
                        move || h3.exit(0),
                    )
                });
            }
            // Opened again (Start menu, desktop icon): show the window.
            {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    while show_signal.wait() {
                        let h = handle.clone();
                        let _ = handle.run_on_main_thread(move || show(&h));
                    }
                });
            }
            Ok(())
        })
        .build(context)
        .expect("A.D.R.I.A.N. failed to start")
        .run(|_, e| {
            // Closing the window isn't quitting: she keeps running in the tray.
            // Quit (in the tray menu) passes an exit code and goes through.
            if let RunEvent::ExitRequested {
                code: None, api, ..
            } = e
            {
                api.prevent_exit();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_she_never_looks_like_she_is_listening() {
        let shared = Shared::new(0.0);
        // Held from before the link fell, or the mic gate open on a loud room.
        shared.ptt.store(true, Ordering::Relaxed);
        shared.talking.store(true, Ordering::Relaxed);
        assert_eq!(frame(&shared).mode, "idle");
        shared.set_online(true);
        shared.ptt.store(true, Ordering::Relaxed);
        assert_eq!(frame(&shared).mode, "listening");
    }

    #[test]
    fn a_drawing_orb_gets_every_frame() {
        let shared = Shared::new(0.0);
        let mut pacer = Pacer::default();
        assert!(pacer.next(&shared, true, false).is_some());
        assert!(pacer.next(&shared, true, false).is_some());
    }

    #[test]
    fn a_paused_orb_still_hears_every_change_and_nothing_else() {
        let shared = Shared::new(0.0);
        let mut pacer = Pacer::default();
        // Only slow ticks look at all.
        assert!(pacer.next(&shared, false, false).is_none());
        assert!(pacer.next(&shared, false, true).is_some());
        assert!(pacer.next(&shared, false, true).is_none());

        // She starts answering behind a full-screen app: the Stop button has to show.
        shared.answering.store(true, Ordering::Relaxed);
        let f = pacer.next(&shared, false, true).expect("a change");
        assert_eq!(f.mode, "thinking");
        assert!(pacer.next(&shared, false, true).is_none());

        // And goes back.
        shared.answering.store(false, Ordering::Relaxed);
        assert_eq!(pacer.next(&shared, false, true).unwrap().mode, "idle");

        // A mic failure counts, for the chip.
        *shared.mic_error.lock().unwrap() = Some("No microphone found.".into());
        let f = pacer.next(&shared, false, true).expect("a change");
        assert_eq!(f.mic_error.as_deref(), Some("No microphone found."));
        assert!(pacer.next(&shared, false, true).is_none());
    }

    #[test]
    fn pausing_again_starts_from_a_fresh_frame() {
        let shared = Shared::new(0.0);
        let mut pacer = Pacer::default();
        assert!(pacer.next(&shared, false, true).is_some());
        assert!(pacer.next(&shared, true, false).is_some());
        assert!(pacer.next(&shared, false, true).is_some());
    }
}
