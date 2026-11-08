//! "Read what I copied": the clipboard's text, only when asked. Anything a
//! password manager marks as secret is refused, images and files are only
//! described, and nothing here is ever logged.

use super::from_wide;
use crate::Outcome;
use std::time::Duration;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// Longest text handed to the model, in characters.
pub const MAX_CHARS: usize = 8000;

const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// Formats whose presence alone marks the content as not for other programs.
const SECRET_IF_PRESENT: [&str; 2] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "Clipboard Viewer Ignore",
];
/// Formats that mark it secret when they hold a DWORD 0 ("don't keep this").
const SECRET_IF_ZERO: [&str; 2] = ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"];

/// One format on the clipboard: its number, its registered name (none for the
/// standard ones), and for the two "Can…" formats, the DWORD it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    pub id: u32,
    pub name: Option<String>,
    pub value: Option<u32>,
}

#[cfg(test)]
impl Format {
    pub fn std(id: u32) -> Self {
        Format {
            id,
            name: None,
            value: None,
        }
    }
    pub fn named(name: &str, value: Option<u32>) -> Self {
        Format {
            id: 0xC000,
            name: Some(name.to_string()),
            value,
        }
    }
}

/// Whether a format's value has to be read to judge the clipboard.
pub fn needs_value(name: &str) -> bool {
    SECRET_IF_ZERO.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// What the clipboard holds, judged from its formats alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holds {
    Empty,
    /// Marked by a password manager (or any app) as not for other programs.
    Secret,
    Text,
    Files,
    Image,
    Other,
}

/// What OLE leaves behind on an emptied clipboard: no content of its own.
const HOUSEKEEPING: [&str; 2] = ["DataObject", "Ole Private Data"];

pub fn classify(formats: &[Format]) -> Holds {
    let content = formats.iter().any(|f| {
        f.name
            .as_deref()
            .is_none_or(|n| !HOUSEKEEPING.iter().any(|h| h.eq_ignore_ascii_case(n)))
    });
    if !content {
        return Holds::Empty;
    }
    let secret = formats.iter().any(|f| {
        let Some(n) = &f.name else { return false };
        SECRET_IF_PRESENT.iter().any(|s| s.eq_ignore_ascii_case(n))
            // A "Can…" format that can't be read is treated as a no too.
            || (needs_value(n) && f.value.is_none_or(|v| v == 0))
    });
    let has = |id: u32| formats.iter().any(|f| f.id == id);
    let named = |n: &str| {
        formats
            .iter()
            .any(|f| f.name.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(n)))
    };
    if secret {
        Holds::Secret
    } else if has(CF_UNICODETEXT) {
        Holds::Text
    } else if has(CF_HDROP) {
        Holds::Files
    } else if has(CF_BITMAP) || has(CF_DIB) || has(CF_DIBV5) || named("PNG") {
        Holds::Image
    } else {
        Holds::Other
    }
}

/// Text for the model: at most MAX_CHARS characters, and a note when cut.
/// `head` is the start of the text (all of it when `total`, the full length in
/// characters, is no longer).
pub fn clip_text(head: &str, total: usize) -> String {
    let whole = total <= head.chars().count();
    let text = if whole {
        head.trim()
    } else {
        head.trim_start()
    };
    let n = if whole { text.chars().count() } else { total };
    if whole && n <= MAX_CHARS {
        return format!("The clipboard text ({n} characters):\n{text}");
    }
    let cut: String = text.chars().take(MAX_CHARS).collect();
    format!(
        "The clipboard text, cut to the first {MAX_CHARS} of {n} characters:\n{cut}\n(The rest was cut off.)"
    )
}

