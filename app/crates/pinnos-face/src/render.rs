//! One render pass: ground, knot, shapes, then text. The window and screenshot mode both
//! draw through [`Renderer::draw`], so their pixels match.
//!
//! Sizes follow the prototype's `resize()` in `v3/concept-loom4.html`: at 5120×1440 one
//! prototype CSS pixel is 1.45 physical pixels.
//!
//! A frame uploads the ground and knot uniforms and the shape list, all under 2 KB. The
//! file glyph buffer is written only when the file list changes. Text is laid out and
//! prepared only when its words, size or fade change.

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};
use crate::files::{self, FileInst};
use crate::gpu::Gpu;
use crate::knot;
use crate::vitals;
use pinnos_theme::{self as theme, Rgb};
use wgpu::util::DeviceExt;

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

/// Everything one frame draws.
#[derive(Clone, Copy)]
pub struct Scene<'a> {
    pub view: View,
    pub knot: knot::Uniform,
    /// Wordmark crumbs after "PinnOS", e.g. `["dev", "PinnOS"]`.
    pub crumbs: &'a [&'a str],
    /// The knot's hover label and its fade, 0..1.
    pub label: Option<&'a str>,
    pub label_alpha: f32,
    /// File glyphs, and the generation that names their contents.
    pub files: &'a [FileInst],
    pub files_gen: u64,
    /// A hovered file's path, the glyph centre it labels, and its fade.
    pub file_label: Option<(&'a str, [f32; 2])>,
    pub file_label_alpha: f32,
    /// `+N` for files the core counts but does not list.
    pub more: Option<&'a str>,
    /// The vitals stack, when there are readings.
    pub vitals: Option<&'a vitals::Frame>,
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
const MAX_SHAPES: usize = 96;
const KIND_RECT: f32 = 0.0;
const KIND_BEAD: f32 = 1.0;
const KIND_CLOSE: f32 = 2.0;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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

/// Ground-light peak above ink. Dormant: mostly raised panel with a trace of ember, the
/// prototype's dormant backdrop peaking near (24, 21, 17). Live: its warm backdrop,
/// rgba(104, 58, 16, .5) over ink, peaking near (57, 34, 12). `live` blends the two.
fn ground_light(live: f32) -> [f32; 4] {
    let ink = theme::INK.srgb_f32();
    let slab = theme::SLAB_2.srgb_f32();
    let ember = theme::EMBER.srgb_f32();
    let warm = [104.0 / 255.0, 58.0 / 255.0, 16.0 / 255.0];
    let ch = |i: usize| {
        let dormant = 1.25 * (slab[i] - ink[i]) + 0.015 * (ember[i] - ink[i]);
        let lit = 0.5 * (warm[i] - ink[i]);
        dormant + (lit - dormant) * live
    };
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

fn rgba_rect(x: f32, y: f32, w: f32, h: f32, c: Rgb, a: f32) -> Shape {
    let [r, g, b] = c.srgb_f32();
    Shape { rect: [x, y, w, h], color: [r * a, g * a, b * a, a], color2: [0.0; 4], params: [KIND_RECT, 0.0, 0.0, 0.0] }
}

/// `labels` are hover label boxes `[x, y, w, h]` and their fades. `extra` comes first: the
/// vitals rules and tooltip box.
fn shapes(l: &Layout, view: View, strip_h: f32, labels: &[([f32; 4], f32)], extra: &[Shape]) -> Vec<Shape> {
    let mut out = Vec::with_capacity(MAX_SHAPES);
    out.extend_from_slice(extra);

    // Hover label boxes: ink with a rule hairline, as the prototype's hovered `.lbl`.
    for &([x, y, w, h], a) in labels.iter().filter(|(_, a)| *a > 0.0) {
        out.push(rgba_rect(x, y, w, h, theme::RULE, a));
        out.push(rgba_rect(x + l.hair, y + l.hair, w - 2.0 * l.hair, h - 2.0 * l.hair, theme::INK, a));
    }

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
        self.area_alpha(1.0)
    }

    fn area_alpha(&self, a: f32) -> TextArea<'_> {
        let Rgb(r, g, b) = self.color;
        TextArea {
            buffer: &self.buf,
            left: self.left,
            top: self.top,
            scale: 1.0,
            bounds: self.bounds,
            default_color: Color::rgba(r, g, b, (a.clamp(0.0, 1.0) * 255.0).round() as u8),
            custom_glyphs: &[],
        }
    }
}

