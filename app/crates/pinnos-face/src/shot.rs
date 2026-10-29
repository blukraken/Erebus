//! Screenshot mode: offscreen frames through the window's renderer, written as PNG. One still,
//! or with `--frames` a numbered sequence for video.

use crate::app::{context_files, context_text};
use crate::gpu::Gpu;
use crate::knot::{self, Flare, Sim};
use crate::render::{Renderer, Scene, View};
use pinnos_protocol::{ChangedFile, Context};
use std::path::{Path, PathBuf};

pub struct Shot {
    /// The still; unused with `frames`.
    pub path: Option<PathBuf>,
    pub size: (u32, u32),
    /// Writes only this centred region of each frame.
    pub crop: Option<(u32, u32)>,
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

    /// Draws `scene` and reads back the centred `crop` as packed RGB.
    fn capture(&self, gpu: &Gpu, renderer: &mut Renderer, scene: &Scene, crop: (u32, u32)) -> Result<Vec<u8>, String> {
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
            let (cw, ch) = crop;
            let (x0, y0) = ((w - cw) / 2, (h - ch) / 2);
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
    let crop = shot.crop.unwrap_or(shot.size);
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

    let mut renderer = Renderer::new(&gpu.device, &gpu.queue, format);
    let Some(frames) = &shot.frames else {
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
            more: more.as_deref(),
        };
        if k.bench > 0 {
            bench(&gpu, &mut renderer, &target.view, (w, h), &scene, &sim, k);
        }
        let path = shot.path.as_deref().ok_or("no --screenshot path")?;
        write_png(path, crop, &target.capture(&gpu, &mut renderer, &scene, crop)?)?;
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
        }
        if pending.is_some_and(|at| f64::from(i) * dt >= at) {
            pending = None;
            commit(&mut sim);
        }
        let more = if live { sim.files.more() } else { None };
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
            more: more.as_deref(),
        };
        let rgb = target.capture(&gpu, &mut renderer, &scene, crop)?;
        write_png(&frames.dir.join(format!("frame_{i:05}.png")), crop, &rgb)?;
        renderer.trim();
    }
    let line = format!(
        "frames {count} at {} fps, {}x{} -> {} in {:.1} s",
        frames.fps,
        crop.0,
        crop.1,
        frames.dir.display(),
        t0.elapsed().as_secs_f64()
    );
    log::info!("{line}");
    println!("{line}");
    Ok(())
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

/// Renders frames as the window would, minus present, and logs CPU per frame.
fn bench(gpu: &Gpu, renderer: &mut Renderer, target: &wgpu::TextureView, size: (u32, u32), scene: &Scene, sim: &Sim, k: &KnotShot) {
    let mut sim = sim.clone();
    let c0 = process_cpu_us();
    let t0 = std::time::Instant::now();
    for _ in 0..k.bench {
        sim.step(1.0 / 60.0, k.reduced);
        let frame = Scene { knot: sim.uniform(size.0, size.1, 1.0, k.reduced), ..*scene };
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        renderer.draw(gpu, &mut encoder, target, size, 1.0, &frame);
        gpu.queue.submit([encoder.finish()]);
        renderer.trim();
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    }
    let cpu = process_cpu_us() - c0;
    let wall = t0.elapsed().as_micros() as u64;
    let n = u64::from(k.bench);
    let line = format!("bench {n} frames: CPU {} us/frame, wall {} us/frame", cpu / n, wall / n);
    log::info!("{line}");
    println!("{line}");
}
