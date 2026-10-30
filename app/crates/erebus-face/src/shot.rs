//! Screenshot mode: offscreen frames through the window's renderer, written as PNG. One still,
//! or with `--frames` a numbered sequence for video.

use crate::app::{context_files, context_text};
use crate::feed::Feed;
use crate::gpu::Gpu;
use crate::knot::{self, Flare, Sim};
use crate::net::{self, Net};
use crate::panel::{self, Panel};
use crate::render::{Renderer, Scene, View};
use crate::vitals::{self, Vitals};
use erebus_protocol::{ChangedFile, Context};
use std::path::{Path, PathBuf};

pub struct Shot {
    /// The still; unused with `frames`.
    pub path: Option<PathBuf>,
    pub size: (u32, u32),
    /// Writes only this region of each frame: centred, or at `crop_at`.
    pub crop: Option<(u32, u32)>,
    pub crop_at: Option<(u32, u32)>,
    /// Scales the written image up this many times, nearest neighbour.
    pub zoom: u32,
    pub view: View,
    pub knot: KnotShot,
    pub frames: Option<Frames>,
}

/// A consecutive frame sequence: `dir/frame_00000.png` onward.
pub struct Frames {
    pub dir: PathBuf,
    pub duration: f64,
    pub fps: f64,
}

/// Knot state for a still.
#[derive(Default)]
pub struct KnotShot {
    pub context: Option<Context>,
    /// Animation clock, seconds.
    pub time: f64,
    pub hover: bool,
    pub flare: Option<Flare>,
    pub glance: bool,
    pub reduced: bool,
    /// Replaces the context's file list.
    pub files: Option<Vec<ChangedFile>>,
    /// Seconds since a commit of the first `commit_count` files (all by default). Negative
    /// is a commit still to come: a frame sequence fires it that many seconds in.
    pub commit_at: Option<f64>,
    pub commit_count: Option<usize>,
    /// Hovers the glyph of this path.
    pub hover_file: Option<String>,
    /// Frames to render offscreen before the still, timing the CPU each costs.
    pub bench: u32,
    /// Leaves the vitals stack out.
    pub no_vitals: bool,
    /// Shows this vitals row's tooltip.
    pub hover_vital: Option<vitals::Row>,
    /// Opens the vitals panel on this row: fully open in a still, unfolding in a sequence.
    pub vitals_panel: Option<vitals::Row>,
    /// Feed events: a still shows them settled; a sequence holds back the last three and
    /// delivers them 0.8 s apart.
    pub feed: Option<Vec<erebus_protocol::FeedEvent>>,
    /// Scrolls the feed back this many rows and lets the wheel rest, so the live chip shows.
    pub feed_scroll: Option<f32>,
    /// Session globes. A sequence can flip the second session busy (the knot pulls it in)
    /// and later let the first go, as if it had been idle past the release time. It can also
    /// hold back the first session's workers until one time and finish them at another.
    pub sessions: Option<Vec<erebus_protocol::SessionInfo>>,
    pub sessions_busy_at: Option<f64>,
    pub sessions_release_at: Option<f64>,
    pub sessions_workers_at: Option<f64>,
    pub sessions_workers_done_at: Option<f64>,
    /// Holds the last session back until it arrives, and ends it, as if its harness
    /// started and closed.
    pub sessions_arrive_at: Option<f64>,
    pub sessions_end_at: Option<f64>,
    /// Fires a canned burst of feed events at the first session: three at once (a tool, its
    /// failure, an ok), then one from its first worker and a granted permission.
    pub sessions_pulse_at: Option<f64>,
}

/// How far into a `--flare` the still is taken.
const FLARE_AT: f32 = 0.35;

/// An offscreen colour target and its readback buffer.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    row: u32,
    size: (u32, u32),
}

