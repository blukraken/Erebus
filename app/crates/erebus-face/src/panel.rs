//! Panel host: a surface built beside its source, tied to it by a tether. After the
//! prototype's `.surface` and `drawTethers` in `v3/concept-loom4.html`.
//!
//! Nanites build a panel from left to right. Its corner ticks and a dotted outline go up
//! first, motes stream down the tether, and a swarm rides a glowing front that lays the panel
//! down as grain. Everything left of the front is finished; contents show as the front passes
//! them. Closing runs the front back to the left while the swarm eats the panel and scatters.
//! With reduced motion the panel only fades, in 0.14 s.
//!
//! The vitals history is the first tenant: 90 s sparklines with exact figures for RAM, GPU
//! utilisation, VRAM, GPU temperature, CPU and every core. It changes once per sample, as the
//! prototype redraws it.

use crate::vitals::{self, Column, Row, Seg, Vitals};
use erebus_protocol::Sample;
use erebus_theme::{self as theme, Rgb};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Seconds the front takes to cross a panel.
const BUILD: f32 = 0.6;
const EAT: f32 = 0.45;
const REDUCED: f32 = 0.14;
/// Swarm motes, tether motes, and the grain band's depth behind the front, CSS pixels.
const SWARM: u32 = 240;
const STREAM: u32 = 14;
const BAND: f32 = 46.0;
/// Grain cell, CSS pixels.
const GRAIN: f32 = 5.0;
/// GPU temperature line, the prototype's `#ff8a76`.
const TEMP: Rgb = Rgb::hex(0xff8a76);
/// Samples further apart than this break a sparkline.
const GAP_MS: u64 = 2500;
const SPAN_MS: f32 = 90_000.0;

// Prototype CSS pixels: `.s-head`, `.vp-body`, `.vp-row`, `.vp-cores`.
const WIDTH: f32 = 620.0;
const HEAD: f32 = 7.0 + 12.0 * 1.3 + 6.0;
const PAD_X: f32 = 14.0;
const PAD_TOP: f32 = 8.0;
const PAD_BOTTOM: f32 = 12.0;
const ROW: f32 = 40.0;
const LABEL_W: f32 = 118.0;
const VALUE_W: f32 = 170.0;
const GAP: f32 = 12.0;
const SPARK_H: f32 = 30.0;
const CORE_H: f32 = 16.0;
const CORE_GAP_Y: f32 = 4.0;
const CORES_PAD: (f32, f32) = (4.0, 6.0);
const SRC_GAP: f32 = 8.0;
const BODY_SIZE: f32 = 11.5;
const ID_SIZE: f32 = 12.0;
const TICK: f32 = 9.0;
const TICK_OUT: f32 = 5.0;
/// The collapse control brightens under the cursor over this long, seconds.
const LIT: f32 = 0.12;

/// Open state and how much of the panel is built.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Panel {
    /// The row it opened from; kept while it is eaten.
    row: Option<Row>,
    open: bool,
    /// Built share, 0..1: where the front stands across the panel.
    built: f32,
    /// Seconds of building or eating, for the swarm's motion.
    clock: f32,
    /// The cursor is on "— collapse", and 0..1, how lit the control is.
    pub collapse_hover: bool,
    collapse_lit: f32,
}

impl Panel {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The row it is open on.
    pub fn focus(&self) -> Option<Row> {
        self.row.filter(|_| self.open)
    }

    /// Something of it is drawn.
    pub fn visible(&self) -> bool {
        self.row.is_some() && (self.open || self.built > 0.0)
    }

    /// It is being built or eaten.
    fn building(&self) -> bool {
        if self.open { self.built < 1.0 } else { self.built > 0.0 }
    }

    /// It is being built or eaten, or the collapse control is fading.
    pub fn busy(&self) -> bool {
        self.building() || self.collapse_lit != f32::from(u8::from(self.collapse_hover))
    }

