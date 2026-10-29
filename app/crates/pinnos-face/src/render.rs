//! One render pass: ground, shapes, then text. The window and screenshot mode both draw
//! through [`Renderer::draw`], so their pixels match.
//!
//! Sizes follow the prototype's `resize()` in `v3/concept-loom4.html`: at 5120×1440 one
//! prototype CSS pixel is 1.45 physical pixels.

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};
use crate::gpu::Gpu;
use pinnos_theme::{self as theme, Rgb};

/// Every frame renders into this format family: unorm targets holding sRGB values.
pub fn is_target_format(f: wgpu::TextureFormat) -> bool {
    matches!(f, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm)
}

/// Close button state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    Rest,
    Hover,
    Press,
}

/// What the frame shows, independent of the target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub palette: bool,
    /// Link bead state: 0 down, 1 up, eased in between.
    pub link: f32,
    pub close: Close,
}

/// Window chrome in physical pixels, shared by drawing and hit testing.
#[derive(Clone, Copy, Debug)]
pub struct Chrome {
    /// Close hit box `[x0, y0, x1, y1]`. It runs out to the top and right edges, so a
    /// cursor thrown into the corner lands on it.
    pub close_hit: [f32; 4],
    /// Height of the top rail; the drag strip in windowed mode.
    pub rail: f32,
}

pub fn chrome(w: u32, h: u32, scale: f64) -> Chrome {
    let l = Layout::new(w, h, scale);
    let (cx, cy, r) = l.close();
    let m = 6.0 * l.k;
    Chrome { close_hit: [cx - r - m, 0.0, l.w, cy + r + m], rail: 44.0 * l.k }
}

/// Darkening at the far corners.
const VIGNETTE: f32 = 0.14;
const MAX_SHAPES: usize = 64;
const KIND_RECT: f32 = 0.0;
const KIND_BEAD: f32 = 1.0;
const KIND_CLOSE: f32 = 2.0;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    size: [f32; 2],
    centre: [f32; 2],
    radius: f32,
    vignette: f32,
    _pad: [f32; 2],
    ink: [f32; 4],
    light: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Shape {
    rect: [f32; 4],
    color: [f32; 4],
    color2: [f32; 4],
    params: [f32; 4],
}

fn rgba(c: Rgb, a: f32) -> [f32; 4] {
    let [r, g, b] = c.srgb_f32();
    [r, g, b, a]
}

fn rect(x: f32, y: f32, w: f32, h: f32, c: Rgb) -> Shape {
    Shape { rect: [x, y, w, h], color: rgba(c, 1.0), color2: [0.0; 4], params: [KIND_RECT, 0.0, 0.0, 0.0] }
}

/// Ground-light peak above ink: mostly raised panel, a trace of ember. Matches the
/// prototype's dormant backdrop, which peaks near (24, 21, 17).
fn ground_light() -> [f32; 4] {
    let ink = theme::INK.srgb_f32();
    let slab = theme::SLAB_2.srgb_f32();
    let ember = theme::EMBER.srgb_f32();
    let ch = |i: usize| 1.25 * (slab[i] - ink[i]) + 0.015 * (ember[i] - ink[i]);
    [ch(0), ch(1), ch(2), 0.0]
}

/// Physical-pixel layout for one target size.
#[derive(Clone, Copy)]
struct Layout {
    /// Physical pixels per prototype CSS pixel.
    k: f32,
    pad: f32,
    hair: f32,
    w: f32,
    h: f32,
}

impl Layout {
    fn new(w: u32, h: u32, scale: f64) -> Self {
        let s = scale as f32;
        let (lw, lh) = (w as f32 / s, h as f32 / s);
        let u = (lh / 1440.0).min(lw / 3400.0).clamp(0.2, 2.0);
        let kf = (1.45 * u).max(1.0);
        Layout {
            k: kf * s,
            pad: (14.0 * kf).max(lw * 0.012) * s,
            hair: s.round().max(1.0),
            w: w as f32,
            h: h as f32,
        }
    }

    /// Bead centre and radius: 14 px across at 5120, centred in the 44 px top rail.
    fn bead(&self) -> (f32, f32, f32) {
        let r = 4.8 * self.k;
        (self.pad + r, 22.0 * self.k, r)
    }

