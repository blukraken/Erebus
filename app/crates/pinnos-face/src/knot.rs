//! The knot: the nucleus at the screen centre. Ported from `drawKnot` and `drawHaze` in
//! `v3/concept-loom4.html`.
//!
//! Every dot lives in one static instance buffer built at startup. The vertex shader turns
//! and flows them from a few clock phases, so a frame uploads only [`Uniform`]. Glows, the
//! tick ring, flare light and the merge flash come from one quad shaded in `knot.wgsl`.
//!
//! The shell, trefoil and core turn slowly. Along the turning trefoil, two bright heads
//! travel in opposite directions, each a jittering cluster of dots. Every trail dot lights
//! by how far it lies behind each head, so each head drags a trail one loop long that stays
//! bright for a quarter loop, then fades slowly to a faint floor. The whole knot is always
//! traced, and a short dark gap runs in front of each head. The heads pass each other without
//! a flash. Depth only dims and shrinks the far side a little, continuously, so nothing
//! flares as it turns toward the viewer.
//!
//! The knot is pearl, so the Claude entity's ember stays distinct; only the centre is hot.
//!
//! State mapping:
//! - `live` eases toward 1 while the link is up with a context: pearl shell, trefoil, ticks
//!   and haze, hot centre. At 0 the knot is dormant: everything pearl dims toward `DIM`, the
//!   centre cools, the haze goes, and ambient motion runs at a third of the speed.
//! - A context change flares for [`FLARE_SECS`]: a ring for a branch, a pulse for a head, a
//!   shimmer for the changed count.
//! - Changed files are glyphs that turn with the shell; see [`crate::files`].
//!
//! The context gauge (ticks lit by tokens used) waits for sessions; the ring stays pearl.

use crate::files::{self, Files, Spin};
use bytemuck::{Pod, Zeroable};
use pinnos_protocol::{ChangedFile, GitState};
use std::f64::consts::TAU;

pub const FLARE_SECS: f32 = 0.6;
const LIVE_SECS: f32 = 0.9;
const LOD_SECS: f32 = 0.5;
const HOVER_SECS: f32 = 0.18;
/// Shell turn per ambient millisecond.
const TURN: f64 = 0.00016;
/// Head travel per ambient millisecond: one loop in 11 s while live, 0.35x that dormant.
const HEAD: f64 = TAU / 11000.0;
/// Dots in each head cluster; the rest of the trefoil is trail.
const HEAD_N: usize = 70;
/// Haze dots drawn at full detail; Glance fades out every other one.
const HAZE: u32 = 160;

const SHELL_N: usize = 820;
const TREFOIL_N: usize = 1400;
const CORE_N: usize = 420;
const HAZE_N: usize = 1100;

const G_SHELL: f32 = 0.0;
const G_TREFOIL: f32 = 1.0;
const G_CORE: f32 = 2.0;
const G_HAZE: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flare {
    Branch,
    Head,
    Changed,
}

/// The flare a git change earns: the strongest kind that differs.
pub fn flare_for(old: &GitState, new: &GitState) -> Option<Flare> {
    if old.branch != new.branch {
        Some(Flare::Branch)
    } else if old.head != new.head {
        Some(Flare::Head)
    } else if old.changed != new.changed {
        Some(Flare::Changed)
    } else {
        None
    }
}

/// One dot. `p` is its position, orbit or lane; `q` = (group, index in group, flag or
/// variance, kept in Glance).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Particle {
    p: [f32; 4],
    q: [f32; 4],
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

