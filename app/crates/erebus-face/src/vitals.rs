//! Vitals stack: RAM, GPU and CPU rows in the left column, after Loom 4's `drawGauge`
//! (`v3/concept-loom4.html`) with live readings from the core.
//!
//! Each row is a mono text line over a braid of the last 60 s that flows into an instrument
//! for now. The braid is one quad per row, evaluated in `vitals.wgsl` from a 3 × 64 history
//! buffer written once per sample. Instruments are instanced segments, rebuilt on the CPU
//! and written only when they change. Text refreshes at most twice a second.

use bytemuck::{Pod, Zeroable};
use erebus_protocol::{Hardware, Sample, Vitals as Msg, VITALS_HISTORY};
use erebus_theme::{self as theme, Rgb};
use std::collections::VecDeque;
use std::f32::consts::PI;

pub const ROWS: usize = 3;
/// History slots per row; one second each, the newest last.
pub const SLOTS: usize = 64;
/// Instrument segments the buffer holds.
pub const MAX_SEGS: usize = 2048;
/// Panel strokes the panel buffer holds: 90 s sparklines for every measure and core.
pub const PANEL_SEGS: usize = 8192;

/// Easing time constants, seconds, as in the prototype.
const EASE_CORES: f32 = 0.22;
const EASE_GPU: f32 = 0.5;
const EASE_RAM: f32 = 0.4;
const TEXT_EVERY: f64 = 0.5;
/// Seconds the braid spans.
const SPAN: f32 = 60.0;
/// Braid phase, radians per second; motes cross in 1 / (0.06 × (0.4 + 1.6 load)) s.
const PHASE_RATE: f64 = 2.4;
const MOTE_RATE: f64 = 0.06;
/// Phase wraps here: a whole number of turns for both the tight (×1) and loose (×0.6) strands.
const PHASE_WRAP: f64 = 10.0 * std::f64::consts::PI;
/// A core burns ember this far above its 60 s mean, in percent.
const CORE_HOT: f32 = 14.0;

/// Bone-3 as the prototype lifts it inside the vitals column and on surfaces.
pub const LABEL: Rgb = Rgb::hex(0xb9ae98);
/// Tooltip ground, the prototype's `#vtTip` background.
pub const TIP_GROUND: Rgb = Rgb::hex(0x0e0c0b);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Ram,
    Gpu,
    Cpu,
}

impl Row {
    pub const ALL: [Row; ROWS] = [Row::Ram, Row::Gpu, Row::Cpu];

    pub fn parse(s: &str) -> Option<Row> {
        match s {
            "ram" => Some(Row::Ram),
            "gpu" => Some(Row::Gpu),
            "cpu" => Some(Row::Cpu),
            _ => None,
        }
    }

    /// Braid, instrument and left-rule colour.
    fn color(self) -> Rgb {
        match self {
            Row::Ram => theme::BONE_2,
            Row::Gpu => theme::EMBER,
            Row::Cpu => theme::BONE,
        }
    }

    /// Prototype gauge heights, before scaling.
    fn base_height(self) -> f32 {
        match self {
            Row::Ram => 104.0,
            Row::Gpu => 132.0,
            Row::Cpu => 116.0,
        }
    }
}

/// One row's boxes, physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowBox {
    /// The whole row `[x, y, w, h]`: hover target and left rule.
    pub rect: [f32; 4],
    /// Text line's left and top.
    pub text: [f32; 2],
    /// Braid and instrument `[x, y, w, h]`.
    pub gauge: [f32; 4],
}

/// The vitals column, physical pixels. Follows the prototype's `resize()` and
/// `sizeVitals()`: the column is the live feed's width, and the rows sit at the bottom of
/// the space between the band top and the feed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Column {
    /// Physical pixels per prototype CSS pixel, and the same times the type scale.
    pub px: f32,
    pub k: f32,
    /// The column's right edge; the tooltip opens past it.
    pub right: f32,
    /// Physical pixels per prototype `U`.
    pub u: f32,
    /// Band top and bottom: the space between the top rail and the bottom bar.
    pub band: [f32; 2],
    pub rows: [RowBox; ROWS],
}

impl Column {
    pub fn hit(&self, x: f32, y: f32) -> Option<Row> {
        Row::ALL.into_iter().zip(self.rows).find_map(|(row, b)| {
            let [rx, ry, rw, rh] = b.rect;
            (x >= rx && x < rx + rw && y >= ry && y < ry + rh).then_some(row)
        })
    }
}

/// `None` below the prototype's phone width, where the column is hidden.
pub fn column(w: u32, h: u32, scale: f64) -> Option<Column> {
    let s = scale as f32;
    let (lw, lh) = (w as f32 / s, h as f32 / s);
    if lw < 760.0 {
        return None;
    }
    let u = (lh / 1440.0).min(lw / 3400.0).clamp(0.2, 2.0);
    let kf = (1.45 * u).max(1.0);
    let wide = lw / lh >= 2.3 && lw >= 1700.0;
    let feed_w = if wide { (560.0 * u).round() } else { (lw * 0.17).clamp(200.0, 300.0).round() };

    // Band: under the 44 px top rail plus 26, over the 52 px bottom bar plus 8.
    let (y0, y1) = (70.0 * kf, lh - 60.0 * kf);
    let band_h = (y1 - y0).max(120.0);
    let feed_h = (band_h * 0.5).min(296.0 * kf).round();
    let vh = (y1 - feed_h - y0 - 14.0 * kf).max(0.0);
    let gap = (16.0 * u + 4.0).round();

    let line = 13.0 * kf * 1.3;
    let text_h = 3.0 * (line + 11.0 * kf);
    let base: f32 = Row::ALL.iter().map(|r| r.base_height()).sum();
    let sc = (1.25 * u).min(((vh - text_h - 2.0 * gap) / base).max(0.2));
    let gauge_h = Row::ALL.map(|r| (r.base_height() * sc).round());
    let row_h = gauge_h.map(|g| 3.0 * kf + line + 4.0 * kf + g + 4.0 * kf);

    let pad_l = (12.0 * kf).max(lw * 0.006);
    let row_w = feed_w - pad_l - 10.0 * kf;
    let inset = 2.0 + 10.0 * kf;
    let mut y = y0 + vh - row_h.iter().sum::<f32>() - 2.0 * gap;
    let rows = [0, 1, 2].map(|i| {
        let b = RowBox {
            rect: [pad_l * s, y * s, row_w * s, row_h[i] * s],
            text: [(pad_l + inset) * s, (y + 3.0 * kf) * s],
            gauge: [(pad_l + inset) * s, (y + 7.0 * kf + line) * s, (row_w - inset) * s, gauge_h[i] * s],
        };
        y += row_h[i] + gap;
        b
    });
    Some(Column { px: s, k: kf * s, right: feed_w * s, u: u * s, band: [y0 * s, y1 * s], rows })
}