    /// Close button centre and ring radius: 39 px across at 5120, centred in the rail and
    /// flush with the right padding.
    fn close(&self) -> (f32, f32, f32) {
        let r = 13.5 * self.k;
        (self.w - self.pad - r, 22.0 * self.k, r)
    }

    /// Strip padding from its top edge to the swatch top and the first line's cap top.
    fn strip_inset(&self) -> f32 {
        18.0 * self.k
    }

    fn strip_cell(&self, i: usize) -> (f32, f32) {
        let cell_w = (self.w - 2.0 * self.pad) / theme::TOKENS.len() as f32;
        (self.pad + i as f32 * cell_w, cell_w)
    }

    fn swatch(&self) -> f32 {
        (26.0 * self.k).round()
    }
}

fn shapes(l: &Layout, view: View, strip_h: f32) -> Vec<Shape> {
    let mut out = Vec::with_capacity(MAX_SHAPES);

    if view.palette {
        let y0 = l.h - strip_h;
        out.push(rect(0.0, y0, l.w, l.h - y0, theme::SLAB));
        out.push(rect(0.0, y0, l.w, l.hair, theme::RULE));
        let sw = l.swatch();
        for (i, tok) in theme::TOKENS.iter().enumerate() {
            let (x, _) = l.strip_cell(i);
            let (x, y) = (x.round(), (y0 + l.strip_inset()).round());
            out.push(rect(x, y, sw, sw, theme::RULE_2));
            out.push(rect(x + l.hair, y + l.hair, sw - 2.0 * l.hair, sw - 2.0 * l.hair, tok.rgb));
        }
    }

    let (cx, cy, r) = l.bead();
    let sigma = 2.2 * l.k;
    let half = r + 8.0 * sigma;
    out.push(Shape {
        rect: [cx - half, cy - half, 2.0 * half, 2.0 * half],
        color: rgba(theme::DIM, 1.0),
        color2: rgba(theme::EMBER, 1.0),
        params: [KIND_BEAD, r, view.link.clamp(0.0, 1.0), sigma],
    });

    out.push(close_shape(l, view.close));
    out
}

/// Fine-tick ring around a braided ×. Brass at rest, ember with a glow on hover, a
/// darker ember while pressed.
fn close_shape(l: &Layout, state: Close) -> Shape {
    let (cx, cy, r) = l.close();
    let half = r * 2.2;
    let pressed = shade(theme::EMBER, 0.72);
    let (ticks, cross, glow) = match state {
        Close::Rest => (rgba(theme::BRASS, 0.85), rgba(theme::BONE_3, 1.0), 0.0),
        Close::Hover => (rgba(theme::EMBER, 0.9), rgba(theme::EMBER, 1.0), 1.0),
        Close::Press => (pressed, pressed, 0.35),
    };
    Shape {
        rect: [cx - half, cy - half, 2.0 * half, 2.0 * half],
        color: ticks,
        color2: cross,
        params: [KIND_CLOSE, r, glow, (0.9 * l.k).max(1.0)],
    }
}

fn shade(c: Rgb, f: f32) -> [f32; 4] {
    let [r, g, b] = c.srgb_f32();
    [r * f, g * f, b * f, 1.0]
}

struct Placed {
    buf: Buffer,
    left: f32,
    top: f32,
    color: Rgb,
    bounds: TextBounds,
}

impl Placed {
    fn area(&self) -> TextArea<'_> {
        let Rgb(r, g, b) = self.color;
        TextArea {
            buffer: &self.buf,
            left: self.left,
            top: self.top,
            scale: 1.0,
            bounds: self.bounds,
            default_color: Color::rgb(r, g, b),
            custom_glyphs: &[],
        }
    }
}

struct Text {
    fonts: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    mono: &'static str,
    /// Target size and scale the buffers were laid out for.
    key: Option<(u32, u32, u64)>,
    /// Palette strip height, sized to its text.
    strip_h: f32,
    wordmark: Option<Placed>,
    cells: Vec<Placed>,
}

const UI: &str = "Segoe UI";