pub fn particles() -> Vec<Particle> {
    let mut out = Vec::with_capacity(SHELL_N + TREFOIL_N + CORE_N + HAZE_N);
    let dot = |p: [f64; 4], g: f32, i: usize, flag: f64, keep: bool| Particle {
        p: p.map(|v| v as f32),
        q: [g, i as f32, flag as f32, if keep { 1.0 } else { 0.0 }],
    };

    // Trefoil: p = (curve phase, offset from the strand, angle about it, head). Trail dots
    // (head -1) sit spread evenly along the curve. Head dots (head 0 or 1) hold a phase
    // offset from their head instead. The flag is a variance in 0..1 for size and jitter.
    let mut r = prng(4242);
    let trail = TREFOIL_N - 2 * HEAD_N;
    for i in 0..trail {
        let t = (i as f64 + r()) / trail as f64 * TAU;
        let off = 0.22 * r().cbrt();
        out.push(dot([t, off, r() * TAU, -1.0], G_TREFOIL, i, r(), i % 2 == 0));
    }
    for i in 0..2 * HEAD_N {
        // Denser toward the head, with a short tail behind it.
        let u = 2.0 * r() - 1.0;
        let lead = if u > 0.0 { 0.05 * u * u } else { -0.16 * u * u };
        let off = 0.2 * r().powf(1.5);
        let head = (i / HEAD_N) as f64;
        out.push(dot([lead, off, r() * TAU, head], G_TREFOIL, trail + i, r(), true));
    }

    // Shell: a Fibonacci sphere.
    for i in 0..SHELL_N {
        let y = 1.0 - 2.0 * (i as f64 + 0.5) / SHELL_N as f64;
        let q = (1.0 - y * y).sqrt();
        let th = i as f64 * 2.39996;
        out.push(dot([th.cos() * q, y, th.sin() * q, 0.0], G_SHELL, i, (i as f64 * 0.618_034) % 1.0, i % 2 == 0));
    }

    // Hot core: a ball denser toward the middle; every third dot is white.
    let mut r = prng(22);
    for i in 0..CORE_N {
        let u = r() * 2.0 - 1.0;
        let a = r() * TAU;
        let s = (1.0 - u * u).sqrt();
        let d = r().powf(2.2);
        let w = r();
        let white = if i % 3 == 0 { 1.0 } else { 0.0 };
        out.push(dot([a.cos() * s * d, u * d, a.sin() * s * d, w], G_CORE, i, white, true));
    }

    // Haze: (angle, radius in knot radii, phase, speed). Speeds are sixteenths, so the
    // twinkle clock can wrap without a seam.
    let mut r = prng(31);
    for i in 0..HAZE_N {
        let p = [r() * TAU, 1.22 + 0.9 * r().powf(1.6), r() * TAU, 0.5 + (r() * 16.0).floor() / 16.0];
        out.push(dot(p, G_HAZE, i, 0.0, i % 2 == 0));
    }

    out
}

/// `knot.wgsl` behind its palette, which comes from `pinnos_theme`.
pub fn shader() -> String {
    use pinnos_theme as theme;
    use std::fmt::Write;

    // Dormant pearl: pearl dimmed toward `DIM`.
    let (dim, pearl) = (theme::DIM.srgb_f32(), theme::PEARL.srgb_f32());
    let pearl_dim = [0, 1, 2].map(|i| dim[i] + 0.4 * (pearl[i] - dim[i]));
    let palette = [
        ("BONE", theme::BONE.srgb_f32()),
        ("PEARL", pearl),
        ("PEARL_HI", theme::PEARL_HI.srgb_f32()),
        ("PEARL_DIM", pearl_dim),
        ("EMBER", theme::EMBER.srgb_f32()),
        ("HOT", theme::HOT.srgb_f32()),
        ("HOT_CORE", theme::HOT_CORE.srgb_f32()),
    ];
    let mut s = String::new();
    for (name, [r, g, b]) in palette {
        let _ = writeln!(s, "const {name} = vec3f({r:.6}, {g:.6}, {b:.6});");
    }
    s + include_str!("knot.wgsl")
}

