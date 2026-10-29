#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Erebus face: the window that draws Loom.
//!
//! `erebus-face [--windowed]` opens the window, fullscreen unless `--windowed`. Only one
//! face runs per core pipe; a second exits 0 at once.
//! Diagnostics for the window: `--trace` logs every redraw with its reason and per-second
//! input and frame counts; `--idle-present <hz>` presents an unchanged frame at that rate
//! while idle; `--no-motion-frames` stops fullscreen presenting while the cursor moves.
//! `--reduced-motion` holds the knot still, as Windows does with animation effects off.
//! `erebus-face --screenshot <png> [--size WxH] [--palette] [--link up|down]
//! [--close-hover|--close-press] [--windowed-chrome] [--context <json>] [--time <s>]
//! [--hover-knot] [--flare branch|head|changed] [--glance] [--reduced-motion]` renders one
//! offscreen frame and exits, with no window and no IPC. `--windowed-chrome` renders the
//! F11 window: 3/5 of `--size`. `--context` reads a protocol `Context` as JSON; the knot is
//! live with `--link up` and a context. `--time` sets the animation clock; `--flare` shows
//! that flare 35% of the way through. `--files <json>` replaces the context's file list
//! with a JSON array of protocol `ChangedFile`. `--commit-at <s>` shows the still that many
//! seconds after a commit of the first `--commit-count <n>` files (all by default).
//! `--hover-file <path>` hovers that file's glyph. `--bench <n>` first renders n frames
//! offscreen and prints the CPU each costs, then the GPU time of each pass where the
//! adapter has timestamps. `--crop WxH[+X+Y]` keeps only that region, centred unless placed;
//! `--zoom <n>` scales the output up n times.
//!
//! Stills and sequences show the vitals from a canned history; `--no-vitals` leaves them out
//! and `--hover-vital ram|gpu|cpu` shows that row's tooltip.
//!
//! `--frames <dir> [--duration <s>] [--fps <n>]` replaces the still with a consecutive
//! sequence, `dir/frame_00000.png` onward, stepping the clock 1/fps per frame from `--time`.
//! It takes every screenshot option, `--size` defaults to 1920x540, and a negative
//! `--commit-at` fires the commit that many seconds into the sequence.

mod app;
mod files;
mod gpu;
mod instance;
mod knot;
mod link;
mod logfile;
mod render;
mod shot;
mod vitals;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: erebus-face [--windowed] [--trace] [--idle-present <hz>] [--no-motion-frames] \
                     [--reduced-motion] \
                     | --screenshot <png> [--size WxH] [--palette] [--link up|down] \
                     [--close-hover|--close-press] [--windowed-chrome] [--context <json>] [--time <s>] \
                     [--hover-knot] [--flare branch|head|changed] [--glance] [--reduced-motion] [--bench <n>] \
                     [--files <json>] [--commit-at <s>] [--commit-count <n>] [--hover-file <path>] \
                     [--crop WxH[+X+Y]] [--zoom <n>] [--no-vitals] [--hover-vital ram|gpu|cpu] \
                     | --frames <dir> [--duration <s>] [--fps <n>] [screenshot options]";

fn parse_size(flag: &str, v: &str) -> Result<(u32, u32), String> {
    let parsed = v.split_once(['x', 'X']).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    match parsed {
        Some((w, h)) if w > 0 && h > 0 && w <= 8192 && h <= 8192 => Ok((w, h)),
        _ => Err(format!("bad {flag} {v}")),
    }
}

type Crop = ((u32, u32), Option<(u32, u32)>);

/// `WxH`, or `WxH+X+Y` with its top-left corner.
fn parse_crop(flag: &str, v: &str) -> Result<Crop, String> {
    let mut parts = v.split('+');
    let size = parse_size(flag, parts.next().unwrap_or(""))?;
    let at: Vec<u32> = parts.map(|p| p.parse().map_err(|_| format!("bad {flag} {v}"))).collect::<Result<_, _>>()?;
    match at[..] {
        [] => Ok((size, None)),
        [x, y] => Ok((size, Some((x, y)))),
        _ => Err(format!("bad {flag} {v}")),
    }
}

fn parse_secs(flag: &str, v: &str, range: std::ops::RangeInclusive<f64>) -> Result<f64, String> {
    match v.parse::<f64>() {
        Ok(t) if range.contains(&t) => Ok(t),
        _ => Err(format!("bad {flag} {v}")),
    }
}

enum Mode {
    Window(app::Options),
    Shot(Box<shot::Shot>),
}