/// Loads only the faces the face uses; a full system scan costs tens of MB and ~100 ms.
fn font_system() -> (FontSystem, &'static str) {
    let mut db = glyphon::fontdb::Database::new();
    let dir = std::path::Path::new(r"C:\Windows\Fonts");
    for f in ["segoeui.ttf", "seguisb.ttf", "CascadiaMono.ttf", "consola.ttf"] {
        let _ = db.load_font_file(dir.join(f));
    }
    fn has(db: &glyphon::fontdb::Database, name: &str) -> bool {
        db.faces().any(|f| f.families.iter().any(|(n, _)| n == name))
    }
    if !has(&db, UI) {
        log::warn!("Segoe UI not found; loading all system fonts");
        db.load_system_fonts();
    }
    let mono = if has(&db, "Cascadia Mono") { "Cascadia Mono" } else { "Consolas" };
    (FontSystem::new_with_locale_and_db("en-US".into(), db), mono)
}

fn line(
    fonts: &mut FontSystem,
    text: &str,
    size: f32,
    family: &str,
    weight: Weight,
    tracking: f32,
    wrap_w: Option<f32>,
) -> Buffer {
    let mut b = Buffer::new(fonts, Metrics::new(size, (size * 1.3).round()));
    b.set_wrap(if wrap_w.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
    b.set_size(wrap_w, None);
    let attrs = Attrs::new().family(Family::Name(family)).weight(weight).letter_spacing(tracking);
    b.set_text(text, &attrs, Shaping::Advanced, None);
    b.shape_until_scroll(fonts, false);
    b
}

fn baseline(b: &Buffer) -> f32 {
    b.layout_runs().next().map_or(0.0, |r| r.line_y)
}

fn height(b: &Buffer) -> f32 {
    b.layout_runs().last().map_or(0.0, |r| r.line_top + r.line_height)
}

fn bounds(x0: f32, y0: f32, x1: f32, y1: f32) -> TextBounds {
    TextBounds { left: x0.floor() as i32, top: y0.floor() as i32, right: x1.ceil() as i32, bottom: y1.ceil() as i32 }
}

impl Text {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let (fonts, mono) = font_system();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::with_color_mode(device, queue, &cache, format, ColorMode::Web);
        let renderer = TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        Text {
            fonts,
            swash: SwashCache::new(),
            viewport,
            atlas,
            renderer,
            mono,
            key: None,
            strip_h: 0.0,
            wordmark: None,
            cells: Vec::new(),
        }
    }

    fn layout(&mut self, l: &Layout, key: (u32, u32, u64)) {
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        let k = l.k;
        let fonts = &mut self.fonts;

        // Wordmark: prototype `.brand b`, 600 13px, tracking .06em, caps centred on the bead.
        let (bx, by, br) = l.bead();
        let size = 13.0 * k;
        let buf = line(fonts, "PinnOS", size, UI, Weight::SEMIBOLD, 0.06, None);
        let left = (bx + br + 8.0 * k).round();
        let top = (by - baseline(&buf) + 0.35 * size).round();
        let b = bounds(left, top, l.w, top + height(&buf));
        self.wordmark = Some(Placed { buf, left, top, color: theme::BONE_2, bounds: b });

        // Palette strip: per token, name / hex / role beside its swatch. Shaped first, so
        // the strip height can follow the tallest column.
        self.cells.clear();
        let sw = l.swatch();
        let name_size = 13.5 * k;
        let mut columns = Vec::with_capacity(theme::TOKENS.len());
        for (i, tok) in theme::TOKENS.iter().enumerate() {
            let (cx, cw) = l.strip_cell(i);
            let left = (cx.round() + sw + 10.0 * k).round();
            let right = cx + cw - 10.0 * k;
            let col_w = (right - left).max(1.0);
            let Rgb(r, g, b) = tok.rgb;
            let hex = format!("#{r:02x}{g:02x}{b:02x}");
            let rows = [
                (line(fonts, tok.name, name_size, UI, Weight::SEMIBOLD, 0.0, None), theme::BONE),
                (line(fonts, &hex, 12.0 * k, self.mono, Weight::NORMAL, 0.0, None), theme::BONE_3),
                (line(fonts, tok.role, 12.0 * k, UI, Weight::NORMAL, 0.0, Some(col_w)), theme::BONE_2),
            ];
            // Space inside the first line above its cap top.
            let lead = baseline(&rows[0].0) - 0.7 * name_size;
            let tall = rows.iter().map(|(b, _)| height(b)).sum::<f32>() - lead;
            columns.push((left, right, lead, tall, rows));
        }

        let content = columns.iter().map(|c| c.3).fold(0.0, f32::max);
        self.strip_h = (l.strip_inset() + content + 14.0 * k).round();
        let y0 = l.h - self.strip_h;
        for (left, right, lead, _, rows) in columns {
            // First line's cap top sits level with the swatch top.
            let mut top = (y0 + l.strip_inset() - lead).round();
            for (buf, color) in rows {
                let h = height(&buf);
                let b = bounds(left, top, right, top + h);
                self.cells.push(Placed { buf, left, top, color, bounds: b });
                top += h;
            }
        }
    }

    fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, palette: bool) {
        self.viewport.update(queue, Resolution { width: w, height: h });
        let mut areas: Vec<TextArea> = self.wordmark.iter().map(Placed::area).collect();
        if palette {
            areas.extend(self.cells.iter().map(Placed::area));
        }
        if let Err(e) = self.renderer.prepare(
            device,
            queue,
            &mut self.fonts,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash,
        ) {
            log::error!("text prepare: {e}");
        }
    }
}

