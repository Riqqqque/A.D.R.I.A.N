//! Window layout: snap to a half or a quarter, maximize, restore, center, move
//! to another monitor, put back, and list the monitors.
//!
//! Everything is in physical pixels. Each call switches its own thread to the
//! per-monitor (v2) DPI context for its duration, so window rects, DWM frame
//! bounds and monitor work areas agree on every monitor, whatever DPI mode the
//! process itself runs in. Snaps use the work area (the taskbar's edge, any
//! edge, auto-hide or not) and the window's visible frame, so the invisible
//! resize borders don't leave gaps or overlaps.
//!
//! Windows are only moved with SetWindowPos and ShowWindow: no keystrokes, no
//! hooks. Every move is read back, and only what really happened is reported.
//! Windows of games, full-screen windows, windows on another virtual desktop
//! and windows that aren't responding are left alone, and nothing is raised
//! over a full-screen game or a game in front. One action is a handful of
//! system calls: no threads, no polling.

use super::apps::Index;
use super::wnd::{self, Found, Win};
use super::{exe_stem, from_wide};
use crate::Outcome;
use serde_json::Value;
use std::sync::Mutex;
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{
    BOOL, CloseHandle, E_ACCESSDENIED, ERROR_ACCESS_DENIED, ERROR_SUCCESS, GetLastError, HANDLE,
    HWND, LPARAM, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    GetDpiForWindow, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    IVirtualDesktopManager, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    SHQueryUserNotificationState, VirtualDesktopManager,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, HWND_TOP, IsHungAppWindow, IsIconic, IsWindow,
    IsWindowVisible, IsZoomed, SMTO_ABORTIFHUNG, SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageTimeoutW, SetWindowPos, ShowWindow, WM_NULL, WS_CAPTION, WS_EX_TOPMOST,
    WS_MAXIMIZEBOX, WS_THICKFRAME,
};

use geo::{Mon, Rect, Zone};

/// The geometry, kept apart from Windows so it can be tested with made-up
/// monitor layouts. All rects are physical pixels in virtual-screen
/// coordinates, right and bottom exclusive, like Windows' RECT.
pub mod geo {
    use crate::matching::{normalize, score};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Rect {
        pub l: i32,
        pub t: i32,
        pub r: i32,
        pub b: i32,
    }