impl Target {
    fn new(gpu: &Gpu, (w, h): (u32, u32), format: wgpu::TextureFormat) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shot"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shot readback"),
            size: row as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Target { texture, view, readback, row, size: (w, h) }
    }

    /// Draws `scene` and reads back `crop`, `[x, y, w, h]`, as packed RGB.
    fn capture(&self, gpu: &Gpu, renderer: &mut Renderer, scene: &Scene, crop: [u32; 4]) -> Result<Vec<u8>, String> {
        let (w, h) = self.size;
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        renderer.draw(gpu, &mut encoder, &self.view, (w, h), 1.0, scene);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| {
            if let Err(e) = r {
                log::error!("shot map: {e}");
            }
        });
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("poll: {e}"))?;
        let rgb = {
            let data = slice.get_mapped_range().map_err(|e| format!("map: {e}"))?;
            let [x0, y0, cw, ch] = crop;
            let mut rgb = Vec::with_capacity((cw * ch * 3) as usize);
            for y in y0 as usize..(y0 + ch) as usize {
                let start = y * self.row as usize + x0 as usize * 4;
                for px in data[start..start + cw as usize * 4].as_chunks::<4>().0 {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            rgb
        };
        self.readback.unmap();
        Ok(rgb)
    }
}

/// Scales packed RGB up `z` times, nearest neighbour.
fn zoom(rgb: Vec<u8>, w: u32, z: u32) -> Vec<u8> {
    if z <= 1 {
        return rgb;
    }
    let (w, z) = (w as usize, z as usize);
    let mut out = Vec::with_capacity(rgb.len() * z * z);
    for row in rgb.chunks_exact(w * 3) {
        let wide: Vec<u8> = row.as_chunks::<3>().0.iter().flat_map(|px| px.repeat(z)).collect();
        for _ in 0..z {
            out.extend_from_slice(&wide);
        }
    }
    out
}

fn write_png(path: &Path, (w, h): (u32, u32), rgb: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = enc.write_header().map_err(|e| format!("png: {e}"))?;
    writer.write_image_data(rgb).map_err(|e| format!("png: {e}"))
}