/// Readings as drawn: eased toward the newest sample.
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    ram_mb: f32,
    cores: Vec<f32>,
    util: f32,
    vram_mb: f32,
    temp: f32,
    power: f32,
    gpu: bool,
}

impl Shown {
    fn of(s: &Sample) -> Shown {
        let g = s.gpu;
        Shown {
            ram_mb: s.ram_used_mb as f32,
            cores: s.cores.iter().map(|&c| f32::from(c)).collect(),
            util: g.map_or(0.0, |g| f32::from(g.util)),
            vram_mb: g.map_or(0.0, |g| g.vram_used_mb as f32),
            temp: g.map_or(0.0, |g| f32::from(g.temp_c)),
            power: g.map_or(0.0, |g| f32::from(g.power_w)),
            gpu: g.is_some(),
        }
    }

    fn cpu(&self) -> f32 {
        if self.cores.is_empty() { 0.0 } else { self.cores.iter().sum::<f32>() / self.cores.len() as f32 }
    }
}

/// Text for one frame. Changes at most twice a second, so the renderer keys layout on it.
#[derive(Clone, Debug, PartialEq)]
pub struct TextFrame {
    pub rows: Vec<RowText>,
    pub tip: Option<TipText>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowText {
    pub left: f32,
    pub top: f32,
    /// The label first, then each figure, with its colour.
    pub spans: Vec<(String, Rgb)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TipText {
    pub left: f32,
    pub top: f32,
    pub lines: Vec<String>,
}

/// One braid row for `vitals.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct RowUniform {
    /// Braid `[x, y, w, h]`: from the gauge's left edge to the instrument.
    pub rect: [f32; 4],
    /// Strand colour and the load above which hot specks show.
    pub color: [f32; 4],
    /// Load now 0..1, age of the newest sample in seconds, unused, unit `q` (height / 66).
    pub live: [f32; 4],
    /// Mote phase, mote count, dot size, one CSS pixel.
    pub motes: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Uniform {
    pub rows: [RowUniform; ROWS],
    /// Braid phase, wall seconds mod 60, reduced motion, unused.
    pub clock: [f32; 4],
    /// Target width and height.
    pub size: [f32; 4],
}

/// A straight stroke from `a` to `b` with butt ends. A square dot is a stroke as long as it
/// is wide.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Seg {
    pub ab: [f32; 4],
    /// Premultiplied colour.
    pub color: [f32; 4],
    /// Width, then unused.
    pub width: [f32; 4],
}

pub fn seg(a: [f32; 2], b: [f32; 2], c: Rgb, alpha: f32, width: f32) -> Seg {
    let [r, g, bl] = c.srgb_f32();
    Seg { ab: [a[0], a[1], b[0], b[1]], color: [r * alpha, g * alpha, bl * alpha, alpha], width: [width, 0.0, 0.0, 0.0] }
}

/// A round, anti-aliased dot of diameter `size`.
pub fn bead(x: f32, y: f32, size: f32, c: Rgb, alpha: f32) -> Seg {
    let mut s = seg([x, y], [x, y], c, alpha, size);
    s.width[1] = 1.0;
    s
}

/// A soft round glow; `size` spans about five standard deviations.
pub fn glow(x: f32, y: f32, size: f32, c: Rgb, alpha: f32) -> Seg {
    let mut s = seg([x, y], [x, y], c, alpha, size);
    s.width[1] = 2.0;
    s
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Everything the renderer needs for one frame.
pub struct Frame {
    pub uniform: Uniform,
    pub history: [f32; ROWS * SLOTS],
    /// Names the history contents; the buffer is written when it changes.
    pub generation: u64,
    pub segs: Vec<Seg>,
    pub text: TextFrame,
    /// Each row's left rule `[x, y, w, h]` and colour.
    pub rules: [([f32; 4], Rgb); ROWS],
}

/// Strings for each row and each row's tooltip, refreshed at most twice a second.
#[derive(Clone, Debug, Default)]
struct Strings {
    rows: [Vec<(String, Rgb)>; ROWS],
    tips: [Vec<String>; ROWS],
}

/// Vitals state: hardware, the last 90 samples, and what is drawn now.
#[derive(Clone, Debug, Default)]
pub struct Vitals {
    hw: Option<Hardware>,
    hist: VecDeque<Sample>,
    target: Shown,
    shown: Shown,
    /// Per-core mean over the last 60 samples.
    means: Vec<f32>,
    phase: f64,
    motes: [f64; ROWS],
    /// GPU nucleus turn, radians mod TAU; runs faster under load.
    stir: f64,
    /// Drawn time, seconds; paces the text.
    clock: f64,
    text_at: Option<f64>,
    strings: Strings,
    pub hover: Option<Row>,
    generation: u64,
}

fn gb(mb: f32) -> f32 {
    mb / 1024.0
}

/// GPU name without the vendor and brand: "RTX 5080".
pub fn short_gpu(name: &str) -> &str {
    let n = name.strip_prefix("NVIDIA ").unwrap_or(name);
    n.strip_prefix("GeForce ").unwrap_or(n)
}

impl Vitals {
    pub fn has_data(&self) -> bool {
        self.hw.is_some() && !self.hist.is_empty()
    }

    pub fn newest_ms(&self) -> Option<u64> {
        self.hist.back().map(|s| s.unix_ms)
    }

    pub fn hardware(&self) -> Option<&Hardware> {
        self.hw.as_ref()
    }

    /// The last 90 samples, oldest first.
    pub fn samples(&self) -> &VecDeque<Sample> {
        &self.hist
    }

    /// Changes whenever the samples do.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Forgets everything; the core replays its history on reconnect.
    pub fn clear(&mut self) {
        let hover = self.hover;
        *self = Vitals { hover, generation: self.generation + 1, ..Default::default() };
    }

    /// Takes a `Vitals` message. Samples at or before the newest held are replays and skipped.
    /// Returns how many samples were new.
    pub fn ingest(&mut self, msg: Msg) -> usize {
        let first = !self.has_data();
        self.hw = Some(msg.hw);
        let newest = self.hist.back().map_or(0, |s| s.unix_ms);
        let mut added = 0;
        for s in msg.samples.into_iter().filter(|s| first || s.unix_ms > newest) {
            self.hist.push_back(s);
            added += 1;
        }
        while self.hist.len() > VITALS_HISTORY {
            self.hist.pop_front();
        }
        let Some(last) = self.hist.back() else { return added };
        self.target = Shown::of(last);
        if first || self.shown.cores.len() != self.target.cores.len() {
            self.shown = self.target.clone();
        }
        let n = self.target.cores.len();
        let recent: Vec<&Sample> = self.hist.iter().rev().take(SPAN as usize).collect();
        self.means = (0..n)
            .map(|i| recent.iter().map(|s| f32::from(s.cores.get(i).copied().unwrap_or(0))).sum::<f32>() / recent.len() as f32)
            .collect();
        if first {
            self.text_at = None;
        }
        self.generation += 1;
        added
    }

    /// Eases the readings, runs the braid clocks, and refreshes text when due.
    pub fn step(&mut self, dt: f32, reduced: bool) {
        if !self.has_data() {
            return;
        }
        let dt = dt.max(0.0);
        let k = |tau: f32| 1.0 - (-dt / tau).exp();
        let (t, s) = (&self.target, &mut self.shown);
        let (kc, kg, kr) = (k(EASE_CORES), k(EASE_GPU), k(EASE_RAM));
        for (v, &tg) in s.cores.iter_mut().zip(&t.cores) {
            *v += (tg - *v) * kc;
        }
        for (v, tg) in [(&mut s.util, t.util), (&mut s.vram_mb, t.vram_mb), (&mut s.temp, t.temp), (&mut s.power, t.power)] {
            *v += (tg - *v) * kg;
        }
        s.ram_mb += (t.ram_mb - s.ram_mb) * kr;
        s.gpu = t.gpu;

        if !reduced {
            let dt = f64::from(dt);
            self.phase = (self.phase + dt * PHASE_RATE) % PHASE_WRAP;
            for row in Row::ALL {
                let v = f64::from(self.load(row));
                let m = &mut self.motes[row as usize];
                *m = (*m + dt * MOTE_RATE * (0.4 + 1.6 * v)) % 1.0;
            }
            let u = f64::from(self.shown.util / 100.0).clamp(0.0, 1.0);
            self.stir = (self.stir + dt * (0.12 + 1.1 * u)) % std::f64::consts::TAU;
        }
        self.clock += f64::from(dt);
        if self.text_at.is_none_or(|t| self.clock - t >= TEXT_EVERY) {
            self.text_at = Some(self.clock);
            self.strings = self.strings();
        }
    }

    /// Jumps the readings to the newest sample: on resume, nothing eases from stale values.
    pub fn snap(&mut self) {
        self.shown = self.target.clone();
        self.text_at = None;
        self.step(0.0, true);
    }

    fn ram_total_mb(&self) -> f32 {
        self.hw.as_ref().map_or(1.0, |h| h.ram_total_mb.max(1) as f32)
    }

    fn vram_total_mb(&self) -> Option<f32> {
        self.hw.as_ref()?.gpu.as_ref().map(|g| g.vram_total_mb.max(1) as f32)
    }

    /// Load now, 0..1, as the braid and motes read it.
    fn load(&self, row: Row) -> f32 {
        let s = &self.shown;
        match row {
            Row::Ram => s.ram_mb / self.ram_total_mb(),
            Row::Gpu => s.util / 100.0,
            Row::Cpu => s.cpu() / 100.0,
        }
        .clamp(0.0, 1.0)
    }

    fn sample_load(&self, row: Row, s: &Sample) -> f32 {
        match row {
            Row::Ram => s.ram_used_mb as f32 / self.ram_total_mb(),
            Row::Gpu => s.gpu.map_or(0.0, |g| f32::from(g.util) / 100.0),
            Row::Cpu => s.cpu / 100.0,
        }
        .clamp(0.0, 1.0)
    }

    fn strings(&self) -> Strings {
        let s = &self.shown;
        let total = self.ram_total_mb();
        let used = gb(s.ram_mb);
        let ram_row = vec![
            ("RAM".into(), LABEL),
            (format!("{used:.1}/{:.0}", gb(total)), theme::BONE),
            ("GB".into(), LABEL),
        ];
        let ram_tip = vec![format!("RAM {used:.1} / {:.1} GB used · {:.1} GB free", gb(total), gb(total - s.ram_mb).max(0.0))];

        let gpu_info = self.hw.as_ref().and_then(|h| h.gpu.as_ref());
        let (gpu_row, gpu_tip) = match (gpu_info, s.gpu) {
            (Some(g), true) => {
                let vt = gb(g.vram_total_mb as f32);
                (
                    vec![
                        ("GPU".into(), LABEL),
                        (format!("{:.0}%", s.util), theme::BONE),
                        (format!("{:.1}/{vt:.0} GB", gb(s.vram_mb)), LABEL),
                        (format!("{:.0}°C", s.temp), LABEL),
                    ],
                    vec![
                        format!("{} · util {:.0} % · {:.0} W · {:.0} °C", short_gpu(&g.name), s.util, s.power, s.temp),
                        format!("VRAM {:.1} / {vt:.1} GB", gb(s.vram_mb)),
                    ],
                )
            }
            (info, _) => (
                vec![("GPU".into(), LABEL), ("—".into(), LABEL)],
                vec![match info {
                    Some(g) => format!("{} · no reading", short_gpu(&g.name)),
                    None => "GPU · no reading (NVML unavailable)".into(),
                }],
            ),
        };

        let n = s.cores.len();
        let cpu = s.cpu();
        let cpu_row = vec![("CPU".into(), LABEL), (format!("{cpu:.0}%"), theme::BONE), (format!("{n} cores"), LABEL)];
        let busiest = s.cores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1));
        let mut cpu_tip = vec![format!("{n} cores · {cpu:.0} % overall")];
        if let Some((i, v)) = busiest {
            cpu_tip.push(format!("busiest core {i} · {v:.0} %"));
        }
        Strings { rows: [ram_row, gpu_row, cpu_row], tips: [ram_tip, gpu_tip, cpu_tip] }
    }

