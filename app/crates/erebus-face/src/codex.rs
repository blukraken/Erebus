//! A Codex session's globe, after Loom 4's `codexBall` (`v3/concept-loom4.html`): a frost
//! icosphere wireframe round a counter-turning icosahedron and an ice core, turning in steps
//! with a flash on each one, ringed by twelve double ticks. A headless run (`codex exec`) is
//! the hollow outer shell alone with six tick pairs.
//!
//! Drawn as network strokes; nothing here touches the GPU directly.

use crate::vitals::{self, Seg};
use erebus_theme as theme;
use std::f32::consts::{PI, TAU};
use std::sync::OnceLock;

/// Seconds per step of the turn, and the share of each step spent snapping round.
const PERIOD: f32 = 1.5;
const SNAP: f32 = 0.24;
/// Each step turns this far.
const STEP: f32 = TAU / 10.0;

pub struct Mesh {
    pub v: Vec<[f32; 3]>,
    pub e: Vec<[usize; 2]>,
}

fn edges(faces: &[[usize; 3]]) -> Vec<[usize; 2]> {
    let mut e: Vec<[usize; 2]> = faces.iter().flat_map(|f| [[f[0], f[1]], [f[1], f[2]], [f[2], f[0]]]).map(|[a, b]| [a.min(b), a.max(b)]).collect();
    e.sort_unstable();
    e.dedup();
    e
}

fn norm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / l, v[1] / l, v[2] / l]
}

const ICO_F: [[usize; 3]; 20] = [
    [0, 11, 5], [0, 5, 1], [0, 1, 7], [0, 7, 10], [0, 10, 11], [1, 5, 9], [5, 11, 4], [11, 10, 2], [10, 7, 6], [7, 1, 8],
    [3, 9, 4], [3, 4, 2], [3, 2, 6], [3, 6, 8], [3, 8, 9], [4, 9, 5], [2, 4, 11], [6, 2, 10], [8, 6, 7], [9, 8, 1],
];

fn ico1_verts() -> Vec<[f32; 3]> {
    let t = (1.0 + 5f32.sqrt()) / 2.0;
    [[-1.0, t, 0.0], [1.0, t, 0.0], [-1.0, -t, 0.0], [1.0, -t, 0.0], [0.0, -1.0, t], [0.0, 1.0, t], [0.0, -1.0, -t], [0.0, 1.0, -t], [t, 0.0, -1.0], [t, 0.0, 1.0], [-t, 0.0, -1.0], [-t, 0.0, 1.0]]
        .map(norm)
        .to_vec()
}

/// The icosahedron: 12 vertices, 30 edges.
pub fn ico1() -> &'static Mesh {
    static M: OnceLock<Mesh> = OnceLock::new();
    M.get_or_init(|| Mesh { v: ico1_verts(), e: edges(&ICO_F) })
}

/// One subdivision of it, projected onto the sphere: 42 vertices, 120 edges.
pub fn ico2() -> &'static Mesh {
    static M: OnceLock<Mesh> = OnceLock::new();
    M.get_or_init(|| {
        let mut v = ico1_verts();
        let mut mid = std::collections::HashMap::new();
        let mut m = |a: usize, b: usize, v: &mut Vec<[f32; 3]>| {
            *mid.entry((a.min(b), a.max(b))).or_insert_with(|| {
                let p = norm([(v[a][0] + v[b][0]) / 2.0, (v[a][1] + v[b][1]) / 2.0, (v[a][2] + v[b][2]) / 2.0]);
                v.push(p);
                v.len() - 1
            })
        };
        let mut faces = Vec::new();
        for [a, b, c] in ICO_F {
            let (ab, bc, ca) = (m(a, b, &mut v), m(b, c, &mut v), m(c, a, &mut v));
            faces.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        Mesh { e: edges(&faces), v }
    })
}

/// Prototype `eInOut`.
fn e_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// The turn and flash `secs` into a globe's life: whole steps plus an eased snap at the end
/// of each, and a flash that peaks just after the snap starts.
pub fn turn(secs: f32) -> (f32, f32) {
    let step = (secs / PERIOD).floor();
    let ph = secs / PERIOD - step;
    let snap = e_in_out((ph - (1.0 - SNAP)) / SNAP);
    let flash = (1.0 - (ph - 0.88).abs() / 0.12).max(0.0);
    ((step + snap) * STEP, flash)
}

pub struct Ball {
    pub c: [f32; 2],
    pub r: f32,
    pub alpha: f32,
    /// Seconds on the globe's own clock, and its seed's phase.
    pub secs: f32,
    pub phase: f32,
    pub headless: bool,
    /// 0..1: how far it has assembled, or how much is left as it breaks up.
    pub form: f32,
    pub leaving: bool,
    /// Pixels per prototype `U`.
    pub u: f32,
    pub reduced: bool,
}