    /// A click on a vitals row: opens on it, moves to it, or closes when already on it.
    pub fn toggle(&mut self, row: Row) {
        if self.open && self.row == Some(row) {
            self.close();
        } else {
            self.open = true;
            self.row = Some(row);
        }
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn step(&mut self, dt: f32, reduced: bool) {
        let dt = dt.max(0.0);
        if self.building() {
            self.clock += dt;
        }
        let lit = f32::from(u8::from(self.collapse_hover));
        self.collapse_lit = if lit > self.collapse_lit { (self.collapse_lit + dt / LIT).min(lit) } else { (self.collapse_lit - dt / LIT).max(lit) };
        self.built = if self.open {
            (self.built + dt / if reduced { REDUCED } else { BUILD }).min(1.0)
        } else {
            (self.built - dt / if reduced { REDUCED } else { EAT }).max(0.0)
        };
        if !self.open && self.built <= 0.0 {
            self.row = None;
        }
    }

    /// Finishes any change at once.
    pub fn snap(&mut self) {
        self.step(1.0, true);
    }
}

/// Panel boxes in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// `[x, y, w, h]`.
    pub rect: [f32; 4],
    /// The "— collapse" hit box.
    pub collapse: [f32; 4],
}

impl Layout {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        inside(self.rect, x, y)
    }

    pub fn over_collapse(&self, x: f32, y: f32) -> bool {
        inside(self.collapse, x, y)
    }
}

fn inside([rx, ry, rw, rh]: [f32; 4], x: f32, y: f32) -> bool {
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

fn core_cols(cores: usize) -> usize {
    if cores > 32 { 8 } else { 4 }
}

/// Body height in CSS pixels for `cores` cores.
fn body_h(cores: usize) -> f32 {
    let rows = cores.div_ceil(core_cols(cores)) as f32;
    let grid = if rows > 0.0 { rows * CORE_H + (rows - 1.0) * CORE_GAP_Y } else { 0.0 };
    PAD_TOP + 5.0 * ROW + CORES_PAD.0 + grid + CORES_PAD.1 + SRC_GAP + BODY_SIZE * 1.3 + PAD_BOTTOM
}

/// Sits right of the vitals column, its bottom level with the column's.
pub fn layout(col: &Column, cores: usize, (w, _): (u32, u32)) -> Layout {
    let k = col.k;
    let x = (col.right + 32.0 * col.u).round();
    let pw = (WIDTH * k).min(w as f32 - x - 14.0 * k).max(200.0 * k).round();
    let h = ((HEAD + 1.0 + body_h(cores)) * k).round();
    let [b0, b1] = col.band;
    let bottom = col.rows[vitals::ROWS - 1].rect;
    let y = (bottom[1] + bottom[3] - h).min(b1 - h - 8.0 * k).max(b0 + 8.0 * k).round();
    let cw = 10.0 * 0.6 * BODY_SIZE * k + 8.0 * k;
    let collapse = [x + pw - 10.0 * k - cw, y, cw + 10.0 * k, (HEAD * k).round()];
    Layout { rect: [x, y, pw, h], collapse }
}

/// A row's tether end: its right edge, half way down.
fn source(col: &Column, row: Row) -> [f32; 2] {
    let [x, y, w, h] = col.rows[row as usize].rect;
    [x + w, y + h * 0.5]
}

/// One frame of the panel.
pub struct Frame {
    /// Ground, border, ticks, rules and the focus bar: `[x, y, w, h]`, colour, opacity.
    pub rects: Vec<([f32; 4], Rgb, f32)>,
    pub segs: Vec<Seg>,
    /// Names `segs`; the painter uploads them when it changes.
    pub key: u64,
    pub text: Text,
    /// Text opacity, and the front: text right of it is not drawn yet.
    pub alpha: f32,
    pub clip: f32,
}

/// The panel's words. Changes once per sample, so the renderer keys layout on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub items: Vec<Item>,
    /// Type scale they were placed for.
    pub k: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Left edge, or right edge when `right`, and line top.
    pub x: f32,
    pub y: f32,
    pub right: bool,
    /// CSS pixels; the renderer scales by `k`.
    pub size: f32,
    pub mono: bool,
    pub bold: bool,
    pub text: String,
    pub color: Rgb,
}