    /// Builds one frame at wall time `now_ms`, or `None` without data or a column.
    pub fn frame(&self, col: Option<&Column>, now_ms: u64, size: (u32, u32), reduced: bool) -> Option<Frame> {
        let col = col.filter(|_| self.has_data())?;
        let newest = self.hist.back()?;
        let age = ((now_ms.saturating_sub(newest.unix_ms)) as f32 / 1000.0).clamp(0.0, 2.0);

        let mut history = [0.0; ROWS * SLOTS];
        for row in Row::ALL {
            let slots = &mut history[row as usize * SLOTS..][..SLOTS];
            let n = self.hist.len().min(SLOTS);
            let from = self.hist.len() - n;
            let oldest = self.sample_load(row, &self.hist[from]);
            for (i, slot) in slots.iter_mut().enumerate() {
                *slot = match (i + n).checked_sub(SLOTS) {
                    Some(j) => self.sample_load(row, &self.hist[from + j]),
                    None => oldest,
                };
            }
        }

        let mut uniform = Uniform {
            clock: [self.phase as f32, ((now_ms % 60_000) as f32) / 1000.0, if reduced { 1.0 } else { 0.0 }, 0.0],
            size: [size.0 as f32, size.1 as f32, 0.0, 0.0],
            ..Default::default()
        };
        let mut segs = Vec::with_capacity(1024);
        for row in Row::ALL {
            let [gx, gy, gw, gh] = col.rows[row as usize].gauge;
            let q = gh / 66.0;
            let inst_w = match row {
                Row::Gpu => (gw * 0.3).min(gh),
                Row::Ram | Row::Cpu => (gw * 0.38).min(gh * 1.6),
            };
            let hx = gw - inst_w - 12.0 * q;
            let v = self.load(row);
            let dim = row == Row::Gpu && !self.shown.gpu;
            let color = if dim { theme::DIM } else { row.color() };
            let [r, g, b] = color.srgb_f32();
            let thr = if row == Row::Ram { 0.8 } else { 0.5 };
            let dd = (1.5 * q).max(col.px);
            uniform.rows[row as usize] = RowUniform {
                rect: [gx, gy, hx, gh],
                color: [r, g, b, thr],
                live: [v, age, 0.0, q],
                motes: [self.motes[row as usize] as f32, 2.0 + (v * 7.0).round(), dd, col.px],
            };
            let inst = Inst { x: gx + hx + 12.0 * q, y: gy, w: inst_w, h: gh, q, px: col.px };
            match row {
                Row::Ram => self.ram_inst(&inst, &mut segs),
                Row::Gpu => self.gpu_inst(&inst, &mut segs),
                Row::Cpu => self.cpu_inst(&inst, &mut segs),
            }
        }
        segs.truncate(MAX_SEGS);

        let rows = Row::ALL
            .map(|row| {
                let [left, top] = col.rows[row as usize].text;
                RowText { left, top, spans: self.strings.rows[row as usize].clone() }
            })
            .to_vec();
        let tip = self.hover.map(|row| TipText {
            left: col.right + 10.0 * col.k,
            top: col.rows[row as usize].rect[1],
            lines: self.strings.tips[row as usize].clone(),
        });
        let rules = Row::ALL.map(|row| {
            let [x, y, _, h] = col.rows[row as usize].rect;
            ([x, y, 2.0 * col.px, h], row.color())
        });
        Some(Frame { uniform, history, generation: self.generation, segs, text: TextFrame { rows, tip }, rules })
    }