/// Per-frame knot uniform; see `knot.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Uniform {
    /// Target width, height, centre x, centre y.
    pub frame: [f32; 4],
    /// Knot radius, dot scale, line width, tick length unit; all physical pixels.
    pub scale: [f32; 4],
    /// Shell rotation, core jitter, twinkle and haze orbit phases.
    pub clock: [f32; 4],
    /// live, lod, reduced, haze count.
    pub state: [f32; 4],
    /// Branch ring, head pulse and changed shimmer progress (1 = idle), file hover fade.
    pub flare: [f32; 4],
    /// Event clock since the file epoch, hovered glyph (-1 for none), glyph count, merge
    /// flash running.
    pub files: [f32; 4],
    /// Head phase, head-dot spin about the strand (wraps at 16 turns), unused.
    pub aux: [f32; 4],
}

impl Uniform {
    pub fn radius(&self) -> f32 {
        self.scale[0]
    }
}

/// Knot radius at this target: 270 prototype pixels at 1440 tall, 540 px across at 5120.
pub fn radius(w: u32, h: u32, scale: f64) -> f32 {
    let s = scale as f32;
    let u = (h as f32 / s / 1440.0).min(w as f32 / s / 3400.0).clamp(0.2, 2.0);
    270.0 * u * s
}

fn approach(v: &mut f32, target: f32, step: f32) {
    *v = if *v < target { (*v + step).min(target) } else { (*v - step).max(target) };
}