fn item(x: f32, y: f32, size: f32, text: String, color: Rgb) -> Item {
    Item { x, y, right: false, size, mono: true, bold: false, text, color }
}

fn gb(mb: f32) -> f32 {
    mb / 1024.0
}

/// Uniform 0..1 from an index and a salt; the swarm is the same every time.
fn hash(i: u32, salt: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9e37_79b1) ^ salt.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2c1b_3c6d);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297a_2d39);
    x ^= x >> 15;
    x as f32 / u32::MAX as f32
}

/// The part of `[x, y, w, h]` left of `front`.
pub(crate) fn clip_rect([x, y, w, h]: [f32; 4], front: f32) -> Option<[f32; 4]> {
    let w = w.min(front - x);
    (w > 0.0).then_some([x, y, w, h])
}

/// A stroke cut at `front`, or nothing when it starts past it.
pub(crate) fn clip_seg(a: [f32; 2], b: [f32; 2], front: f32) -> Option<([f32; 2], [f32; 2])> {
    if a[0] > front {
        return None;
    }
    if b[0] <= front {
        return Some((a, b));
    }
    let t = (front - a[0]) / (b[0] - a[0]);
    Some((a, [front, a[1] + (b[1] - a[1]) * t]))
}

/// A 90 s sparkline in `[x0, y0, x1, h]`, after the prototype's `spark()`: a rule along the
/// bottom and the series scaled into 27 of 30 units. Missing values and gaps break the line.
#[allow(clippy::too_many_arguments)]
fn spark(
    out: &mut Vec<Seg>,
    hist: &std::collections::VecDeque<Sample>,
    value: impl Fn(&Sample) -> Option<f32>,
    (lo, hi): (f32, f32),
    [x0, y0, x1, h]: [f32; 4],
    color: Rgb,
    front: f32,
    px: f32,
) {
    let Some(newest) = hist.back().map(|s| s.unix_ms) else { return };
    let base = y0 + h * 29.5 / 30.0;
    if let Some((a, b)) = clip_seg([x0, base], [x1, base], front) {
        out.push(vitals::seg(a, b, theme::RULE, 1.0, px));
    }
    let mut prev: Option<([f32; 2], u64)> = None;
    for s in hist {
        let Some(v) = value(s) else {
            prev = None;
            continue;
        };
        let x = x1 - (newest - s.unix_ms) as f32 / SPAN_MS * (x1 - x0);
        let n = ((v - lo) / (hi - lo).max(1e-3)).clamp(0.0, 1.0);
        let y = y0 + (29.0 - n * 27.0) / 30.0 * h;
        if let Some((p, _)) = prev.filter(|(_, t)| s.unix_ms - t <= GAP_MS) {
            if let Some((a, b)) = clip_seg(p, [x, y], front) {
                out.push(vitals::seg(a, b, color, 1.0, 1.4 * px));
            }
        }
        prev = Some(([x, y], s.unix_ms));
    }
}

/// Two strands from the source to the panel's near edge, a square where they land and a
/// glow at the source, as the prototype's `drawTethers`.
fn tether(out: &mut Vec<Seg>, src: [f32; 2], [fx, fy, _, fh]: [f32; 4], z: f32, px: f32, alpha: f32) {
    let (tx, ty) = (fx, src[1].clamp(fy, fy + fh));
    let mx = (src[0] + tx) * 0.5;
    let o = 2.2 * z;
    for off in [-o, o] {
        let at = |t: f32| {
            let u = 1.0 - t;
            let x = u * u * u * src[0] + 3.0 * u * u * t * mx + 3.0 * u * t * t * mx + t * t * t * tx;
            let y = u * u * u * src[1] + 3.0 * u * u * t * src[1] + 3.0 * u * t * t * ty + t * t * t * ty;
            [x, y + off]
        };
        for i in 0..16 {
            let (a, b) = (at(i as f32 / 16.0), at((i + 1) as f32 / 16.0));
            out.push(vitals::seg(a, b, theme::BONE_2, 0.75 * alpha, px));
        }
    }
    out.push(vitals::seg([tx - 2.0 * z, ty], [tx + 2.0 * z, ty], theme::BONE_2, 0.95 * alpha, 4.0 * z));
    out.push(vitals::glow(src[0], src[1], 24.0 * z, theme::EMBER, 0.8 * alpha));
}