    /// Settling reservoir: a rounded vessel filled with particles up to the share of RAM in
    /// use. The surface shimmers, particles drift slowly, and brass ticks on the left mark
    /// every 8 GB.
    fn ram_inst(&self, i: &Inst, out: &mut Vec<Seg>) {
        let (w, h) = ((i.w * 0.9).min(i.h * 1.5), i.h * 0.84);
        let (x0, y0) = (i.x + (i.w - w) * 0.5, i.y + (i.h - h) * 0.5);
        let rad = h * 0.18;
        let lw = (0.9 * i.q).max(i.px);
        let phase = self.phase as f32;

        // Vessel outline, corners as quarter arcs.
        let corners = [(x0 + w - rad, y0 + rad, -0.5), (x0 + w - rad, y0 + h - rad, 0.0), (x0 + rad, y0 + h - rad, 0.5), (x0 + rad, y0 + rad, 1.0)];
        let mut pts = Vec::with_capacity(4 * 7 + 1);
        for (cx, cy, a0) in corners {
            for j in 0..=6 {
                let t = (a0 + j as f32 / 12.0) * PI;
                pts.push([cx + t.cos() * rad, cy + t.sin() * rad]);
            }
        }
        pts.push(pts[0]);
        for p in pts.windows(2) {
            out.push(seg(p[0], p[1], theme::RULE_2, 0.9, lw));
        }

        let total = gb(self.ram_total_mb());
        let mut g = 8.0;
        while g < total {
            let y = y0 + h * (1.0 - g / total);
            out.push(seg([x0 - 5.0 * i.q, y], [x0 - 1.5 * i.q, y], theme::BRASS, 0.7, lw));
            g += 8.0;
        }

        // Inside test for the rounded rectangle, inset so particles clear the wall.
        let pad = 2.0 * i.q;
        let inside = |x: f32, y: f32| {
            let qx = ((x - (x0 + w * 0.5)).abs() - (w * 0.5 - rad)).max(0.0);
            let qy = ((y - (y0 + h * 0.5)).abs() - (h * 0.5 - rad)).max(0.0);
            (qx * qx + qy * qy).sqrt() < rad - pad
        };

        let fill = (self.shown.ram_mb / self.ram_total_mb()).clamp(0.0, 1.0);
        let level = y0 + h * (1.0 - fill);
        let dd = (1.1 * i.q).max(1.5 * i.px);
        let n = 420;
        for k in 0..n {
            let (fx, fy) = (hash01(k * 2), hash01(k * 2 + 1));
            let m = if k % 2 == 0 { 0.2 } else { 0.6 };
            let x = x0 + pad + fx * (w - 2.0 * pad) + 1.2 * dd * (phase * m + k as f32).sin();
            let y = y0 + pad + fy * (h - 2.0 * pad) + 0.8 * dd * (phase * m + k as f32 * 1.3).cos();
            if y < level + dd || !inside(x, y) {
                continue;
            }
            let near = 1.0 - ((y - level) / (h * 0.3)).clamp(0.0, 1.0);
            let c = mix(theme::BONE_2, theme::EMBER, near * 0.8);
            out.push(bead(x, y, dd, c, 0.45 + 0.4 * near));
        }

        if fill > 0.0 {
            let span = |x: f32| {
                let qx = ((x - (x0 + w * 0.5)).abs() - (w * 0.5 - rad)).max(0.0);
                let dy = (level - (y0 + h * 0.5)).abs() - (h * 0.5 - rad);
                dy <= 0.0 || qx * qx + dy * dy < rad * rad
            };
            let steps = 40;
            let mut prev: Option<[f32; 2]> = None;
            for j in 0..=steps {
                let x = x0 + w * j as f32 / steps as f32;
                let y = level + 0.9 * i.q * (x / (w * 0.12) + phase).sin();
                let p = [x, y];
                match prev {
                    Some(a) if span(x) && span(a[0]) => out.push(seg(a, p, theme::HOT, 0.85, lw * 1.3)),
                    _ => {}
                }
                prev = Some(p);
            }
            out.push(glow(x0 + w * 0.5, level, w * 0.9, theme::EMBER, 0.08));
        }
    }

