//! System-tray icon for the satellite.
//!
//! Windows: `tray-icon` requires a win32 message loop on the thread that
//! created the icon, so we spawn a dedicated thread that owns the icon and
//! pumps messages with `PeekMessage`. The menu carries a live status line
//! (fed by a `watch` channel) and Quit, which notifies the run loop.
//!
//! Other platforms: a no-op stub — the satellite runs in the console there.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{Notify, watch};

#[cfg(windows)]
pub fn start(status: watch::Receiver<String>, quit: Arc<Notify>) -> Result<()> {
    use tray_icon::{
        Icon, TrayIconBuilder,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };

    std::thread::Builder::new()
        .name("adrian-tray".into())
        .spawn(move || {
            let status = status;
            let menu = Menu::new();
            let status_item = MenuItem::new("Adrian: starting", false, None);
            let _ = menu.append(&status_item);
            let _ = menu.append(&PredefinedMenuItem::separator());
            let quit_item = MenuItem::new("Quit", true, None);
            let _ = menu.append(&quit_item);
            let quit_id = quit_item.id().clone();

            let icon = match Icon::from_rgba(make_icon(), 32, 32) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("tray icon unavailable: {e}");
                    return;
                }
            };
            let _tray = match TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Adrian Satellite")
                .with_icon(icon)
                .build()
            {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("tray unavailable: {e}");
                    return;
                }
            };

            // Win32 message pump + event drains. PeekMessage keeps this
            // responsive to menu clicks and status/watch updates without
            // blocking on GetMessage (which would starve the channel polls).
            let mut msg = MSG::default();
            loop {
                unsafe {
                    while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                while let Ok(ev) = MenuEvent::receiver().try_recv() {
                    if ev.id == quit_id {
                        quit.notify_one();
                        return;
                    }
                }
                if status.has_changed().unwrap_or(false) {
                    let text = status.borrow().clone();
                    status_item.set_text(format!("Adrian: {text}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })?;
    Ok(())
}

/// 32x32 RGBA icon: a filled circle. Generated so the binary needs no
/// embedded asset.
#[cfg(windows)]
fn make_icon() -> Vec<u8> {
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for y in 0..32u32 {
        for x in 0..32u32 {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if dx * dx + dy * dy <= 14.0 * 14.0 {
                let i = ((y * 32 + x) * 4) as usize;
                rgba[i] = 0x3b; // steel blue
                rgba[i + 1] = 0x82;
                rgba[i + 2] = 0xf6;
                rgba[i + 3] = 0xff;
            }
        }
    }
    rgba
}

#[cfg(not(windows))]
pub fn start(_status: watch::Receiver<String>, _quit: Arc<Notify>) -> Result<()> {
    Ok(())
}
