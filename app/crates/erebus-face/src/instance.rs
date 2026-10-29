//! One face per core pipe, held by a named mutex for the life of the process.
//!
//! The default pipe takes `Local\Erebus.Face`. A `EREBUS_PIPE` override gets its own
//! suffixed name, so a test face never collides with the owner's.

use erebus_protocol::PIPE_NAME;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Holds the mutex; the OS releases it when the process ends.
pub struct Guard(#[allow(dead_code)] HANDLE);

pub fn mutex_name(pipe: &str) -> String {
    if pipe == PIPE_NAME {
        return r"Local\Erebus.Face".into();
    }
    let tag: String = pipe.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!(r"Local\Erebus.Face.{tag}")
}

/// `None` when another face already holds this pipe's mutex.
pub fn acquire(pipe: &str) -> Result<Option<Guard>, String> {
    let wide: Vec<u16> = mutex_name(pipe).encode_utf16().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; the handle stays open until the process ends.
    let (handle, err) = unsafe { (CreateMutexW(std::ptr::null(), 0, wide.as_ptr()), GetLastError()) };
    if handle.is_null() {
        return Err(format!("single-instance mutex: {}", std::io::Error::from_raw_os_error(err as i32)));
    }
    Ok(if err == ERROR_ALREADY_EXISTS { None } else { Some(Guard(handle)) })
}