    /// VRAM ring, one tick per GB, around a hot nucleus that swells, heats and stirs with
    /// utilisation. The ring's gap faces the braid and holds the temperature arc.
    fn gpu_inst(&self, i: &Inst, out: &mut Vec<Seg>) {
        let s = &self.shown;
        let (cx, cy) = (i.x + i.w * 0.5, i.y + i.h * 0.5);
        let rr = i.w.min(i.h) * 0.38;
        let at = |t: f32, r: f32| [cx + t.cos() * r, cy + t.sin() * r];
        let (start, sweep) = (-0.75 * PI, 1.5 * PI);

        let ticks = self.vram_total_mb().map_or(16, |mb| gb(mb).round().max(1.0) as usize);
        let vram = if s.gpu { quant(gb(s.vram_mb), 50.0) } else { 0.0 };
        let lw = (1.6 * i.q).max(i.px);
        for k in 0..ticks {
            let t = start + k as f32 / (ticks.max(2) - 1) as f32 * sweep;
            let on = (k as f32) < vram;
            let r1 = rr + if on { 7.0 } else { 4.0 } * i.q;
            out.push(if on { seg(at(t, rr), at(t, r1), theme::EMBER, 0.95, lw) } else { seg(at(t, rr), at(t, r1), theme::DIM, 0.55, lw) });
        }

        let temp = if s.gpu { quant(s.temp, 2.0) } else { 0.0 };
        let lw = i.q.max(i.px);
        for k in 0..10 {
            let f = k as f32 / 9.0;
            let t = 0.8 * PI + f * 0.4 * PI;
            let tick_c = 40.0 + 50.0 * f;
            let (a, b) = (at(t, rr + i.q), at(t, rr + 5.0 * i.q));
            out.push(if s.gpu && tick_c <= temp {
                let c = if tick_c > 80.0 { theme::VERM } else { mix(theme::DIM, theme::EMBER, 0.3 + 0.7 * (tick_c - 40.0) / 40.0) };
                seg(a, b, c, 0.85, lw)
            } else {
                seg(a, b, theme::DIM, 0.45, lw)
            });
        }

        // Nucleus: dots on a sunflower spiral inside a radius that grows with load. Inner
        // dots run white-hot, outer ones ember; each turns at 1-3x the stir clock, so the
        // pattern wraps without a seam.

        let u = if s.gpu { quant(s.util / 100.0, 400.0).clamp(0.0, 1.0) } else { 0.0 };
        let reach = rr * (0.16 + 0.62 * u.sqrt());
        let dd = (1.4 * i.q).max(2.6 * i.px);
        if s.gpu {
            out.push(glow(cx, cy, reach * 3.0, theme::EMBER, 0.3 + 0.2 * u));
            out.push(glow(cx, cy, reach * 1.4, theme::HOT, 0.3 + 0.35 * u));
        }

        // Dot count follows the nucleus area, so a small idle nucleus stays sparse and legible.

        let n = (18.0 + 92.0 * u) as usize;
        let golden = PI * (3.0 - 5f32.sqrt());
        let stir = self.stir as f32;
        for k in 0..n {
            let f = (k as f32 + 0.5) / n as f32;
            let m = (k % 3 + 1) as f32 * if k % 2 == 0 { 1.0 } else { -1.0 };
            let breathe = 1.0 + (0.06 + 0.1 * u) * (stir * 2.0 + k as f32 * 1.7).sin();
            let r = reach * f.sqrt() * breathe;
            let t = k as f32 * golden + stir * m;
            let heat = (1.0 - f) * (0.4 + 0.6 * u);
            let (c, a) = if s.gpu {
                (mix(theme::EMBER, theme::HOT, heat * 1.4), 0.3 + 0.6 * u * (1.0 - 0.5 * f) + 0.1 * (1.0 - f))
            } else {
                (theme::DIM, 0.4)
            };
            out.push(bead(cx + t.cos() * r, cy + t.sin() * r, dd, c, a.min(1.0)));
        }
        if s.gpu {
            out.push(glow(cx, cy, rr * (0.25 + 0.3 * u), theme::HOT_CORE, 0.3 + 0.6 * u));
        }
    }

