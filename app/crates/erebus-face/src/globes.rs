//! Agent globes: session and worker nodes of the net. Ported from `globe`, `hotCore` and the
//! `session` and `worker` cases of `drawNode` in `v3/concept-loom4.html`.
//!
//! A session globe is a dotted sphere with a hot ball at its centre; a worker globe is a
//! coarser sphere around an ember trefoil knotlet. Every dot of both kinds lives in one
//! static point buffer built at startup. The caller places globes each frame as
//! [`GlobeInst`]s; one instanced draw turns them all, and `form` gathers each globe out of
//! scattered dust or dissolves it back.
//!
//! Glows, tick rings and flares around a globe are not drawn here.


use bytemuck::{Pod, Zeroable};
use std::f64::consts::{PI, TAU};

pub const MAX_GLOBES: usize = 64;

/// Session core dots: `ballPts(200, 21)`.
const S_CORE: usize = 200;
/// Worker core dots: every third of the prototype's 60-point trefoil.
const W_CORE: usize = 20;

/// One globe as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct GlobeInst {
    /// Centre x and y in physical pixels, radius in pixels, rotation in radians.
    pub pos: [f32; 4],
    /// Tint as sRGB 0..1, and opacity 0..1.
    pub color: [f32; 4],
    /// Kind (0 session, 1 worker), form 0..1 (0 scattered dust, 1 whole), core jitter amplitude, seed.
    pub form: [f32; 4],
    /// Grey 0..1, as a waiting session's braid greys; the rest unused.
    pub look: [f32; 4],
}

impl GlobeInst {
    fn worker(&self) -> bool {
        self.form[0] >= 0.5
    }
}

/// Per-frame globe uniform; see `globes.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct Uniform {
    /// Target width, height, dot scale, core jitter phase.
    size: [f32; 4],
    /// lod, session globes, worker globes, unused.
    state: [f32; 4],
}

/// The prototype's `prng`: mulberry32.
fn prng(seed: u32) -> impl FnMut() -> f64 {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6D2B_79F5);
        let mut t = (a ^ (a >> 15)).wrapping_mul(1 | a);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// The prototype's `latGlobe`: rings of dots at even latitudes, each ring turned by a
/// random offset.
fn lat_globe(rings: u32, per_eq: f64, seed: u32) -> Vec<[f32; 4]> {
    let mut r = prng(seed);
    let mut out = Vec::new();
    for i in 1..rings {
        let lat = -PI / 2.0 + f64::from(i) / f64::from(rings) * PI;
        let (y, rr) = (lat.sin(), lat.cos());
        let n = (per_eq * rr).round().max(6.0) as u32;
        let off = r() * TAU;
        for j in 0..n {
            let a = off + f64::from(j) / f64::from(n) * TAU;
            out.push([(a.cos() * rr) as f32, y as f32, (a.sin() * rr) as f32, 0.0]);
        }
    }
    out
}

/// The prototype's `ballPts`: a ball denser toward the middle; w is a variance in 0..1.
fn ball(n: usize, seed: u32, pow: f64) -> Vec<[f32; 4]> {
    let mut r = prng(seed);
    (0..n)
        .map(|_| {
            let u = r() * 2.0 - 1.0;
            let a = r() * TAU;
            let s = (1.0 - u * u).sqrt();
            let d = r().powf(pow);
            [a.cos() * s * d, u * d, a.sin() * s * d, r()].map(|v| v as f32)
        })
        .collect()
}

/// The worker knotlet: the prototype's `TREF`, every third point.
fn trefoil() -> Vec<[f32; 4]> {
    (0..60)
        .step_by(3)
        .map(|i| {
            let t = f64::from(i) / 60.0 * TAU;
            let p = [t.sin() + 2.0 * (2.0 * t).sin(), t.cos() - 2.0 * (2.0 * t).cos(), -(3.0 * t).sin(), 0.0];
            p.map(|v| v as f32)
        })
        .collect()
}