    impl Rect {
        pub const fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
            Rect { l, t, r, b }
        }
        /// `w` by `h` with its top left corner at `l`, `t`.
        pub const fn sized(l: i32, t: i32, w: i32, h: i32) -> Self {
            Rect {
                l,
                t,
                r: l + w,
                b: t + h,
            }
        }
        pub const fn w(&self) -> i32 {
            self.r - self.l
        }
        pub const fn h(&self) -> i32 {
            self.b - self.t
        }
        pub const fn is_empty(&self) -> bool {
            self.w() <= 0 || self.h() <= 0
        }
        pub fn overlap(&self, o: &Rect) -> i64 {
            let w = (self.r.min(o.r) - self.l.max(o.l)).max(0) as i64;
            let h = (self.b.min(o.b) - self.t.max(o.t)).max(0) as i64;
            w * h
        }
        pub fn covers(&self, o: &Rect) -> bool {
            self.l <= o.l && self.t <= o.t && self.r >= o.r && self.b >= o.b
        }
        /// Every edge within `tol` pixels.
        pub fn near(&self, o: &Rect, tol: i32) -> bool {
            (self.l - o.l).abs() <= tol
                && (self.t - o.t).abs() <= tol
                && (self.r - o.r).abs() <= tol
                && (self.b - o.b).abs() <= tol
        }
        fn center(&self) -> (i64, i64) {
            (
                (self.l as i64 + self.r as i64) / 2,
                (self.t as i64 + self.b as i64) / 2,
            )
        }
    }

    /// How far a window's rect reaches past what you see of it: the
    /// invisible resize borders DWM draws around normal windows (about 7 px a
    /// side at 100%, more at higher scales), none for borderless windows.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Insets {
        pub l: i32,
        pub t: i32,
        pub r: i32,
        pub b: i32,
    }

    /// The borders between a window's rect (`GetWindowRect`) and its visible
    /// frame (`DWMWA_EXTENDED_FRAME_BOUNDS`). Nonsense (a frame outside the
    /// rect, or "borders" wider than any real one) counts as none.
    pub fn insets(outer: Rect, frame: Rect) -> Insets {
        if frame.is_empty() || !outer.covers(&frame) {
            return Insets::default();
        }
        let i = Insets {
            l: frame.l - outer.l,
            t: frame.t - outer.t,
            r: outer.r - frame.r,
            b: outer.b - frame.b,
        };
        if [i.l, i.t, i.r, i.b].iter().any(|&v| v > 64) {
            return Insets::default();
        }
        i
    }

    /// The window rect that shows exactly `visible`.
    pub fn outer_for(visible: Rect, i: Insets) -> Rect {
        Rect::new(
            visible.l - i.l,
            visible.t - i.t,
            visible.r + i.r,
            visible.b + i.b,
        )
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Zone {
        Left,
        Right,
        Top,
        Bottom,
        TopLeft,
        TopRight,
        BottomLeft,
        BottomRight,
    }

    /// Where along one axis a zone sits.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Span {
        Start,
        End,
        Whole,
    }

    impl Zone {
        pub const ALL: [Zone; 8] = [
            Zone::Left,
            Zone::Right,
            Zone::Top,
            Zone::Bottom,
            Zone::TopLeft,
            Zone::TopRight,
            Zone::BottomLeft,
            Zone::BottomRight,
        ];

        /// "left", "top_right", "upper left", "bottom-left quarter"...
        pub fn parse(s: &str) -> Option<Zone> {
            let s = s.to_lowercase().replace(['_', '-'], " ");
            let words: Vec<&str> = s
                .split_whitespace()
                .filter(|w| !["the", "half", "side", "quarter", "corner", "of"].contains(w))
                .collect();
            let v = |w: &str| ["top", "upper", "up"].contains(&w);
            let d = |w: &str| ["bottom", "lower", "down"].contains(&w);
            Some(match words.as_slice() {
                ["left"] => Zone::Left,
                ["right"] => Zone::Right,
                [w] if v(w) => Zone::Top,
                [w] if d(w) => Zone::Bottom,
                [a, "left"] | ["left", a] if v(a) => Zone::TopLeft,
                [a, "right"] | ["right", a] if v(a) => Zone::TopRight,
                [a, "left"] | ["left", a] if d(a) => Zone::BottomLeft,
                [a, "right"] | ["right", a] if d(a) => Zone::BottomRight,
                _ => return None,
            })
        }

        /// For sentences: "the left half", "the top right quarter".
        pub fn words(self) -> &'static str {
            match self {
                Zone::Left => "the left half",
                Zone::Right => "the right half",
                Zone::Top => "the top half",
                Zone::Bottom => "the bottom half",
                Zone::TopLeft => "the top left quarter",
                Zone::TopRight => "the top right quarter",
                Zone::BottomLeft => "the bottom left quarter",
                Zone::BottomRight => "the bottom right quarter",
            }
        }

        fn across(self) -> Span {
            match self {
                Zone::Left | Zone::TopLeft | Zone::BottomLeft => Span::Start,
                Zone::Right | Zone::TopRight | Zone::BottomRight => Span::End,
                Zone::Top | Zone::Bottom => Span::Whole,
            }
        }

        fn down(self) -> Span {
            match self {
                Zone::Top | Zone::TopLeft | Zone::TopRight => Span::Start,
                Zone::Bottom | Zone::BottomLeft | Zone::BottomRight => Span::End,
                Zone::Left | Zone::Right => Span::Whole,
            }
        }
    }

    /// A zone of a work area. The two halves meet at one middle line, so
    /// windows on either side touch with no gap and no overlap, odd sizes too.
    pub fn zone_rect(work: Rect, z: Zone) -> Rect {
        let mx = work.l + work.w() / 2;
        let my = work.t + work.h() / 2;
        let (l, r) = match z.across() {
            Span::Start => (work.l, mx),
            Span::End => (mx, work.r),
            Span::Whole => (work.l, work.r),
        };
        let (t, b) = match z.down() {
            Span::Start => (work.t, my),
            Span::End => (my, work.b),
            Span::Whole => (work.t, work.b),
        };
        Rect::new(l, t, r, b)
    }

    /// How close a visible frame must be to count as already in a zone.
    pub const SNAP_TOL: i32 = 4;

    /// The zone a visible frame already fills, if any.
    pub fn zone_of(visible: Rect, work: Rect) -> Option<Zone> {
        Zone::ALL
            .into_iter()
            .find(|&z| visible.near(&zone_rect(work, z), SNAP_TOL))
    }

    /// `r` moved (not resized) to lie inside `work`; when it's bigger than
    /// `work`, its top left corner goes to the work area's.
    pub fn clamp_into(r: Rect, work: Rect) -> Rect {
        let (w, h) = (r.w(), r.h());
        let l = if w >= work.w() {
            work.l
        } else {
            r.l.clamp(work.l, work.r - w)
        };
        let t = if h >= work.h() {
            work.t
        } else {
            r.t.clamp(work.t, work.b - h)
        };
        Rect::sized(l, t, w, h)
    }

    /// `r`'s size (at most the work area's) in the middle of `work`.
    pub fn centered(r: Rect, work: Rect) -> Rect {
        let (w, h) = (r.w().min(work.w()), r.h().min(work.h()));
        Rect::sized(
            work.l + (work.w() - w) / 2,
            work.t + (work.h() - h) / 2,
            w,
            h,
        )
    }

    /// Where a `w` by `h` window goes when it can't take the zone's size (a
    /// minimum or maximum size): against the zone's outer edges, centered
    /// along a side it spans, and on screen when it fits.
    pub fn anchor(target: Rect, w: i32, h: i32, z: Option<Zone>, work: Rect) -> Rect {
        let pos = |span: Option<Span>, start: i32, end: i32, size: i32| match span {
            Some(Span::Start) => start,
            Some(Span::End) => end - size,
            _ => start + (end - start - size) / 2,
        };
        let l = pos(z.map(Zone::across), target.l, target.r, w);
        let t = pos(z.map(Zone::down), target.t, target.b, h);
        let r = Rect::sized(l, t, w, h);
        let (fits_w, fits_h) = (w <= work.w(), h <= work.h());
        if fits_w && fits_h {
            clamp_into(r, work)
        } else {
            r
        }
    }

    /// What to say when a window kept its own size limits: "Discord can't
    /// get that narrow, so it's as narrow as it goes on the left half."
    pub fn fit_note(app: &str, target: Rect, got: Rect, place: &str) -> Option<String> {
        const TOL: i32 = 2;
        let mut limits = Vec::new();
        if got.w() > target.w() + TOL {
            limits.push("narrow");
        } else if got.w() < target.w() - TOL {
            limits.push("wide");
        }
        if got.h() > target.h() + TOL {
            limits.push("short");
        } else if got.h() < target.h() - TOL {
            limits.push("tall");
        }
        match limits.as_slice() {
            [] => None,
            [a] => Some(format!(
                "{app} can't get that {a}, so it's as {a} as it goes on {place}."
            )),
            [a, b] => Some(format!(
                "{app} can't get that {a} or {b}, so it's as close as it gets to {place}."
            )),
            _ => None,
        }
    }

    /// Full screen: covering the whole monitor, taskbar included, without a
    /// title bar, and not merely maximized (a maximized window covers the
    /// whole monitor when the taskbar auto-hides).
    pub fn is_fullscreen(outer: Rect, monitor: Rect, captioned: bool, zoomed: bool) -> bool {
        !zoomed && !captioned && outer.covers(&monitor)
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct Mon {
        /// As Windows numbers it: the n of `\\.\DISPLAYn`.
        pub num: u32,
        pub primary: bool,
        pub rect: Rect,
        /// Without the taskbar and docked toolbars.
        pub work: Rect,
        /// Effective DPI: 96 is 100%, 144 is 150%.
        pub dpi: u32,
        /// The monitor's own name ("LG ULTRAGEAR"), empty when not looked up.
        pub name: String,
        /// The HMONITOR, as a number (0 in tests).
        pub id: isize,
    }

    /// The monitor showing most of `r`; the nearest one when none does.
    pub fn monitor_of(r: Rect, mons: &[Mon]) -> usize {
        let best = (0..mons.len()).max_by_key(|&i| mons[i].rect.overlap(&r));
        match best {
            Some(i) if mons[i].rect.overlap(&r) > 0 => i,
            _ => {
                let (cx, cy) = r.center();
                (0..mons.len())
                    .min_by_key(|&i| {
                        let (mx, my) = mons[i].rect.center();
                        (mx - cx).pow(2) + (my - cy).pow(2)
                    })
                    .unwrap_or(0)
            }
        }
    }

    /// Where each monitor sits, when they're in one row ("left", "middle",
    /// "right") or one column ("top", "middle", "bottom") of up to three.
    pub fn positions(mons: &[Mon]) -> Vec<Option<&'static str>> {
        let n = mons.len();
        let mut out = vec![None; n];
        if !(2..=3).contains(&n) {
            return out;
        }
        let xs: Vec<i64> = mons.iter().map(|m| m.rect.center().0).collect();
        let ys: Vec<i64> = mons.iter().map(|m| m.rect.center().1).collect();
        let spread = |v: &[i64]| v.iter().max().unwrap_or(&0) - v.iter().min().unwrap_or(&0);
        let (keys, names) = if spread(&xs) >= spread(&ys) {
            (xs, ["left", "middle", "right"])
        } else {
            (ys, ["top", "middle", "bottom"])
        };
        let row = names[0] == "left";
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| keys[i]);
        let span = |i: usize| {
            let r = &mons[i].rect;
            if row { (r.l, r.r) } else { (r.t, r.b) }
        };
        // One row (or column): side by side, none reaching into the next.
        if order.windows(2).any(|p| span(p[1]).0 < span(p[0]).1) {
            return out;
        }
        for (rank, &i) in order.iter().enumerate() {
            out[i] = Some(match (n, rank) {
                (2, 0) | (3, 0) => names[0],
                (2, 1) | (3, 2) => names[2],
                _ => names[1],
            });
        }
        out
    }

    /// "the main monitor", "the left monitor", "monitor 3".
    pub fn describe(mons: &[Mon], i: usize) -> String {
        if mons[i].primary {
            return "the main monitor".into();
        }
        if mons.len() == 2 {
            return match positions(mons)[i] {
                Some(p) => format!("the {p} monitor"),
                None => "the other monitor".into(),
            };
        }
        match positions(mons)[i] {
            Some(p) => format!("the {p} monitor"),
            None => format!("monitor {}", mons[i].num),
        }
    }

    /// Scale as people say it: 144 DPI â†’ 150.
    pub fn scale_percent(dpi: u32) -> u32 {
        (dpi * 100 + 48) / 96
    }

    /// "1 (main, right): 2560x1440 at 125%".
    pub fn summary(mons: &[Mon], i: usize) -> String {
        let m = &mons[i];
        let mut tags: Vec<String> = Vec::new();
        if m.primary {
            tags.push("main".into());
        }
        if let Some(p) = positions(mons)[i] {
            tags.push(p.into());
        }
        if !m.name.is_empty() {
            tags.push(m.name.clone());
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", tags.join(", "))
        };
        format!(
            "{}{tags}: {}x{} at {}%",
            m.num,
            m.rect.w(),
            m.rect.h(),
            scale_percent(m.dpi)
        )
    }

    fn number(s: &str) -> Option<u32> {
        if let Ok(n) = s.parse() {
            return Some(n);
        }
        let words = [
            ["one", "first"],
            ["two", "second"],
            ["three", "third"],
            ["four", "fourth"],
            ["five", "fifth"],
            ["six", "sixth"],
            ["seven", "seventh"],
            ["eight", "eighth"],
            ["nine", "ninth"],
        ];
        words
            .iter()
            .position(|w| w.contains(&s))
            .map(|i| i as u32 + 1)
    }

    /// A spoken monitor without the filler: "my main screen" â†’ "main",
    /// "monitor 2" â†’ "2", "the TV" â†’ "tv".
    pub fn clean(spec: &str) -> String {
        let mut s = normalize(spec);
        for lead in ["monitor ", "screen ", "display ", "number ", "no "] {
            if let Some(rest) = s.strip_prefix(lead) {
                s = rest.to_string();
            }
        }
        for tail in [" monitor", " screen", " display", " one"] {
            if let Some(rest) = s.strip_suffix(tail) {
                s = rest.to_string();
            }
        }
        if let Some(rest) = s.strip_prefix("the ") {
            s = rest.to_string();
        }
        match s.as_str() {
            "television" => "tv".into(),
            "centre" | "center" => "middle".into(),
            "primary" | "main" => "main".into(),
            "leftmost" => "left".into(),
            "rightmost" => "right".into(),
            _ => s,
        }
    }

    /// Whether `pick` needs the monitors' own names for this (it's not a
    /// word like "main", "other", "left" or a number).
    pub fn needs_names(spec: &str) -> bool {
        let s = clean(spec);
        !(s.is_empty()
            || number(&s).is_some()
            || [
                "main", "other", "left", "right", "top", "bottom", "middle", "this", "current",
                "same",
            ]
            .contains(&s.as_str()))
    }

    fn choices(mons: &[Mon]) -> String {
        let all: Vec<String> = (0..mons.len())
            .map(|i| {
                let d = describe(mons, i);
                let d = d.strip_prefix("the ").unwrap_or(&d).to_string();
                format!("{} ({d})", mons[i].num)
            })
            .collect();
        format!("This PC has {} monitors: {}.", mons.len(), all.join(", "))
    }

    /// The monitor someone means. `current` is the one the window is on.
    pub fn pick(spec: &str, mons: &[Mon], current: usize) -> Result<usize, String> {
        if mons.is_empty() {
            return Err("I can't find any monitors.".into());
        }
        let s = clean(spec);
        let extreme = |key: &dyn Fn(&Mon) -> i64, max: bool| -> Result<usize, String> {
            let v: Vec<i64> = mons.iter().map(key).collect();
            let want = if max { v.iter().max() } else { v.iter().min() }.copied();
            let hits: Vec<usize> = (0..mons.len()).filter(|&i| Some(v[i]) == want).collect();
            match hits.as_slice() {
                [i] if mons.len() > 1 => Ok(*i),
                _ if mons.len() == 1 => Err("There's only one monitor.".into()),
                _ => Err(format!("I can't tell which one is {s}. {}", choices(mons))),
            }
        };
        let cx = |m: &Mon| (m.rect.l as i64 + m.rect.r as i64) / 2;
        let cy = |m: &Mon| (m.rect.t as i64 + m.rect.b as i64) / 2;
        match s.as_str() {
            "" | "this" | "current" | "same" => return Ok(current.min(mons.len() - 1)),
            "main" => return Ok(mons.iter().position(|m| m.primary).unwrap_or(0)),
            "other" | "another" | "next" => {
                return match mons.len() {
                    1 => Err("There's only one monitor.".into()),
                    2 => Ok(1 - current.min(1)),
                    _ => Err(format!("Which one? {}", choices(mons))),
                };
            }
            "left" => return extreme(&cx, false),
            "right" => return extreme(&cx, true),
            "top" | "upper" => return extreme(&cy, false),
            "bottom" | "lower" => return extreme(&cy, true),
            "middle" => {
                return positions(mons)
                    .iter()
                    .position(|p| *p == Some("middle"))
                    .ok_or_else(|| format!("There's no middle monitor. {}", choices(mons)));
            }
            _ => {}
        }
        if let Some(n) = number(&s) {
            return mons.iter().position(|m| m.num == n).ok_or_else(|| {
                if mons.len() == 1 {
                    format!("There's no monitor {n}: this PC has only one.")
                } else {
                    format!("There's no monitor {n}. {}", choices(mons))
                }
            });
        }
        let scores: Vec<u32> = mons
            .iter()
            .map(|m| score(&s, &normalize(&m.name)))
            .collect();
        let best = scores.iter().copied().max().unwrap_or(0);
        let hits: Vec<usize> = (0..mons.len())
            .filter(|&i| best > 0 && scores[i] == best)
            .collect();
        match hits.as_slice() {
            [i] => Ok(*i),
            _ => Err(format!(
                "I don't know which monitor \"{}\" is. {}",
                spec.trim(),
                choices(mons)
            )),
        }
    }

    fn scaled(v: i32, num: u32, den: u32) -> i32 {
        if den == 0 || num == 0 {
            return v;
        }
        ((v as i64 * num as i64 + den as i64 / 2) / den as i64) as i32
    }

    /// Where a window shown at `visible` on `from` goes on `to`. A snapped or
    /// full window takes the same zone there; any other keeps its size in
    /// real terms (scaled by the monitors' DPI, at most the work area) and its
    /// spot relative to the work area.
    pub fn map_rect(visible: Rect, from: &Mon, to: &Mon) -> Rect {
        if let Some(z) = zone_of(visible, from.work) {
            return zone_rect(to.work, z);
        }
        if visible.near(&from.work, SNAP_TOL) {
            return to.work;
        }
        let w = scaled(visible.w(), to.dpi, from.dpi).clamp(1, to.work.w().max(1));
        let h = scaled(visible.h(), to.dpi, from.dpi).clamp(1, to.work.h().max(1));
        let (cx, cy) = visible.center();
        let fx = ((cx - from.work.l as i64) as f64 / from.work.w().max(1) as f64).clamp(0.0, 1.0);
        let fy = ((cy - from.work.t as i64) as f64 / from.work.h().max(1) as f64).clamp(0.0, 1.0);
        let ncx = to.work.l + (fx * to.work.w() as f64).round() as i32;
        let ncy = to.work.t + (fy * to.work.h() as f64).round() as i32;
        clamp_into(Rect::sized(ncx - w / 2, ncy - h / 2, w, h), to.work)
    }

    /// Where a window that can't be resized goes in zone `z` of `to`: against
    /// the zone's outer edges at the size it will have there, since a monitor
    /// with another DPI rescales it (the app itself, or Windows for an app that
    /// isn't DPI aware).
    pub fn fixed_anchor(normal: Rect, from: &Mon, to: &Mon, z: Zone) -> Rect {
        let w = scaled(normal.w(), to.dpi, from.dpi);
        let h = scaled(normal.h(), to.dpi, from.dpi);
        anchor(zone_rect(to.work, z), w, h, Some(z), to.work)
    }

    /// What a change may do to a window, given what's around it.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Allowed {
        /// Bring it to the top.
        pub raise: bool,
        /// Maximize it (which also brings it to the front).
        pub maximize: bool,
    }

    /// `busy`: Windows says a full-screen app is in front. `game_in_front`: a
    /// game (or a program that might be one) has the focus. `full_screen_there`:
    /// another window is full screen on the monitor it ends up on.
    pub fn allowed(busy: bool, game_in_front: bool, full_screen_there: bool) -> Allowed {
        Allowed {
            raise: !full_screen_there && !game_in_front,
            maximize: !busy && !game_in_front && !full_screen_there,
        }
    }

    /// Words that mean the window in front rather than an app's name.
    pub fn means_front(name: &str) -> bool {
        let n = normalize(name);
        let n = n.strip_suffix(" window").unwrap_or(&n);
        [
            "", "it", "this", "that", "window", "this one", "that one", "current", "active",
            "front", "focused", "this app", "that app",
        ]
        .contains(&n)
    }

    /// Of several windows of one program (front first), the one a name
    /// means: the one whose title it fits best ("the reddit chrome window"),
    /// else the most recently used.
    pub fn best_titled(query: &str, titles: &[&str]) -> usize {
        let q = normalize(query);
        let q = q
            .strip_suffix(" window")
            .or_else(|| q.strip_suffix(" tab"))
            .unwrap_or(&q);
        let scores: Vec<u32> = titles.iter().map(|t| score(q, &normalize(t))).collect();
        let best = scores.iter().copied().max().unwrap_or(0);
        if best == 0 || scores.iter().filter(|&&s| s == best).count() > 1 {
            return 0;
        }
        scores.iter().position(|&s| s == best).unwrap_or(0)
    }
}