    /// One tick column per logical core, ten ticks each. The top lit tick is hot; a core
    /// well above its 60 s mean burns ember.
    fn cpu_inst(&self, i: &Inst, out: &mut Vec<Seg>) {
        let cores = &self.shown.cores;
        let n = cores.len().max(1);
        let cw = i.w / n as f32;
        let th = i.h * 0.82;
        let y0 = i.y + (i.h - th) * 0.5;
        let lw = (1.3 * i.q).max(i.px);
        for (c, &v) in cores.iter().enumerate() {
            let lit = (v / 10.0).round() as usize;
            let hot = v > self.means.get(c).copied().unwrap_or(100.0) + CORE_HOT;
            let x = i.x + cw * (c as f32 + 0.5);
            for j in 0..10 {
                let y = y0 + th - (j as f32 + 0.5) * th / 10.0;
                let (a, b) = ([x - cw * 0.32, y], [x + cw * 0.32, y]);
                out.push(if j < lit {
                    let c = if j + 1 == lit { theme::HOT } else if hot { theme::EMBER } else { theme::BONE };
                    seg(a, b, c, 0.9, lw)
                } else {
                    seg(a, b, theme::DIM, 0.4, lw)
                });
            }
        }
    }
}

/// An instrument's box and units.
struct Inst {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    q: f32,
    px: f32,
}

/// Rounds to `1 / steps`, so eased values change the instruments a few times a second,
/// not every frame.
/// A fixed pseudo-random value in 0..1 for index `k`.
fn hash01(k: usize) -> f32 {
    let mut x = (k as u32).wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    (x & 0xFF_FFFF) as f32 / 16_777_216.0
}

fn quant(v: f32, steps: f32) -> f32 {
    (v * steps).round() / steps
}

/// `vitals.wgsl` behind its palette, which comes from `erebus_theme`.
pub fn shader() -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for (name, c) in [("HOT", theme::HOT), ("BRASS", theme::BRASS)] {
        let [r, g, b] = c.srgb_f32();
        let _ = writeln!(s, "const {name} = vec3f({r:.6}, {g:.6}, {b:.6});");
    }
    s + include_str!("vitals.wgsl")
}

/// Braid and instrument pipelines and their buffers. Each frame writes the uniform; the
/// history and segments are written only when they change.
///
/// Panel strokes share the instrument pipeline from a buffer of their own, drawn later,
/// over the panel ground.
pub struct Painter {
    braid: wgpu::RenderPipeline,
    segs: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    history: wgpu::Buffer,
    seg_buf: wgpu::Buffer,
    panel_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
    last_uniform: Option<Uniform>,
    last_generation: Option<u64>,
    last_segs: Vec<Seg>,
    last_panel: Option<u64>,
}