/// Every globe dot in shader order: session shell and core, then worker shell and core.
/// Also returns the session and worker shell counts.
fn points() -> (Vec<[f32; 4]>, usize, usize) {
    let s_shell = lat_globe(22, 64.0, 11);
    let w_shell = lat_globe(13, 34.0, 12);
    let (ns, nw) = (s_shell.len(), w_shell.len());
    let mut out = s_shell;
    out.extend(ball(S_CORE, 21, 1.8));
    out.extend(w_shell);
    out.extend(trefoil());
    (out, ns, nw)
}

/// `globes.wgsl` behind its palette and point counts.
fn shader(s_shell: usize, w_shell: usize) -> String {
    use erebus_theme as theme;
    use std::fmt::Write;
    let mut s = String::new();
    for (name, c) in [("EMBER", theme::EMBER), ("HOT", theme::HOT)] {
        let [r, g, b] = c.srgb_f32();
        let _ = writeln!(s, "const {name} = vec3f({r:.6}, {g:.6}, {b:.6});");
    }
    for (name, n) in [("S_SHELL", s_shell), ("S_CORE", S_CORE), ("W_SHELL", w_shell), ("W_CORE", W_CORE)] {
        let _ = writeln!(s, "const {name} = {n}u;");
    }
    s + include_str!("globes.wgsl")
}

/// The prototype's dot unit at this target size.
fn dot_scale(w: u32, h: u32) -> f32 {
    let u = (h as f32 / 1440.0).min(w as f32 / 3400.0).clamp(0.2, 2.0);
    (1.6 * u).max(1.0)
}

/// The globe pipeline and its buffers. Each frame writes the uniform if it changed; globes
/// are written only when they change.
pub struct Painter {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    globes: wgpu::Buffer,
    bind: wgpu::BindGroup,
    /// Dots per session and per worker globe.
    per: (u32, u32),
    last_uniform: Option<Uniform>,
    /// Globes as written: sessions first, then workers.
    last: Vec<GlobeInst>,
    /// This frame's globes in that order, kept to reuse its allocation.
    next: Vec<GlobeInst>,
    sessions: u32,
    workers: u32,
    #[cfg(test)]
    uploads: u32,
}