/// What the text was laid out for: target size and scale, wordmark crumbs, labels, `+N`.
#[derive(Clone, PartialEq)]
struct TextKey {
    size: (u32, u32, u64),
    crumbs: Vec<String>,
    label: Option<String>,
    /// File path and its glyph centre, whole pixels.
    file_label: Option<(String, i32, i32)>,
    more: Option<String>,
}

struct Text {
    fonts: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    mono: &'static str,
    key: Option<TextKey>,
    /// Palette, label fades (in 1/255 steps) and layouts of the last prepare.
    prepared: Option<(bool, u8, u8, TextKey, Option<vitals::TextFrame>)>,
    /// Vitals rows and tooltip, and the text and type scale they were laid out for.
    vitals_key: Option<(vitals::TextFrame, u32)>,
    vitals_rows: Vec<Placed>,
    vitals_tip: Option<(Placed, [f32; 4])>,
    /// Palette strip height, sized to its text.
    strip_h: f32,
    wordmark: Option<Placed>,
    /// Hover labels and their boxes `[x, y, w, h]`.
    label: Option<(Placed, [f32; 4])>,
    file_label: Option<(Placed, [f32; 4])>,
    more: Option<Placed>,
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
            prepared: None,
            vitals_key: None,
            vitals_rows: Vec::new(),
            vitals_tip: None,
            strip_h: 0.0,
            wordmark: None,
            label: None,
            file_label: None,
            more: None,
            cells: Vec::new(),
        }
    }

    /// Lays out the wordmark and label when their words change, and the strip when the
    /// target size changes. `knot_r` places the label under the knot.
    fn layout(&mut self, l: &Layout, key: TextKey, knot_r: f32) {
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let resized = self.key.as_ref().is_none_or(|old| old.size != key.size);
        let k = l.k;
        let fonts = &mut self.fonts;

        // Wordmark: prototype `.brand b`, 600 13px, tracking .06em, caps centred on the bead.
        // Crumbs follow in the regular weight.
        let (bx, by, br) = l.bead();
        let size = 13.0 * k;
        let mut buf = Buffer::new(fonts, Metrics::new(size, (size * 1.3).round()));
        buf.set_wrap(Wrap::None);
        buf.set_size(None, None);
        let base = Attrs::new().family(Family::Name(UI)).letter_spacing(0.06);
        let tail: String = key.crumbs.iter().map(|c| format!(" · {c}")).collect();
        buf.set_rich_text(
            [("PinnOS", base.clone().weight(Weight::SEMIBOLD)), (tail.as_str(), base.clone().weight(Weight::NORMAL))],
            &base,
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(fonts, false);
        let left = (bx + br + 8.0 * k).round();
        let top = (by - baseline(&buf) + 0.35 * size).round();
        let b = bounds(left, top, l.w, top + height(&buf));
        self.wordmark = Some(Placed { buf, left, top, color: theme::BONE_2, bounds: b });

        // Hover label: prototype `.lbl.hov .m`, mono 12.5px, centred under the knot at
        // 1.22 r plus a 6px gap.
        self.label = key.label.as_deref().map(|text| {
            let buf = line(fonts, text, 12.5 * k, self.mono, Weight::NORMAL, 0.02, None);
            let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            let (px, py) = (7.0 * k, 3.0 * k);
            let bw = (w + 2.0 * px).round();
            let bh = (height(&buf) + 2.0 * py).round();
            let bx = (l.w * 0.5 - bw * 0.5).round();
            let by = (l.h * 0.5 + knot_r * 1.22 + 6.0 * k).round();
            let (left, top) = (bx + px, by + py);
            let b = bounds(left, top, left + w + 1.0, top + height(&buf));
            (Placed { buf, left, top, color: theme::BONE_2, bounds: b }, [bx, by, bw, bh])
        });

        // File label: the same mono box, centred above the hovered glyph, kept on screen.
        self.file_label = key.file_label.as_ref().map(|(text, gx, gy)| {
            let buf = line(fonts, text, 12.5 * k, self.mono, Weight::NORMAL, 0.02, None);
            let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            let (px, py) = (7.0 * k, 3.0 * k);
            let bw = (w + 2.0 * px).round();
            let bh = (height(&buf) + 2.0 * py).round();
            let bx = (*gx as f32 - bw * 0.5).clamp(l.pad, (l.w - l.pad - bw).max(l.pad)).round();
            let above = *gy as f32 - 16.0 * k - bh;
            let by = if above > 48.0 * k { above } else { *gy as f32 + 16.0 * k }.round();
            let (left, top) = (bx + px, by + py);
            let b = bounds(left, top, left + w + 1.0, top + height(&buf));
            (Placed { buf, left, top, color: theme::BONE, bounds: b }, [bx, by, bw, bh])
        });

        // `+N`: small mono text low and right of the knot.
        self.more = key.more.as_deref().map(|text| {
            let buf = line(fonts, text, 12.0 * k, self.mono, Weight::NORMAL, 0.04, None);
            let left = (l.w * 0.5 + knot_r * 1.6).round();
            let top = (l.h * 0.5 + knot_r * 1.2).round();
            let b = bounds(left, top, left + 120.0 * k, top + height(&buf));
            Placed { buf, left, top, color: theme::BONE_3, bounds: b }
        });

        self.key = Some(key);
        if !resized {
            return;
        }

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

    /// Lays out the vitals rows and tooltip when their words change. Prototype `.vl`: mono
    /// 13px, the label in weight 500 with .1em tracking and at least 36px wide, 9px gaps.
    /// Tooltip `#vtTip`: mono 12.5px, line height 1.45, padding 6px by 10px.
    fn layout_vitals(&mut self, k: f32, text: Option<&vitals::TextFrame>) {
        let Some(text) = text else {
            self.vitals_key = None;
            self.vitals_rows.clear();
            self.vitals_tip = None;
            return;
        };
        if self.vitals_key.as_ref().is_some_and(|(t, kb)| t == text && *kb == k.to_bits()) {
            return;
        }
        let fonts = &mut self.fonts;
        self.vitals_rows.clear();
        for row in &text.rows {
            let mut x = row.left;
            for (i, (words, color)) in row.spans.iter().enumerate() {
                let (weight, tracking) = if i == 0 { (Weight::MEDIUM, 0.1) } else { (Weight::NORMAL, 0.0) };
                let buf = line(fonts, words, 13.0 * k, self.mono, weight, tracking, None);
                let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
                let (left, top) = (x.round(), row.top.round());
                let b = bounds(left, top, left + w + 1.0, top + height(&buf));
                self.vitals_rows.push(Placed { buf, left, top, color: *color, bounds: b });
                x += if i == 0 { w.max(36.0 * k) } else { w } + 9.0 * k;
            }
        }
        self.vitals_tip = text.tip.as_ref().map(|tip| {
            let size = 12.5 * k;
            let mut buf = Buffer::new(fonts, Metrics::new(size, (size * 1.45).round()));
            buf.set_wrap(Wrap::None);
            buf.set_size(None, None);
            let attrs = Attrs::new().family(Family::Name(self.mono));
            buf.set_text(&tip.lines.join("\n"), &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(fonts, false);
            let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            let (px, py) = (10.0 * k, 6.0 * k);
            let (bx, by) = (tip.left.round(), tip.top.round());
            let (bw, bh) = ((w + 2.0 * px).round(), (height(&buf) + 2.0 * py).round());
            let (left, top) = (bx + px, by + py);
            let b = bounds(left, top, left + w + 1.0, top + height(&buf));
            (Placed { buf, left, top, color: theme::BONE, bounds: b }, [bx, by, bw, bh])
        });
        self.vitals_key = Some((text.clone(), k.to_bits()));
    }

    /// Prepares text when anything shown changed. Returns whether it did. `fades` are the
    /// knot label's and the file label's.
    fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, palette: bool, fades: [f32; 2]) -> bool {
        let byte = |a: f32| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        let (fade, file_fade) = (byte(fades[0]), byte(fades[1]));
        let Some(key) = self.key.clone() else { return false };
        let vitals = self.vitals_key.as_ref().map(|(t, _)| t.clone());
        let state = (palette, fade, file_fade, key, vitals);
        if self.prepared.as_ref() == Some(&state) {
            return false;
        }
        self.prepared = Some(state);
        self.viewport.update(queue, Resolution { width: w, height: h });
        let mut areas: Vec<TextArea> = self.wordmark.iter().map(Placed::area).collect();
        if let Some((p, _)) = self.label.as_ref().filter(|_| fade > 0) {
            areas.push(p.area_alpha(fades[0]));
        }
        if let Some((p, _)) = self.file_label.as_ref().filter(|_| file_fade > 0) {
            areas.push(p.area_alpha(fades[1]));
        }
        areas.extend(self.more.iter().map(Placed::area));
        areas.extend(self.vitals_rows.iter().map(Placed::area));
        areas.extend(self.vitals_tip.iter().map(|(p, _)| p.area()));
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
        true
    }
}

pub struct Renderer {
    ground: wgpu::RenderPipeline,
    shape: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    shapes: wgpu::Buffer,
    bind: wgpu::BindGroup,
    knot_field: wgpu::RenderPipeline,
    knot_dots: wgpu::RenderPipeline,
    file_glyphs: wgpu::RenderPipeline,
    file_trails: wgpu::RenderPipeline,
    /// File glyphs, written when their generation changes.
    files: wgpu::Buffer,
    files_gen: Option<u64>,
    knot_uniform: wgpu::Buffer,
    knot_bind: wgpu::BindGroup,
    particles: wgpu::Buffer,
    particle_count: u32,
    vitals: vitals::Painter,
    /// With timestamps on, a frame splits into three passes, before, during and after the
    /// vitals, each timed; see [`Renderer::time_passes`].
    timing: Option<Timing>,
    text: Text,
    /// The atlas was trimmed after the last text prepare.
    trimmed: bool,
    /// Last uploads; a frame writes only what changed, since each write costs a copy at submit.
    last_globals: Option<Globals>,
    last_knot: Option<knot::Uniform>,
    last_shapes: Vec<Shape>,
}

fn buffer_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn uniform_layout(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[buffer_entry(0, wgpu::BufferBindingType::Uniform)],
    })
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Pass timestamps: the query set and where they resolve.
struct Timing {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
}