use geo::{
    allowed, anchor, centered, describe, fit_note, fixed_anchor, insets, is_fullscreen, map_rect,
    means_front, monitor_of, needs_names, outer_for, pick, summary, zone_rect,
};

/// How a window was before a change, so "put it back" can undo it.
#[derive(Clone, Copy, Debug)]
struct Saved {
    hwnd: isize,
    pid: u32,
    /// The visible frame in its normal (not maximized or minimized) state.
    normal: Rect,
    zoomed: bool,
    iconic: bool,
}

/// The last changes, newest last. Only windows moved here, in memory only.
static HISTORY: Mutex<Vec<Saved>> = Mutex::new(Vec::new());
const HISTORY_MAX: usize = 32;

fn remember(s: Saved) {
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    push_bounded(&mut h, s, HISTORY_MAX);
}

fn push_bounded<T>(v: &mut Vec<T>, item: T, max: usize) {
    v.push(item);
    if v.len() > max {
        let extra = v.len() - max;
        v.drain(..extra);
    }
}

/// Runs this thread in per-monitor v2 DPI mode until dropped.
struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    fn per_monitor() -> Self {
        DpiScope(unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        })
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        // The old context; null when switching failed (then nothing changed).
        if !self.0.0.is_null() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// Why a window couldn't be changed.
#[derive(Debug, PartialEq)]
pub(super) enum Fail {
    Admin,
    Hung,
    Other,
}

fn says(f: Fail, app: &str) -> Outcome {
    Outcome::no(match f {
        Fail::Admin => format!("{app} runs as administrator, so I can't move it."),
        Fail::Hung => format!("{app} isn't responding right now, so I left it alone."),
        Fail::Other => format!("Windows wouldn't let me move {app}."),
    })
}

fn rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

fn outer(hwnd: HWND) -> Rect {
    let mut r = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut r) };
    rect(r)
}

/// What you see of the window: its DWM frame, else its rect.
fn visible(hwnd: HWND) -> Rect {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok();
    let f = rect(r);
    if ok && !f.is_empty() { f } else { outer(hwnd) }
}

fn style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 }
}

fn zoomed(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

fn iconic(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

unsafe extern "system" fn collect_monitor(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lp.0 as *mut Vec<HMONITOR>) };
    out.push(h);
    TRUE
}

/// The monitors, by Windows' numbers. Names only when `names` (it asks the
/// display drivers, a few milliseconds).
fn monitors(names: bool) -> Vec<Mon> {
    let mut hs: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(collect_monitor),
            LPARAM(&mut hs as *mut Vec<HMONITOR> as isize),
        );
    }
    let friendly = if names { friendly_names() } else { Vec::new() };
    let mut out: Vec<Mon> = Vec::new();
    for (i, h) in hs.into_iter().enumerate() {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }
            .as_bool()
        {
            continue;
        }
        let device = from_wide(&mi.szDevice);
        let (mut dx, mut dy) = (96u32, 96u32);
        if unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }.is_err() {
            dx = 96;
        }
        let num = device
            .trim_start_matches(r"\\.\DISPLAY")
            .parse()
            .unwrap_or(i as u32 + 1);
        out.push(Mon {
            num,
            primary: mi.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            rect: rect(mi.monitorInfo.rcMonitor),
            work: rect(mi.monitorInfo.rcWork),
            dpi: dx,
            name: friendly
                .iter()
                .find(|(d, _)| *d == device)
                .map(|(_, n)| n.clone())
                .unwrap_or_default(),
            id: h.0 as isize,
        });
    }
    // Numbers have to be unique to be any use; otherwise count them.
    let mut nums: Vec<u32> = out.iter().map(|m| m.num).collect();
    nums.sort();
    nums.dedup();
    if nums.len() != out.len() {
        for (i, m) in out.iter_mut().enumerate() {
            m.num = i as u32 + 1;
        }
    }
    out.sort_by_key(|m| m.num);
    out
}

/// GDI device names (`\\.\DISPLAY1`) and the monitors' own names.
fn friendly_names() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        for p in paths.iter().take(np as usize) {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) == 0
                && DisplayConfigGetDeviceInfo(&mut tgt.header) == 0
            {
                let name = from_wide(&tgt.monitorFriendlyDeviceName);
                if !name.trim().is_empty() {
                    out.push((from_wide(&src.viewGdiDeviceName), name.trim().to_string()));
                }
            }
        }
    }
    out
}

/// Which monitor a window is on, by Windows' own reckoning (for a minimized
/// window, where it will come back).
fn monitor_index(hwnd: HWND, mons: &[Mon]) -> usize {
    let h = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    mons.iter()
        .position(|m| m.id == h.0 as isize)
        .unwrap_or_else(|| monitor_of(visible(hwnd), mons))
}