/// The nanites at work: the front's glowing edge, grain settling (or lifting) behind it, the
/// swarm riding it, and motes running along the tether. `building` sets their direction.
#[allow(clippy::too_many_arguments)]
fn nanites(out: &mut Vec<Seg>, [x, y, w, h]: [f32; 4], front: f32, src: [f32; 2], clock: f32, building: bool, k: f32, px: f32) {
    let band = BAND * k;
    let dot = (2.0 * px).max(1.3 * k);

    // Edge: a hot seam with a soft glow along it.
    let mut gy = y;
    while gy < y + h {
        out.push(vitals::glow(front, gy, 22.0 * k, theme::EMBER, 0.22));
        gy += 10.0 * k;
    }
    let wob = |yy: f32| 1.5 * k * (yy * 0.05 + clock * 23.0).sin();
    let mut sy = y;
    while sy < y + h {
        let ny = (sy + 6.0 * k).min(y + h);
        out.push(vitals::seg([front + wob(sy), sy], [front + wob(ny), ny], theme::HOT, 0.55, 1.2 * px));
        sy = ny;
    }

    // Grain: cells near the front flicker as they are laid down or taken up.
    let cell = GRAIN * k;
    let rows = (h / cell) as u32;
    let cols = (band / cell) as u32;
    let tick = (clock * 30.0) as u32;
    for r in 0..rows {
        for c in 0..cols {
            let cx = front - (c as f32 + 0.5) * cell;
            if cx < x {
                continue;
            }
            let depth = c as f32 / cols as f32;
            let seed = r * 977 + c;
            if hash(seed, tick) < 0.75 * (1.0 - depth) {
                let cy = y + (r as f32 + 0.5) * cell;
                let tone = if hash(seed, 7) < 0.3 { theme::EMBER } else { theme::BONE_2 };
                let s = cell * 0.55;
                out.push(vitals::seg([cx - s * 0.5, cy], [cx + s * 0.5, cy], tone, 0.35 * (1.0 - depth), s));
            }
        }
    }

    // Swarm: each mote drifts across the front over its life, fading as it settles or
    // scatters.
    for i in 0..SWARM {
        let life = (clock * (1.2 + hash(i, 3) * 1.6) + hash(i, 2)).fract();
        let along = if building { 0.25 - 1.1 * life } else { -0.2 + 1.4 * life };
        let my = y + hash(i, 1) * h + 4.0 * k * (clock * (5.0 + 4.0 * hash(i, 4)) + i as f32).sin();
        let mx = front + along * band + 2.0 * k * (clock * 9.0 + i as f32 * 1.7).cos();
        if mx < x - 2.0 * k || mx > x + w + band {
            continue;
        }
        let tone = if hash(i, 5) < 0.4 { theme::HOT } else { theme::EMBER };
        let a = (1.0 - life) * (0.55 + 0.45 * hash(i, 6));
        out.push(vitals::bead(mx, my, dot * (1.0 + hash(i, 8)), tone, a));
        if hash(i, 9) < 0.08 {
            out.push(vitals::glow(mx, my, 10.0 * k, theme::EMBER, 0.5 * a));
        }
    }

    // Stream: motes run from the row to the front while building, and back while eating.
    for i in 0..STREAM {
        let q = (clock * 1.8 + hash(i, 11)).fract();
        let q = if building { q } else { 1.0 - q };
        let ty = y + hash(i, 12) * h;
        let end = [front.max(x), ty];
        let mid = [(src[0] + end[0]) * 0.5, src[1] + (ty - src[1]) * 0.2];
        let u = 1.0 - q;
        let p = [
            u * u * src[0] + 2.0 * u * q * mid[0] + q * q * end[0],
            u * u * src[1] + 2.0 * u * q * mid[1] + q * q * end[1],
        ];
        let a = (4.0 * q * (1.0 - q)).min(1.0);
        out.push(vitals::bead(p[0], p[1], dot * 1.4, theme::EMBER, 0.85 * a));
    }
}

