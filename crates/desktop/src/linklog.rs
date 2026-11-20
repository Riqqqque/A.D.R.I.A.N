//! A short local record of the server link: how each connection ended, how
//! long she was gone and why the tries failed. Kept beside the settings, two
//! files of at most `MAX_BYTES` each, so it never grows. No address or token
//! goes in it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// One file's size before it's moved aside for a new one.
const MAX_BYTES: u64 = 128 * 1024;

#[derive(Default)]
pub struct LinkLog {
    file: Mutex<Option<PathBuf>>,
}

impl LinkLog {
    /// Write to this file from now on (the settings folder's link.log).
    pub fn to(&self, file: PathBuf) {
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    }

    /// One line, after the time (UTC). Nothing happens without a file.
    pub fn line(&self, text: &str) {
        let guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let Some(file) = guard.as_ref() else { return };
        if std::fs::metadata(file).is_ok_and(|m| m.len() > MAX_BYTES) {
            let _ = std::fs::rename(file, file.with_extension("old.log"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{} {}", utc_now(), text.replace(['\r', '\n'], " "));
        }
    }
}

/// Now as `2026-11-20 16:46:05.123Z`.
fn utc_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    utc(ms)
}

fn utc(ms: u64) -> String {
    let (days, rest) = ((ms / 86_400_000) as i64, ms % 86_400_000);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_times_read_right() {
        assert_eq!(utc(0), "1970-01-01 00:00:00.000Z");
        // 2026-11-20 16:46:05.123 UTC.
        assert_eq!(utc(1_795_193_165_123), "2026-11-20 16:46:05.123Z");
        // A leap day.
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00.000Z");
    }

    #[test]
    fn stays_small_and_writes_nothing_without_a_file() {
        LinkLog::default().line("nowhere");
        let dir = std::env::temp_dir().join(format!("adrian-linklog-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("link.log");
        let log = LinkLog::default();
        log.to(file.clone());
        let long = "x".repeat(1000);
        for _ in 0..400 {
            log.line(&long);
        }
        let size = |p: &PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        assert!(size(&file) <= MAX_BYTES + 1100, "{}", size(&file));
        assert!(size(&file.with_extension("old.log")) > 0);
        log.line("two\nlines");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.trim_end().ends_with("two lines"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