/// Flare progress for the shader; a waiting flare reads as idle.
fn progress(f: f32) -> f32 {
    if f < 0.0 { 1.0 } else { f / FLARE_SECS }
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Knot animation state. Advances only while frames are drawn, so time never jumps.
#[derive(Clone, Debug)]
pub struct Sim {
    /// Ambient clock in prototype milliseconds; runs at a third of the speed while dormant.
    amb_ms: f64,
    live: f32,
    pub live_target: f32,
    lod: f32,
    pub lod_target: f32,
    /// Seconds into each flare: branch, head, changed. Negative is a flare still waiting.
    flares: [f32; 3],
    hover: f32,
    pub hover_target: f32,
    /// Event clock, seconds. Runs whenever frames are drawn, reduced motion or not.
    ev: f64,
    pub files: Files,
    /// Listed glyph under the cursor, and its fade.
    pub hovered: Option<usize>,
    file_hover: f32,
}

impl Default for Sim {
    fn default() -> Self {
        let idle = FLARE_SECS;
        Sim {
            amb_ms: 0.0,
            live: 0.0,
            live_target: 0.0,
            lod: 1.0,
            lod_target: 1.0,
            flares: [idle; 3],
            hover: 0.0,
            hover_target: 0.0,
            ev: 0.0,
            files: Files::default(),
            hovered: None,
            file_hover: 0.0,
        }
    }
}

impl Sim {
    pub fn set_time(&mut self, secs: f64) {
        self.amb_ms = secs * 1000.0;
    }

    pub fn flare(&mut self, f: Flare) {
        self.flares[f as usize] = 0.0;
    }

    /// Starts a flare after `delay` seconds.
    pub fn flare_after(&mut self, f: Flare, delay: f32) {
        self.flares[f as usize] = -delay.max(0.0);
    }

    /// Starts a flare part way through, for stills.
    pub fn flare_at(&mut self, f: Flare, progress: f32) {
        self.flares[f as usize] = progress.clamp(0.0, 1.0) * FLARE_SECS;
    }

    pub fn hover(&self) -> f32 {
        smooth(self.hover)
    }

    pub fn file_hover(&self) -> f32 {
        smooth(self.file_hover)
    }

    /// Shell rotation now, as the uniform carries it.
    pub fn rot(&self) -> f32 {
        (self.amb_ms * TURN % (10.0 * TAU)) as f32
    }

    /// Takes a new file list; see [`Files::update`]. A dash fires the head pulse as the first
    /// file arrives.
    pub fn set_files(&mut self, list: &[ChangedFile], changed: u32, commit: Option<&str>, dash: bool) -> files::Change {
        let rate = 0.35 + 0.65 * f64::from(smooth(self.live_target.max(self.live)));
        let spin = Spin { rot: f64::from(self.rot()), rate: TURN * 1000.0 * rate };
        let c = self.files.update(list, changed, commit, dash, self.ev, spin);
        if let Some(t) = c.first_arrival {
            self.flare_after(Flare::Head, t as f32);
        }
        // A hovered glyph's index may have moved.
        self.hovered = None;
        c
    }

    /// Ends the file hover fade at once, for stills.
    pub fn settle_hover(&mut self) {
        self.file_hover = if self.hovered.is_some() { 1.0 } else { 0.0 };
    }

    /// Runs every clock forward in small steps, for stills.
    pub fn advance(&mut self, secs: f64, reduced: bool) {
        let n = (secs * 240.0).round() as u32;
        for _ in 0..n {
            self.step(1.0 / 240.0, reduced);
        }
    }

    /// Jumps every transition to its end: used on resume, where missed events do not replay.
    pub fn snap(&mut self) {
        self.live = self.live_target;
        self.lod = self.lod_target;
        self.hover = self.hover_target;
        self.file_hover = if self.hovered.is_some() { 1.0 } else { 0.0 };
        self.flares = [FLARE_SECS; 3];
        // Every file event ends: grown, faded or merged.
        self.ev = self.ev.max(self.files.busy_until) + 0.001;
    }

    pub fn step(&mut self, dt: f32, reduced: bool) {
        let dt = dt.clamp(0.0, 0.1);
        if !reduced {
            let rate = 0.35 + 0.65 * f64::from(smooth(self.live));
            self.amb_ms += f64::from(dt) * 1000.0 * rate;
        }
        approach(&mut self.live, self.live_target, dt / LIVE_SECS);
        approach(&mut self.lod, self.lod_target, dt / LOD_SECS);
        approach(&mut self.hover, self.hover_target, dt / HOVER_SECS);
        let fh = if self.hovered.is_some() { 1.0 } else { 0.0 };
        approach(&mut self.file_hover, fh, dt / HOVER_SECS);
        self.ev += f64::from(dt);
        for f in &mut self.flares {
            *f = (*f + dt).min(FLARE_SECS);
        }
    }

    /// A transition is running, so frames must keep coming even with ambient motion off.
    pub fn busy(&self) -> bool {
        self.live != self.live_target
            || self.lod != self.lod_target
            || self.hover != self.hover_target
            || self.file_hover != if self.hovered.is_some() { 1.0 } else { 0.0 }
            || self.flares.iter().any(|&f| f < FLARE_SECS)
            || self.ev < self.files.busy_until
    }

    pub fn uniform(&self, w: u32, h: u32, scale: f64, reduced: bool) -> Uniform {
        let s = scale as f32;
        let u = (h as f32 / s / 1440.0).min(w as f32 / s / 3400.0).clamp(0.2, 2.0);
        let ds = (1.6 * u).max(1.0) * s;
        let lw = (1.6 * u).max(1.0) * s;
        let sc = 2.0 * u * s;
        let a = self.amb_ms;
        let wrap = |v: f64, period: f64| (v % period) as f32;
        Uniform {
            frame: [w as f32, h as f32, w as f32 * 0.5, h as f32 * 0.5],
            scale: [radius(w, h, scale), ds, lw, sc],
            clock: [
                wrap(a * TURN, 10.0 * TAU),
                wrap(a * 0.003, TAU),
                wrap(a * 0.0012, 16.0 * TAU),
                wrap(a * 0.00003, TAU),
            ],
            state: [smooth(self.live), smooth(self.lod), if reduced { 1.0 } else { 0.0 }, HAZE as f32],
            flare: [
                progress(self.flares[0]),
                progress(self.flares[1]),
                progress(self.flares[2]),
                self.file_hover(),
            ],
            files: [
                (self.ev - self.files.epoch) as f32,
                self.hovered.map_or(-1.0, |i| i as f32),
                self.files.instances().len() as f32,
                if self.ev < self.files.flash_until { 1.0 } else { 0.0 },
            ],
            aux: [wrap(a * HEAD, TAU), wrap(a * 0.003, 16.0 * TAU), 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTO_4242: [f64; 3] = [0.5467061335220933, 0.27860878920182586, 0.9312369171530008];

    #[test]
    fn prng_matches_prototype() {
        // mulberry32(4242) as the prototype computes it in JavaScript.
        let mut r = prng(4242);
        let got: Vec<f64> = (0..3).map(|_| r()).collect();
        assert_eq!(got, PROTO_4242);
    }

    #[test]
    fn flare_priority() {
        let g = |b: &str, h: &str, c| GitState {
            branch: Some(b.into()),
            head: Some(h.into()),
            changed: c,
            files: Vec::new(),
            ahead: None,
            behind: None,
        };
        assert_eq!(flare_for(&g("main", "a", 0), &g("dev", "b", 3)), Some(Flare::Branch));
        assert_eq!(flare_for(&g("main", "a", 0), &g("main", "b", 3)), Some(Flare::Head));
        assert_eq!(flare_for(&g("main", "a", 0), &g("main", "a", 3)), Some(Flare::Changed));
        assert_eq!(flare_for(&g("main", "a", 0), &g("main", "a", 0)), None);
    }

    #[test]
    fn time_is_monotonic_across_live_changes() {
        let mut s = Sim::default();
        s.step(0.05, false);
        let a = s.amb_ms;
        s.live_target = 1.0;
        s.step(0.05, false);
        assert!(s.amb_ms > a);
        s.step(0.05, true);
        let b = s.amb_ms;
        s.step(0.05, true);
        assert_eq!(s.amb_ms, b);
    }

    /// Parses and validates the shaders on the CPU, with no GPU device.
    #[test]
    fn shaders_validate() {
        use wgpu::naga::{front::wgsl, valid};
        for (name, src) in [("knot.wgsl", shader()), ("face.wgsl", include_str!("face.wgsl").into())] {
            let src = src.as_str();
            let module = wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
            valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::empty())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
        }
    }

    /// Knot colours come from the generated palette, never from literals in the shader.
    #[test]
    fn knot_palette_comes_from_theme() {
        let src = include_str!("knot.wgsl");
        assert!(!src.lines().any(|l| l.trim_start().starts_with("const") && l.contains("vec3f(")));
        let gen = shader();
        assert!(gen.contains("const PEARL = vec3f(0.870588, 0.858824, 0.827451);"), "{gen}");
        assert!(gen.contains("const PEARL_HI = vec3f(0.960784, 0.952941, 0.933333);"));
    }

    /// The uniform matches the WGSL struct: seven vec4s.
    #[test]
    fn uniform_size() {
        assert_eq!(size_of::<Uniform>(), 7 * 16);
        assert_eq!(size_of::<files::FileInst>(), 3 * 16);
    }

    #[test]
    fn commit_delays_the_head_pulse_to_the_first_arrival() {
        use pinnos_protocol::FileStatus;
        let f = |p: &str| ChangedFile { path: p.into(), status: FileStatus::Modified };
        let mut s = Sim { live_target: 1.0, ..Default::default() };
        s.snap();
        s.set_files(&[f("a"), f("b")], 2, None, true);
        s.snap();
        let c = s.set_files(&[], 0, Some("beef"), true);
        assert_eq!(c.dashed, 2);
        let u = s.uniform(5120, 1440, 1.0, false);
        assert_eq!(u.flare[1], 1.0, "pulse waits");
        assert!(s.busy());
        s.advance(c.first_arrival.unwrap() + 0.05, false);
        let u = s.uniform(5120, 1440, 1.0, false);
        assert!(u.flare[1] < 1.0, "pulse runs once the first file arrives");
        assert_eq!(u.files[3], 1.0, "merge flash runs");
        s.advance(3.0, false);
        assert!(!s.busy());
    }
}
