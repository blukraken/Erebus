//! Core link client. One thread owns the pipe and waits on three things at once: a read
//! completing, a UI send arriving, and the heartbeat deadline. The pipe is overlapped, so
//! a send never queues behind a pending read.
//!
//! Up after a `Hello` with a matching protocol. Down on EOF, a read error, a version
//! mismatch, or three missed heartbeats. Reconnects every second while down. Sends made
//! while down are dropped.

use erebus_protocol::{
    read_frame, write_frame, Context, CoreMsg, FaceMsg, Vitals, HEARTBEAT_MS, MAX_FRAME, PIPE_NAME, PROTOCOL_VERSION,
};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const RETRY: Duration = Duration::from_secs(1);
const READ_CHUNK: usize = 64 * 1024;
const WRITE_TIMEOUT_MS: u32 = 2000;

pub fn pipe_name() -> String {
    std::env::var("EREBUS_PIPE").unwrap_or_else(|_| PIPE_NAME.to_string())
}

#[derive(Debug)]
pub enum LinkEvent {
    Up,
    Down,
    /// The core's current context; sent after `Up` and on every change.
    Context(Box<Context>),
    /// Vitals samples: the core's history after `Up`, then one a second.
    Vitals(Box<Vitals>),
    Show,
    Exit,
}

/// UI-side handle: queues a message for the link thread and wakes it.
#[derive(Clone)]
pub struct Link {
    tx: Sender<FaceMsg>,
    wake: Arc<Event>,
    up: Arc<AtomicBool>,
}

impl Link {
    /// The link state now. It flips before the matching `Up` or `Down` event is sent.
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    pub fn send(&self, msg: FaceMsg) {
        if self.tx.send(msg).is_ok() {
            self.wake.set();
        }
    }
}

pub fn spawn(notify: impl Fn(LinkEvent) + Send + 'static) -> Result<Link, String> {
    let name = pipe_name();
    let wake = Arc::new(Event::new(false).map_err(|e| format!("link event: {e}"))?);
    let (tx, rx) = mpsc::channel();
    let thread_wake = wake.clone();
    let up = Arc::new(AtomicBool::new(false));
    let thread_up = up.clone();
    let notify = move |e: LinkEvent| {
        match e {
            LinkEvent::Up => thread_up.store(true, Ordering::Release),
            LinkEvent::Down => thread_up.store(false, Ordering::Release),
            LinkEvent::Show | LinkEvent::Exit | LinkEvent::Context(_) | LinkEvent::Vitals(_) => {}
        }
        notify(e);
    };
    thread::Builder::new()
        .name("erebus-link".into())
        .spawn(move || run(&name, &notify, &rx, &thread_wake))
        .map_err(|e| format!("link thread: {e}"))?;
    Ok(Link { tx, wake, up })
}

fn run(name: &str, notify: &dyn Fn(LinkEvent), rx: &Receiver<FaceMsg>, wake: &Event) {
    log::info!("link: connecting to {name}");
    // Retries repeat the same failure; each distinct one is logged once.
    let mut last = String::new();
    loop {
        let failure = match Pipe::open(name) {
            Ok(pipe) => {
                let (was_up, reason) = session(pipe, notify, rx, wake);
                if was_up {
                    notify(LinkEvent::Down);
                    log::info!("link down: {reason}");
                    last.clear();
                    None
                } else {
                    Some(format!("refused: {reason}"))
                }
            }
            Err(e) => Some(format!("core not reachable ({e})")),
        };
        if let Some(f) = failure.filter(|f| *f != last) {
            log::info!("link: {f}; retrying every 1 s");
            last = f;
        }
        idle(rx, RETRY);
    }
}