pub struct Renderer {
    ground: wgpu::RenderPipeline,
    shape: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    shapes: wgpu::Buffer,
    bind: wgpu::BindGroup,
    text: Text,
    /// What the uploaded buffers and prepared text show, with the shape count.
    prepared: Option<(View, u32, u32, u64, u32)>,
    /// The atlas was trimmed after the last prepare.
    trimmed: bool,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::include_wgsl!("face.wgsl"));
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shapes = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shapes"),
            size: (size_of::<Shape>() * MAX_SHAPES) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("face"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let pipeline = |label, vs, fs, buffers: &[Option<wgpu::VertexBufferLayout>], blend, topology| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let ground = pipeline("ground", "ground_vs", "ground_fs", &[], None, wgpu::PrimitiveTopology::TriangleList);
        let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4];
        let shape_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<Shape>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &attrs,
        };
        let shape = pipeline(
            "shapes",
            "shape_vs",
            "shape_fs",
            &[Some(shape_layout)],
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            wgpu::PrimitiveTopology::TriangleStrip,
        );

        Renderer { ground, shape, globals, shapes, bind, text: Text::new(device, queue, format), prepared: None, trimmed: true }
    }

    /// Uploads globals and shapes and prepares text. Returns the shape count.
    fn prepare(&mut self, gpu: &Gpu, (w, h): (u32, u32), scale: f64, view: View) -> u32 {
        let (device, queue) = (&gpu.device, &gpu.queue);
        let l = Layout::new(w, h, scale);
        let (wf, hf) = (w as f32, h as f32);
        let globals = Globals {
            size: [wf, hf],
            centre: [wf * 0.5, hf * 0.5],
            radius: (wf * 0.32).max(hf * 0.7),
            vignette: VIGNETTE,
            _pad: [0.0; 2],
            ink: rgba(theme::INK, 1.0),
            light: ground_light(),
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        self.text.layout(&l, (w, h, scale.to_bits()));
        let shapes = shapes(&l, view, self.text.strip_h);
        queue.write_buffer(&self.shapes, 0, bytemuck::cast_slice(&shapes));

        self.text.prepare(device, queue, w, h, view.palette);
        shapes.len() as u32
    }

    /// Records one frame. A frame that repeats the last one reuses its uploads and
    /// prepared text.
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        (w, h): (u32, u32),
        scale: f64,
        view: View,
    ) {
        let key = (view, w, h, scale.to_bits());
        let count = match self.prepared {
            Some((v, pw, ph, ps, n)) if (v, pw, ph, ps) == key => n,
            _ => {
                let n = self.prepare(gpu, (w, h), scale, view);
                self.prepared = Some((view, w, h, scale.to_bits(), n));
                self.trimmed = false;
                n
            }
        };

        let [r, g, b] = theme::INK.srgb_f32();
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("face"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_pipeline(&self.ground);
        pass.draw(0..3, 0..1);
        pass.set_pipeline(&self.shape);
        pass.set_vertex_buffer(0, self.shapes.slice(..));
        pass.draw(0..4, 0..count);
        if let Err(e) = self.text.renderer.render(&self.text.atlas, &self.text.viewport, &mut pass) {
            log::error!("text render: {e}");
        }
    }

    /// Frees glyphs not used by the last prepared frame. A repeated frame skips this:
    /// glyphs are marked used only when text is prepared.
    pub fn trim(&mut self) {
        if !self.trimmed {
            self.text.atlas.trim();
            self.trimmed = true;
        }
    }
}
