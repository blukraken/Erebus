//! PinnOS core: the always-on background service. Serves the named pipe the face connects to
//! and owns the tray icon.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hub;
mod icon;
mod ids;
mod log;
mod route;
mod server;
mod tray;

use std::future::{poll_fn, Future};
use std::pin::{pin, Pin};
use std::process::ExitCode;
use std::task::Poll;

use log::{log_error, log_info};
use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
use tokio::sync::oneshot;

fn main() -> ExitCode {
    log::init();
    std::panic::set_hook(Box::new(|p| log_error!("panic: {p}")));

    let name = match std::env::var("PINNOS_PIPE") {
        Ok(n) if !n.is_empty() => n,
        _ => pinnos_protocol::PIPE_NAME.to_owned(),
    };
    log_info!("starting pinnos-core {} pid {} pipe {name}", env!("CARGO_PKG_VERSION"), std::process::id());

    let rt = match tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build() {
        Ok(rt) => rt,
        Err(e) => {
            log_error!("runtime failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(run(&name));
    log_info!("stopped");
    code
}

/// `PINNOS_TRAY=0` turns the tray off.
fn tray_enabled() -> bool {
    std::env::var("PINNOS_TRAY").map_or(true, |v| v.trim() != "0")
}

async fn run(name: &str) -> ExitCode {
    let first = match server::bind(name) {
        Ok(p) => p,
        Err(e) if server::is_already_running(&e) => {
            log_info!("already running on {name}; exiting");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            log_error!("cannot create pipe {name}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (hub_tx, hub_rx) = hub::channel();
    let tray = if tray_enabled() {
        tray::spawn(hub_tx.clone())
    } else {
        log_info!("tray disabled by PINNOS_TRAY=0");
        None
    };
    let (quit_tx, quit_rx) = oneshot::channel();
    hub::spawn(hub_rx, tray.as_ref().map(tray::Tray::hwnd), quit_tx);

    let mut serve = pin!(server::serve(first, name, server::now_ms(), hub_tx));
    let mut stop = pin!(shutdown_signal(quit_rx));
    let why = poll_fn(|cx| {
        if let Poll::Ready(why) = stop.as_mut().poll(cx) {
            return Poll::Ready(why);
        }
        match serve.as_mut().poll(cx) {
            Poll::Ready(never) => match never {},
            Poll::Pending => Poll::Pending,
        }
    })
    .await;
    log_info!("shutting down: {why}");
    if let Some(tray) = tray {
        tray.shutdown();
    }
    ExitCode::SUCCESS
}

/// Resolves on Ctrl+C, Ctrl+Break, console close, logoff, system shutdown, or the tray's Quit.
async fn shutdown_signal(mut quit: oneshot::Receiver<&'static str>) -> &'static str {
    let sigs = (|| {
        Ok::<_, std::io::Error>((ctrl_c()?, ctrl_break()?, ctrl_close()?, ctrl_logoff()?, ctrl_shutdown()?))
    })();
    let (mut c, mut b, mut cl, mut lo, mut sh) = match sigs {
        Ok(s) => s,
        Err(e) => {
            log_error!("signal handlers unavailable: {e}");
            return std::future::pending().await;
        }
    };
    let mut quit_open = true;
    poll_fn(|cx| {
        if c.poll_recv(cx).is_ready() {
            Poll::Ready("ctrl-c")
        } else if b.poll_recv(cx).is_ready() {
            Poll::Ready("ctrl-break")
        } else if cl.poll_recv(cx).is_ready() {
            Poll::Ready("console close")
        } else if lo.poll_recv(cx).is_ready() {
            Poll::Ready("logoff")
        } else if sh.poll_recv(cx).is_ready() {
            Poll::Ready("system shutdown")
        } else if quit_open {
            match Pin::new(&mut quit).poll(cx) {
                Poll::Ready(Ok(why)) => Poll::Ready(why),
                Poll::Ready(Err(_)) => {
                    quit_open = false;
                    Poll::Pending
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    })
    .await
}