/// A stable 0..1 value for element `k` of a mesh, so the same edge always assembles at the
/// same moment.
fn order(k: usize, salt: f32) -> f32 {
    let v = ((k as f32 + 1.0) * 12.9898 + salt * 78.233).sin() * 43758.547;
    v - v.floor()
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Draws one Codex globe into `out`.
///
/// Assembling (`form` rising), vertices spark in first, then each edge grows out of one of
/// its ends, the inner icosahedron after the shell and the ice core last. Leaving (`form`
/// falling), each edge shrinks to its middle and the vertices and broken edges drift
/// outward as frost dust.
pub fn ball(out: &mut Vec<Seg>, b: &Ball) {
    let (u, a, f) = (b.u, b.alpha, b.form.clamp(0.0, 1.0));
    if f <= 0.0 {
        return;
    }
    let (sc, lws, ds) = (2.0 * u, (1.6 * u).max(1.0), (1.6 * u).max(1.0));
    let (spin, flash) = if b.reduced { (0.0, 0.0) } else { turn(b.secs) };
    let rot = b.phase + spin;
    let [x, y] = b.c;
    // Leaving, everything drifts outward as it breaks up.
    let drift = if b.leaving { 1.0 + 0.6 * (1.0 - f) } else { 1.0 };
    out.push(vitals::glow(x, y, b.r * 3.2 * 2.0, theme::FROST, 0.34 * a * smooth(f / 0.6)));
    // `start..start + span` of `form` is when this mesh assembles.
    let wire = |out: &mut Vec<Seg>, m: &Mesh, rr: f32, ro: f32, tilt: f32, am: f32, lwm: f32, start: f32, span: f32| {
        let (sr, cr) = ro.sin_cos();
        let (st, ct) = tilt.sin_cos();
        let pts: Vec<[f32; 3]> = m
            .v
            .iter()
            .map(|v| {
                let (xx, zz) = (v[0] * cr - v[2] * sr, v[0] * sr + v[2] * cr);
                [x + xx * rr * drift, y + (v[1] * ct - zz * st) * rr * drift, v[1] * st + zz * ct]
            })
            .collect();
        let at = |k: usize, salt: f32, share: f32| smooth((f - start - order(k, salt) * span * share) / (span * (1.0 - share)));
        for (k, [i, j]) in m.e.iter().enumerate() {
            let q = at(k, 1.0, 0.6);
            if q <= 0.0 {
                continue;
            }
            let (p0, p1) = ([pts[*i][0], pts[*i][1]], [pts[*j][0], pts[*j][1]]);
            let (s0, s1) = if b.leaving { (0.5 - q / 2.0, 0.5 + q / 2.0) } else { (0.0, q) };
            let lerp = |t: f32| [p0[0] + (p1[0] - p0[0]) * t, p0[1] + (p1[1] - p0[1]) * t];
            let dd = (pts[*i][2] + pts[*j][2] + 2.0) / 4.0;
            let al = (0.12 + 0.62 * dd + 0.25 * flash * dd) * am * a;
            out.push(vitals::seg(lerp(s0), lerp(s1), theme::FROST, al, lwm * lws));
            if b.leaving && q < 1.0 {
                // The broken edge's dust, flung outward from the middle.
                let m = lerp(0.5);
                let (dx, dy) = (m[0] - x, m[1] - y);
                let push = 1.0 + 0.5 * (1.0 - q);
                out.push(vitals::bead(x + dx * push, y + dy * push, 1.2 * ds, theme::FROST, 0.6 * (1.0 - q) * f * am * a));
            }
        }
        for (k, p) in pts.iter().enumerate() {
            let v = at(k, 2.0, 0.5);
            if v <= 0.0 {
                continue;
            }
            let dd = (p[2] + 1.0) / 2.0;
            let col = if dd > 0.55 { theme::ICE } else { theme::FROST };
            // A spark: larger and brighter as it lands, settling to its resting size.
            let spark = if b.leaving { 1.0 } else { 1.0 + 1.5 * (1.0 - v) };
            out.push(vitals::bead(p[0], p[1], (0.9 + 1.5 * dd) * ds * spark, col, (0.25 + 0.7 * dd) * am * a * v.max(0.0)));
        }
    };
    if b.headless {
        wire(out, ico2(), b.r, rot, 0.38, 1.0, 1.0, 0.0, 1.0);
    } else {
        wire(out, ico2(), b.r, rot, 0.38, 1.0, 1.0, 0.0, 0.75);
        wire(out, ico1(), b.r * 0.42, -rot * 1.5 + 0.4, 0.5, 0.9, 1.1, 0.45, 0.45);
        let core = smooth((f - 0.8) / 0.2);
        if core > 0.0 {
            out.push(vitals::glow(x, y, b.r * 0.34 * 2.0, theme::ICE, (0.55 + 0.3 * flash) * a * core));
        }
    }
    // Double ticks: a hexagonal rhythm against Claude's 72 fine ticks. They draw in round
    // the ring as the globe assembles.
    let n = if b.headless { 6 } else { 12 };
    for i in 0..n {
        let k = smooth((f - 0.5 - 0.4 * i as f32 / n as f32) / 0.1);
        if k <= 0.0 {
            continue;
        }
        for o in [-0.035, 0.035] {
            let t = i as f32 / n as f32 * TAU + o - PI / 2.0;
            let (s, c) = t.sin_cos();
            let (r0, r1) = ((b.r + 5.0 * sc) * drift, (b.r + 10.0 * sc) * drift);
            out.push(vitals::seg([x + c * r0, y + s * r0], [x + c * r1, y + s * r1], theme::FROST, 0.5 * a * k, 1.2 * lws));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ball_at(headless: bool) -> Ball {
        Ball { c: [500.0, 300.0], r: 60.0, alpha: 1.0, secs: 0.3, phase: 0.0, headless, form: 1.0, leaving: false, u: 1.0, reduced: false }
    }

    #[test]
    fn meshes_match_the_prototype() {
        assert_eq!((ico1().v.len(), ico1().e.len()), (12, 30));
        assert_eq!((ico2().v.len(), ico2().e.len()), (42, 120));
        assert!(ico2().v.iter().all(|v| ((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt() - 1.0).abs() < 1e-5));
    }

    #[test]
    fn the_turn_steps_a_tenth_per_period_without_jumps() {
        let (r0, _) = turn(0.0);
        let (r1, _) = turn(PERIOD);
        assert!((r1 - r0 - STEP).abs() < 1e-4);
        // Still for most of the period, then a snap.
        assert_eq!(turn(PERIOD * 0.5).0, turn(0.0).0);
        // The eased snap peaks at three times its mean rate; nothing moves faster than that.
        let mut prev = turn(0.0).0;
        let dt = 1.0 / 120.0;
        let fastest = STEP * 3.0 * dt / (PERIOD * SNAP);
        for i in 1..(PERIOD * 3.0 / dt) as u32 {
            let r = turn(i as f32 * dt).0;
            assert!(r - prev >= -1e-5 && r - prev <= fastest * 1.01, "the turn jumps at {i}");
            prev = r;
        }
    }

    /// Total light and how far the drawing reaches from the centre, at one `form`.
    fn light(form: f32, leaving: bool, headless: bool) -> (f32, f32) {
        let mut out = Vec::new();
        ball(&mut out, &Ball { form, leaving, ..ball_at(headless) });
        let sum = out.iter().filter(|s| s.width[1] < 1.5).map(|s| s.color[3]).sum();
        let reach = out.iter().map(|s| ((s.ab[0] - 500.0).hypot(s.ab[1] - 300.0)).max((s.ab[2] - 500.0).hypot(s.ab[3] - 300.0))).fold(0.0, f32::max);
        (sum, reach)
    }

    #[test]
    fn it_assembles_and_breaks_up_without_steps() {
        for headless in [false, true] {
            let full = light(1.0, false, headless).0;
            assert_eq!(light(0.0, false, headless).0, 0.0, "nothing before it starts");
            for leaving in [false, true] {
                let mut prev = 0.0;
                for i in 1..=240 {
                    let l = light(i as f32 / 240.0, leaving, headless).0;
                    assert!((l - prev).abs() < full * 0.04, "a step at {i}: {prev} -> {l} (leaving {leaving})");
                    prev = l;
                }
                assert!((prev - full).abs() < full * 0.05);
            }
        }
        // Breaking up flings it outward; assembling stays in place.
        assert!(light(0.4, true, false).1 > light(1.0, false, false).1 * 1.15);
        assert!(light(0.4, false, false).1 <= light(1.0, false, false).1 + 1.0);
    }

    #[test]
    fn headless_is_the_hollow_shell() {
        let (mut full, mut hollow) = (Vec::new(), Vec::new());
        ball(&mut full, &ball_at(false));
        ball(&mut hollow, &ball_at(true));
        // Outer glow, 120 edges, 42 specks, and tick pairs; the full ball adds 30 edges,
        // 12 specks, the core glow and six more tick pairs.
        assert_eq!(hollow.len(), 1 + 120 + 42 + 12);
        assert_eq!(full.len(), 1 + 120 + 42 + 30 + 12 + 1 + 24);
        assert!(full.iter().chain(&hollow).all(|s| s.color[3] <= 1.0));
    }
}
