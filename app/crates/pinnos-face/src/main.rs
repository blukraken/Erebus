#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! PinnOS face: the window that draws Loom.
//!
//! `pinnos-face [--windowed]` opens the window, fullscreen unless `--windowed`. Only one
//! face runs per core pipe; a second exits 0 at once.
//! Diagnostics for the window: `--trace` logs every redraw with its reason and per-second
//! input and frame counts; `--idle-present <hz>` presents an unchanged frame at that rate
//! while idle; `--no-motion-frames` stops fullscreen presenting while the cursor moves.
//! `pinnos-face --screenshot <png> [--size WxH] [--palette] [--link up|down]
//! [--close-hover|--close-press] [--windowed-chrome]` renders one offscreen frame and
//! exits, with no window and no IPC. `--windowed-chrome` renders the F11 window: 3/5 of
//! `--size`.

mod app;
mod gpu;
mod instance;
mod link;
mod logfile;
mod render;
mod shot;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: pinnos-face [--windowed] [--trace] [--idle-present <hz>] [--no-motion-frames] \
                     | --screenshot <png> [--size WxH] [--palette] [--link up|down] \
                     [--close-hover|--close-press] [--windowed-chrome]";

enum Mode {
    Window(app::Options),
    Shot(shot::Shot),
}

fn parse_window(args: &[String]) -> Result<app::Options, String> {
    let mut o = app::Options { windowed: false, trace: false, idle_present_hz: 0.0, no_motion_frames: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--windowed" => o.windowed = true,
            "--trace" => o.trace = true,
            "--no-motion-frames" => o.no_motion_frames = true,
            "--idle-present" => {
                let v = it.next().ok_or("--idle-present needs a value")?;
                o.idle_present_hz = match v.parse::<f64>() {
                    Ok(hz) if (0.0..=1000.0).contains(&hz) => hz,
                    _ => return Err(format!("bad --idle-present {v}")),
                };
            }
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok(o)
}

fn parse(args: &[String]) -> Result<Mode, String> {
    if !args.iter().any(|a| a == "--screenshot") {
        return parse_window(args).map(Mode::Window);
    }
    let mut path = None;
    let mut size = (5120, 1440);
    let mut windowed = false;
    let mut view = render::View { palette: false, link: 0.0, close: render::Close::Rest };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--screenshot" => path = Some(PathBuf::from(value()?)),
            "--size" => {
                let v = value()?;
                let parsed = v.split_once(['x', 'X']).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                size = match parsed {
                    Some((w, h)) if w > 0 && h > 0 && w <= 8192 && h <= 8192 => (w, h),
                    _ => return Err(format!("bad --size {v}")),
                };
            }
            "--palette" => view.palette = true,
            "--link" => {
                view.link = match value()?.as_str() {
                    "up" => 1.0,
                    "down" => 0.0,
                    v => return Err(format!("bad --link {v}")),
                }
            }
            "--close-hover" => view.close = render::Close::Hover,
            "--close-press" => view.close = render::Close::Press,
            "--windowed-chrome" => windowed = true,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    if windowed {
        size = (size.0 * 3 / 5, size.1 * 3 / 5);
    }
    match path {
        Some(path) => Ok(Mode::Shot(shot::Shot { path, size, view })),
        None => Err("screenshot options need --screenshot <png>".into()),
    }
}

fn main() -> ExitCode {
    logfile::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match parse(&args) {
        Ok(Mode::Shot(s)) => {
            log::info!("face {} start, pid {}, screenshot", env!("CARGO_PKG_VERSION"), std::process::id());
            shot::run(&s)
        }
        Ok(Mode::Window(opts)) => {
            let pid = std::process::id();
            match instance::acquire(&link::pipe_name()) {
                Ok(Some(_guard)) => {
                    log::info!("face {} start, pid {pid}, window, {:?}", env!("CARGO_PKG_VERSION"), args);
                    app::run(opts)
                }
                Ok(None) => {
                    log::info!("face already running; pid {pid} exits");
                    return ExitCode::SUCCESS;
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => {
            log::error!("{e}");
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