/// The clipboard, open; closed again when dropped, whatever happens.
struct Open;
impl Open {
    /// Another program may have it open for a moment: try for about 150 ms.
    fn new() -> Option<Open> {
        for _ in 0..10 {
            // SAFETY: no owner window; closed in Drop.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Some(Open);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        None
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: opened by Open::new on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

/// A clipboard memory block, locked for reading.
struct Locked {
    h: HGLOBAL,
    ptr: *const u8,
    len: usize,
}
impl Locked {
    fn new(h: HANDLE) -> Option<Locked> {
        let h = HGLOBAL(h.0);
        // SAFETY: a handle from GetClipboardData for a memory format; unlocked in Drop.
        let ptr = unsafe { GlobalLock(h) } as *const u8;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the same handle.
        let len = unsafe { GlobalSize(h) };
        Some(Locked { h, ptr, len })
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: GlobalSize bytes are readable while locked.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: locked in Locked::new.
        let _ = unsafe { GlobalUnlock(self.h) };
    }
}

/// The DWORD a format holds, if it holds one.
fn dword(id: u32) -> Option<u32> {
    // SAFETY: the clipboard is open (callers hold an Open).
    let h = unsafe { GetClipboardData(id) }.ok()?;
    let l = Locked::new(h)?;
    let b = l.bytes();
    (b.len() >= 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn formats() -> Vec<Format> {
    let mut out = Vec::new();
    let mut id = 0u32;
    loop {
        // SAFETY: the clipboard is open; 0 ends the list.
        id = unsafe { EnumClipboardFormats(id) };
        if id == 0 || out.len() > 200 {
            break;
        }
        let mut buf = [0u16; 128];
        // SAFETY: a buffer of the length given; 0 for standard formats.
        let n = unsafe { GetClipboardFormatNameW(id, &mut buf) } as usize;
        let name = (n > 0).then(|| from_wide(&buf[..n.min(buf.len())]));
        let value = name
            .as_deref()
            .filter(|n| needs_value(n))
            .and_then(|_| dword(id));
        out.push(Format { id, name, value });
    }
    out
}

/// The start of the text (up to the first NUL, at most MAX_CHARS plus a little
/// to trim), and its full length in characters. Never copies more than that.
fn unicode_text() -> Option<(String, usize)> {
    // SAFETY: the clipboard is open.
    let h = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let l = Locked::new(h)?;
    let units = l
        .bytes()
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0);
    let mut head = String::new();
    let mut total = 0usize;
    for c in char::decode_utf16(units) {
        if total < MAX_CHARS + 256 {
            head.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        total += 1;
    }
    Some((head, total))
}

fn file_count() -> u32 {
    // SAFETY: the clipboard is open; 0xFFFFFFFF asks for the count.
    unsafe {
        match GetClipboardData(CF_HDROP) {
            Ok(h) => DragQueryFileW(HDROP(h.0), u32::MAX, None),
            Err(_) => 0,
        }
    }
}

pub fn read() -> Outcome {
    let Some(_open) = Open::new() else {
        return Outcome::no(
            "Another program is using the clipboard right now. Try again in a moment.",
        );
    };
    // SAFETY: the clipboard is open.
    if unsafe { CountClipboardFormats() } == 0 {
        return Outcome::ok("The clipboard is empty.");
    }
    match classify(&formats()) {
        Holds::Empty => Outcome::ok("The clipboard is empty."),
        Holds::Secret => Outcome::no("That looks like a password, so I won't read it."),
        Holds::Text => match unicode_text() {
            Some((t, n)) if n == t.chars().count() && t.trim().is_empty() => {
                Outcome::ok("The clipboard holds only blank text.")
            }
            Some((t, n)) => Outcome::ok(clip_text(&t, n)),
            None => Outcome::no("Couldn't read the clipboard text."),
        },
        Holds::Files => match file_count() {
            1 => Outcome::ok(
                "The clipboard holds a copied file, not text. I don't open files from it.",
            ),
            n => Outcome::ok(format!(
                "The clipboard holds {n} copied files, not text. I don't open files from it."
            )),
        },
        Holds::Image => Outcome::ok(
            "The clipboard holds an image, not text. I can't look at clipboard images; if it's on screen, ask about the screen.",
        ),
        Holds::Other => Outcome::ok("The clipboard holds something that isn't text."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_text() {
        let f = [Format::std(CF_UNICODETEXT), Format::std(1), Format::std(7)];
        assert_eq!(classify(&f), Holds::Text);
    }

    #[test]
    fn nothing_is_empty() {
        assert_eq!(classify(&[]), Holds::Empty);
        // What a cleared clipboard still lists.
        let cleared = [
            Format::named("DataObject", None),
            Format::named("Ole Private Data", None),
        ];
        assert_eq!(classify(&cleared), Holds::Empty);
    }

    #[test]
    fn password_manager_marks_are_refused() {
        for mark in SECRET_IF_PRESENT {
            let f = [Format::std(CF_UNICODETEXT), Format::named(mark, None)];
            assert_eq!(classify(&f), Holds::Secret, "{mark}");
        }
        // Names are matched however they're cased.
        let f = [
            Format::std(CF_UNICODETEXT),
            Format::named("clipboard viewer ignore", None),
        ];
        assert_eq!(classify(&f), Holds::Secret);
    }

    #[test]
    fn history_and_cloud_flags_count_only_when_zero() {
        for flag in SECRET_IF_ZERO {
            let no = [Format::std(CF_UNICODETEXT), Format::named(flag, Some(0))];
            assert_eq!(classify(&no), Holds::Secret, "{flag} = 0");
            let yes = [Format::std(CF_UNICODETEXT), Format::named(flag, Some(1))];
            assert_eq!(classify(&yes), Holds::Text, "{flag} = 1");
            // Unreadable: safer to treat as a no.
            let unknown = [Format::std(CF_UNICODETEXT), Format::named(flag, None)];
            assert_eq!(classify(&unknown), Holds::Secret, "{flag} unreadable");
        }
        assert!(needs_value("CanIncludeInClipboardHistory"));
        assert!(!needs_value("ExcludeClipboardContentFromMonitorProcessing"));
    }

    #[test]
    fn a_secret_image_is_still_secret() {
        let f = [
            Format::std(CF_DIB),
            Format::named("Clipboard Viewer Ignore", None),
        ];
        assert_eq!(classify(&f), Holds::Secret);
    }

    #[test]
    fn files_and_images_are_described() {
        assert_eq!(
            classify(&[
                Format::std(CF_HDROP),
                Format::named("Shell IDList Array", None)
            ]),
            Holds::Files
        );
        assert_eq!(
            classify(&[Format::std(CF_DIB), Format::std(CF_DIBV5)]),
            Holds::Image
        );
        assert_eq!(classify(&[Format::std(CF_BITMAP)]), Holds::Image);
        assert_eq!(classify(&[Format::named("PNG", None)]), Holds::Image);
        // A picture copied from a browser also carries its address as text: read that.
        assert_eq!(
            classify(&[Format::named("PNG", None), Format::std(CF_UNICODETEXT)]),
            Holds::Text
        );
        assert_eq!(
            classify(&[Format::named("Rich Text Format", None)]),
            Holds::Other
        );
    }

    #[test]
    fn long_text_is_cut_with_a_note() {
        let all = |s: &str| clip_text(s, s.chars().count());
        assert_eq!(
            all("  hello  "),
            "The clipboard text (5 characters):\nhello"
        );
        let long = "é".repeat(MAX_CHARS + 500);
        let t = all(&long);
        assert!(t.starts_with(&format!(
            "The clipboard text, cut to the first {MAX_CHARS} of {} characters",
            MAX_CHARS + 500
        )));
        assert_eq!(t.matches('é').count(), MAX_CHARS);
        assert!(t.ends_with("(The rest was cut off.)"));
        // Exactly the limit is not cut.
        assert!(!all(&"a".repeat(MAX_CHARS)).contains("cut"));
    }

    #[test]
    fn only_the_start_of_huge_text_is_needed() {
        // The reader hands over the first part and the full count.
        let head = format!("  {}", "x".repeat(MAX_CHARS + 100));
        let t = clip_text(&head, 2_000_000);
        assert!(
            t.starts_with(&format!(
                "The clipboard text, cut to the first {MAX_CHARS} of 2000000 characters:\nxxx"
            )),
            "{}",
            &t[..120]
        );
        let body = t.split_once('\n').unwrap().1;
        assert_eq!(body.matches('x').count(), MAX_CHARS);
    }
}
