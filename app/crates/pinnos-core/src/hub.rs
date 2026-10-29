//! Owns the list of connected faces and acts on tray actions. Everything reaches it as an
//! [`Event`] on one channel, so no state is shared across tasks or threads.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use pinnos_protocol::CoreMsg;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};

use crate::log::{log_error, log_info, log_warn};
use crate::route::{plan, Effect, TrayAction};
use crate::tray;

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How long Quit waits for the faces' writers to flush `Exit` before stopping anyway.
const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// A message for one face's writer task. `done` fires once the frame is written.
pub struct Out {
    pub msg: CoreMsg,
    pub done: Option<oneshot::Sender<()>>,
}

pub enum Event {
    /// A client sent a valid `Hello`: client id, face pid, writer.
    Up(u64, u32, UnboundedSender<Out>),
    /// A client went away. Ignored for ids that never registered.
    Down(u64),
    Tray(TrayAction),
}

pub fn channel() -> (UnboundedSender<Event>, UnboundedReceiver<Event>) {
    unbounded_channel()
}

/// Runs the hub on the current runtime. `tray` is the tray window handle, if any. `shutdown`
/// fires after a Quit has delivered `Exit` to the faces.
pub fn spawn(rx: UnboundedReceiver<Event>, tray: Option<usize>, shutdown: oneshot::Sender<&'static str>) {
    tokio::spawn(run(rx, tray, Some(shutdown)));
}

async fn run(
    mut rx: UnboundedReceiver<Event>,
    tray: Option<usize>,
    mut shutdown: Option<oneshot::Sender<&'static str>>,
) {
    let mut faces: Vec<(u64, u32, UnboundedSender<Out>)> = Vec::new();
    let publish = |n: usize| {
        if let Some(hwnd) = tray {
            tray::faces_changed(hwnd, n);
        }
    };
    while let Some(event) = rx.recv().await {
        match event {
            Event::Up(id, pid, tx) => {
                faces.push((id, pid, tx));
                log_info!("face {id} registered; {} connected", faces.len());
                publish(faces.len());
            }
            Event::Down(id) => {
                let before = faces.len();
                faces.retain(|(f, _, _)| *f != id);
                if faces.len() != before {
                    log_info!("face {id} gone; {} connected", faces.len());
                    publish(faces.len());
                }
            }
            Event::Tray(action) => {
                log_info!("tray action {action:?} with {} face(s)", faces.len());
                let ids: Vec<u64> = faces.iter().map(|(id, _, _)| *id).collect();
                let mut acks = Vec::new();
                for effect in plan(action, &ids) {
                    match effect {
                        Effect::Send(id, msg) => {
                            let Some((_, pid, tx)) = faces.iter().find(|(f, _, _)| *f == id) else { continue };
                            if msg == CoreMsg::Show {
                                allow_foreground(*pid);
                            }
                            let (done, ack) = oneshot::channel();
                            if tx.send(Out { msg, done: Some(done) }).is_ok() {
                                acks.push(ack);
                            }
                        }
                        Effect::SpawnFace => spawn_face(),
                        Effect::Shutdown => {
                            let deadline = Instant::now() + FLUSH_WAIT;
                            for ack in acks.drain(..) {
                                let _ = timeout_at(deadline, ack).await;
                            }
                            if let Some(tx) = shutdown.take() {
                                let _ = tx.send("tray quit");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Passes this process's foreground right to the face, so its `SetForegroundWindow` on `Show`
/// is honoured. The core holds that right only just after a tray click.
fn allow_foreground(pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    // SAFETY: plain Win32 call with a process id; failure only means Windows keeps the old focus.
    if unsafe { AllowSetForegroundWindow(pid) } == 0 {
        log_warn!("AllowSetForegroundWindow({pid}) refused");
    }
}

/// `PINNOS_FACE_EXE`, else `pinnos-face.exe` next to this executable.
fn face_exe() -> Option<PathBuf> {
    match std::env::var_os("PINNOS_FACE_EXE") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => Some(std::env::current_exe().ok()?.parent()?.join("pinnos-face.exe")),
    }
}

fn spawn_face() {
    let Some(exe) = face_exe() else {
        log_error!("cannot locate pinnos-face.exe");
        return;
    };
    let spawned = Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    match spawned {
        Ok(child) => {
            allow_foreground(child.id());
            log_info!("spawned face pid {} from {}", child.id(), exe.display());
        }
        Err(e) => log_warn!("cannot spawn {}: {e}", exe.display()),
    }
}