pub fn run(shot: &Shot) -> Result<(), String> {
    let (gpu, _) = Gpu::new(None)?;
    let (w, h) = shot.size;
    let (cw, ch) = shot.crop.unwrap_or(shot.size);
    let (cx, cy) = shot.crop_at.unwrap_or(((w - cw) / 2, (h - ch) / 2));
    let crop = [cx, cy, cw, ch];
    let out_size = (cw * shot.zoom, ch * shot.zoom);
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = Target::new(&gpu, shot.size, format);

    let k = &shot.knot;
    let mut context = k.context.clone();
    if let (Some(files), Some(git)) = (&k.files, context.as_mut().and_then(|c| c.project.as_mut()?.git.as_mut())) {
        git.changed = git.changed.max(files.len() as u32);
        git.files = files.clone();
    }
    let live = shot.view.link >= 1.0 && context.is_some();
    let (crumbs, label) = context_text(context.as_ref(), live);
    let crumbs: Vec<&str> = crumbs.iter().map(String::as_str).collect();
    let mut sim = Sim::default();
    sim.live_target = if live { 1.0 } else { 0.0 };
    sim.hover_target = if k.hover && label.is_some() { 1.0 } else { 0.0 };
    sim.lod_target = if k.glance { 0.0 } else { 1.0 };
    sim.snap();
    sim.set_time(k.time);

    // Files settle in first; a commit then plays forward to the asked moment.
    let (list, changed) = context_files(context.as_ref());
    sim.set_files(list, changed, None, false);
    sim.snap();
    let n = k.commit_count.unwrap_or(list.len()).min(list.len());
    let commit = |sim: &mut Sim| {
        let c = sim.set_files(&list[n..], changed.saturating_sub(n as u32), Some("c0ffee1"), !k.reduced);
        log::info!("shot commit: {} dash, {} fade", c.dashed, c.faded);
    };
    let mut pending = None;
    match k.commit_at {
        Some(at) if at >= 0.0 => {
            commit(&mut sim);
            sim.advance(at, k.reduced);
        }
        Some(at) => pending = Some(-at),
        None => {}
    }
    if let Some(f) = k.flare {
        sim.flare_at(f, FLARE_AT);
    }
    let (r, centre) = (knot::radius(w, h, 1.0), [w as f32 * 0.5, h as f32 * 0.5]);
    if let Some(path) = &k.hover_file {
        sim.hovered = sim.files.find(path);
        sim.settle_hover();
        if sim.hovered.is_none() {
            log::warn!("--hover-file {path}: no such listed file");
        }
    }

    // Canned vitals, 0.4 s after the newest sample at `--time`.
    let mut now_ms = vitals::CANNED_T0 + 400 + (k.time * 1000.0) as u64;
    let mut vt = if k.no_vitals { Vitals::default() } else { vitals::canned(canned_index(now_ms)) };
    vt.hover = k.hover_vital;
    vt.snap();
    let column = vitals::column(w, h, 1.0);
    let mut vpanel = Panel::default();
    if let Some(row) = k.vitals_panel {
        vpanel.toggle(row);
    }
    let panel_frame = |p: &Panel, vt: &Vitals| panel::frame(p, vt, column.as_ref()?, (w, h), k.reduced);
    let mut feed = Feed::new();
    let mut held: Vec<erebus_protocol::FeedEvent> = k.feed.clone().unwrap_or_default();
    let hold = if shot.frames.is_some() { held.len().min(3) } else { 0 };
    feed.ingest(held.drain(..held.len() - hold).collect());
    if let Some(rows) = k.feed_scroll {
        feed.scroll_by(rows);
        for _ in 0..60 {
            feed.step(1.0 / 60.0, k.reduced);
        }
    }
    let feed_frame = |f: &Feed| f.frame(column.as_ref()?, k.reduced);
    let mut net_ms = vitals::CANNED_T0;
    let mut vnet = Net::new();
    let area = column.as_ref().map(|c| net::Area::new(c, w, h, knot::radius(w, h, 1.0)));
    if let Some(a) = area {
        vnet.set_area(a);
    }
    // The sessions as they stand `at` seconds in: the first one's workers between their
    // start and finish times, the last session between its arrival and end.
    let between = |at: f64, from: Option<f64>, to: Option<f64>| from.is_none_or(|t| at >= t) && to.is_none_or(|t| at < t);
    let snapshot = |at: f64| {
        k.sessions.clone().map(|mut s| {
            if !between(at, k.sessions_workers_at, k.sessions_workers_done_at) {
                if let Some(x) = s.first_mut() {
                    x.workers.clear();
                }
            }
            if !between(at, k.sessions_arrive_at, k.sessions_end_at) {
                s.pop();
            }
            s
        })
    };
    if let Some(s) = snapshot(0.0) {
        vnet.ingest_sessions(&s, net_ms);
    }
    let net_frame = |n: &Net| n.frame(area.as_ref()?, k.reduced);

    let mut renderer = Renderer::new(&gpu.device, &gpu.queue, format);
    let Some(frames) = &shot.frames else {
        vpanel.snap();
        // A still has no time for history to fade in.
        feed.snap();
        // A still has no time to condense in, so its globes are shown fully formed.
        vnet.finish();
        let vf = vt.frame(column.as_ref(), now_ms, (w, h), k.reduced);
        let pf = panel_frame(&vpanel, &vt);
        let ff = feed_frame(&feed);
        let nf = net_frame(&vnet);
        let more = if live { sim.files.more() } else { None };
        let file_label = file_label(&sim, k, centre, r);
        let scene = Scene {
            view: shot.view,
            knot: sim.uniform(w, h, 1.0, k.reduced),
            crumbs: &crumbs,
            label: label.as_deref(),
            label_alpha: sim.hover(),
            files: sim.files.instances(),
            files_gen: sim.files.generation,
            file_label,
            file_label_alpha: sim.file_hover(),
            chips: &[],
            more: more.as_deref(),
            vitals: vf.as_ref(),
            panel: nf.as_ref().and_then(|n| n.card.as_ref()).or(pf.as_ref()),
            feed: ff.as_ref(),
            net: nf.as_ref(),
        };
        if k.bench > 0 {
            bench(&gpu, &mut renderer, &target.view, (w, h), &scene, &sim, k, &vt, now_ms);
        }
        let path = shot.path.as_deref().ok_or("no --screenshot path")?;
        let rgb = target.capture(&gpu, &mut renderer, &scene, crop)?;
        write_png(path, out_size, &zoom(rgb, cw, shot.zoom))?;
        log::info!("screenshot {w}x{h} {:?} live {live} -> {}", shot.view, path.display());
        return Ok(());
    };

    let count = (frames.duration * frames.fps).round() as u32;
    let dt = 1.0 / frames.fps;
    std::fs::create_dir_all(&frames.dir).map_err(|e| format!("{}: {e}", frames.dir.display()))?;
    let t0 = std::time::Instant::now();
    for i in 0..count {
        if i > 0 {
            sim.step(dt as f32, k.reduced);
            now_ms = vitals::CANNED_T0 + 400 + ((k.time + f64::from(i) * dt) * 1000.0) as u64;
            feed_canned(&mut vt, now_ms);
            vt.step(dt as f32, k.reduced);
            vpanel.step(dt as f32, k.reduced);
            feed.step(dt as f32, k.reduced);
            net_ms += (dt * 1000.0) as u64;
            vnet.step(dt as f32, k.reduced, net_ms);
            renderer.net_clock += dt as f32;
        }
        let at = f64::from(i) * dt;
        if let Some(s) = k.sessions.as_ref().filter(|_| k.sessions_busy_at.is_some_and(|t| at >= t && at - dt < t)) {
            let mut s = s.clone();
            if let Some(x) = s.get_mut(1) {
                x.status = erebus_protocol::SessionStatus::Busy;
            }
            vnet.ingest_sessions(&s, net_ms);
        }
        let crossed = |t: Option<f64>| t.is_some_and(|t| at >= t && at - dt < t);
        let times = [k.sessions_workers_at, k.sessions_workers_done_at, k.sessions_arrive_at, k.sessions_end_at];
        if times.into_iter().any(crossed) {
            vnet.ingest_sessions(&snapshot(at).unwrap_or_default(), net_ms);
        }
        let first = k.sessions.as_ref().and_then(|s| s.first());
        if let (Some(t0), Some(s)) = (k.sessions_pulse_at, first) {
            use erebus_protocol::{FeedEvent, FeedKind};
            let ev = |kind, phrase: &str, worker: Option<&str>| FeedEvent {
                unix_ms: net_ms,
                kind,
                phrase: format!("{} · {phrase}", s.label),
                session: s.id.clone(),
                worker: worker.map(str::to_owned),
                backlog: false,
            };
            let w = s.workers.first().map(|w| w.id.as_str());
            if crossed(Some(t0)) {
                vnet.pulse(&[ev(FeedKind::Tool, "Bash cargo", None), ev(FeedKind::Error, "Bash failed", None), ev(FeedKind::Result, "Bash ok", None)]);
            }
            if crossed(Some(t0 + 2.5)) {
                vnet.pulse(&[ev(FeedKind::Tool, "Read lib.rs", w), ev(FeedKind::Perm, "granted Bash", None)]);
            }
        }
        if let Some(s) = k.sessions.as_ref().filter(|_| k.sessions_release_at.is_some_and(|t| at >= t && at - dt < t)) {
            let mut s = s.clone();
            s[0].status = erebus_protocol::SessionStatus::Idle;
            net_ms += net::RELEASE_AFTER;
            vnet.ingest_sessions(&s, net_ms);
        }
        let due = (f64::from(i) * dt / 0.8).floor() as usize;
        if !held.is_empty() && hold - held.len() < due {
            feed.ingest(vec![held.remove(0)]);
        }
        if pending.is_some_and(|at| f64::from(i) * dt >= at) {
            pending = None;
            commit(&mut sim);
        }
        let more = if live { sim.files.more() } else { None };
        let vf = vt.frame(column.as_ref(), now_ms, (w, h), k.reduced);
        let pf = panel_frame(&vpanel, &vt);
        let ff = feed_frame(&feed);
        let nf = net_frame(&vnet);
        let scene = Scene {
            view: shot.view,
            knot: sim.uniform(w, h, 1.0, k.reduced),
            crumbs: &crumbs,
            label: label.as_deref(),
            label_alpha: sim.hover(),
            files: sim.files.instances(),
            files_gen: sim.files.generation,
            file_label: file_label(&sim, k, centre, r),
            file_label_alpha: sim.file_hover(),
            chips: &[],
            more: more.as_deref(),
            vitals: vf.as_ref(),
            panel: nf.as_ref().and_then(|n| n.card.as_ref()).or(pf.as_ref()),
            feed: ff.as_ref(),
            net: nf.as_ref(),
        };
        let rgb = target.capture(&gpu, &mut renderer, &scene, crop)?;
        write_png(&frames.dir.join(format!("frame_{i:05}.png")), out_size, &zoom(rgb, cw, shot.zoom))?;
        renderer.trim();
    }
    let line = format!(
        "frames {count} at {} fps, {}x{} -> {} in {:.1} s",
        frames.fps,
        out_size.0,
        out_size.1,
        frames.dir.display(),
        t0.elapsed().as_secs_f64()
    );
    log::info!("{line}");
    println!("{line}");
    Ok(())
}