impl Painter {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, blend: wgpu::BlendState) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vitals.wgsl"),
            source: wgpu::ShaderSource::Wgsl(shader().into()),
        });
        let buffer = |label, size: usize, usage| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size as u64, usage, mapped_at_creation: false })
        };
        let copy = wgpu::BufferUsages::COPY_DST;
        let uniform = buffer("vitals", size_of::<Uniform>(), wgpu::BufferUsages::UNIFORM | copy);
        let history = buffer("vitals history", 4 * ROWS * SLOTS, wgpu::BufferUsages::STORAGE | copy);
        let seg_buf = buffer("vitals segments", size_of::<Seg>() * MAX_SEGS, wgpu::BufferUsages::VERTEX | copy);
        let panel_buf = buffer("panel segments", size_of::<Seg>() * PANEL_SEGS, wgpu::BufferUsages::VERTEX | copy);

        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vitals"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
            ],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vitals"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: history.as_entire_binding() },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vitals"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = |label, vs, fs, buffers: &[Option<wgpu::VertexBufferLayout>]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let braid = pipeline("vitals braids", "braid_vs", "braid_fs", &[]);
        let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let seg_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<Seg>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &attrs,
        };
        let segs = pipeline("vitals instruments", "seg_vs", "seg_fs", &[Some(seg_layout)]);
        Painter {
            braid,
            segs,
            uniform,
            history,
            seg_buf,
            panel_buf,
            bind,
            last_uniform: None,
            last_generation: None,
            last_segs: Vec::new(),
            last_panel: None,
        }
    }

    /// Writes panel strokes when `key` changes.
    pub fn prepare_panel(&mut self, queue: &wgpu::Queue, key: u64, segs: &[Seg]) {
        if self.last_panel != Some(key) {
            let n = segs.len().min(PANEL_SEGS);
            if n > 0 {
                queue.write_buffer(&self.panel_buf, 0, bytemuck::cast_slice(&segs[..n]));
            }
            self.last_panel = Some(key);
        }
    }

    pub fn draw_panel(&self, pass: &mut wgpu::RenderPass, count: usize) {
        let n = count.min(PANEL_SEGS) as u32;
        if n > 0 {
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_pipeline(&self.segs);
            pass.set_vertex_buffer(0, self.panel_buf.slice(..));
            pass.draw(0..4, 0..n);
        }
    }

    pub fn prepare(&mut self, queue: &wgpu::Queue, f: &Frame) {
        if self.last_uniform.as_ref() != Some(&f.uniform) {
            queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&f.uniform));
            self.last_uniform = Some(f.uniform);
        }
        if self.last_generation != Some(f.generation) {
            queue.write_buffer(&self.history, 0, bytemuck::cast_slice(&f.history));
            self.last_generation = Some(f.generation);
        }
        if f.segs != self.last_segs {
            if !f.segs.is_empty() {
                queue.write_buffer(&self.seg_buf, 0, bytemuck::cast_slice(&f.segs));
            }
            self.last_segs.clone_from(&f.segs);
        }
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass, f: &Frame) {
        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_pipeline(&self.braid);
        pass.draw(0..4, 0..ROWS as u32);
        if !f.segs.is_empty() {
            pass.set_pipeline(&self.segs);
            pass.set_vertex_buffer(0, self.seg_buf.slice(..));
            pass.draw(0..4, 0..f.segs.len() as u32);
        }
    }
}

// Canned history for screenshots and frame sequences: deterministic, shaped like this
// machine (31 GB usable, 32 cores, RTX 5080 16 GB).

/// Wall time of canned sample 0.
pub const CANNED_T0: u64 = 1_761_700_000_000;

fn nz(t: f64, s: f64) -> f64 {
    (t * 0.000_31 + s).sin() * 0.6 + (t * 0.001_17 + s * 2.3).sin() * 0.3 + (t * 0.0041 + s * 5.1).sin() * 0.1
}

/// Rises over 3 s into `[a, b]` seconds and falls over 3 s after.
fn burst(i: f64, a: f64, b: f64) -> f64 {
    ((i - a + 3.0) / 3.0).clamp(0.0, 1.0) * ((b + 3.0 - i) / 3.0).clamp(0.0, 1.0)
}

pub fn canned_hw() -> Hardware {
    Hardware {
        ram_total_mb: 31_640,
        cores: 32,
        gpu: Some(erebus_protocol::GpuInfo { name: "NVIDIA GeForce RTX 5080".into(), vram_total_mb: 16_303 }),
    }
}

/// Canned sample `i`, one second apart; 0 is "now" at [`CANNED_T0`].
pub fn canned_sample(i: i64) -> Sample {
    let t = i as f64 * 1000.0;
    let fi = i as f64;
    // A build from 38 s to 28 s ago loads two thirds of the cores.
    let build = burst(fi, -38.0, -28.0);
    let cores: Vec<u8> = (0..32)
        .map(|j| {
            let base = 4.0 + ((j * 37) % 23) as f64;
            let load = if j < 22 { 62.0 * build } else { 0.0 };
            (base + 5.0 * nz(t, j as f64 * 1.9) + load).clamp(1.0, 100.0).round() as u8
        })
        .collect();
    let cpu = cores.iter().map(|&c| f32::from(c)).sum::<f32>() / cores.len() as f32;
    // A render starts 20 s ago and holds the GPU near two thirds.
    let render = burst(fi, -20.0, 60.0);
    let util = (28.0 + 4.0 * nz(t, 7.7) + 36.0 * render).clamp(0.0, 100.0);
    Sample {
        unix_ms: (CANNED_T0 as i64 + i * 1000) as u64,
        ram_used_mb: (14_300.0 + 140.0 * nz(t, 3.3) + 1_100.0 * build) as u32,
        cpu,
        cores,
        gpu: Some(erebus_protocol::GpuSample {
            util: util.round() as u8,
            vram_used_mb: (8_400.0 + 900.0 * render) as u32,
            temp_c: (52.0 + 0.3 * util).round() as u8,
            power_w: (70.0 + 3.2 * util).round() as u16,
        }),
    }
}