/// Waits out the retry delay, dropping any sends.
fn idle(rx: &Receiver<FaceMsg>, wait: Duration) {
    let until = Instant::now() + wait;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(msg) => log::info!("link: dropped {msg:?}; link is down"),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Runs one connection to its end. Returns whether the link came up, and why it ended.
fn session(mut pipe: Pipe, notify: &dyn Fn(LinkEvent), rx: &Receiver<FaceMsg>, wake: &Event) -> (bool, String) {
    let hello = FaceMsg::Hello {
        protocol: PROTOCOL_VERSION,
        face_version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
    };
    if let Err(e) = pipe.send(&hello) {
        return (false, format!("send hello: {e}"));
    }
    if let Err(e) = pipe.start_read() {
        return (false, format!("read: {e}"));
    }

    // Any bytes from the core prove it is alive, so each read restarts the timer.
    let timeout = Duration::from_millis(HEARTBEAT_MS * 3);
    let mut heard = Instant::now();
    let mut up = false;
    let reason = 'session: loop {
        let left = timeout.saturating_sub(heard.elapsed());
        if left.is_zero() {
            break "3 heartbeats missed".into();
        }
        let handles = [pipe.read_event.0, wake.0];
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: both handles are live events owned by this function's callers.
        let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) };
        match woke {
            WAIT_OBJECT_0 => {
                match pipe.finish_read() {
                    Ok(_) => heard = Instant::now(),
                    Err(e) if is_closed(&e) => break "core closed the pipe".into(),
                    Err(e) => break format!("read: {e}"),
                }
                while let Some(frame) = next_frame(&mut pipe.inbox) {
                    match frame {
                        Ok(CoreMsg::Hello { protocol, core_version, pid, .. }) => {
                            if protocol != PROTOCOL_VERSION {
                                break 'session format!("core speaks protocol {protocol}, face speaks {PROTOCOL_VERSION}");
                            }
                            if !up {
                                up = true;
                                log::info!("link up: core {core_version}, pid {pid}");
                                notify(LinkEvent::Up);
                            }
                        }
                        Ok(CoreMsg::Heartbeat { .. }) => {}
                        Ok(CoreMsg::Vitals(v)) if up => notify(LinkEvent::Vitals(Box::new(v))),
                        Ok(CoreMsg::Context(ctx)) if up => {
                            log::info!("link: context {ctx:?}");
                            notify(LinkEvent::Context(Box::new(ctx)));
                        }
                        Ok(msg @ (CoreMsg::Show | CoreMsg::Exit)) if up => {
                            log::info!("link: got {msg:?}");
                            notify(if msg == CoreMsg::Show { LinkEvent::Show } else { LinkEvent::Exit });
                        }
                        Ok(msg) => log::warn!("link: {msg:?} before hello; ignored"),
                        Err(e) => break 'session format!("read: {e}"),
                    }
                }
                if let Err(e) = pipe.start_read() {
                    break if is_closed(&e) { "core closed the pipe".into() } else { format!("read: {e}") };
                }
            }
            w if w == WAIT_OBJECT_0 + 1 => {
                while let Ok(msg) = rx.try_recv() {
                    if !up {
                        log::info!("link: dropped {msg:?}; link is not up");
                        continue;
                    }
                    match pipe.send(&msg) {
                        Ok(()) => log::info!("link: sent {msg:?}"),
                        Err(e) => break 'session format!("send {msg:?}: {e}"),
                    }
                }
            }
            WAIT_TIMEOUT => {}
            _ => break format!("wait: {}", io::Error::last_os_error()),
        }
    };
    (up, reason)
}

fn is_closed(e: &io::Error) -> bool {
    matches!(e.raw_os_error().map(|c| c as u32), Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED))
        || e.kind() == io::ErrorKind::UnexpectedEof
}

/// Takes one complete frame off the front of `buf`, if one is there.
fn next_frame(buf: &mut Vec<u8>) -> Option<io::Result<CoreMsg>> {
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?);
    if len > MAX_FRAME {
        return Some(Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large")));
    }
    let end = 4 + len as usize;
    if buf.len() < end {
        return None;
    }
    let msg = read_frame(&mut &buf[..end]);
    buf.drain(..end);
    Some(msg)
}

/// A Win32 event handle, closed on drop.
struct Event(HANDLE);