impl Painter {
    /// `blend` is the renderer's additive blend (src One, dst One; alpha Zero/One).
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, blend: wgpu::BlendState) -> Self {
        use wgpu::util::DeviceExt;
        let (pts, s_shell, w_shell) = points();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("globes.wgsl"),
            source: wgpu::ShaderSource::Wgsl(shader(s_shell, w_shell).into()),
        });
        let buffer = |label, size: usize, usage| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size as u64, usage, mapped_at_creation: false })
        };
        let copy = wgpu::BufferUsages::COPY_DST;
        let uniform = buffer("globes", size_of::<Uniform>(), wgpu::BufferUsages::UNIFORM | copy);
        let globes = buffer("globe instances", size_of::<GlobeInst>() * MAX_GLOBES, wgpu::BufferUsages::STORAGE | copy);
        let points = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globe points"),
            contents: bytemuck::cast_slice(&pts),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let storage = wgpu::BufferBindingType::Storage { read_only: true };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globes"),
            entries: &[entry(0, wgpu::BufferBindingType::Uniform), entry(1, storage), entry(2, storage)],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globes"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: points.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: globes.as_entire_binding() },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("globes"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("globes"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("globe_vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("globe_fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Painter {
            pipeline,
            uniform,
            globes,
            bind,
            per: ((s_shell + S_CORE) as u32, (w_shell + W_CORE) as u32),
            last_uniform: None,
            last: Vec::new(),
            next: Vec::new(),
            sessions: 0,
            workers: 0,
            #[cfg(test)]
            uploads: 0,
        }
    }

    /// Writes the small per-frame uniform (target size, dot scale, clock seconds, lod 0..1 where
    /// 0 drops every other dot as the knot does in Glance) and the instances only when they changed.
    ///
    /// Globes past [`MAX_GLOBES`] are dropped.
    pub fn prepare(&mut self, queue: &wgpu::Queue, size: (u32, u32), clock: f32, lod: f32, globes: &[GlobeInst]) {
        let globes = &globes[..globes.len().min(MAX_GLOBES)];
        self.next.clear();
        self.next.extend(globes.iter().filter(|g| !g.worker()));
        let sessions = self.next.len() as u32;
        self.next.extend(globes.iter().filter(|g| g.worker()));
        if self.next != self.last {
            std::mem::swap(&mut self.next, &mut self.last);
            if !self.last.is_empty() {
                queue.write_buffer(&self.globes, 0, bytemuck::cast_slice(&self.last));
            }
            self.sessions = sessions;
            self.workers = self.last.len() as u32 - sessions;
            #[cfg(test)]
            {
                self.uploads += 1;
            }
        }

        let (w, h) = size;
        let u = Uniform {
            size: [w as f32, h as f32, dot_scale(w, h), (clock * 3.0).rem_euclid(TAU as f32)],
            state: [lod.clamp(0.0, 1.0), self.sessions as f32, self.workers as f32, 0.0],
        };
        if self.last_uniform != Some(u) {
            queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&u));
            self.last_uniform = Some(u);
        }
    }

    /// Draws every prepared globe in one instanced draw into an open render pass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass) {
        let n = self.sessions * self.per.0 + self.workers * self.per.1;
        if n > 0 {
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_pipeline(&self.pipeline);
            pass.draw(0..4, 0..n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::Gpu;

    #[test]
    fn point_counts() {
        let (pts, s, w) = points();
        // Counts from the prototype's own latGlobe.
        assert_eq!((s, w), (894, 282));
        assert_eq!(pts.len(), s + S_CORE + w + W_CORE);
    }

    #[test]
    fn matches_prototype() {
        // First and last session dots and the first core dot, as the prototype computes them.
        let s = lat_globe(22, 64.0, 11);
        let close = |a: [f32; 4], b: [f64; 4]| a.iter().zip(b).all(|(x, y)| (f64::from(*x) - y).abs() < 1e-6);
        assert!(close(s[0], [-0.14193784487040317, -0.9898214418809327, -0.010351878394603897, 0.0]), "{:?}", s[0]);
        assert!(close(s[893], [-0.14002289189160724, 0.9898214418809327, 0.02543821807954648, 0.0]), "{:?}", s[893]);
        let w = lat_globe(13, 34.0, 12);
        assert!(close(w[0], [-0.05681837504549308, -0.970941817426052, 0.23247292192981267, 0.0]), "{:?}", w[0]);
        let c = ball(S_CORE, 21, 1.8);
        let want = [-0.09670946770251118, -0.013985395211394297, 0.033431085257595125, 0.38238615170121193];
        assert!(close(c[0], want), "{:?}", c[0]);
    }

    #[test]
    fn shells_are_unit_and_balls_inside() {
        let (pts, s, w) = points();
        let len = |p: &[f32; 4]| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        let worker_shell = &pts[s + S_CORE..s + S_CORE + w];
        for p in pts[..s].iter().chain(worker_shell) {
            assert!((len(p) - 1.0).abs() < 1e-5, "{p:?}");
        }
        for p in &pts[s..s + S_CORE] {
            assert!(len(p) <= 1.0 + 1e-6, "{p:?}");
            assert!((0.0..1.0).contains(&p[3]));
        }
    }

    #[test]
    fn deterministic() {
        assert_eq!(points().0, points().0);
        assert_ne!(lat_globe(13, 34.0, 12), lat_globe(13, 34.0, 13));
    }

    #[test]
    fn shader_validates() {
        use wgpu::naga::{front::wgsl, valid};
        let (_, s, w) = points();
        let src = shader(s, w);
        let module = wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::empty())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
    }

    #[test]
    fn sizes_match_wgsl() {
        assert_eq!(size_of::<GlobeInst>(), 4 * 16);
        assert_eq!(size_of::<Uniform>(), 2 * 16);
    }

    // GPU tests below need an adapter.

    const ADDITIVE: wgpu::BlendState = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    };
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    fn ember() -> [f32; 4] {
        let [r, g, b] = erebus_theme::EMBER.srgb_f32();
        [r, g, b, 1.0]
    }

    fn globe(kind: f32, x: f32, y: f32, r: f32, form: f32, seed: f32) -> GlobeInst {
        GlobeInst { pos: [x, y, r, 0.7], color: ember(), form: [kind, form, 0.16, seed], look: [0.0; 4] }
    }

    struct Target {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        size: (u32, u32),
    }

    impl Target {
        fn new(gpu: &Gpu, (w, h): (u32, u32)) -> Self {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("globes test"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            Target { texture, view, size: (w, h) }
        }

        fn pass<'a>(&self, encoder: &'a mut wgpu::CommandEncoder, ts: Option<wgpu::RenderPassTimestampWrites<'a>>) -> wgpu::RenderPass<'a> {
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("globes test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: ts,
                occlusion_query_set: None,
                multiview_mask: None,
            })
        }

        /// Draws the prepared globes over black and reads back packed RGB.
        fn render(&self, gpu: &Gpu, painter: &Painter) -> Vec<u8> {
            let (w, h) = self.size;
            let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("globes readback"),
                size: u64::from(row) * u64::from(h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            {
                let mut pass = self.pass(&mut encoder, None);
                painter.draw(&mut pass);
            }
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            gpu.queue.submit([encoder.finish()]);
            let slice = readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let data = slice.get_mapped_range().expect("mapped");
            let mut rgb = Vec::with_capacity((w * h * 3) as usize);
            for y in 0..h as usize {
                let start = y * row as usize;
                for px in data[start..start + w as usize * 4].as_chunks::<4>().0 {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            rgb
        }
    }

    /// Writes a PNG into `$EREBUS_TEST_PNG` when it is set, for looking at.
    fn save(name: &str, (w, h): (u32, u32), rgb: &[u8]) {
        let Some(dir) = std::env::var_os("EREBUS_TEST_PNG") else { return };
        let path = std::path::Path::new(&dir).join(name);
        let file = std::fs::File::create(&path).expect("png file");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().expect("png").write_image_data(rgb).expect("png");
    }

    /// Brightest channel in a box, and how many pixels in it are lit.
    fn region(rgb: &[u8], w: u32, [x0, y0, x1, y1]: [u32; 4]) -> (u8, usize) {
        let mut max = 0;
        let mut lit = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * w + x) * 3) as usize;
                let m = rgb[i..i + 3].iter().copied().max().unwrap_or(0);
                max = max.max(m);
                lit += usize::from(m > 24);
            }
        }
        (max, lit)
    }

    #[test]
    fn renders_whole_globes_and_hides_dust() {
        let (gpu, _) = Gpu::new(None).expect("gpu");
        let size = (512, 256);
        let target = Target::new(&gpu, size);
        let mut painter = Painter::new(&gpu.device, FORMAT, ADDITIVE);
        let globes = [globe(1.0, 384.0, 128.0, 40.0, 1.0, 3.0), globe(0.0, 128.0, 128.0, 64.0, 1.0, 1.0), globe(1.0, 256.0, 200.0, 40.0, 0.0, 2.0)];
        painter.prepare(&gpu.queue, size, 1.25, 1.0, &globes);
        assert_eq!(painter.uploads, 1);
        assert_eq!((painter.sessions, painter.workers), (1, 2));
        painter.prepare(&gpu.queue, size, 1.25, 1.0, &globes);
        assert_eq!(painter.uploads, 1, "identical globes are not written again");
        let rgb = target.render(&gpu, &painter);
        save("globes-test.png", size, &rgb);

        let (session, session_lit) = region(&rgb, 512, [64, 64, 192, 192]);
        let (worker, worker_lit) = region(&rgb, 512, [344, 88, 424, 168]);
        assert!(session > 200 && session_lit > 400, "session globe: max {session}, lit {session_lit}");
        assert!(worker > 150 && worker_lit > 100, "worker globe: max {worker}, lit {worker_lit}");
        assert_eq!(region(&rgb, 512, [200, 140, 312, 256]), (0, 0), "form 0 worker is invisible");
        assert_eq!(region(&rgb, 512, [200, 0, 330, 60]), (0, 0), "far from the globes is black");
        assert_eq!(region(&rgb, 512, [450, 0, 512, 256]), (0, 0), "far right is black");
    }

    #[test]
    fn changes_are_written() {
        let (gpu, _) = Gpu::new(None).expect("gpu");
        let mut painter = Painter::new(&gpu.device, FORMAT, ADDITIVE);
        let mut g = [globe(0.0, 100.0, 100.0, 64.0, 1.0, 1.0)];
        painter.prepare(&gpu.queue, (512, 256), 0.0, 1.0, &g);
        g[0].pos[3] += 0.01;
        painter.prepare(&gpu.queue, (512, 256), 0.0, 1.0, &g);
        assert_eq!(painter.uploads, 2);
        painter.prepare(&gpu.queue, (512, 256), 0.0, 1.0, &[]);
        assert_eq!(painter.uploads, 3);
        assert_eq!((painter.sessions, painter.workers), (0, 0));
        painter.prepare(&gpu.queue, (512, 256), 0.0, 1.0, &[]);
        assert_eq!(painter.uploads, 3);
    }

    /// Form 0, .25, .5, .75 and 1 in a row, far enough apart that dust stays in its column:
    /// sessions above, workers below. Light grows with form.
    #[test]
    fn form_sweep_condenses() {
        let (gpu, _) = Gpu::new(None).expect("gpu");
        let size = (1600, 440);
        let target = Target::new(&gpu, size);
        let mut painter = Painter::new(&gpu.device, FORMAT, ADDITIVE);
        let forms = [0.0, 0.25, 0.5, 0.75, 1.0];
        let mut globes = Vec::new();
        for (i, &f) in forms.iter().enumerate() {
            let x = 160.0 + 320.0 * i as f32;
            globes.push(globe(0.0, x, 140.0, 64.0, f, 7.0));
            globes.push(globe(1.0, x, 330.0, 40.0, f, 8.0));
        }
        painter.prepare(&gpu.queue, size, 0.5, 1.0, &globes);
        let rgb = target.render(&gpu, &painter);
        save("globes-form.png", size, &rgb);

        // Per column: all light, and light within 1.05 radii of the column's globes.
        let (mut total, mut inside) = ([0u64; 5], [0u64; 5]);
        for y in 0..440u32 {
            for x in 0..1600u32 {
                let i = (x / 320) as usize;
                let p = ((y * 1600 + x) * 3) as usize;
                let v = rgb[p..p + 3].iter().map(|&c| u64::from(c)).sum::<u64>();
                let dx = x as f32 - (160.0 + 320.0 * i as f32);
                let near = dx.hypot(y as f32 - 140.0) < 64.0 * 1.05 || dx.hypot(y as f32 - 330.0) < 40.0 * 1.05;
                total[i] += v;
                inside[i] += if near { v } else { 0 };
            }
        }
        let share: Vec<f64> = (1..5).map(|i| inside[i] as f64 / total[i] as f64).collect();
        println!("form sweep: light {total:?}, share inside the globes {share:.3?}");
        assert_eq!(total[0], 0, "form 0 is invisible");
        assert!(total[1] < total[2] && total[2] < total[3], "light grows as dust gathers: {total:?}");
        assert!(total[1] * 3 < total[4], "quarter form is faint: {total:?}");
        assert!(share.windows(2).all(|w| w[0] < w[1]), "dust gathers into the globes: {share:?}");
        assert!(share[3] > 0.999, "a whole globe has no dust: {share:?}");
    }

    /// GPU cost at 5120 × 1440: a clear alone, 3 sessions and 3 workers, and 32 of each.
    /// Rotation changes every frame, so instances upload every frame. Run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost() {
        let (gpu, _) = Gpu::new(None).expect("gpu");
        let size = (5120, 1440);
        let target = Target::new(&gpu, size);
        let mut painter = Painter::new(&gpu.device, FORMAT, ADDITIVE);
        let frame = |i: u32, each: usize| -> Vec<GlobeInst> {
            let rot = i as f32 * 0.01;
            let mut g = Vec::new();
            for k in 0..each {
                let (x, y) = (2600.0 + 260.0 * (k / 4) as f32, 200.0 + 340.0 * (k % 4) as f32);
                g.push(GlobeInst { pos: [x, y, 64.0, rot], color: ember(), form: [0.0, 1.0, 0.16, k as f32], look: [0.0; 4] });
                g.push(GlobeInst { pos: [x + 130.0, y + 170.0, 40.0, rot], color: ember(), form: [1.0, 1.0, 0.0, 100.0 + k as f32], look: [0.0; 4] });
            }
            g
        };

        const FRAMES: u32 = 1000;
        let timed = gpu.device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        // The first run warms the GPU clocks.
        for each in [3, 0, 3, 32] {
            let queries = timed.then(|| {
                gpu.device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("globe times"), ty: wgpu::QueryType::Timestamp, count: 2 * FRAMES })
            });
            let start = std::time::Instant::now();
            for i in 0..FRAMES {
                painter.prepare(&gpu.queue, size, i as f32 / 60.0, 1.0, &frame(i, each));
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                {
                    let ts = queries.as_ref().map(|q| wgpu::RenderPassTimestampWrites {
                        query_set: q,
                        beginning_of_pass_write_index: Some(2 * i),
                        end_of_pass_write_index: Some(2 * i + 1),
                    });
                    let mut pass = target.pass(&mut encoder, ts);
                    painter.draw(&mut pass);
                }
                gpu.queue.submit([encoder.finish()]);
            }
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let wall = start.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES);
            let dots = painter.sessions * painter.per.0 + painter.workers * painter.per.1;
            print!("{each} sessions + {each} workers ({dots} dots): wall {wall:.4} ms/frame");

            let Some(q) = queries else {
                println!(", no TIMESTAMP_QUERY");
                continue;
            };
            let bytes = 8 * 2 * u64::from(FRAMES);
            let buf = |usage| gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: bytes, usage, mapped_at_creation: false });
            let resolve = buf(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC);
            let readback = buf(wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ);
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            encoder.resolve_query_set(&q, 0..2 * FRAMES, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, bytes);
            gpu.queue.submit([encoder.finish()]);
            let slice = readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let ticks: Vec<u64> = slice.get_mapped_range().expect("mapped").as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b)).collect();
            let period = f64::from(gpu.queue.get_timestamp_period()) / 1e6;
            let mut ms: Vec<f64> = ticks.as_chunks::<2>().0.iter().map(|[a, b]| b.saturating_sub(*a) as f64 * period).collect();
            ms.sort_by(f64::total_cmp);
            let mean = ms.iter().sum::<f64>() / ms.len() as f64;
            println!(", gpu pass (clear + globes) mean {mean:.4} ms, median {:.4} ms, p99 {:.4} ms", ms[ms.len() / 2], ms[ms.len() * 99 / 100]);
        }

        painter.prepare(&gpu.queue, size, 1.0, 1.0, &frame(70, 3));
        // A crop at full size, for looking at.
        let rgb = target.render(&gpu, &painter);
        let (x0, y0, cw, ch) = (2500u32, 100u32, 400u32, 1250u32);
        let mut crop = Vec::with_capacity((cw * ch * 3) as usize);
        for y in y0..y0 + ch {
            let s = ((y * 5120 + x0) * 3) as usize;
            crop.extend_from_slice(&rgb[s..s + cw as usize * 3]);
        }
        save("globes-5120.png", (cw, ch), &crop);
    }
}
