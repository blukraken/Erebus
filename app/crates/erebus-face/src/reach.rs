//! The end of a background task's reach off a session globe, after Loom 4's "tool reach":
//! a shell ends in the binding clasp, with a faint glow creeping out along the thread every
//! few seconds; a monitor ends in a small ring that sends slow ripples outward, listening.
//!
//! The thread itself is a fine two-strand braid drawn by `net`.

use crate::vitals::{self, Seg};
use erebus_protocol::TaskKind;
use erebus_theme::{self as theme, Rgb};
use std::f32::consts::{PI, TAU};

/// A glow creeps out a shell's thread once in this long, taking `CREEP_RUN` of it, seconds.
const CREEP_EVERY: f32 = 3.5;
const CREEP_RUN: f32 = 1.2;
/// A monitor sends a ripple once in this long, each living `RIPPLE_LIFE`, seconds.
const RIPPLE_EVERY: f32 = 3.0;
const RIPPLE_LIFE: f32 = 2.4;

pub fn color(kind: TaskKind) -> Rgb {
    match kind {
        TaskKind::Shell => theme::EMBER,
        TaskKind::Monitor => theme::LILAC,
    }
}

pub struct End {
    pub kind: TaskKind,
    /// Where the thread leaves the globe's rim, and where it ends.
    pub from: [f32; 2],
    pub to: [f32; 2],
    /// 0..1: how formed the end is.
    pub form: f32,
    /// Seconds since the task started, offset per task so siblings don't beat together.
    pub secs: f32,
    pub hover: f32,
    pub alpha: f32,
    /// Pixels per prototype `U`.
    pub u: f32,
    pub reduced: bool,
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn circle(out: &mut Vec<Seg>, c: [f32; 2], r: f32, col: Rgb, a: f32, w: f32) {
    let n = 28;
    for i in 0..n {
        let (a0, a1) = (i as f32 / n as f32 * TAU, (i + 1) as f32 / n as f32 * TAU);
        out.push(vitals::seg([c[0] + a0.cos() * r, c[1] + a0.sin() * r], [c[0] + a1.cos() * r, c[1] + a1.sin() * r], col, a, w));
    }
}

/// Draws a reach's end, and a shell's creeping glow along its thread.
pub fn end(out: &mut Vec<Seg>, e: &End) {
    let (sc, lws) = (2.0 * e.u, (1.6 * e.u).max(1.0));
    let k = smooth((e.form - 0.7) / 0.3) * e.alpha;
    if k <= 0.0 {
        return;
    }
    let grow = 1.0 + 0.14 * e.hover;
    let [x, y] = e.to;
    let (dx, dy) = (x - e.from[0], y - e.from[1]);
    let len = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let col = color(e.kind);
    match e.kind {
        TaskKind::Shell => {
            // The clasp: a bar across the thread's end, a shorter one just before it, and two
            // hot specks at the bar's tips.
            let w = (2.6 + 3.5) * sc * grow;
            out.push(vitals::seg([x + nx * w, y + ny * w], [x - nx * w, y - ny * w], col, 0.9 * k, 1.5 * lws));
            let (bx, by) = (x - ux * 6.0 * sc, y - uy * 6.0 * sc);
            out.push(vitals::seg([bx + nx * w * 0.7, by + ny * w * 0.7], [bx - nx * w * 0.7, by - ny * w * 0.7], col, 0.9 * k, lws));
            for s in [1.0, -1.0] {
                out.push(vitals::bead(x + nx * w * s, y + ny * w * s, 2.0 * lws, theme::HOT, 0.8 * k));
            }
            if !e.reduced {
                let p = e.secs % CREEP_EVERY;
                if p < CREEP_RUN {
                    let t = smooth(p / CREEP_RUN);
                    let (cx, cy) = (e.from[0] + dx * t, e.from[1] + dy * t);
                    out.push(vitals::glow(cx, cy, 12.0 * sc, col, 0.4 * k * (PI * p / CREEP_RUN).sin()));
                }
            }
        }
        TaskKind::Monitor => {
            let r = 4.5 * sc * grow;
            circle(out, e.to, r, col, 0.85 * k, 1.2 * lws);
            out.push(vitals::bead(x, y, 2.2 * lws, theme::HOT, 0.7 * k));
            if !e.reduced {
                // Ripples spread out from the ring, each fading as it widens.
                let mut t = e.secs % RIPPLE_EVERY;
                while t < RIPPLE_LIFE {
                    let q = t / RIPPLE_LIFE;
                    circle(out, e.to, r + (22.0 * sc - r) * q, col, 0.45 * (1.0 - q) * smooth(q / 0.1) * k, lws);
                    t += RIPPLE_EVERY;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(kind: TaskKind, form: f32, secs: f32) -> Vec<Seg> {
        let mut out = Vec::new();
        end(&mut out, &End { kind, from: [0.0, 0.0], to: [100.0, 0.0], form, secs, hover: 0.0, alpha: 1.0, u: 1.0, reduced: false });
        out
    }

    #[test]
    fn shells_and_monitors_end_differently() {
        let (shell, monitor) = (at(TaskKind::Shell, 1.0, 0.1), at(TaskKind::Monitor, 1.0, 0.1));
        let lilac = theme::LILAC.srgb_f32();
        let is = |s: &Seg, c: [f32; 3]| (s.color[1] / s.color[3].max(1e-6) - c[1]).abs() < 0.01;
        assert!(monitor.iter().filter(|s| is(s, lilac)).count() >= 28, "the ring");
        assert!(!shell.iter().any(|s| is(s, lilac)));
        assert!(shell.iter().filter(|s| s.width[1] == 0.0).count() >= 2, "the clasp's bars");
    }

    #[test]
    fn the_end_forms_late_and_without_a_pop() {
        assert!(at(TaskKind::Shell, 0.5, 0.0).is_empty(), "the thread grows first");
        let total = |f: f32| at(TaskKind::Monitor, f, 2.9).iter().map(|s| s.color[3]).sum::<f32>();
        let mut prev = total(0.7);
        for i in 1..=60 {
            let l = total(0.7 + 0.3 * i as f32 / 60.0);
            assert!((l - prev).abs() < total(1.0) * 0.06, "a step at {i}");
            prev = l;
        }
    }

    #[test]
    fn ripples_widen_and_fade() {
        let ring = |secs: f32| at(TaskKind::Monitor, 1.0, secs).len();
        assert!(ring(0.5) > ring(2.8), "a ripple is out early in the cycle and gone before the next");
    }
}