/// Passes a timed frame splits into.
const TIMED_PASSES: u32 = 3;

/// Adds light: each knot layer sums onto what is below, as the prototype's `lighter` does.
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

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let face = device.create_shader_module(wgpu::include_wgsl!("face.wgsl"));
        let knot_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("knot.wgsl"),
            source: wgpu::ShaderSource::Wgsl(knot::shader().into()),
        });
        let globals = uniform_buffer(device, "globals", size_of::<Globals>());
        let knot_uniform = uniform_buffer(device, "knot", size_of::<knot::Uniform>());
        let files = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("file glyphs"),
            size: (size_of::<FileInst>() * files::CAP) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shapes = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shapes"),
            size: (size_of::<Shape>() * MAX_SHAPES) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dots = knot::particles();
        let particles = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("knot particles"),
            contents: bytemuck::cast_slice(&dots),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let bgl = uniform_layout(device, "globals");
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let knot_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("knot"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
            ],
        });
        let knot_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("knot"),
            layout: &knot_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: knot_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: files.as_entire_binding() },
            ],
        });
        let layout = |bgl: &wgpu::BindGroupLayout| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            })
        };
        let (face_layout, knot_layout) = (layout(&bgl), layout(&knot_bgl));

        let pipeline = |label,
                        (module, layout): (&wgpu::ShaderModule, &wgpu::PipelineLayout),
                        vs,
                        fs,
                        buffers: &[Option<wgpu::VertexBufferLayout>],
                        blend,
                        topology| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
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

        let f = (&face, &face_layout);
        let kn = (&knot_module, &knot_layout);
        let strip = wgpu::PrimitiveTopology::TriangleStrip;
        let ground = pipeline("ground", f, "ground_vs", "ground_fs", &[], None, wgpu::PrimitiveTopology::TriangleList);
        let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4];
        let shape_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<Shape>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &attrs,
        };
        let shape = pipeline(
            "shapes",
            f,
            "shape_vs",
            "shape_fs",
            &[Some(shape_layout)],
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            strip,
        );
        let knot_field = pipeline("knot field", kn, "field_vs", "field_fs", &[], Some(ADDITIVE), strip);
        let dot_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
        let dot_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<knot::Particle>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &dot_attrs,
        };
        let knot_dots =
            pipeline("knot dots", kn, "particle_vs", "particle_fs", &[Some(dot_layout)], Some(ADDITIVE), strip);
        // Glyphs cover the knot; trails write zero alpha, so the same blend adds them.
        let over = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let file_glyphs = pipeline("file glyphs", kn, "glyph_vs", "glyph_fs", &[], over, strip);
        let file_trails = pipeline("file trails", kn, "trail_vs", "trail_fs", &[], over, strip);

        Renderer {
            ground,
            shape,
            globals,
            shapes,
            bind,
            knot_field,
            knot_dots,
            file_glyphs,
            file_trails,
            files,
            files_gen: None,
            knot_uniform,
            knot_bind,
            particles,
            particle_count: dots.len() as u32,
            vitals: vitals::Painter::new(device, format, ADDITIVE),
            timing: None,
            text: Text::new(device, queue, format),
            trimmed: true,
            last_globals: None,
            last_knot: None,
            last_shapes: Vec::new(),
        }
    }

    /// Uploads uniforms and shapes, and prepares text if it changed. Returns the shape count.
    fn prepare(&mut self, gpu: &Gpu, (w, h): (u32, u32), scale: f64, scene: &Scene) -> u32 {
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
            light: ground_light(scene.knot.state[0]),
        };
        if self.last_globals.as_ref() != Some(&globals) {
            queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
            self.last_globals = Some(globals);
        }
        if self.last_knot.as_ref() != Some(&scene.knot) {
            queue.write_buffer(&self.knot_uniform, 0, bytemuck::bytes_of(&scene.knot));
            self.last_knot = Some(scene.knot);
        }
        if self.files_gen != Some(scene.files_gen) {
            let n = scene.files.len().min(files::CAP);
            if n > 0 {
                queue.write_buffer(&self.files, 0, bytemuck::cast_slice(&scene.files[..n]));
            }
            self.files_gen = Some(scene.files_gen);
        }

        let key = TextKey {
            size: (w, h, scale.to_bits()),
            crumbs: scene.crumbs.iter().map(|c| c.to_string()).collect(),
            label: scene.label.map(str::to_string),
            file_label: scene.file_label.map(|(p, [x, y])| (p.to_string(), x.round() as i32, y.round() as i32)),
            more: scene.more.map(str::to_string),
        };
        self.text.layout(&l, key, scene.knot.radius());
        self.text.layout_vitals(l.k, scene.vitals.map(|v| &v.text));
        let mut extra = Vec::with_capacity(5);
        if let Some(v) = scene.vitals {
            self.vitals.prepare(queue, v);
            // Left rules at .9, as the prototype's `--fc`.
            extra.extend(v.rules.iter().map(|&([x, y, w, h], c)| rgba_rect(x, y, w, h, c, 0.9)));
            if let Some((_, [x, y, w, h])) = self.text.vitals_tip {
                extra.push(rect(x, y, w, h, theme::RULE_2));
                extra.push(rect(x + l.hair, y + l.hair, w - 2.0 * l.hair, h - 2.0 * l.hair, vitals::TIP_GROUND));
            }
        }
        let mut labels = Vec::with_capacity(2);
        labels.extend(self.text.label.as_ref().map(|(_, b)| (*b, scene.label_alpha)));
        labels.extend(self.text.file_label.as_ref().map(|(_, b)| (*b, scene.file_label_alpha)));
        let shapes = shapes(&l, scene.view, self.text.strip_h, &labels, &extra);
        if shapes != self.last_shapes {
            queue.write_buffer(&self.shapes, 0, bytemuck::cast_slice(&shapes));
        }

        if self.text.prepare(device, queue, w, h, scene.view.palette, [scene.label_alpha, scene.file_label_alpha]) {
            self.trimmed = false;
        }
        let n = shapes.len() as u32;
        self.last_shapes = shapes;
        n
    }

    /// Records one frame.
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        (w, h): (u32, u32),
        scale: f64,
        scene: &Scene,
    ) {
        let count = self.prepare(gpu, (w, h), scale, scene);

        let [r, g, b] = theme::INK.srgb_f32();
        let clear = wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 });
        let timing = self.timing.as_ref();
        let begin = |encoder: &mut wgpu::CommandEncoder, load, i: u32| {
            encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("face"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: timing.map(|t| wgpu::RenderPassTimestampWrites {
                        query_set: &t.queries,
                        beginning_of_pass_write_index: Some(2 * i),
                        end_of_pass_write_index: Some(2 * i + 1),
                    }),
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime()
        };
        let mut pass = begin(encoder, clear, 0);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_pipeline(&self.ground);
        pass.draw(0..3, 0..1);

        pass.set_bind_group(0, &self.knot_bind, &[]);
        pass.set_pipeline(&self.knot_field);
        pass.draw(0..4, 0..1);
        pass.set_pipeline(&self.knot_dots);
        pass.set_vertex_buffer(0, self.particles.slice(..));
        pass.draw(0..4, 0..self.particle_count);
        let glyphs = scene.files.len().min(files::CAP) as u32;
        if glyphs > 0 {
            pass.set_pipeline(&self.file_trails);
            pass.draw(0..4, 0..glyphs);
            pass.set_pipeline(&self.file_glyphs);
            pass.draw(0..4, 0..glyphs);
        }

        if timing.is_some() {
            drop(pass);
            pass = begin(encoder, wgpu::LoadOp::Load, 1);
        }
        if let Some(v) = scene.vitals {
            self.vitals.draw(&mut pass, v);
        }
        if timing.is_some() {
            drop(pass);
            pass = begin(encoder, wgpu::LoadOp::Load, 2);
        }

        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_pipeline(&self.shape);
        pass.set_vertex_buffer(0, self.shapes.slice(..));
        pass.draw(0..4, 0..count);
        if let Err(e) = self.text.renderer.render(&self.text.atlas, &self.text.viewport, &mut pass) {
            log::error!("text render: {e}");
        }
        drop(pass);
        if let Some(t) = timing {
            encoder.resolve_query_set(&t.queries, 0..2 * TIMED_PASSES, &t.resolve, 0);
            encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, t.resolve.size());
        }
    }

    /// Times each frame's passes from now on: before, during and after the vitals. Needs
    /// `TIMESTAMP_QUERY`; returns false without it.
    pub fn time_passes(&mut self, device: &wgpu::Device) -> bool {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return false;
        }
        let n = 2 * TIMED_PASSES;
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("pass times"),
            ty: wgpu::QueryType::Timestamp,
            count: n,
        });
        let size = 8 * n as u64;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass times"),
            size,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass times readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.timing = Some(Timing { queries, resolve, readback });
        true
    }

    /// GPU milliseconds of the last submitted frame's passes: before, during and after the
    /// vitals. Waits for the GPU.
    pub fn pass_times(&self, gpu: &Gpu) -> Option<[f64; TIMED_PASSES as usize]> {
        let t = self.timing.as_ref()?;
        let slice = t.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        let ticks: Vec<u64> = {
            let data = slice.get_mapped_range().ok()?;
            data.as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b)).collect()
        };
        t.readback.unmap();
        let period = f64::from(gpu.queue.get_timestamp_period()) / 1e6;
        Some([0, 1, 2].map(|i| ticks[2 * i + 1].saturating_sub(ticks[2 * i]) as f64 * period))
    }

    /// Frees glyphs not used by the last text prepare. Frames that reuse prepared text skip
    /// this: glyphs are marked used only when text is prepared.
    pub fn trim(&mut self) {
        if !self.trimmed {
            self.text.atlas.trim();
            self.trimmed = true;
        }
    }
}