fn full_screen_state() -> (bool, bool) {
    let q = unsafe { SHQueryUserNotificationState() }.unwrap_or_default();
    (
        q == QUNS_RUNNING_D3D_FULL_SCREEN,
        q == QUNS_BUSY || q == QUNS_PRESENTATION_MODE || q == QUNS_RUNNING_D3D_FULL_SCREEN,
    )
}

fn fullscreen(w: &Win, mons: &[Mon]) -> bool {
    let st = style(w.hwnd);
    let mon = &mons[monitor_index(w.hwnd, mons)];
    is_fullscreen(
        outer(w.hwnd),
        mon.rect,
        st & WS_CAPTION.0 == WS_CAPTION.0,
        zoomed(w.hwnd),
    )
}

/// Whether a process runs at a higher integrity level than this one (an
/// elevated app, when this one isn't): Windows won't let us move its windows.
fn runs_above_us(pid: u32) -> bool {
    fn level(process: HANDLE) -> Option<u32> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            // u64s: aligned for the pointer inside.
            let mut buf = [0u64; 16];
            let mut len = 0u32;
            let r = GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buf.as_mut_ptr().cast()),
                std::mem::size_of_val(&buf) as u32,
                &mut len,
            );
            let _ = CloseHandle(token);
            r.ok()?;
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let sid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return None;
            }
            Some(*GetSidSubAuthority(sid, count as u32 - 1))
        }
    }
    let Some(ours) = level(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    let theirs = unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let l = level(h);
                let _ = CloseHandle(h);
                l
            }
            Err(_) => None,
        }
    };
    theirs.is_some_and(|t| t > ours)
}

/// A window that's alive and taking messages, or why not. Waits at most
/// 300 ms. Also used before a picture of a window (PrintWindow would wait for
/// a hung one for as long as it hangs).
pub(super) fn responsive(hwnd: HWND, pid: u32) -> Result<(), Fail> {
    if runs_above_us(pid) {
        return Err(Fail::Admin);
    }
    unsafe {
        if IsHungAppWindow(hwnd).as_bool() {
            return Err(Fail::Hung);
        }
        // WM_NULL does nothing: it only shows the window's thread answers.
        let mut out = 0usize;
        let r = SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            300,
            Some(&mut out as *mut usize),
        );
        if r.0 == 0 {
            return Err(if GetLastError() == ERROR_ACCESS_DENIED {
                Fail::Admin
            } else {
                Fail::Hung
            });
        }
    }
    Ok(())
}

