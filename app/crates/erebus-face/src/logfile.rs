//! Small file log at `%LOCALAPPDATA%\Erebus\logs\face.log`.
//!
//! Face lines at info and above; dependency lines (wgpu, naga) at warn and above.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The log restarts empty once it passes this size.
const MAX_BYTES: u64 = 1024 * 1024;

struct FileLog {
    file: Mutex<File>,
}

impl Log for FileLog {
    fn enabled(&self, m: &Metadata) -> bool {
        let floor = if m.target().starts_with("erebus") { Level::Info } else { Level::Warn };
        m.level() <= floor
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("{} {:<5} {}\n", timestamp(), r.level(), r.args());
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
    }

    fn flush(&self) {}
}

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Erebus").join("logs").join("face.log"))
}

pub fn init() {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let full = fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES);
    let mut opts = OpenOptions::new();
    if full {
        opts.write(true).create(true).truncate(true);
    } else {
        opts.append(true).create(true);
    }
    let Ok(file) = opts.open(&path) else { return };
    if log::set_boxed_logger(Box::new(FileLog { file: Mutex::new(file) })).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
}

/// Local wall-clock time, `YYYY-MM-DD HH:MM:SS.mmm`.
fn timestamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}
