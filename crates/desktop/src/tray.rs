//! The tray icon. Most of the time it's all there is of her, so it says how
//! she's doing: the tooltip has the state, and a dot on the icon means she
//! can't hear you. Only ever updated when that state changes.

use std::sync::Mutex;
use tauri::AppHandle;
use tauri::image::Image;

/// Windows' tooltip field holds 128 UTF-16 units, and tray-icon copies a longer
/// text in with no room for the terminator.
const TIP_LIMIT: usize = 127;
/// The dot, and the dark edge that keeps it apart from the icon.
const DOT: [u8; 4] = [0xff, 0x8a, 0x3d, 0xff];
const EDGE: [u8; 4] = [0x06, 0x08, 0x0b, 0xff];

#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub tip: String,
    pub warn: bool,
}

/// What the tray should show for the link's status and any audio failure.
pub fn view(connected: bool, status: &str, audio_error: Option<&str>) -> View {
    let lines = [Some("A.D.R.I.A.N."), Some(status), audio_error];
    let tip = lines
        .into_iter()
        .flatten()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    View {
        tip: cut(&tip),
        warn: !connected || audio_error.is_some(),
    }
}

fn cut(text: &str) -> String {
    if text.encode_utf16().count() <= TIP_LIMIT {
        return text.to_string();
    }
    let mut used = 1; // the ellipsis
    let mut out = String::new();
    for c in text.chars() {
        used += c.len_utf16();
        if used > TIP_LIMIT {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The icon with the dot in its bottom right corner. It has to read at tray
/// size, and the source is about that small, so it's a big dot.
pub fn marked(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let size = width.min(height) as f32;
    let radius = size * 0.18;
    let edge = radius + size * 0.06;
    let (cx, cy) = (width as f32 - edge, height as f32 - edge);
    let mut out = rgba.to_vec();
    for (i, pixel) in out.chunks_exact_mut(4).enumerate() {
        let i = i as u32;
        let (x, y) = ((i % width) as f32 + 0.5, (i / width) as f32 + 0.5);
        let from_center = (x - cx).hypot(y - cy);
        if from_center <= radius {
            pixel.copy_from_slice(&DOT);
        } else if from_center <= edge {
            pixel.copy_from_slice(&EDGE);
        }
    }
    out
}

/// Put `view` in the tray if it isn't there already. Main thread only: that
/// keeps two updates from crossing, and tray calls run there anyway.
pub fn show(app: &AppHandle, view: View, shown: &Mutex<Option<View>>) {
    let mut shown = shown.lock().unwrap_or_else(|e| e.into_inner());
    if shown.as_ref() == Some(&view) {
        return;
    }
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let _ = tray.set_tooltip(Some(&view.tip));
    if shown.as_ref().is_some_and(|s| s.warn) != view.warn
        && let Some(icon) = app.default_window_icon()
    {
        let (w, h) = (icon.width(), icon.height());
        let image = if view.warn {
            Image::new_owned(marked(icon.rgba(), w, h), w, h)
        } else {
            icon.clone()
        };
        let _ = tray.set_icon(Some(image));
    }
    *shown = Some(view);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_she_is_and_marks_anything_wrong() {
        let up = view(true, "Connected as Desk", None);
        assert_eq!(up.tip, "A.D.R.I.A.N.\nConnected as Desk");
        assert!(!up.warn);
        let refused = view(false, "The server refused this PC (bad token).", None);
        assert!(refused.tip.contains("refused"));
        assert!(refused.warn);
        let no_mic = view(true, "Connected as Desk", Some("No microphone found."));
        assert!(
            no_mic
                .tip
                .ends_with("Connected as Desk\nNo microphone found.")
        );
        assert!(no_mic.warn);
    }

    #[test]
    fn says_something_before_the_first_status() {
        assert_eq!(view(false, "", None).tip, "A.D.R.I.A.N.");
    }

    #[test]
    fn a_long_status_stays_inside_the_tooltip_field() {
        for long in ["x".repeat(500), "€".repeat(500), "😀".repeat(500)] {
            let tip = view(false, &long, None).tip;
            assert!(tip.encode_utf16().count() <= TIP_LIMIT, "{tip}");
            assert!(tip.ends_with('…'));
        }
        let fits = "x".repeat(TIP_LIMIT - "A.D.R.I.A.N.\n".len());
        assert!(view(false, &fits, None).tip.ends_with(&fits));
    }

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> &[u8] {
        let at = ((y * width + x) * 4) as usize;
        &rgba[at..at + 4]
    }

    #[test]
    fn the_dot_sits_in_the_corner_and_leaves_the_rest_alone() {
        // The size of the icon Tauri actually hands out, and a bigger one.
        for size in [16u32, 32, 256] {
            let plain = vec![0x40; (size * size * 4) as usize];
            let dotted = marked(&plain, size, size);
            assert_eq!(dotted.len(), plain.len());
            let c = size * 3 / 4;
            assert_eq!(pixel(&dotted, size, c, c), DOT);
            assert_eq!(pixel(&dotted, size, 0, 0), &[0x40; 4]);
            assert_eq!(pixel(&dotted, size, size - 1, 0), &[0x40; 4]);
            assert_eq!(pixel(&dotted, size, 0, size - 1), &[0x40; 4]);
            let dots = dotted.chunks_exact(4).filter(|p| *p == DOT).count();
            // About a tenth of the icon: visible at 16 px without hiding it.
            let share = dots as f32 / (size * size) as f32;
            assert!((0.07..0.15).contains(&share), "{size}: {share}");
        }
    }
}
