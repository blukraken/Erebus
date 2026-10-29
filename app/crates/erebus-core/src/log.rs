//! Append-only file log at `%LOCALAPPDATA%\Erebus\logs\core.log`, rotated to `core.log.1` at 1 MB.
//!
//! `EREBUS_LOG_DIR` overrides the directory. Debug builds also echo to stderr.

use std::fmt::Arguments;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;

struct Sink {
    path: PathBuf,
    file: File,
    size: u64,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

macro_rules! log_info {
    ($($t:tt)*) => { $crate::log::write("INFO", format_args!($($t)*)) };
}
macro_rules! log_warn {
    ($($t:tt)*) => { $crate::log::write("WARN", format_args!($($t)*)) };
}
macro_rules! log_error {
    ($($t:tt)*) => { $crate::log::write("ERROR", format_args!($($t)*)) };
}
pub(crate) use {log_error, log_info, log_warn};

pub fn init() {
    let dir = match std::env::var_os("EREBUS_LOG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => match std::env::var_os("LOCALAPPDATA") {
            Some(p) => PathBuf::from(p).join("Erebus").join("logs"),
            None => return,
        },
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("core.log");
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let Ok(file) = open(&path, false) else { return };
    let mut sink = Sink { path, file, size };
    if size >= MAX_BYTES {
        sink.rotate();
    }
    let _ = SINK.set(Mutex::new(sink));
}

pub fn write(level: &str, args: Arguments) {
    let line = format!("{} {level:<5} {args}\n", timestamp());
    #[cfg(debug_assertions)]
    eprint!("{line}");

    let Some(sink) = SINK.get() else { return };
    let Ok(mut sink) = sink.lock() else { return };
    if sink.size + line.len() as u64 > MAX_BYTES {
        sink.rotate();
    }
    if sink.file.write_all(line.as_bytes()).is_ok() {
        sink.size += line.len() as u64;
    }
}

impl Sink {
    /// Moves the current log to `core.log.1`, or truncates it if the move fails.
    fn rotate(&mut self) {
        let old = self.path.with_extension("log.1");
        let moved = fs::rename(&self.path, old).is_ok();
        if let Ok(file) = open(&self.path, !moved) {
            self.file = file;
            self.size = 0;
        }
    }
}

fn open(path: &Path, truncate: bool) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    if truncate {
        o.write(true).create(true).truncate(true);
    } else {
        o.append(true).create(true);
    }
    o.open(path)
}

/// UTC, millisecond precision.
fn timestamp() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let (secs, ms) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_654), (2026, 7, 20));
    }
}