// SAFETY: event handles may be signalled and waited on from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new(manual_reset: bool) -> io::Result<Event> {
        // SAFETY: no attributes, no name; the handle is owned by the returned value.
        let h = unsafe { CreateEventW(std::ptr::null(), manual_reset.into(), 0, std::ptr::null()) };
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Event(h))
        }
    }

    fn set(&self) {
        // SAFETY: the handle is a live event.
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// An overlapped pipe client with at most one read in flight.
struct Pipe {
    handle: HANDLE,
    read_event: Event,
    /// Boxed so their addresses stay fixed while the kernel holds them.
    read_ov: Box<OVERLAPPED>,
    read_buf: Box<[u8]>,
    reading: bool,
    /// Bytes received and not yet parsed into frames.
    inbox: Vec<u8>,
}

impl Pipe {
    fn open(name: &str) -> io::Result<Pipe> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated; the handle is owned by the returned value.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let read_event = match Event::new(true) {
            Ok(e) => e,
            Err(e) => {
                // SAFETY: owned handle, closed once.
                unsafe { CloseHandle(handle) };
                return Err(e);
            }
        };
        Ok(Pipe {
            handle,
            read_event,
            // SAFETY: OVERLAPPED is plain data; all-zero is its documented initial state.
            read_ov: Box::new(unsafe { std::mem::zeroed() }),
            read_buf: vec![0; READ_CHUNK].into_boxed_slice(),
            reading: false,
            inbox: Vec::new(),
        })
    }

    fn start_read(&mut self) -> io::Result<()> {
        // SAFETY: OVERLAPPED is plain data; it is reset before each new read.
        *self.read_ov = unsafe { std::mem::zeroed() };
        self.read_ov.hEvent = self.read_event.0;
        // SAFETY: the buffer and OVERLAPPED are boxed and outlive the read; Drop cancels
        // and waits for any read still in flight.
        let ok = unsafe {
            ReadFile(self.handle, self.read_buf.as_mut_ptr(), READ_CHUNK as u32, std::ptr::null_mut(), &mut *self.read_ov)
        };
        // SAFETY: reads this thread's last error.
        if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }
        self.reading = true;
        Ok(())
    }

    /// Collects a finished read into the inbox. Call once its event is signalled.
    fn finish_read(&mut self) -> io::Result<usize> {
        let mut n = 0u32;
        // SAFETY: the read was started on this handle with this OVERLAPPED.
        let ok = unsafe { GetOverlappedResult(self.handle, &*self.read_ov, &mut n, 0) };
        self.reading = false;
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        self.inbox.extend_from_slice(&self.read_buf[..n as usize]);
        Ok(n as usize)
    }

    /// Writes one frame and waits for it to land.
    fn send(&self, msg: &FaceMsg) -> io::Result<()> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, msg)?;
        let done = Event::new(true)?;
        // SAFETY: OVERLAPPED is plain data; all-zero is its documented initial state.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = done.0;
        let mut n = 0u32;
        // SAFETY: `bytes` and `ov` live on this frame, and every path below waits for the
        // write to finish or be cancelled before they drop.
        unsafe {
            let ok = WriteFile(self.handle, bytes.as_ptr(), bytes.len() as u32, std::ptr::null_mut(), &mut ov);
            if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                return Err(io::Error::last_os_error());
            }
            if WaitForSingleObject(done.0, WRITE_TIMEOUT_MS) != WAIT_OBJECT_0 {
                CancelIoEx(self.handle, &ov);
                GetOverlappedResult(self.handle, &ov, &mut n, 1);
                return Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"));
            }
            if GetOverlappedResult(self.handle, &ov, &mut n, 0) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if n as usize != bytes.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
        }
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: cancels this handle's read, waits until the kernel lets go of the buffer,
        // then closes the handle once.
        unsafe {
            if self.reading {
                let mut n = 0u32;
                CancelIoEx(self.handle, &*self.read_ov);
                GetOverlappedResult(self.handle, &*self.read_ov, &mut n, 1);
            }
            CloseHandle(self.handle);
        }
    }
}