/// Out of maximized or minimized, without activating it.
fn normalize_state(hwnd: HWND) {
    for _ in 0..2 {
        if !zoomed(hwnd) && !iconic(hwnd) {
            return;
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// Put the visible frame at `target`: its size too when `resize`, else only
/// its top left corner. Read back after each try: a move to a monitor with a
/// different DPI makes the app rescale itself, and its borders change size,
/// so the second try lands it. Returns where it really is.
fn place(hwnd: HWND, target: Rect, raise: bool, resize: bool) -> Result<Rect, Fail> {
    let mut last = visible(hwnd);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    for pass in 0..3 {
        let ins = insets(outer(hwnd), visible(hwnd));
        let o = outer(hwnd);
        let want = if resize {
            outer_for(target, ins)
        } else {
            Rect::sized(target.l - ins.l, target.t - ins.t, o.w(), o.h())
        };
        let raise_now = raise && pass == 0;
        let mut flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if !raise_now {
            flags |= SWP_NOZORDER;
        }
        if !resize {
            flags |= SWP_NOSIZE;
        }
        let r = unsafe {
            SetWindowPos(
                hwnd,
                if raise_now { HWND_TOP } else { HWND::default() },
                want.l,
                want.t,
                want.w(),
                want.h(),
                flags,
            )
        };
        if let Err(e) = r {
            return Err(if e.code() == E_ACCESSDENIED {
                Fail::Admin
            } else {
                Fail::Other
            });
        }
        if pass == 0 && unsafe { GetDpiForWindow(hwnd) } != dpi {
            // Onto a monitor with another DPI: some apps rescale themselves a
            // moment after the move. Once, before reading where it ended up.
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let now = visible(hwnd);
        let there = if resize {
            now.near(&target, 1)
        } else {
            (now.l - target.l).abs() <= 1 && (now.t - target.t).abs() <= 1
        };
        if there || (pass > 0 && now == last) {
            return Ok(now);
        }
        last = now;
    }
    Ok(last)
}

fn maximize_now(hwnd: HWND) -> bool {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
    }
    zoomed(hwnd)
}

/// The window someone means, and what to call it.
struct Target {
    win: Win,
    app: String,
}

fn protected_says(index: &Index, w: &Win, app: &str) -> Outcome {
    let what = if index.is_game(&w.exe) {
        "is a game"
    } else {
        "might be a game: I can't tell which program it is"
    };
    Outcome::no(format!(
        "{app} {what}. I don't move games' windows, so anti-cheat never sees me."
    ))
}

fn on_current_desktop(hwnd: HWND) -> bool {
    let _com = super::Com::init();
    unsafe {
        let m: windows::core::Result<IVirtualDesktopManager> =
            CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL);
        match m {
            Ok(m) => m
                .IsWindowOnCurrentVirtualDesktop(hwnd)
                .map(|b| b.as_bool())
                .unwrap_or(true),
            Err(_) => true,
        }
    }
}

fn target(name: &str, all: &[Win]) -> Result<Target, String> {
    let shown: Vec<Win> = all.iter().filter(|w| w.cloaked == 0).cloned().collect();
    if means_front(name) {
        let fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
        let topmost = |w: &Win| unsafe {
            (GetWindowLongPtrW(w.hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST.0 != 0
        };
        // The window in front; when that's this app, the desktop or the
        // taskbar, the one used last.
        let w = shown
            .iter()
            .find(|w| w.hwnd == fg)
            .or_else(|| shown.iter().find(|w| !iconic(w.hwnd) && !topmost(w)))
            .cloned()
            .ok_or("I can't tell which window you mean. Say the app's name.")?;
        let app = wnd::label(&w);
        return Ok(Target { win: w, app });
    }
    let wins = match wnd::find_in(name, shown) {
        Found::Some(w) => w,
        Found::Many(ask) => return Err(ask),
        Found::None => {
            let hidden: Vec<Win> = all.iter().filter(|w| w.cloaked != 0).cloned().collect();
            if let Found::Some(w) = wnd::find_in(name, hidden)
                && w.iter().any(|w| !on_current_desktop(w.hwnd))
            {
                return Err(format!(
                    "{} is on another virtual desktop, so I left it there.",
                    wnd::label(&w[0])
                ));
            }
            return Err(format!("Nothing called \"{name}\" is open."));
        }
    };
    let titles: Vec<&str> = wins.iter().map(|w| w.title.as_str()).collect();
    let w = wins[geo::best_titled(name, &titles)].clone();
    let app = wnd::label(&w);
    Ok(Target { win: w, app })
}

fn is_protected_window(index: &Index, hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let pid = pid_of(hwnd);
    if pid == std::process::id() {
        return false;
    }
    match wnd::exe_path(pid) {
        Some(exe) => {
            // The desktop and taskbar are Explorer: not a game.
            exe_stem(&exe) != "explorer" && index.is_protected(&exe)
        }
        None => true,
    }
}

/// A full-screen window on this monitor other than `except`: its app and
/// whether it's (or might be) a game.
fn fullscreen_on(
    index: &Index,
    all: &[Win],
    mons: &[Mon],
    m: usize,
    except: HWND,
) -> Option<(String, bool)> {
    all.iter()
        .filter(|w| w.cloaked == 0 && w.hwnd != except && !iconic(w.hwnd))
        .find(|w| monitor_index(w.hwnd, mons) == m && fullscreen(w, mons))
        .map(|w| (wnd::label(w), index.is_protected(&w.exe)))
}

pub fn run(index: &Index, action: &str, args: &Value) -> Outcome {
    let _dpi = DpiScope::per_monitor();
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    // "monitor": "2" or 2
    let monitor = match args.get("monitor") {
        Some(Value::Number(n)) => n.to_string(),
        _ => s("monitor"),
    };
    // A side given as the action ("left") is a snap.
    let (action, side) = match Zone::parse(action) {
        Some(_) => ("snap", action.to_string()),
        None => (action, s("side")),
    };
    match action {
        "monitors" => list_monitors(index),
        "snap" | "maximize" | "restore" | "center" | "move" | "back" => {
            change(index, action, &s("name"), &side, &monitor)
        }
        _ => Outcome::no(
            "pc_windows action must be snap, maximize, restore, center, move, back or monitors.",
        ),
    }
}

fn list_monitors(index: &Index) -> Outcome {
    let mons = monitors(true);
    if mons.is_empty() {
        return Outcome::no("I can't find any monitors.");
    }
    let mut apps: Vec<Vec<String>> = vec![Vec::new(); mons.len()];
    for w in wnd::list() {
        if iconic(w.hwnd) {
            continue;
        }
        let m = monitor_index(w.hwnd, &mons);
        let game = if index.is_game(&w.exe) { " (game)" } else { "" };
        let a = format!("{}{game}", wnd::label(&w));
        if !apps[m].contains(&a) && apps[m].len() < 8 {
            apps[m].push(a);
        }
    }
    let parts: Vec<String> = (0..mons.len())
        .map(|i| {
            let open = if apps[i].is_empty() {
                "no windows".to_string()
            } else {
                apps[i].join(", ")
            };
            format!("{}; {open}", summary(&mons, i))
        })
        .collect();
    let n = mons.len();
    Outcome::ok(format!(
        "{n} monitor{}. {}.",
        if n == 1 { "" } else { "s" },
        parts.join(". ")
    ))
}

fn change(index: &Index, action: &str, name: &str, side: &str, spec: &str) -> Outcome {
    let (exclusive, busy) = full_screen_state();
    if exclusive {
        return Outcome::no(
            "A game is running in exclusive full screen, so I'm leaving the windows alone until it closes.",
        );
    }
    let all = wnd::list_all();
    if action == "back" {
        return back(index, name, &all);
    }
    let zone = if action == "snap" {
        match Zone::parse(side) {
            Some(z) => Some(z),
            None => {
                return Outcome::no(
                    "Say where: left, right, top, bottom, or a corner like top_left.",
                );
            }
        }
    } else {
        None
    };
    let t = match target(name, &all) {
        Ok(t) => t,
        Err(e) => return Outcome::no(e),
    };
    let (hwnd, app) = (t.win.hwnd, t.app.clone());
    if index.is_protected(&t.win.exe) {
        return protected_says(index, &t.win, &app);
    }
    let mons = monitors(needs_names(spec));
    if mons.is_empty() {
        return Outcome::no("I can't find any monitors.");
    }
    if fullscreen(&t.win, &mons) {
        return Outcome::no(format!(
            "{app} is full screen. Once it's out of full screen I can move it."
        ));
    }
    let pid = pid_of(hwnd);
    if let Err(f) = responsive(hwnd, pid) {
        return says(f, &app);
    }
    let cur = monitor_index(hwnd, &mons);
    let dest = if spec.is_empty() && action == "move" {
        match mons.len() {
            1 => return Outcome::no("There's only one monitor."),
            2 => 1 - cur.min(1),
            _ => match pick("other", &mons, cur) {
                Ok(i) => i,
                Err(e) => return Outcome::no(e),
            },
        }
    } else {
        match pick(spec, &mons, cur) {
            Ok(i) => i,
            Err(e) => return Outcome::no(e),
        }
    };
    let on = if dest != cur {
        format!(" on {}", describe(&mons, dest))
    } else {
        String::new()
    };
    // Never onto a monitor a game fills; never over anything full screen
    // there, nor over a game in front.
    let blocker = fullscreen_on(index, &all, &mons, dest, hwnd);
    if let Some((other, true)) = &blocker {
        return Outcome::no(format!(
            "{other} is full screen on {}, so I left {app} where it is.",
            describe(&mons, dest)
        ));
    }
    let fg = unsafe { GetForegroundWindow() };
    let game_in_front = fg != hwnd && is_protected_window(index, fg);
    let may = allowed(busy, game_in_front, blocker.is_some());
    let (raise, maximize_ok) = (may.raise, may.maximize);

    let was = Saved {
        hwnd: hwnd.0 as isize,
        pid,
        normal: Rect::default(),
        zoomed: zoomed(hwnd),
        iconic: iconic(hwnd),
    };
    let before = visible(hwnd);
    let resizable = style(hwnd) & WS_THICKFRAME.0 != 0;
    let work = mons[dest].work;

    // Nothing to do: say so before touching it.
    match action {
        "snap" => {
            let z = zone.expect("snap has a zone");
            if !was.zoomed && !was.iconic && before.near(&zone_rect(work, z), geo::SNAP_TOL) {
                return Outcome::ok(format!("{app} is already on {}{on}.", z.words()));
            }
        }
        "maximize" => {
            if was.zoomed && dest == cur {
                return Outcome::ok(format!("{app} is already maximized."));
            }
            if style(hwnd) & WS_MAXIMIZEBOX.0 == 0 {
                return Outcome::no(format!("{app} can't be maximized."));
            }
            if let Some((other, _)) = &blocker {
                return Outcome::no(format!(
                    "{other} is full screen on {}, so I won't maximize {app} over it. I can snap it instead.",
                    describe(&mons, dest)
                ));
            }
            if !maximize_ok {
                return Outcome::no(format!(
                    "Something is full screen in front, so I won't maximize {app} right now. I can snap it instead."
                ));
            }
        }
        "restore" => {
            if !was.zoomed && !was.iconic {
                return Outcome::ok(format!("{app} isn't maximized or minimized."));
            }
        }
        "center" => {
            if !was.zoomed
                && !was.iconic
                && dest == cur
                && before.near(&centered(before, work), geo::SNAP_TOL)
            {
                return Outcome::ok(format!("{app} is already centered."));
            }
        }
        "move" => {
            if dest == cur && !was.iconic {
                return Outcome::ok(format!("{app} is already on {}.", describe(&mons, dest)));
            }
        }
        _ => {}
    }

    // From here on the window changes: remember how it was.
    normalize_state(hwnd);
    let normal = visible(hwnd);
    let changed =
        |now: Rect| now != before || zoomed(hwnd) != was.zoomed || iconic(hwnd) != was.iconic;
    let finish = |r: Result<Rect, Fail>, ok: &dyn Fn(Rect) -> Outcome| -> Outcome {
        let now = visible(hwnd);
        if changed(now) {
            remember(Saved { normal, ..was });
        }
        match r {
            Ok(got) => ok(got),
            Err(f) => says(f, &app),
        }
    };
    let src = mons[cur].clone();
    let to = mons[dest].clone();

    match action {
        "snap" => {
            let z = zone.expect("snap has a zone");
            let want = zone_rect(work, z);
            let r = if resizable {
                place(hwnd, want, raise, true).and_then(|got| {
                    // Kept its own size: against the zone's outer edges.
                    let a = anchor(want, got.w(), got.h(), Some(z), work);
                    if a.near(&got, 1) {
                        Ok(got)
                    } else {
                        place(hwnd, a, false, false)
                    }
                })
            } else {
                // At the size it will have there, then once more at the size it
                // really got, in case the app rescaled itself differently.
                let a = fixed_anchor(normal, &src, &to, z);
                place(hwnd, a, raise, false).and_then(|got| {
                    let a = anchor(want, got.w(), got.h(), Some(z), work);
                    if a.near(&got, 1) {
                        Ok(got)
                    } else {
                        place(hwnd, a, false, false)
                    }
                })
            };
            finish(r, &|got| {
                let place_words = format!("{}{on}", z.words());
                if !resizable {
                    return Outcome::ok(format!(
                        "{app} can't be resized, so I moved it to {place_words} as it is."
                    ));
                }
                if got.near(&want, 2) {
                    return Outcome::ok(format!("{app} is on {place_words}."));
                }
                if got.near(&before, 1) {
                    return Outcome::no(format!("{app} didn't move."));
                }
                Outcome::ok(
                    fit_note(&app, want, got, &place_words)
                        .unwrap_or_else(|| format!("{app} is on {place_words}.")),
                )
            })
        }
        "maximize" => {
            let r = if dest != cur {
                place(hwnd, map_rect(normal, &src, &to), false, resizable)
            } else {
                Ok(normal)
            };
            let r = r.and_then(|got| {
                if maximize_now(hwnd) {
                    Ok(got)
                } else {
                    Err(Fail::Other)
                }
            });
            finish(r, &|_| {
                if zoomed(hwnd) && monitor_index(hwnd, &mons) == dest {
                    Outcome::ok(format!("{app} is maximized{on}."))
                } else {
                    Outcome::no(format!("{app} didn't maximize there."))
                }
            })
        }
        "restore" => finish(Ok(normal), &|_| {
            if zoomed(hwnd) || iconic(hwnd) {
                Outcome::no(format!("{app} didn't restore."))
            } else {
                Outcome::ok(format!("{app} is back to its normal size."))
            }
        }),
        "center" => {
            let from = if dest != cur {
                map_rect(normal, &src, &to)
            } else {
                normal
            };
            let want = centered(from, work);
            let resize = resizable && (want.w() != normal.w() || want.h() != normal.h());
            let r = place(hwnd, want, raise, resize);
            finish(r, &|got| {
                if got.near(&want, 2) || (!resize && (got.l - want.l).abs() <= 1) {
                    Outcome::ok(format!("{app} is centered{on}."))
                } else if got.near(&before, 1) {
                    Outcome::no(format!("{app} didn't move."))
                } else {
                    Outcome::ok(format!("{app} is close to the middle{on}."))
                }
            })
        }
        _ => {
            // move: same zone, same spot, maximized stays maximized
            let want = map_rect(normal, &src, &to);
            let rezoom = was.zoomed && maximize_ok;
            let r = place(hwnd, want, raise && !rezoom, resizable).and_then(|got| {
                if rezoom && !maximize_now(hwnd) {
                    return Err(Fail::Other);
                }
                Ok(got)
            });
            finish(r, &|_| {
                let now = monitor_index(hwnd, &mons);
                if now != dest {
                    return Outcome::no(format!("{app} didn't move."));
                }
                let d = describe(&mons, dest);
                if was.zoomed && !rezoom {
                    let what = match &blocker {
                        Some((other, _)) => format!("{other} is full screen there"),
                        None => "Something is full screen in front".to_string(),
                    };
                    Outcome::ok(format!(
                        "{app} is on {d}. {what}, so I didn't maximize it again."
                    ))
                } else {
                    Outcome::ok(format!("{app} is on {d}."))
                }
            })
        }
    }
}

/// "Put it back": undo the last change to that window, or to any window.
fn back(index: &Index, name: &str, all: &[Win]) -> Outcome {
    let want: Option<isize> = if means_front(name) {
        None
    } else {
        match target(name, all) {
            Ok(t) => Some(t.win.hwnd.0 as isize),
            Err(e) => return Outcome::no(e),
        }
    };
    let saved = {
        let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
        // Forget windows that closed (or whose handle now means another).
        h.retain(|s| {
            let hw = HWND(s.hwnd as *mut _);
            (unsafe { IsWindow(hw).as_bool() }) && pid_of(hw) == s.pid
        });
        let at = h.iter().rposition(|s| want.is_none_or(|w| w == s.hwnd));
        at.map(|i| h.remove(i))
    };
    let Some(s) = saved else {
        return Outcome::no(if want.is_some() {
            "I haven't moved that window, so there's nothing to put back."
        } else {
            "I haven't moved any windows, so there's nothing to put back."
        });
    };
    let hwnd = HWND(s.hwnd as *mut _);
    let w = all.iter().find(|w| w.hwnd == hwnd).cloned().unwrap_or(Win {
        hwnd,
        title: String::new(),
        exe: wnd::exe_path(s.pid).unwrap_or_default(),
        cloaked: 0,
        frame_host: false,
    });
    let app = wnd::label(&w);
    if index.is_protected(&w.exe) {
        return protected_says(index, &w, &app);
    }
    // Things may have changed since: the same rules as any other change.
    // `cloaked` is only known for windows in the list, so ask the shell too.
    let shown = unsafe { IsWindowVisible(hwnd).as_bool() };
    if !shown || w.cloaked != 0 || !on_current_desktop(hwnd) {
        return Outcome::no(format!(
            "{app} is hidden or on another virtual desktop now, so I left it there."
        ));
    }
    let mons = monitors(false);
    if mons.is_empty() {
        return Outcome::no("I can't find any monitors.");
    }
    if fullscreen(&w, &mons) {
        return Outcome::no(format!("{app} is full screen now, so I left it as it is."));
    }
    let dest = monitor_of(s.normal, &mons);
    let blocker = fullscreen_on(index, all, &mons, dest, hwnd);
    if let Some((other, true)) = &blocker {
        return Outcome::no(format!(
            "{other} is full screen on {}, so I left {app} where it is.",
            describe(&mons, dest)
        ));
    }
    if let Err(f) = responsive(hwnd, s.pid) {
        return says(f, &app);
    }
    let (_, busy) = full_screen_state();
    let fg = unsafe { GetForegroundWindow() };
    let game_in_front = fg != hwnd && is_protected_window(index, fg);
    let may = allowed(busy, game_in_front, blocker.is_some());
    normalize_state(hwnd);
    let resizable = style(hwnd) & WS_THICKFRAME.0 != 0;
    if let Err(f) = place(hwnd, s.normal, false, resizable) {
        return says(f, &app);
    }
    if s.zoomed && may.maximize {
        maximize_now(hwnd);
    } else if s.zoomed {
        return Outcome::ok(format!(
            "{app} is back where it was. Something is full screen, so I didn't maximize it again."
        ));
    } else if s.iconic {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        }
    }
    let now = visible(hwnd);
    if !s.zoomed && !s.iconic && !now.near(&s.normal, 2) {
        return Outcome::ok(format!("{app} is about where it was."));
    }
    Outcome::ok(format!("{app} is back where it was."))
}

#[cfg(test)]
mod tests {
    use super::geo::*;

    fn mon(num: u32, primary: bool, rect: Rect, work: Rect, dpi: u32) -> Mon {
        Mon {
            num,
            primary,
            rect,
            work,
            dpi,
            name: String::new(),
            id: 0,
        }
    }

    /// A 4K TV at 150% on the left (taskbar at the bottom, 72 px) and a
    /// 1440p monitor at 125%, the main one, on the right (taskbar on the
    /// left, 60 px), numbered the other way round.
    fn two() -> Vec<Mon> {
        vec![
            mon(
                1,
                false,
                Rect::new(-3840, -340, 0, 1820),
                Rect::new(-3840, -340, 0, 1748),
                144,
            ),
            mon(
                2,
                true,
                Rect::new(0, 0, 2560, 1440),
                Rect::new(60, 0, 2560, 1440),
                120,
            ),
        ]
    }

    /// Three 1080p monitors in a row at 100%; the middle one is main.
    fn three() -> Vec<Mon> {
        let m = |num, l, primary| {
            mon(
                num,
                primary,
                Rect::sized(l, 0, 1920, 1080),
                Rect::sized(l, 0, 1920, 1040),
                96,
            )
        };
        vec![m(1, 0, true), m(2, -1920, false), m(3, 1920, false)]
    }

    #[test]
    fn a_fixed_size_window_lands_in_the_corner_at_its_new_size() {
        let m = two();
        // A 400x300 dialog on the 125% main monitor, to the TV's bottom right
        // corner at 150%: it will be 480x360 there.
        let dialog = Rect::sized(500, 400, 400, 300);
        let a = fixed_anchor(dialog, &m[1], &m[0], Zone::BottomRight);
        assert_eq!((a.w(), a.h()), (480, 360));
        assert_eq!((a.r, a.b), (m[0].work.r, m[0].work.b));
        assert!(m[0].work.covers(&a));
        // And back: 150% to 125% shrinks it, still against the corner.
        let big = Rect::sized(-2000, 200, 480, 360);
        let b = fixed_anchor(big, &m[0], &m[1], Zone::TopRight);
        assert_eq!((b.w(), b.h()), (400, 300));
        assert_eq!((b.r, b.t), (m[1].work.r, m[1].work.t));
        // Same monitor: same size.
        let c = fixed_anchor(dialog, &m[1], &m[1], Zone::Left);
        assert_eq!((c.l, c.w(), c.h()), (m[1].work.l, 400, 300));
    }

    #[test]
    fn nothing_is_maximized_over_a_full_screen_window() {
        // A show full screen on the TV while working on the main monitor:
        // Windows isn't "busy", but nothing goes over it.
        let a = allowed(false, false, true);
        assert!(!a.maximize);
        assert!(!a.raise);
        assert_eq!(
            allowed(false, false, false),
            Allowed {
                raise: true,
                maximize: true
            }
        );
        assert!(!allowed(true, false, false).maximize);
        assert!(allowed(true, false, false).raise);
        let game = allowed(false, true, false);
        assert!(!game.maximize && !game.raise);
    }

    #[test]
    fn halves_meet_without_gaps_on_odd_sizes() {
        let work = Rect::new(-3841, 7, 0, 1748); // 3841 wide, 1741 tall
        let l = zone_rect(work, Zone::Left);
        let r = zone_rect(work, Zone::Right);
        assert_eq!((l.l, l.r, r.l, r.r), (-3841, -1921, -1921, 0));
        assert_eq!(l.w() + r.w(), work.w());
        let t = zone_rect(work, Zone::Top);
        let b = zone_rect(work, Zone::Bottom);
        assert_eq!(t.b, b.t);
        assert_eq!(t.h() + b.h(), work.h());
        assert_eq!((t.l, t.r), (work.l, work.r));
    }

    #[test]
    fn quarters_tile_the_work_area() {
        let work = Rect::new(60, 0, 2560, 1440);
        let q: Vec<Rect> = [
            Zone::TopLeft,
            Zone::TopRight,
            Zone::BottomLeft,
            Zone::BottomRight,
        ]
        .iter()
        .map(|&z| zone_rect(work, z))
        .collect();
        assert_eq!(q[0], Rect::new(60, 0, 1310, 720));
        assert_eq!(q[1], Rect::new(1310, 0, 2560, 720));
        assert_eq!(q[2], Rect::new(60, 720, 1310, 1440));
        assert_eq!(q[3], Rect::new(1310, 720, 2560, 1440));
        let area: i64 = q.iter().map(|r| r.w() as i64 * r.h() as i64).sum();
        assert_eq!(area, work.w() as i64 * work.h() as i64);
        for i in 0..4 {
            for j in i + 1..4 {
                assert_eq!(q[i].overlap(&q[j]), 0);
            }
        }
    }

    #[test]
    fn zones_parse_the_ways_people_say_them() {
        assert_eq!(Zone::parse("left"), Some(Zone::Left));
        assert_eq!(Zone::parse("Right half"), Some(Zone::Right));
        assert_eq!(Zone::parse("top_left"), Some(Zone::TopLeft));
        assert_eq!(Zone::parse("upper-right corner"), Some(Zone::TopRight));
        assert_eq!(Zone::parse("bottom left quarter"), Some(Zone::BottomLeft));
        assert_eq!(Zone::parse("right bottom"), Some(Zone::BottomRight));
        assert_eq!(Zone::parse("the top"), Some(Zone::Top));
        assert_eq!(Zone::parse("lower"), Some(Zone::Bottom));
        assert_eq!(Zone::parse("middle"), None);
        assert_eq!(Zone::parse(""), None);
        assert_eq!(Zone::parse("left right"), None);
        assert_eq!(Zone::TopRight.words(), "the top right quarter");
    }

    #[test]
    fn frame_borders_are_compensated_exactly() {
        // A normal window at 100%: 7 px invisible borders left, right, bottom.
        let outer = Rect::new(93, 50, 1107, 857);
        let frame = Rect::new(100, 50, 1100, 850);
        let i = insets(outer, frame);
        assert_eq!(
            i,
            Insets {
                l: 7,
                t: 0,
                r: 7,
                b: 7
            }
        );
        let want = Rect::new(0, 0, 1280, 1392);
        let o = outer_for(want, i);
        assert_eq!(o, Rect::new(-7, 0, 1287, 1399));
        // Shrinking the rect by the borders gives exactly the target back.
        assert_eq!(Rect::new(o.l + i.l, o.t + i.t, o.r - i.r, o.b - i.b), want);
        // At 150% the borders are bigger; the same math holds.
        let i150 = insets(Rect::new(-11, 0, 1291, 1403), Rect::new(0, 0, 1280, 1392));
        assert_eq!(
            i150,
            Insets {
                l: 11,
                t: 0,
                r: 11,
                b: 11
            }
        );
    }

    #[test]
    fn borderless_and_nonsense_frames_mean_no_borders() {
        let r = Rect::new(0, 0, 800, 600);
        assert_eq!(insets(r, r), Insets::default());
        assert_eq!(insets(r, Rect::default()), Insets::default());
        // A frame outside the rect (stale DWM data) or huge "borders".
        assert_eq!(insets(r, Rect::new(-5, 0, 800, 600)), Insets::default());
        assert_eq!(insets(r, Rect::new(100, 0, 800, 600)), Insets::default());
    }

    #[test]
    fn picks_monitors_by_role_position_and_number() {
        let m = two();
        // The window is on the main monitor (index 1).
        assert_eq!(pick("main", &m, 0), Ok(1));
        assert_eq!(pick("my primary screen", &m, 0), Ok(1));
        assert_eq!(pick("other", &m, 1), Ok(0));
        assert_eq!(pick("the other monitor", &m, 0), Ok(1));
        // By where they are, not their numbers.
        assert_eq!(pick("left", &m, 1), Ok(0));
        assert_eq!(pick("right monitor", &m, 0), Ok(1));
        assert_eq!(pick("2", &m, 0), Ok(1));
        assert_eq!(pick("monitor one", &m, 1), Ok(0));
        assert_eq!(pick("second", &m, 0), Ok(1));
        assert_eq!(pick("", &m, 1), Ok(1));
        assert_eq!(pick("this one", &m, 0), Ok(0));
        let e = pick("3", &m, 0).unwrap_err();
        assert!(e.starts_with("There's no monitor 3."), "{e}");
        assert!(
            e.contains("1 (left monitor)") && e.contains("2 (main monitor)"),
            "{e}"
        );
    }

    #[test]
    fn other_needs_exactly_two() {
        let one = vec![two().remove(1)];
        assert_eq!(
            pick("other", &one, 0),
            Err("There's only one monitor.".into())
        );
        assert_eq!(
            pick("left", &one, 0),
            Err("There's only one monitor.".into())
        );
        assert_eq!(pick("main", &one, 0), Ok(0));
        let e = pick("other", &three(), 0).unwrap_err();
        assert!(e.starts_with("Which one?"), "{e}");
        assert!(e.contains("3 (right monitor)"), "{e}");
    }

    #[test]
    fn three_in_a_row_have_a_middle() {
        let m = three();
        assert_eq!(pick("left", &m, 0), Ok(1));
        assert_eq!(pick("middle", &m, 0), Ok(0));
        assert_eq!(pick("center screen", &m, 1), Ok(0));
        assert_eq!(pick("right", &m, 0), Ok(2));
        assert_eq!(
            positions(&m),
            vec![Some("middle"), Some("left"), Some("right")]
        );
        assert_eq!(describe(&m, 0), "the main monitor");
        assert_eq!(describe(&m, 2), "the right monitor");
        assert!(pick("bottom", &m, 0).is_err()); // all at the same height
    }

    #[test]
    fn stacked_monitors_are_top_and_bottom() {
        let m = vec![
            mon(
                1,
                true,
                Rect::sized(0, 0, 2560, 1440),
                Rect::sized(0, 0, 2560, 1392),
                96,
            ),
            mon(
                2,
                false,
                Rect::sized(320, -1080, 1920, 1080),
                Rect::sized(320, -1080, 1920, 1080),
                96,
            ),
        ];
        assert_eq!(positions(&m), vec![Some("bottom"), Some("top")]);
        assert_eq!(pick("top", &m, 0), Ok(1));
        assert_eq!(pick("upper", &m, 0), Ok(1));
        assert_eq!(describe(&m, 1), "the top monitor");
    }

    #[test]
    fn monitors_can_be_named() {
        let mut m = two();
        m[0].name = "Sony TV".into();
        m[1].name = "Dell U2720Q".into();
        assert_eq!(pick("the tv", &m, 1), Ok(0));
        assert_eq!(pick("television", &m, 1), Ok(0));
        assert_eq!(pick("dell", &m, 0), Ok(1));
        assert!(
            pick("projector", &m, 0)
                .unwrap_err()
                .starts_with("I don't know which monitor \"projector\" is.")
        );
        assert!(needs_names("the tv"));
        assert!(!needs_names("main"));
        assert!(!needs_names("monitor 2"));
        assert!(!needs_names(""));
        assert_eq!(summary(&m, 0), "1 (left, Sony TV): 3840x2160 at 150%");
        assert_eq!(
            summary(&m, 1),
            "2 (main, right, Dell U2720Q): 2560x1440 at 125%"
        );
    }

    #[test]
    fn snapped_windows_keep_their_zone_on_another_monitor() {
        let m = two();
        let left_main = zone_rect(m[1].work, Zone::Left);
        assert_eq!(
            map_rect(left_main, &m[1], &m[0]),
            zone_rect(m[0].work, Zone::Left)
        );
        let q = zone_rect(m[0].work, Zone::BottomRight);
        assert_eq!(
            map_rect(q, &m[0], &m[1]),
            zone_rect(m[1].work, Zone::BottomRight)
        );
        // A window filling the work area fills the other one.
        assert_eq!(map_rect(m[0].work, &m[0], &m[1]), m[1].work);
    }

    #[test]
    fn free_windows_keep_their_real_size_and_spot() {
        let m = two();
        // 1000x600 in the middle of the main monitor (125%) â†’ the TV (150%):
        // 1200x720 physical, the same size to the eye, in the TV's middle.
        let w = centered(Rect::sized(0, 0, 1000, 600), m[1].work);
        let got = map_rect(w, &m[1], &m[0]);
        assert_eq!((got.w(), got.h()), (1200, 720));
        let (cx, cy) = ((got.l + got.r) / 2, (got.t + got.b) / 2);
        assert!((cx - (-1920)).abs() <= 1, "{got:?}");
        assert!((cy - 704).abs() <= 1, "{got:?}");
        assert!(m[0].work.covers(&got));
        // Too big for the smaller one: clamped to its work area, on screen.
        let big = Rect::sized(-3800, -300, 3000, 2000);
        let got = map_rect(big, &m[0], &m[1]);
        assert!(m[1].work.covers(&got), "{got:?}");
        assert_eq!(got.h(), m[1].work.h());
    }

    #[test]
    fn clamping_and_centering_stay_on_screen() {
        let work = Rect::new(60, 0, 2560, 1440);
        assert_eq!(
            clamp_into(Rect::sized(2400, 1300, 400, 300), work),
            Rect::sized(2160, 1140, 400, 300)
        );
        assert_eq!(
            clamp_into(Rect::sized(-50, -50, 400, 300), work),
            Rect::sized(60, 0, 400, 300)
        );
        assert_eq!(clamp_into(Rect::sized(500, 0, 3000, 300), work).l, 60);
        assert_eq!(
            centered(Rect::sized(0, 0, 1000, 600), work),
            Rect::sized(810, 420, 1000, 600)
        );
        assert_eq!(centered(Rect::sized(0, 0, 5000, 600), work).w(), work.w());
    }

    #[test]
    fn windows_with_a_minimum_size_stay_against_the_edge() {
        let work = Rect::new(0, 0, 2560, 1392);
        // Discord won't go narrower than 1400 px: right half â†’ right edge.
        let want = zone_rect(work, Zone::Right);
        let got = anchor(want, 1400, 1392, Some(Zone::Right), work);
        assert_eq!(got, Rect::new(1160, 0, 2560, 1392));
        let got = anchor(
            zone_rect(work, Zone::Left),
            1400,
            1392,
            Some(Zone::Left),
            work,
        );
        assert_eq!(got, Rect::new(0, 0, 1400, 1392));
        // A fixed-size dialog in the bottom right quarter: in that corner.
        let q = zone_rect(work, Zone::BottomRight);
        assert_eq!(
            anchor(q, 400, 300, Some(Zone::BottomRight), work),
            Rect::new(2160, 1092, 2560, 1392)
        );
        // Top half, can't be that wide: centered along the top.
        let t = anchor(zone_rect(work, Zone::Top), 1000, 696, Some(Zone::Top), work);
        assert_eq!(t, Rect::new(780, 0, 1780, 696));
    }

    #[test]
    fn size_limits_are_worded() {
        let want = Rect::new(0, 0, 1280, 1392);
        assert_eq!(
            fit_note(
                "Discord",
                want,
                Rect::new(0, 0, 1400, 1392),
                "the left half"
            ),
            Some(
                "Discord can't get that narrow, so it's as narrow as it goes on the left half."
                    .into()
            )
        );
        assert_eq!(
            fit_note("Calculator", want, Rect::new(0, 0, 900, 1000), "the left half"),
            Some("Calculator can't get that wide or tall, so it's as close as it gets to the left half.".into())
        );
        assert_eq!(
            fit_note(
                "Notepad",
                want,
                Rect::new(1, 0, 1281, 1391),
                "the left half"
            ),
            None
        );
    }

    #[test]
    fn full_screen_is_not_maximized() {
        let mon = Rect::new(0, 0, 2560, 1440);
        assert!(is_fullscreen(mon, mon, false, false));
        assert!(is_fullscreen(
            Rect::new(-1, -1, 2561, 1441),
            mon,
            false,
            false
        ));
        // Maximized with an auto-hide taskbar covers the monitor too.
        assert!(!is_fullscreen(
            Rect::new(-8, -8, 2568, 1448),
            mon,
            true,
            true
        ));
        assert!(!is_fullscreen(mon, mon, false, true));
        assert!(!is_fullscreen(mon, mon, true, false));
        assert!(!is_fullscreen(
            Rect::new(0, 0, 2560, 1392),
            mon,
            false,
            false
        ));
    }

    #[test]
    fn windows_belong_to_the_monitor_showing_most_of_them() {
        let m = two();
        assert_eq!(monitor_of(Rect::sized(-100, 100, 400, 300), &m), 1);
        assert_eq!(monitor_of(Rect::sized(-300, 100, 400, 300), &m), 0);
        // Off every screen: the nearest.
        assert_eq!(monitor_of(Rect::sized(5000, 100, 400, 300), &m), 1);
        assert_eq!(
            zone_of(zone_rect(m[1].work, Zone::TopLeft), m[1].work),
            Some(Zone::TopLeft)
        );
        assert_eq!(zone_of(Rect::sized(300, 300, 900, 600), m[1].work), None);
    }

    #[test]
    fn scale_is_said_in_percent() {
        assert_eq!(scale_percent(96), 100);
        assert_eq!(scale_percent(120), 125);
        assert_eq!(scale_percent(144), 150);
        assert_eq!(scale_percent(168), 175);
    }

    #[test]
    fn it_means_the_window_in_front() {
        for s in [
            "",
            "it",
            "this",
            "this window",
            "that one",
            "the window",
            "the current window",
        ] {
            assert!(means_front(s), "{s}");
        }
        for s in ["spotify", "notepad", "chrome window"] {
            assert!(!means_front(s), "{s}");
        }
    }

    #[test]
    fn a_title_picks_one_of_an_apps_windows() {
        let titles = [
            "Inbox - Google Chrome",
            "Reddit - Google Chrome",
            "YouTube - Google Chrome",
        ];
        assert_eq!(best_titled("reddit", &titles), 1);
        assert_eq!(best_titled("the youtube window", &titles), 2);
        // Just the app: the one used last.
        assert_eq!(best_titled("chrome", &titles), 0);
        assert_eq!(best_titled("zzz", &titles), 0);
    }

    fn win(n: isize, title: &str, exe: &str) -> super::Win {
        super::Win {
            hwnd: super::HWND(n as *mut _),
            title: title.into(),
            exe: exe.into(),
            cloaked: 0,
            frame_host: false,
        }
    }

    #[test]
    fn names_find_one_program_like_pc_apps() {
        use super::Found;
        use super::wnd::find_in;
        let chrome = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
        let wins = || {
            vec![
                win(1, "Reddit - Google Chrome", chrome),
                win(
                    2,
                    "#general | Friends - Discord",
                    r"C:\Users\you\AppData\Local\Discord\app-1.0\Discord.exe",
                ),
                win(3, "Inbox - Google Chrome", chrome),
                win(
                    4,
                    "Spotify Premium",
                    r"C:\Users\you\AppData\Roaming\Spotify\Spotify.exe",
                ),
                win(5, "Calculator", ""),
            ]
        };
        let ids = |f: Found| match f {
            Found::Some(w) => w.iter().map(|w| w.hwnd.0 as isize).collect::<Vec<_>>(),
            _ => vec![],
        };
        // Every window of the program, front first.
        assert_eq!(ids(find_in("chrome", wins())), vec![1, 3]);
        assert_eq!(ids(find_in("google chrome", wins())), vec![1, 3]);
        assert_eq!(ids(find_in("the spotify app", wins())), vec![4]);
        // A Store app goes by its title.
        assert_eq!(ids(find_in("calculator", wins())), vec![5]);
        assert!(matches!(find_in("photoshop", wins()), Found::None));
        // Two programs equally good: ask.
        let two = vec![
            win(1, "Steam", r"C:\Program Files (x86)\Steam\steam.exe"),
            win(
                2,
                "Steam Link",
                r"C:\Program Files (x86)\Steam Link\steamlink.exe",
            ),
        ];
        let Found::Many(ask) = find_in("st", two) else {
            panic!("expected a question")
        };
        assert!(ask.ends_with("Ask which one."), "{ask}");
    }

    #[test]
    fn history_is_bounded() {
        let mut v = Vec::new();
        for i in 0..40 {
            super::push_bounded(&mut v, i, 32);
        }
        assert_eq!(v.len(), 32);
        assert_eq!(v[0], 8);
        assert_eq!(v[31], 39);
    }
}

/// Against a real window: `cargo test -p adrian-pc-control live_notepad --
/// --ignored --nocapture`. It starts its own Notepad (and won't run while
/// another one is open), moves only that one around the main monitor, and
/// closes it.
#[cfg(test)]
mod live {
    use super::*;
    use crate::{Pc, Policy};
    use serde_json::json;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

    struct Closes(HWND);
    impl Drop for Closes {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(self.0, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn notepads() -> Vec<HWND> {
        wnd::list()
            .into_iter()
            .filter(|w| exe_stem(&w.exe) == "notepad")
            .map(|w| w.hwnd)
            .collect()
    }

    #[test]
    #[ignore]
    // The window is closed at the end; the process may only be a launcher.
    #[allow(clippy::zombie_processes)]
    fn live_notepad() {
        if !notepads().is_empty() {
            eprintln!("A Notepad is already open: not touching it.");
            return;
        }
        let pc = Pc::new(Policy::default());
        let _ = pc.run("pc_windows", &json!({"action": "monitors"})); // waits for the app index
        // Its own file, so the window shows that, not the last session's tabs.
        let file = std::env::temp_dir().join("adrian-layout-test.txt");
        std::fs::write(&file, "Window layout test. Nothing to save.").expect("test file");
        std::process::Command::new("notepad.exe")
            .arg(&file)
            .spawn()
            .expect("notepad");
        let deadline = Instant::now() + Duration::from_secs(10);
        let hwnd = loop {
            let mine = wnd::list().into_iter().find(|w| {
                exe_stem(&w.exe) == "notepad" && w.title.starts_with("adrian-layout-test")
            });
            if let Some(w) = mine {
                break w.hwnd;
            }
            assert!(Instant::now() < deadline, "Notepad didn't open");
            std::thread::sleep(Duration::from_millis(100));
        };
        let close = Closes(hwnd);
        std::thread::sleep(Duration::from_millis(500));
        let _dpi = DpiScope::per_monitor();
        let mons = monitors(false);
        let main = mons.iter().position(|m| m.primary).expect("a main monitor");
        let work = mons[main].work;
        let run = |args: Value| {
            let fg = unsafe { GetForegroundWindow() };
            let t = Instant::now();
            let o = pc.run("pc_windows", &args);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let fg_after = unsafe { GetForegroundWindow() };
            println!(
                "{ms:6.1} ms {} {args} -> {}{}",
                if o.ok { "ok" } else { "NO" },
                o.text,
                if fg != fg_after {
                    " (focus changed)"
                } else {
                    ""
                }
            );
            assert!(ms < 100.0, "{args} took {ms} ms");
            o
        };
        let at = |z: Zone| visible(hwnd).near(&zone_rect(work, z), 1);
        for z in Zone::ALL {
            let side = format!("{z:?}");
            let o = run(
                json!({"action": "snap", "name": "notepad", "side": z.words().trim_start_matches("the ").replace(" half", "").replace(" quarter", ""), "monitor": "main"}),
            );
            assert!(o.ok, "{side}: {}", o.text);
            assert!(
                at(z),
                "{side}: {:?} vs {:?}",
                visible(hwnd),
                zone_rect(work, z)
            );
            assert_eq!(monitor_index(hwnd, &mons), main);
        }
        let o = run(
            json!({"action": "snap", "name": "notepad", "side": "bottom_right", "monitor": "main"}),
        );
        assert!(o.text.contains("already"), "{}", o.text);
        let o = run(json!({"action": "maximize", "name": "notepad", "monitor": "main"}));
        assert!(o.ok && zoomed(hwnd), "{}", o.text);
        let o =
            run(json!({"action": "snap", "name": "notepad", "side": "left", "monitor": "main"}));
        assert!(o.ok && !zoomed(hwnd) && at(Zone::Left), "{}", o.text);
        let o = run(json!({"action": "center", "name": "notepad", "monitor": "main"}));
        assert!(o.ok, "{}", o.text);
        let v = visible(hwnd);
        assert!(v.near(&centered(v, work), 1), "{v:?}");
        let o = run(json!({"action": "back"}));
        assert!(o.ok && at(Zone::Left), "{}", o.text);
        let o = run(json!({"action": "restore", "name": "notepad"}));
        assert!(o.text.contains("isn't maximized"), "{}", o.text);
        let o = run(json!({"action": "maximize", "name": "notepad", "monitor": "main"}));
        assert!(o.ok && zoomed(hwnd), "{}", o.text);
        let o = run(json!({"action": "restore", "name": "notepad"}));
        assert!(o.ok && !zoomed(hwnd), "{}", o.text);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        }
        let o = run(json!({"action": "right", "name": "notepad", "monitor": "main"}));
        assert!(o.ok && !iconic(hwnd) && at(Zone::Right), "{}", o.text);
        let o = run(json!({"action": "move", "name": "notepad", "monitor": "main"}));
        assert!(o.text.contains("already on the main monitor"), "{}", o.text);
        let o = run(json!({"action": "move", "name": "notepad", "monitor": "7"}));
        assert!(!o.ok && o.text.contains("no monitor 7"), "{}", o.text);
        let o = run(json!({"action": "snap", "name": "notepad", "side": "sideways"}));
        assert!(!o.ok, "{}", o.text);
        let o = run(json!({"action": "snap", "name": "no such app here", "side": "left"}));
        assert!(!o.ok && o.text.starts_with("Nothing called"), "{}", o.text);
        drop(close);
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::remove_file(&file);
    }
}