fn parse_window(args: &[String]) -> Result<app::Options, String> {
    let mut o = app::Options {
        windowed: false,
        trace: false,
        idle_present_hz: 0.0,
        no_motion_frames: false,
        reduced_motion: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--windowed" => o.windowed = true,
            "--trace" => o.trace = true,
            "--no-motion-frames" => o.no_motion_frames = true,
            "--reduced-motion" => o.reduced_motion = true,
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
    if !args.iter().any(|a| a == "--screenshot" || a == "--frames") {
        return parse_window(args).map(Mode::Window);
    }
    let mut path = None;
    let mut size = None;
    let mut crop = None;
    let mut crop_at = None;
    let mut zoom = 1;
    let mut frames_dir = None;
    let mut duration = 12.0;
    let mut fps = 60.0;
    let mut windowed = false;
    let mut view = render::View { palette: false, link: 0.0, close: render::Close::Rest };
    let mut knot = shot::KnotShot::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--screenshot" => path = Some(PathBuf::from(value()?)),
            "--frames" => frames_dir = Some(PathBuf::from(value()?)),
            "--duration" => duration = parse_secs(a, value()?, 0.01..=600.0)?,
            "--fps" => fps = parse_secs(a, value()?, 1.0..=240.0)?,
            "--size" => size = Some(parse_size(a, value()?)?),
            "--crop" => {
                let (size, at) = parse_crop(a, value()?)?;
                (crop, crop_at) = (Some(size), at);
            }
            "--zoom" => {
                let v = value()?;
                zoom = match v.parse::<u32>() {
                    Ok(z) if (1..=8).contains(&z) => z,
                    _ => return Err(format!("bad --zoom {v}")),
                };
            }
            "--no-vitals" => knot.no_vitals = true,
            "--hover-vital" => {
                let v = value()?;
                knot.hover_vital = Some(vitals::Row::parse(v).ok_or(format!("bad --hover-vital {v}"))?);
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
            "--context" => {
                let v = value()?;
                let text = std::fs::read_to_string(v.as_str()).map_err(|e| format!("--context {v}: {e}"))?;
                knot.context = Some(serde_json::from_str(&text).map_err(|e| format!("--context {v}: {e}"))?);
            }
            "--time" => knot.time = parse_secs(a, value()?, 0.0..=1e6)?,
            "--hover-knot" => knot.hover = true,
            "--files" => {
                let v = value()?;
                let text = std::fs::read_to_string(v.as_str()).map_err(|e| format!("--files {v}: {e}"))?;
                knot.files = Some(serde_json::from_str(&text).map_err(|e| format!("--files {v}: {e}"))?);
            }
            "--commit-at" => knot.commit_at = Some(parse_secs(a, value()?, -600.0..=10.0)?),
            "--commit-count" => {
                let v = value()?;
                knot.commit_count = Some(v.parse().map_err(|_| format!("bad --commit-count {v}"))?);
            }
            "--hover-file" => knot.hover_file = Some(value()?.clone()),
            "--flare" => {
                knot.flare = Some(match value()?.as_str() {
                    "branch" => knot::Flare::Branch,
                    "head" => knot::Flare::Head,
                    "changed" => knot::Flare::Changed,
                    v => return Err(format!("bad --flare {v}")),
                })
            }
            "--glance" => knot.glance = true,
            "--reduced-motion" => knot.reduced = true,
            "--bench" => {
                let v = value()?;
                knot.bench = v.parse().map_err(|_| format!("bad --bench {v}"))?;
            }
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let frames = frames_dir.map(|dir| shot::Frames { dir, duration, fps });
    let mut size = size.unwrap_or(if frames.is_some() { (1920, 540) } else { (5120, 1440) });
    if windowed {
        size = (size.0 * 3 / 5, size.1 * 3 / 5);
    }
    if crop.is_some_and(|(w, h)| w > size.0 || h > size.1) {
        return Err(format!("--crop is larger than the {}x{} frame", size.0, size.1));
    }
    if let (Some((w, h)), Some((x, y))) = (crop, crop_at) {
        if x + w > size.0 || y + h > size.1 {
            return Err(format!("--crop runs off the {}x{} frame", size.0, size.1));
        }
    }
    if path.is_none() && frames.is_none() {
        return Err("screenshot options need --screenshot <png> or --frames <dir>".into());
    }
    Ok(Mode::Shot(Box::new(shot::Shot { path, size, crop, crop_at, zoom, view, knot, frames })))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn frames_mode_parses_with_video_defaults() {
        let Ok(Mode::Shot(s)) = parse(&args("--frames out --fps 30 --duration 2 --commit-at -1.5")) else {
            panic!("frames should enter shot mode");
        };
        assert_eq!(s.size, (1920, 540));
        assert!(s.path.is_none());
        let f = s.frames.as_ref().unwrap();
        assert_eq!((f.fps, f.duration), (30.0, 2.0));
        assert_eq!(s.knot.commit_at, Some(-1.5));
    }

    #[test]
    fn still_keeps_its_size_and_rejects_a_large_crop() {
        let Ok(Mode::Shot(s)) = parse(&args("--screenshot a.png --crop 1920x1080")) else {
            panic!("still should parse");
        };
        assert_eq!((s.size, s.crop), ((5120, 1440), Some((1920, 1080))));
        assert!(parse(&args("--frames out --crop 4000x100")).is_err());
        assert!(parse(&args("--crop 10x10")).is_err());
    }

    #[test]
    fn placed_crop_zoom_and_vitals_flags() {
        let Ok(Mode::Shot(s)) = parse(&args("--screenshot a.png --crop 600x700+0+300 --zoom 2 --hover-vital gpu")) else {
            panic!("still should parse");
        };
        assert_eq!((s.crop, s.crop_at, s.zoom), (Some((600, 700)), Some((0, 300)), 2));
        assert_eq!(s.knot.hover_vital, Some(vitals::Row::Gpu));
        assert!(parse(&args("--screenshot a.png --crop 600x700+5000+0")).is_err());
        assert!(parse(&args("--screenshot a.png --hover-vital disk")).is_err());
    }
}