/// The canned sample index current at `now_ms`.
fn canned_index(now_ms: u64) -> i64 {
    (now_ms as i64 - vitals::CANNED_T0 as i64).div_euclid(1000)
}

/// Delivers the canned samples due by `now_ms`, one a second as the core would.
fn feed_canned(vt: &mut Vitals, now_ms: u64) {
    let due = canned_index(now_ms);
    while let Some(newest) = vt.newest_ms().map(canned_index).filter(|&i| i < due) {
        vt.ingest(erebus_protocol::Vitals { hw: vitals::canned_hw(), samples: vec![vitals::canned_sample(newest + 1)] });
    }
}

/// The hovered glyph's path and where its label points.
fn file_label<'a>(sim: &Sim, k: &'a KnotShot, centre: [f32; 2], r: f32) -> Option<(&'a str, [f32; 2])> {
    let path = k.hover_file.as_deref()?;
    Some((path, sim.files.screen(sim.hovered?, centre, r, sim.rot())))
}

/// Process CPU time, microseconds.
fn process_cpu_us() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut kt, mut u) = (z, z, z, z);
    // SAFETY: the pseudo handle needs no closing; the FILETIMEs are ours.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut kt, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10;
    t(kt) + t(u)
}

/// Renders frames as the window would, minus present, and logs CPU per frame. Then, where
/// the adapter has timestamps, renders as many again with each pass timed and logs the GPU
/// time before, during and after the vitals.
#[allow(clippy::too_many_arguments)]
fn bench(
    gpu: &Gpu,
    renderer: &mut Renderer,
    target: &wgpu::TextureView,
    size: (u32, u32),
    scene: &Scene,
    sim: &Sim,
    k: &KnotShot,
    vt: &Vitals,
    now_ms: u64,
) {
    let column = vitals::column(size.0, size.1, 1.0);
    let run = |renderer: &mut Renderer, timed: bool| {
        let (mut sim, mut vt) = (sim.clone(), vt.clone());
        let mut gpu_ms = [0.0; 3];
        let c0 = process_cpu_us();
        let t0 = std::time::Instant::now();
        for i in 0..k.bench {
            sim.step(1.0 / 60.0, k.reduced);
            let now = now_ms + u64::from(i) * 1000 / 60;
            feed_canned(&mut vt, now);
            vt.step(1.0 / 60.0, k.reduced);
            let vf = vt.frame(column.as_ref(), now, size, k.reduced);
            let frame = Scene { knot: sim.uniform(size.0, size.1, 1.0, k.reduced), vitals: vf.as_ref(), ..*scene };
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            renderer.draw(gpu, &mut encoder, target, size, 1.0, &frame);
            gpu.queue.submit([encoder.finish()]);
            renderer.trim();
            if timed {
                if let Some(t) = renderer.pass_times(gpu) {
                    gpu_ms = [0, 1, 2].map(|j| gpu_ms[j] + t[j]);
                }
            } else {
                let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
            }
        }
        (process_cpu_us() - c0, t0.elapsed().as_micros() as u64, gpu_ms)
    };
    let n = u64::from(k.bench);
    let (cpu, wall, _) = run(renderer, false);
    let line = format!("bench {n} frames: CPU {} us/frame, wall {} us/frame", cpu / n, wall / n);
    log::info!("{line}");
    println!("{line}");
    if !renderer.time_passes(&gpu.device) {
        println!("bench: no timestamp queries on this adapter; GPU time unmeasured");
        return;
    }
    let (_, _, ms) = run(renderer, true);
    let [a, b, c] = ms.map(|m| m / n as f64);
    let line = format!(
        "bench {n} frames, GPU ms/frame: ground+knot {a:.4}, vitals {b:.4}, shapes+text {c:.4}, total {:.4}",
        a + b + c
    );
    log::info!("{line}");
    println!("{line}");
}