/// Canned vitals holding samples `-89..=last`.
pub fn canned(last: i64) -> Vitals {
    let mut v = Vitals::default();
    v.ingest(Msg { hw: canned_hw(), samples: (last - 89..=last).map(canned_sample).collect() });
    v.snap();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use erebus_protocol::GpuSample;

    fn sample(ms: u64, cpu: f32) -> Sample {
        Sample { unix_ms: ms, ram_used_mb: 8_000, cpu, cores: vec![cpu as u8; 4], gpu: None }
    }

    fn hw() -> Hardware {
        Hardware { ram_total_mb: 16_000, cores: 4, gpu: None }
    }

    #[test]
    fn replays_are_skipped_and_history_is_capped() {
        let mut v = Vitals::default();
        let first: Vec<Sample> = (0..80).map(|i| sample(i * 1000, 10.0)).collect();
        assert_eq!(v.ingest(Msg { hw: hw(), samples: first.clone() }), 80);
        assert_eq!(v.ingest(Msg { hw: hw(), samples: first[70..].to_vec() }), 0);
        let more: Vec<Sample> = (80..100).map(|i| sample(i * 1000, 20.0)).collect();
        assert_eq!(v.ingest(Msg { hw: hw(), samples: more }), 20);
        assert_eq!(v.hist.len(), VITALS_HISTORY);
        assert_eq!(v.hist.back().unwrap().unix_ms, 99_000);
    }

    #[test]
    fn readings_ease_toward_the_newest_sample() {
        let mut v = Vitals::default();
        v.ingest(Msg { hw: hw(), samples: vec![sample(0, 0.0)] });
        v.ingest(Msg { hw: hw(), samples: vec![sample(1000, 100.0)] });
        v.step(EASE_CORES, false);
        let c = v.shown.cpu();
        assert!((c - 100.0 * (1.0 - (-1.0f32).exp())).abs() < 0.5, "one time constant: {c}");
        v.snap();
        assert_eq!(v.shown.cpu(), 100.0);
    }

    #[test]
    fn reduced_motion_freezes_phase_and_motes() {
        let mut v = canned(0);
        let (p, m) = (v.phase, v.motes);
        v.step(0.5, true);
        assert_eq!((v.phase, v.motes), (p, m));
        v.step(0.5, false);
        assert_ne!(v.phase, p);
    }

    #[test]
    fn text_refreshes_at_most_twice_a_second() {
        let mut v = canned(0);
        let before = v.text_at;
        v.step(0.2, false);
        assert_eq!(v.text_at, before);
        v.step(0.31, false);
        assert_ne!(v.text_at, before);
    }

    #[test]
    fn row_text_matches_the_owner_format() {
        let mut v = Vitals::default();
        let hw = Hardware {
            ram_total_mb: 31_640,
            cores: 32,
            gpu: Some(erebus_protocol::GpuInfo { name: "NVIDIA GeForce RTX 5080".into(), vram_total_mb: 16_303 }),
        };
        let s = Sample {
            unix_ms: 1,
            ram_used_mb: 14_541,
            cpu: 22.0,
            cores: vec![22; 32],
            gpu: Some(GpuSample { util: 63, vram_used_mb: 9_318, temp_c: 71, power_w: 288 }),
        };
        v.ingest(Msg { hw, samples: vec![s.clone()] });
        v.snap();
        let join = |r: &[(String, Rgb)]| r.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(join(&v.strings.rows[0]), "RAM 14.2/31 GB");
        assert_eq!(join(&v.strings.rows[1]), "GPU 63% 9.1/16 GB 71°C");
        assert_eq!(join(&v.strings.rows[2]), "CPU 22% 32 cores");
        assert_eq!(v.strings.tips[1][0], "RTX 5080 · util 63 % · 288 W · 71 °C");

        v.ingest(Msg { hw: v.hw.clone().unwrap(), samples: vec![Sample { unix_ms: 2, gpu: None, ..s }] });
        v.snap();
        assert_eq!(join(&v.strings.rows[1]), "GPU —");
    }

    #[test]
    fn column_fits_the_wide_screen() {
        let c = column(5120, 1440, 1.0).unwrap();
        assert_eq!(c.right, 560.0);
        let [_, y, _, h] = c.rows[2].rect;
        // Rows end above the reserved feed: band bottom 1353, feed 429 tall, 14 KF gap.
        assert!(y + h <= 1353.0 - 429.0 - 14.0 * 1.45 + 0.5, "{}", y + h);
        let g: Vec<f32> = c.rows.iter().map(|r| r.gauge[3]).collect();
        assert_eq!(g, vec![130.0, 165.0, 145.0]);
        assert!(column(700, 900, 1.0).is_none());
        assert_eq!(c.hit(100.0, c.rows[1].rect[1] + 5.0), Some(Row::Gpu));
        assert_eq!(c.hit(700.0, c.rows[1].rect[1] + 5.0), None);
    }

    #[test]
    fn instruments_count_this_machine() {
        let v = canned(0);
        let col = column(5120, 1440, 1.0).unwrap();
        let f = v.frame(Some(&col), CANNED_T0 + 400, (5120, 1440), false).unwrap();
        assert!(f.segs.len() < MAX_SEGS);
        // RAM: the reservoir holds more particles as more RAM is in use.
        let mut r = Vitals::default();
        let mut out = Vec::new();
        r.ingest(Msg { hw: canned_hw(), samples: vec![canned_sample(0)] });
        let ram = Inst { x: 0.0, y: 0.0, w: 190.0, h: 130.0, q: 2.0, px: 1.0 };
        r.shown.ram_mb = 4_000.0;
        r.ram_inst(&ram, &mut out);
        let low = out.len();
        out.clear();
        r.shown.ram_mb = 28_000.0;
        r.ram_inst(&ram, &mut out);
        assert!(out.len() > low + 50, "{low} -> {}", out.len());
        // CPU: 32 columns of 10.
        out.clear();
        r.cpu_inst(&Inst { x: 0.0, y: 0.0, w: 190.0, h: 145.0, q: 2.0, px: 1.0 }, &mut out);
        assert_eq!(out.len(), 320);
    }

    #[test]
    fn history_pads_with_the_oldest_and_ends_with_the_newest() {
        let mut v = Vitals::default();
        v.ingest(Msg { hw: hw(), samples: vec![sample(0, 10.0), sample(1000, 50.0)] });
        let col = column(5120, 1440, 1.0).unwrap();
        let f = v.frame(Some(&col), 1500, (5120, 1440), false).unwrap();
        let cpu = &f.history[2 * SLOTS..3 * SLOTS];
        assert_eq!((cpu[0], cpu[62], cpu[63]), (0.1, 0.1, 0.5));
        assert_eq!(f.uniform.rows[2].live[1], 0.5);
    }

    #[test]
    fn shader_validates() {
        use wgpu::naga::{front::wgsl, valid};
        let src = shader();
        let module = wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::empty())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
    }

    #[test]
    fn gpu_sizes_match_wgsl() {
        assert_eq!(size_of::<Uniform>(), 14 * 16);
        assert_eq!(size_of::<Seg>(), 3 * 16);
    }
}
