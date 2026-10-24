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

use adrian_pc_control::{Pc, Policy};
use audio::Shared;
use config::Settings;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use link::{Cmd, Event, Link};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
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
    settings_file: String,
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
    SettingsView {
        server_url: s.server_url.clone(),
        paired: s.token().is_some(),
        mic_threshold: s.mic_threshold,
        pc: s.pc.clone(),
        settings_file: config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
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
) -> Result<(), String> {
    if config::normalize_url(&server_url).is_none() {
        return Err("Enter the server address, like homeserver:8791.".into());
    }
    let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let before = (s.url(), s.token());
    s.server_url = server_url.trim().to_string();
    if let Some(t) = token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        if !t.starts_with("sat_") {
            return Err("That doesn't look like a pairing token (they start with sat_).".into());
        }
        s.set_token(&t)?;
    }
    s.mic_threshold = mic_threshold.clamp(0.0, 0.1);
    s.pc = pc;
    config::save(&s)?;
    state
        .shared
        .threshold
        .store(s.mic_threshold.to_bits(), Ordering::Relaxed);
    let new_pc = Arc::new(Pc::new(s.pc.clone()));
    *state.pc.lock().unwrap_or_else(|e| e.into_inner()) = new_pc;
    // Reconnect only when the connection itself changed: a reconnect drops
    // whatever she's in the middle of.
    if (s.url(), s.token()) != before {
        let _ = state.link.send(link_for(&s));
    }
    Ok(())
}

#[tauri::command]
fn send_text(state: tauri::State<'_, AppState>, text: String) {
    let t = text.trim();
    if !t.is_empty() {
        let _ = state.cmds.send(Cmd::Text(t.chars().take(2000).collect()));
    }
}

#[tauri::command]
fn stop_talking(state: tauri::State<'_, AppState>) {
    let _ = state.cmds.send(Cmd::Stop);
}

/// The talk button: held down, the mic is open and what's said is for her.
#[tauri::command]
fn push_to_talk(state: tauri::State<'_, AppState>, down: bool) {
    state.shared.ptt.store(down, Ordering::Relaxed);
    if down {
        let _ = state.cmds.send(Cmd::Ptt);
    }
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
    // Already running: bring that window up instead of starting a second copy.
    let Some(show_signal) = single::claim() else {
        return;
    };
    // At login it starts in the tray; opened by hand, the window shows.
    let background = args.iter().any(|a| a == "--background");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let settings = config::load();
    let shared = Shared::new(settings.mic_threshold);
    let pc = Arc::new(Mutex::new(Arc::new(Pc::new(settings.pc.clone()))));
    let (link_tx, link_rx) = watch::channel(link_for(&settings));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (up_tx, up_rx) = mpsc::unbounded_channel();
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
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_status,
            save_settings,
            send_text,
            stop_talking,
            push_to_talk,
            orb_drawing
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
                        let (shared, cmds) = (shared.clone(), cmd_tx.clone());
                        std::thread::spawn(move || {
                            while let Ok(ev) = GlobalHotKeyEvent::receiver().recv() {
                                if ev.id != key.id() {
                                    continue;
                                }
                                let down = ev.state == HotKeyState::Pressed;
                                shared.ptt.store(down, Ordering::Relaxed);
                                if down {
                                    let _ = cmds.send(Cmd::Ptt);
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

            // Server events → window, and her state ~20 times a second.
            let thinking = Arc::new(AtomicBool::new(false));
            {
                let (handle, thinking) = (handle.clone(), thinking.clone());
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        match ev {
                            Event::Status {
                                connected,
                                text,
                                owner,
                            } => {
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
                            }
                            Event::Thinking(on) => thinking.store(on, Ordering::Relaxed),
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
                    let mut quiet = None;
                    let mut minimized_since: Option<std::time::Instant> = None;
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
                                || thinking.load(Ordering::Relaxed);
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
                            let on = window.is_some() && orb_on_screen(&handle);
                            if drawing != Some(on) {
                                drawing = Some(on);
                                let _ = handle.emit("draw", on);
                            }
                        }
                        if drawing != Some(true) {
                            continue;
                        }
                        let out = f32::from_bits(shared.out_level.load(Ordering::Relaxed));
                        let mic = f32::from_bits(shared.mic_level.load(Ordering::Relaxed));
                        let (mode, level) = if shared.playing() || out > 0.03 {
                            ("speaking", out)
                        } else if shared.talking.load(Ordering::Relaxed)
                            || shared.ptt.load(Ordering::Relaxed)
                        {
                            ("listening", mic)
                        } else if thinking.load(Ordering::Relaxed) {
                            ("thinking", 0.0)
                        } else {
                            ("idle", 0.0)
                        };
                        let mic_error = shared.mic_error.lock().ok().and_then(|e| e.clone());
                        let _ = handle.emit(
                            "frame",
                            Frame {
                                mode,
                                level,
                                mic,
                                mic_error,
                            },
                        );
                    }
                });
            }
            if needs_setup || !background {
                show(&handle);
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
        .build(tauri::generate_context!())
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