/// Builds the vitals panel, or `None` when nothing of it shows.
pub fn frame(p: &Panel, v: &Vitals, col: &Column, size: (u32, u32), reduced: bool) -> Option<Frame> {
    if !p.visible() {
        return None;
    }
    let row = p.row?;
    let hw = v.hardware()?;
    let hist = v.samples();
    let last = hist.back()?;
    let cores = hw.cores as usize;
    let l = layout(col, cores, size);
    let (k, px) = (col.k, col.px);
    let hair = px.round().max(1.0);
    let src = source(col, row);
    let [x, y, w, h] = l.rect;

    // Reduced motion fades the whole panel; otherwise the front cuts it.
    let (fa, front) = if reduced { (p.built, f32::MAX) } else { (1.0, x + p.built * w) };
    let working = !reduced && p.building();

    let mut key = DefaultHasher::new();
    (v.generation(), l.rect.map(f32::to_bits), p.built.to_bits(), p.clock.to_bits()).hash(&mut key);
    (row as u8, p.open, p.collapse_lit.to_bits(), reduced).hash(&mut key);

    let mut rects = Vec::with_capacity(32);
    let mut push = |r: [f32; 4], c: Rgb, a: f32| {
        if let Some(r) = clip_rect(r, front) {
            rects.push((r, c, a * fa));
        }
    };
    push(l.rect, theme::RULE_2, 1.0);
    push([x + hair, y + hair, w - 2.0 * hair, h - 2.0 * hair], theme::SLAB, 1.0);

    // Corner ticks go up first, as scaffolding.
    let mut ticks = Vec::with_capacity(8);
    let scaffold = if reduced { fa } else { (p.built * 8.0).min(1.0) };
    let (o, t) = (TICK_OUT * k, TICK * k);
    let (l0, t0, r1, b1) = (x - o, y - o, x + w + o, y + h + o);
    for (cx, cy, sx, sy) in [(l0, t0, 1.0, 1.0), (r1, t0, -1.0, 1.0), (l0, b1, 1.0, -1.0), (r1, b1, -1.0, -1.0)] {
        let hx = if sx > 0.0 { cx } else { cx - t };
        let vy = if sy > 0.0 { cy } else { cy - t };
        let hy = if sy > 0.0 { cy } else { cy - hair };
        let vx = if sx > 0.0 { cx } else { cx - hair };
        ticks.push(([hx, hy, t, hair], theme::BONE_2, scaffold));
        ticks.push(([vx, vy, hair, t], theme::BONE_2, scaffold));
    }

    let mut segs = Vec::with_capacity(1024 + (5 + cores) * hist.len());
    let tether_a = if reduced { fa } else { (p.built * 5.0).min(1.0) };
    tether(&mut segs, src, l.rect, col.u.max(0.8 * px), px, tether_a);

    // Dotted outline ahead of the front.
    if working {
        let step = 6.0 * k;
        let mut dx = front.max(x) + step;
        while dx < x + w {
            for yy in [y, y + h] {
                segs.push(vitals::seg([dx, yy], [dx + 2.0 * k, yy], theme::BONE_2, 0.3, hair));
            }
            dx += step;
        }
    }

    let mut items = Vec::with_capacity(16 + 2 * cores);
    let head = (HEAD * k).round();
    push([x + hair, y + head, w - 2.0 * hair, hair], theme::RULE, 1.0);
    let (bs, is) = (BODY_SIZE, ID_SIZE);
    items.push(Item { bold: true, mono: false, ..item(x + 12.0 * k, y + 7.0 * k, is, "Vitals".into(), theme::BONE) });
    items.push(item(x + 56.0 * k, y + 7.5 * k, bs, "last 90 s · sampled 1 s".into(), vitals::LABEL));
    let lit = p.collapse_lit;
    let collapse_color = vitals::mix(vitals::LABEL, theme::BONE, lit * lit * (3.0 - 2.0 * lit));
    items.push(Item { right: true, ..item(x + w - 14.0 * k, y + 7.5 * k, bs, "— collapse".into(), collapse_color) });

    let bx = x + PAD_X * k;
    let bw = w - 2.0 * PAD_X * k;
    let by = y + head + hair + PAD_TOP * k;
    let line = bs * 1.3 * k;
    let mid = |top: f32, hgt: f32| (top + (hgt - line) * 0.5).round();

    let ram_total = hw.ram_total_mb.max(1) as f32;
    let gpu_info = hw.gpu.as_ref();
    let vram_total = gpu_info.map_or(1.0, |g| g.vram_total_mb.max(1) as f32);
    let max = |f: &dyn Fn(&Sample) -> Option<f32>| hist.iter().filter_map(f).fold(f32::MIN, f32::max);
    let min = |f: &dyn Fn(&Sample) -> Option<f32>| hist.iter().filter_map(f).fold(f32::MAX, f32::min);
    let ram = |s: &Sample| Some(s.ram_used_mb as f32);
    let util = |s: &Sample| s.gpu.map(|g| f32::from(g.util));
    let vram = |s: &Sample| s.gpu.map(|g| g.vram_used_mb as f32);
    let temp = |s: &Sample| s.gpu.map(|g| f32::from(g.temp_c));
    let cpu = |s: &Sample| Some(s.cpu);
    let g = last.gpu;
    let none = "no reading".to_string();

    // Label, series, range, line colour, the row it focuses, figure.
    type Measure = (&'static str, fn(&Sample) -> Option<f32>, (f32, f32), Rgb, Option<Row>, String);
    let rows: [Measure; 5] = [
        (
            "RAM",
            ram,
            (0.0, ram_total),
            theme::BONE_2,
            Some(Row::Ram),
            format!("{:.1} / {:.0} GB · max {:.1}", gb(last.ram_used_mb as f32), gb(ram_total), gb(max(&ram))),
        ),
        (
            "GPU util",
            util,
            (0.0, 100.0),
            theme::EMBER,
            Some(Row::Gpu),
            g.map_or(none.clone(), |g| format!("{} % · max {:.0}", g.util, max(&util))),
        ),
        (
            "VRAM",
            vram,
            (0.0, vram_total),
            theme::EMBER,
            None,
            g.map_or(none.clone(), |g| format!("{:.1} / {:.1} GB", gb(g.vram_used_mb as f32), gb(vram_total))),
        ),
        ("GPU temp", temp, (40.0, 90.0), TEMP, None, g.map_or(none, |g| format!("{} °C · {} W", g.temp_c, g.power_w))),
        (
            "CPU",
            cpu,
            (0.0, 100.0),
            theme::BONE,
            Some(Row::Cpu),
            format!("{:.0} % · min {:.0} max {:.0}", last.cpu, min(&cpu), max(&cpu)),
        ),
    ];
    let lines_from = segs.len();
    for (i, (label, value, range, color, focus, figure)) in rows.into_iter().enumerate() {
        let ry = by + i as f32 * ROW * k;
        if i > 0 {
            push([bx, ry, bw, hair], theme::RULE, 1.0);
        }
        if focus == Some(row) && p.open {
            push([bx, ry + hair, 2.0 * px, ROW * k - hair], theme::EMBER, 1.0);
        }
        let ty = mid(ry, ROW * k);
        items.push(Item { bold: true, ..item(bx + 8.0 * px, ty, bs, label.into(), theme::BONE) });
        items.push(Item { right: true, ..item(bx + bw, ty, bs, figure, theme::BONE) });
        let sx0 = bx + (LABEL_W + GAP) * k;
        let sx1 = bx + bw - (VALUE_W + GAP) * k;
        let sy = ry + (ROW - SPARK_H) * 0.5 * k;
        spark(&mut segs, hist, value, range, [sx0, sy, sx1, SPARK_H * k], color, front, px);
    }

    let cols = core_cols(cores);
    let cy0 = by + 5.0 * ROW * k + CORES_PAD.0 * k;
    let cell = (bw - (cols - 1) as f32 * GAP * k) / cols as f32;
    for i in 0..cores {
        let cx = bx + (i % cols) as f32 * (cell + GAP * k);
        let cy = cy0 + (i / cols) as f32 * (CORE_H + CORE_GAP_Y) * k;
        let ty = mid(cy, CORE_H * k);
        items.push(item(cx, ty, bs, format!("c{i:02}"), vitals::LABEL));
        let now = last.cores.get(i).copied().unwrap_or(0);
        items.push(Item { right: true, ..item(cx + cell, ty, bs, format!("{now}%"), theme::BONE) });
        let value = move |s: &Sample| s.cores.get(i).map(|&c| f32::from(c));
        let area = [cx + 36.0 * k, cy, cx + cell - 46.0 * k, CORE_H * k];
        spark(&mut segs, hist, value, (0.0, 100.0), area, theme::BONE_2, front, px);
    }
    if fa < 1.0 {
        for s in &mut segs[lines_from..] {
            s.color = s.color.map(|c| c * fa);
        }
    }

    let grid_rows = cores.div_ceil(cols) as f32;
    let sy = cy0 + (grid_rows * CORE_H + (grid_rows - 1.0).max(0.0) * CORE_GAP_Y + CORES_PAD.1 + SRC_GAP) * k;
    let source_line = match gpu_info {
        Some(g) => format!(
            "{} {:.0} GB · {cores} cores · {:.0} GB · sampled 1 s · NVML and Windows counters",
            vitals::short_gpu(&g.name),
            gb(vram_total),
            gb(ram_total)
        ),
        None => format!("no GPU (NVML unavailable) · {cores} cores · {:.0} GB · sampled 1 s · Windows counters", gb(ram_total)),
    };
    items.push(item(bx, sy.round(), bs, source_line, vitals::LABEL));

    rects.extend(ticks);
    if working {
        nanites(&mut segs, l.rect, front, src, p.clock, p.open, k, px);
    }

    segs.truncate(vitals::PANEL_SEGS);
    Some(Frame { rects, segs, key: key.finish(), text: Text { items, k }, alpha: fa, clip: front })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col() -> Column {
        vitals::column(5120, 1440, 1.0).unwrap()
    }

    fn open(row: Row) -> Panel {
        let mut p = Panel::default();
        p.toggle(row);
        p.snap();
        p
    }

    #[test]
    fn toggle_opens_moves_and_closes() {
        let mut p = Panel::default();
        assert!(!p.visible() && !p.busy());
        p.toggle(Row::Gpu);
        assert_eq!(p.focus(), Some(Row::Gpu));
        p.step(1.0, false);
        p.toggle(Row::Cpu);
        assert_eq!(p.focus(), Some(Row::Cpu));
        assert!(!p.busy(), "moving focus does not rebuild");
        p.toggle(Row::Cpu);
        assert!(!p.is_open() && p.visible());
        p.step(1.0, false);
        assert!(!p.visible());
    }

    #[test]
    fn reversing_mid_way_keeps_the_front_where_it_is() {
        let mut p = Panel::default();
        p.toggle(Row::Ram);
        p.step(0.3, false);
        let before = p.built;
        p.close();
        p.step(0.0, false);
        assert_eq!(p.built, before);
        p.step(EAT, false);
        assert!(!p.visible());
    }

    #[test]
    fn the_front_hides_what_is_not_built_yet() {
        let v = vitals::canned(0);
        let mut p = Panel::default();
        p.toggle(Row::Gpu);
        p.step(BUILD * 0.5, false);
        let f = frame(&p, &v, &col(), (5120, 1440), false).unwrap();
        let [x, _, w, _] = layout(&col(), 32, (5120, 1440)).rect;
        assert!((f.clip - (x + 0.5 * w)).abs() < 1.0);
        assert_eq!(f.alpha, 1.0, "no fade: text shows as the front passes it");
        for (r, ..) in &f.rects[..2] {
            assert!(r[0] + r[2] <= f.clip + 0.01);
        }
    }

    #[test]
    fn layout_sits_right_of_the_column_inside_the_band() {
        let c = col();
        let l = layout(&c, 32, (5120, 1440));
        let [x, y, w, h] = l.rect;
        assert!(x > c.right && w > 600.0 && y >= c.band[0] && y + h <= c.band[1]);
        let bottom = c.rows[2].rect;
        assert!((y + h - (bottom[1] + bottom[3])).abs() < 1.0, "bottoms level");
        assert!(l.over_collapse(x + w - 20.0, y + 10.0) && !l.over_collapse(x + 20.0, y + 10.0));
    }

    #[test]
    fn frame_has_every_measure_and_core() {
        let v = vitals::canned(0);
        let f = frame(&open(Row::Cpu), &v, &col(), (5120, 1440), false).unwrap();
        let words: Vec<&str> = f.text.items.iter().map(|i| i.text.as_str()).collect();
        for w in ["RAM", "GPU util", "VRAM", "GPU temp", "CPU", "c00", "c31"] {
            assert!(words.contains(&w), "{w} missing");
        }
        // Tether, then five measures and 32 cores, each with a rule and 89 strokes.
        assert_eq!(f.segs.len(), 34 + (5 + 32) * 90);
        assert_eq!(f.alpha, 1.0);
    }

    #[test]
    fn missing_gpu_samples_break_the_line() {
        let mut v = vitals::canned(0);
        let mut s = vitals::canned_sample(1);
        s.gpu = None;
        v.ingest(erebus_protocol::Vitals { hw: vitals::canned_hw(), samples: vec![s] });
        let f = frame(&open(Row::Gpu), &v, &col(), (5120, 1440), false).unwrap();
        // The newest sample has no GPU: util, VRAM and temp each lose one stroke.
        assert_eq!(f.segs.len(), 34 + (5 + 32) * 90 - 3);
        let words: Vec<&str> = f.text.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(words.iter().filter(|w| **w == "no reading").count(), 3);
    }

    #[test]
    fn the_collapse_control_brightens_gradually() {
        let v = vitals::canned(0);
        let mut p = open(Row::Cpu);
        let color = |p: &Panel| frame(p, &v, &col(), (5120, 1440), false).unwrap().text.items.iter().find(|i| i.text == "— collapse").unwrap().color;
        assert_eq!(color(&p), vitals::LABEL);
        p.collapse_hover = true;
        assert!(p.busy());
        p.step(0.05, false);
        let c = color(&p);
        assert!(c != vitals::LABEL && c != theme::BONE, "between: {c:?}");
        p.step(0.2, false);
        assert_eq!(color(&p), theme::BONE);
        assert!(!p.busy());
        p.collapse_hover = false;
        p.step(0.05, false);
        assert!(color(&p) != theme::BONE, "and dims again gradually");
    }

    #[test]
    fn a_closed_panel_draws_nothing() {
        let v = vitals::canned(0);
        assert!(frame(&Panel::default(), &v, &col(), (5120, 1440), false).is_none());
    }
}
