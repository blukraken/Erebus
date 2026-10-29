//! The agent network: a globe for every live session and its workers, floating free beside
//! the knot.
//!
//! Each globe is a body on a damped spring toward an anchor of its own: busy sessions near
//! the knot's right side, idle ones further out, dimmed and slowed. Globes push each other
//! apart, keep clear of the knot and stay inside the network area; when a group fills its
//! space, its globes shrink. Anchors sway slowly back and forth, so globes drift, and nothing
//! circles the knot.
//!
//! When a session goes busy the knot's nucleus sends a braid out to it like a feeler, which
//! latches and slowly hauls it in; after [`RELEASE_AFTER`] idle the braid flickers once and
//! lets go, and the globe drifts back out.
//! New sessions condense from dust; ended ones dissolve.
//!
//! Positions are in knot radii from the knot's centre, so the layout follows any window size.

use crate::globes::GlobeInst;
use crate::vitals::{self, Seg};
use erebus_protocol::{FeedEvent, SessionInfo, SessionStatus};
use erebus_theme::{self as theme, Rgb};
use std::f32::consts::{PI, TAU};

/// Idle this long, a busy-placed session is let go, milliseconds.
pub const RELEASE_AFTER: u64 = 5 * 60 * 1000;
/// The nucleus feels out to a globe for this long before it hauls; letting go, the braid
/// flickers, then unravels back to the knot.
const GROW: f32 = 2.6;
const FLICKER: f32 = 0.3;
const UNRAVEL: f32 = 0.8;
const CONDENSE: f32 = 0.7;
const WORKER_CONDENSE: f32 = 0.5;
const DISSOLVE: f32 = 1.0;
const REDUCED: f32 = 0.15;
/// Idle globes: opacity and size.
const REST_DIM: f32 = 0.4;
const REST_SCALE: f32 = 0.8;
/// Hover growth and its time constant, seconds.
const HOVER_GROW: f32 = 1.14;
const HOVER_TAU: f32 = 0.11;
const PING_SECS: f32 = 2.4;

// Sizes and distances in knot radii; the knot is 270 px at 5120×1440.
const SESSION_R: f32 = 64.0 / 270.0;
const WORKER_R: f32 = 40.0 / 270.0;
/// Workers sit this far from their parent, fanned ±40°.
const WORKER_D: f32 = 4.2 * SESSION_R;
/// Busy anchors: distance from the knot's centre and the half-spread of their angles.
const NEAR: f32 = 2.2;
const NEAR_SPREAD: f32 = 1.0;
/// Idle anchors are scattered over the open field to the right: across and down.
const FAR_X: [f32; 2] = [3.2, 8.0];
const FAR_Y: f32 = 1.9;
/// Clear space round the knot, and between globes.
const KNOT_CLEAR: f32 = 1.3;
const GAP: f32 = 0.12;
/// Globes a group holds at full size before they start to shrink, and the smallest they get.
const NEAR_ROOM: f32 = 4.0;
const FAR_ROOM: f32 = 7.0;
const MIN_SCALE: f32 = 0.5;
/// Spring stiffness: resting, holding near, and hauled by the braid. Damping is critical.
/// The haul takes up its strength over `HAUL_RAMP` seconds, so the pull starts slow.
const K_REST: f32 = 2.5;
const K_NEAR: f32 = 6.0;
const K_HAUL: f32 = 1.2;
const HAUL_RAMP: f32 = 2.5;
/// Reduced motion steps each globe by its force over this, straight to rest.
const RELAX: f32 = 14.0;
/// The haul gives up and docks this long after the braid latches, seconds.
const HAUL_MAX: f32 = 12.0;
/// Docked, the spring stiffens from the haul's strength to `K_NEAR` over this, seconds.
const SETTLE: f32 = 3.0;
/// Braid twist rate as a share of the prototype's `om`, the same in every phase.
const TWIST_PACE: f32 = 0.55;
/// A waiting session's braid crawls: it twists at this share of the steady rate, breathes
/// once in this long, seconds, dims by up to this share at the bottom of each breath, and
/// is drained of this share of its colour.
const WAIT_TWIST: f32 = 0.15;
const BREATH: f32 = 16.0;
const BREATH_DIM: f32 = 0.45;
const WAIT_GREY: f32 = 0.5;
/// Time constant of the ease into and out of waiting, seconds.
const WAIT_TAU: f32 = 0.8;
const PUSH: f32 = 40.0;
/// Arrived: this close to the anchor, in knot radii.
const ARRIVED: f32 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Resting,
    /// The braid feels out from the nucleus, then slowly hauls the globe in.
    Pulling,
    Docked,
    /// The braid flickers and lets go; the globe drifts out.
    Releasing,
    Dissolving,
}

#[derive(Clone, Debug)]
struct Worker {
    id: String,
    kind: String,
    form: f32,
    leaving: bool,
}

#[derive(Clone, Debug)]
struct Globe {
    id: String,
    label: String,
    /// Working: busy in a turn, or waiting on a program it started. Either holds it near.
    busy: bool,
    waiting: bool,
    /// 0..1, eased toward `waiting`; and seconds of breath while it shows.
    wait: f32,
    breath: f32,
    seed: u32,
    phase: Phase,
    /// Seconds into the current phase, and since it arrived (for the braid's fade).
    t: f32,
    arrived: Option<f32>,
    /// Position and velocity, knot radii.
    p: [f32; 2],
    v: [f32; 2],
    dim: f32,
    scale: f32,
    form: f32,
    /// Wall time of the last sign of work, milliseconds.
    active_ms: u64,
    workers: Vec<Worker>,
    hover: f32,
    /// Seconds since a feed click pinged it, while the ping lasts.
    ping: Option<f32>,
    /// Seconds of braid twist, advanced at one steady rate so the twist never jumps.
    twist: f32,
}

/// The network area in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Area {
    /// Left and right edges free for globes, and the band's top and bottom.
    pub x: [f32; 2],
    pub band: [f32; 2],
    /// Pixels per prototype `U`, the knot's centre and radius.
    pub u: f32,
    pub knot: [f32; 2],
    pub knot_r: f32,
}

impl Area {
    /// Right of the feed column, inside the window's padding.
    pub fn new(col: &vitals::Column, w: u32, h: u32, knot_r: f32) -> Area {
        let x0 = col.right + 24.0 * col.u;
        Area { x: [x0, w as f32 - 14.0 * col.k], band: col.band, u: col.u, knot: [w as f32 * 0.5, h as f32 * 0.5], knot_r }
    }

    fn px(&self, p: [f32; 2]) -> [f32; 2] {
        [self.knot[0] + p[0] * self.knot_r, self.knot[1] + p[1] * self.knot_r]
    }

    /// The area's edges in knot radii: left, right, top, bottom.
    fn bounds(&self) -> [f32; 4] {
        let k = |v: f32, c: f32| (v - c) / self.knot_r;
        [k(self.x[0], self.knot[0]), k(self.x[1], self.knot[0]), k(self.band[0], self.knot[1]), k(self.band[1], self.knot[1])]
    }
}

/// Every globe, and the clocks that drive them.
#[derive(Clone, Debug, Default)]
pub struct Net {
    globes: Vec<Globe>,
    /// Seconds of motion, for drift.
    clock: f32,
    /// Spin angle.
    rot: f32,
    /// The first snapshot after a clear is placed at once.
    placed: bool,
    /// The area of the last frame; motion waits for one.
    area: Option<Area>,
    pub hover: Option<String>,
}

fn hash(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

fn unit(seed: u32, salt: u32) -> f32 {
    let mut x = seed ^ salt.wrapping_mul(0x9e37_79b9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x as f32 / u32::MAX as f32
}

/// A seed as a phase offset in 0..TAU. A raw u32 seed in f32 is too coarse to add a
/// phase to.
fn phase(seed: u32) -> f32 {
    unit(seed, 7) * TAU
}

fn len(v: [f32; 2]) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

impl Globe {
    /// Held near the knot: hauled in, arrived, or about to be let go.
    fn near(&self) -> bool {
        match self.phase {
            Phase::Pulling => self.t >= GROW,
            Phase::Docked => true,
            Phase::Releasing => self.t < FLICKER,
            Phase::Resting | Phase::Dissolving => false,
        }
    }

    /// Where its spring pulls, swaying slowly on two unrelated periods.
    fn anchor(&self, clock: f32, reduced: bool) -> [f32; 2] {
        self.anchor_at(self.near() || (self.phase == Phase::Dissolving && self.dim > 0.7), clock, reduced)
    }

    fn anchor_at(&self, near: bool, clock: f32, reduced: bool) -> [f32; 2] {
        let s = phase(self.seed);
        let (sa, sb) = if reduced { (0.0, 0.0) } else { ((clock * 0.11 + s).sin(), (clock * 0.17 + s * 1.9).sin()) };
        if near {
            let ang = (unit(self.seed, 1) * 2.0 - 1.0) * NEAR_SPREAD + 0.07 * sa;
            let d = NEAR + 0.12 * sb;
            [d * ang.cos(), d * ang.sin()]
        } else {
            let x = FAR_X[0] + unit(self.seed, 2) * (FAR_X[1] - FAR_X[0]);
            let y = (unit(self.seed, 3) * 2.0 - 1.0) * FAR_Y;
            [x + 0.18 * sa, y + 0.14 * sb]
        }
    }

    /// Radius in knot radii, without hover; docked sessions with workers also hold their fan.
    fn body(&self) -> f32 {
        let r = SESSION_R * self.scale;
        if self.phase == Phase::Docked && !self.workers.is_empty() { (WORKER_D + WORKER_R) * self.scale } else { r }
    }
}

impl Net {
    pub fn new() -> Self {
        Net::default()
    }

    /// Forgets every globe; the core replays sessions on reconnect.
    pub fn clear(&mut self) {
        *self = Net::new();
    }

    /// Takes the core's snapshot of live sessions. `now_ms` is wall time.
    pub fn ingest_sessions(&mut self, sessions: &[SessionInfo], now_ms: u64) {
        let instant = !std::mem::replace(&mut self.placed, true);
        for s in sessions {
            let waiting = s.status == SessionStatus::Waiting;
            let busy = s.status == SessionStatus::Busy || waiting;
            let i = match self.globes.iter().position(|g| g.id == s.id && g.phase != Phase::Dissolving) {
                Some(i) => i,
                None => {
                    let mut g = Globe {
                        id: s.id.clone(),
                        label: s.label.clone(),
                        busy: false,
                        waiting: false,
                        wait: 0.0,
                        breath: 0.0,
                        seed: hash(&s.id),
                        phase: Phase::Resting,
                        t: 0.0,
                        arrived: None,
                        p: [FAR_X[0], 0.0],
                        v: [0.0; 2],
                        dim: REST_DIM,
                        scale: REST_SCALE,
                        form: if instant { 1.0 } else { 0.0 },
                        active_ms: s.started_unix_ms,
                        workers: Vec::new(),
                        hover: 0.0,
                        ping: None,
                        twist: 0.0,
                    };
                    g.p = g.anchor(self.clock, true);
                    self.globes.push(g);
                    self.globes.len() - 1
                }
            };
            let g = &mut self.globes[i];
            g.label.clone_from(&s.label);
            g.busy = busy;
            g.waiting = waiting;
            if busy {
                g.active_ms = g.active_ms.max(now_ms);
            }
            for w in &s.workers {
                if !g.workers.iter().any(|x| x.id == w.id && !x.leaving) {
                    g.workers.push(Worker { id: w.id.clone(), kind: w.kind.clone(), form: if instant { 1.0 } else { 0.0 }, leaving: false });
                }
            }
            for x in &mut g.workers {
                if !s.workers.iter().any(|w| w.id == x.id) {
                    x.leaving = true;
                }
            }
            if !s.workers.is_empty() {
                g.active_ms = g.active_ms.max(now_ms);
            }
        }
        for g in &mut self.globes {
            if g.phase != Phase::Dissolving && !sessions.iter().any(|s| s.id == g.id) {
                g.phase = Phase::Dissolving;
                g.t = 0.0;
            }
        }
        self.settle(now_ms);
        if instant {
            self.snap();
        }
    }

    /// Feed events are signs of work: they keep a session from being let go.
    pub fn note_activity(&mut self, events: &[FeedEvent]) {
        for e in events {
            if let Some(g) = self.globes.iter_mut().find(|g| g.id == e.session) {
                g.active_ms = g.active_ms.max(e.unix_ms);
            }
        }
    }

    /// Pulls busy sessions in and lets long-idle ones go.
    fn settle(&mut self, now_ms: u64) {
        for g in &mut self.globes {
            let idle_long = !g.busy && now_ms.saturating_sub(g.active_ms) >= RELEASE_AFTER;
            let next = match g.phase {
                Phase::Resting | Phase::Releasing if g.busy => Some(Phase::Pulling),
                Phase::Docked if idle_long => Some(Phase::Releasing),
                _ => None,
            };
            if let Some(p) = next {
                g.phase = p;
                g.t = 0.0;
                g.arrived = None;
            }
        }
    }

    /// A move, fade or ping runs. The spin and the slow sway ride the knot's own frames.
    pub fn busy(&self) -> bool {
        self.globes.iter().any(|g| {
            matches!(g.phase, Phase::Pulling | Phase::Releasing | Phase::Dissolving)
                || g.form < 1.0
                || len(g.v) > 0.01
                || g.workers.iter().any(|w| w.leaving || w.form < 1.0)
                || g.ping.is_some()
                || (g.hover - if self.hover.as_deref() == Some(&g.id) { 1.0 } else { 0.0 }).abs() > 0.001
        })
    }

    /// Crowding: each group's globes shrink once it holds more than its room.
    fn scales(&self) -> (f32, f32) {
        let live = |near: bool| self.globes.iter().filter(|g| g.phase != Phase::Dissolving && g.near() == near).count() as f32;
        let fit = |n: f32, room: f32| if n <= room { 1.0 } else { (room / n).sqrt().max(MIN_SCALE) };
        (fit(live(true), NEAR_ROOM), REST_SCALE * fit(live(false), FAR_ROOM))
    }

    /// Advances motion by `dt` seconds; `now_ms` drives letting go.
    pub fn step(&mut self, dt: f32, reduced: bool, now_ms: u64) {
        let dt = dt.clamp(0.0, 0.1);
        self.clock += dt;
        if !reduced {
            self.rot = (self.rot + dt * 0.4) % (TAU * 10.0);
        }
        let quick = |secs: f32| if reduced { REDUCED } else { secs };
        let hover = self.hover.clone();
        let (near_scale, far_scale) = self.scales();
        let ease = |tau: f32| if reduced { 1.0 } else { 1.0 - (-dt / tau).exp() };
        for g in &mut self.globes {
            g.t += dt;
            if !reduced {
                g.twist += dt * TWIST_PACE * (1.0 - (1.0 - WAIT_TWIST) * g.wait);
            }
            let w = if g.waiting { 1.0 } else { 0.0 };
            g.wait += (w - g.wait) * ease(WAIT_TAU);
            if (w - g.wait).abs() < 0.001 {
                g.wait = w;
            }
            // The breath starts from full each time waiting begins, and holds still in
            // reduced motion.
            g.breath = if g.wait <= 0.0 { 0.0 } else if reduced { g.breath } else { (g.breath + dt) % BREATH };
            if let Some(a) = &mut g.arrived {
                *a += dt;
            }
            let (target_form, span) = if g.phase == Phase::Dissolving { (0.0, DISSOLVE) } else { (1.0, CONDENSE) };
            g.form = if target_form > g.form { (g.form + dt / quick(span)).min(1.0) } else { (g.form - dt / quick(span)).max(0.0) };
            let near = g.near();
            let (dim, scale) = if near { (1.0, near_scale) } else { (REST_DIM, far_scale) };
            g.dim += (dim - g.dim) * ease(0.6);
            g.scale += (scale - g.scale) * ease(0.6);
            let h = if hover.as_deref() == Some(&g.id) { 1.0 } else { 0.0 };
            g.hover += (h - g.hover) * ease(HOVER_TAU);
            if (h - g.hover).abs() < 0.001 {
                g.hover = h;
            }
            if let Some(p) = &mut g.ping {
                *p += dt;
                if *p >= PING_SECS {
                    g.ping = None;
                }
            }
            let shown = g.phase == Phase::Docked;
            for w in &mut g.workers {
                let target = if shown && !w.leaving { 1.0 } else { 0.0 };
                let span = quick(if target > 0.0 { WORKER_CONDENSE } else { DISSOLVE * 0.6 });
                w.form = if target > w.form { (w.form + dt / span).min(1.0) } else { (w.form - dt / span).max(0.0) };
            }
            g.workers.retain(|w| !(w.leaving && w.form <= 0.0));
        }
        self.globes.retain(|g| !(g.phase == Phase::Dissolving && g.form <= 0.0));
        if let Some(area) = self.area {
            self.physics(dt, reduced, &area);
        }
        // Arrival and departure follow the bodies.
        let clock = self.clock;
        for g in &mut self.globes {
            let off = len([g.p[0] - g.anchor(clock, reduced)[0], g.p[1] - g.anchor(clock, reduced)[1]]);
            match g.phase {
                Phase::Pulling if g.t >= GROW && (off < ARRIVED || g.t > GROW + HAUL_MAX) => {
                    g.phase = Phase::Docked;
                    g.t = 0.0;
                    g.arrived = Some(0.0);
                }
                Phase::Releasing if g.t >= FLICKER && (off < ARRIVED * 2.0 || g.t > 6.0) => {
                    g.phase = Phase::Resting;
                    g.t = 0.0;
                }
                _ => {}
            }
        }
        self.settle(now_ms);
    }

    /// Springs toward the anchors, pushes apart, and keeps clear of the knot and the edges.
    fn physics(&mut self, dt: f32, reduced: bool, area: &Area) {
        let [x0, x1, y0, y1] = area.bounds();
        let n = self.globes.len();
        let mut force = vec![[0.0f32; 2]; n];
        for (i, g) in self.globes.iter().enumerate() {
            // Springs hand over by cross-fading, so the pull never snaps: rest to haul as
            // the braid latches, haul to hold as it docks, hold to rest as it lets go.
            let smooth = |t: f32, span: f32| {
                let x = (t / span).clamp(0.0, 1.0);
                x * x * (3.0 - 2.0 * x)
            };
            let (far, near) = (g.anchor_at(false, self.clock, reduced), g.anchor_at(true, self.clock, reduced));
            let springs = match g.phase {
                Phase::Pulling if g.t >= GROW => {
                    let x = smooth(g.t - GROW, HAUL_RAMP);
                    [(far, K_REST, 1.0 - x), (near, K_HAUL, x)]
                }
                Phase::Docked => {
                    let x = smooth(g.arrived.unwrap_or(SETTLE), SETTLE);
                    [(near, K_HAUL + (K_NEAR - K_HAUL) * x, 1.0), (near, 0.0, 0.0)]
                }
                Phase::Releasing if g.t >= FLICKER => {
                    let x = smooth(g.t - FLICKER, HAUL_RAMP);
                    [(near, K_NEAR, 1.0 - x), (far, K_REST, x)]
                }
                _ => {
                    let k = if g.near() { K_NEAR } else { K_REST };
                    [(g.anchor(self.clock, reduced), k, 1.0), (near, 0.0, 0.0)]
                }
            };
            for (a, k, w) in springs {
                force[i][0] += w * (k * (a[0] - g.p[0]) - 2.0 * k.sqrt() * g.v[0]);
                force[i][1] += w * (k * (a[1] - g.p[1]) - 2.0 * k.sqrt() * g.v[1]);
            }
            let r = g.body();
            for (j, o) in self.globes.iter().enumerate().skip(i + 1) {
                let d = [g.p[0] - o.p[0], g.p[1] - o.p[1]];
                let dl = len(d).max(1e-4);
                let over = r + o.body() + GAP - dl;
                if over > 0.0 {
                    let push = [d[0] / dl * over * PUSH, d[1] / dl * over * PUSH];
                    force[i][0] += push[0];
                    force[i][1] += push[1];
                    force[j][0] -= push[0];
                    force[j][1] -= push[1];
                }
            }
            let dk = len(g.p).max(1e-4);
            let over = KNOT_CLEAR + r - dk;
            if over > 0.0 {
                force[i][0] += g.p[0] / dk * over * PUSH;
                force[i][1] += g.p[1] / dk * over * PUSH;
            }
            let wall = |lo: f32, hi: f32, v: f32| if v - r < lo { (lo - (v - r)) * PUSH } else if v + r > hi { (hi - (v + r)) * PUSH } else { 0.0 };
            force[i][0] += wall(x0, x1, g.p[0]);
            force[i][1] += wall(y0, y1, g.p[1]);
        }
        for (g, f) in self.globes.iter_mut().zip(force) {
            if reduced {
                // No glide: straight to where the forces settle.
                g.v = [0.0; 2];
                g.p = [g.p[0] + f[0] / RELAX, g.p[1] + f[1] / RELAX];
                continue;
            }
            g.v = [g.v[0] + f[0] * dt, g.v[1] + f[1] * dt];
            g.p = [g.p[0] + g.v[0] * dt, g.p[1] + g.v[1] * dt];
        }
    }

    /// Finishes every move at once and settles the bodies apart.
    pub fn snap(&mut self) {
        let (near_scale, far_scale) = self.scales();
        for g in &mut self.globes {
            match g.phase {
                Phase::Pulling => g.phase = Phase::Docked,
                Phase::Releasing => g.phase = Phase::Resting,
                _ => {}
            }
            g.t = 0.0;
            g.arrived = None;
            g.v = [0.0; 2];
            let near = g.near();
            g.dim = if near { 1.0 } else { REST_DIM };
            g.scale = if near { near_scale } else { far_scale };
            g.form = if g.phase == Phase::Dissolving { 0.0 } else { 1.0 };
            g.p = g.anchor(self.clock, true);
            for w in &mut g.workers {
                w.form = if near && !w.leaving { 1.0 } else { 0.0 };
            }
            g.workers.retain(|w| !w.leaving);
        }
        self.globes.retain(|g| g.phase != Phase::Dissolving);
        if let Some(area) = self.area {
            for _ in 0..240 {
                self.physics(1.0 / 60.0, false, &area);
            }
            for g in &mut self.globes {
                g.v = [0.0; 2];
            }
        }
    }

    /// Rings a globe for a moment, for a feed row the person clicked.
    pub fn ping(&mut self, session: &str) {
        if let Some(g) = self.globes.iter_mut().find(|g| g.id == session) {
            g.ping = Some(0.0);
        }
    }

    /// Takes the window's network area. The first one settles every globe in place.
    pub fn set_area(&mut self, area: Area) {
        let first = self.area.is_none();
        self.area = Some(area);
        if first {
            self.snap();
        }
    }
}

/// What the cursor or a ping labels: the words and the point above the globe.
pub struct Label {
    pub text: String,
    pub at: [f32; 2],
    pub alpha: f32,
}

pub struct Frame {
    pub globes: Vec<GlobeInst>,
    pub segs: Vec<Seg>,
    /// Names `segs`; the painter uploads them when it changes.
    pub key: u64,
    pub label: Option<Label>,
}

/// One globe's drawn place and size in pixels.
struct Placed {
    c: [f32; 2],
    r: f32,
    a: f32,
}

impl Net {
    fn place(&self, g: &Globe, area: &Area) -> Placed {
        let r = SESSION_R * area.knot_r * g.scale * (1.0 + (HOVER_GROW - 1.0) * g.hover);
        Placed { c: area.px(g.p), r, a: g.dim }
    }

    fn worker_place(parent: &Placed, scale: f32, i: usize, n: usize, area: &Area) -> Placed {
        let ang = if n <= 1 { 0.0 } else { (-40.0 + 80.0 * i as f32 / (n - 1) as f32).to_radians() };
        let d = WORKER_D * area.knot_r * scale;
        Placed { c: [parent.c[0] + d * ang.cos(), parent.c[1] + d * ang.sin()], r: WORKER_R * area.knot_r * scale, a: parent.a }
    }

    /// The globe under `(x, y)`.
    pub fn hit(&self, area: &Area, x: f32, y: f32) -> Option<String> {
        self.globes.iter().rev().find_map(|g| {
            let p = self.place(g, area);
            let (dx, dy) = (x - p.c[0], y - p.c[1]);
            (g.form > 0.5 && dx * dx + dy * dy < p.r * p.r).then(|| g.id.clone())
        })
    }

    pub fn frame(&self, area: &Area, reduced: bool) -> Option<Frame> {
        if self.globes.is_empty() {
            return None;
        }
        let u = area.u;
        let sc = 2.0 * u;
        let lws = (1.6 * u).max(1.0);
        let mut globes = Vec::with_capacity(16);
        let mut segs = Vec::with_capacity(512);
        let mut label = None;
        for g in &self.globes {
            let p = self.place(g, area);
            let a = p.a * g.form.clamp(0.0, 1.0).sqrt();
            let [er, eg, eb] = theme::EMBER.srgb_f32();
            globes.push(GlobeInst {
                pos: [p.c[0], p.c[1], p.r, self.rot + phase(g.seed) * 0.37],
                color: [er, eg, eb, p.a],
                form: [0.0, g.form, if g.busy { 0.16 } else { 0.06 }, (g.seed % 997) as f32],
            });
            if a > 0.01 {
                segs.push(vitals::glow(p.c[0], p.c[1], 7.2 * p.r, theme::EMBER, 0.5 * a));
                segs.push(vitals::glow(p.c[0], p.c[1], 1.2 * p.r, theme::HOT, 0.55 * a));
                ticks(&mut segs, p.c, p.r + 5.0 * sc * g.scale, 72, 1.8 * sc * g.scale, 6, theme::EMBER, 0.32 * a, lws);
            }
            if let Some(t) = g.ping {
                let k = 1.0 - t / PING_SECS;
                let br = if reduced { 1.0 } else { 0.6 + 0.4 * (t * 8.0).sin() };
                ring(&mut segs, p.c, p.r + 12.0 * sc, theme::BONE, 0.9 * k * br, 1.6 * lws);
            }
            session_braid(&mut segs, area, g, &p, reduced, self.clock);

            let n = g.workers.len();
            for (i, w) in g.workers.iter().enumerate() {
                let wp = Self::worker_place(&p, g.scale, i, n, area);
                let wa = wp.a * w.form.sqrt();
                let [br, bg, bb] = theme::BONE.srgb_f32();
                globes.push(GlobeInst {
                    pos: [wp.c[0], wp.c[1], wp.r, self.rot * 1.3 + phase(g.seed + i as u32)],
                    color: [br, bg, bb, wp.a],
                    form: [1.0, w.form, 0.0, ((g.seed + 31 * i as u32) % 997) as f32],
                });
                if wa > 0.01 {
                    // The worker braid grows as the worker condenses and unravels as it goes.
                    let spec = Braid { strands: 3, fibres: 3, lam: 104.0, amp: 6.0 * g.scale, om: 3.0, width: 1.1, alpha: 0.8, col: theme::BONE };
                    braid(&mut segs, p.c, wp.c, 0.08, w.form, false, wp.a, &spec, area.u, g.twist, g.seed + i as u32, reduced);
                    segs.push(vitals::glow(wp.c[0], wp.c[1], 6.0 * wp.r, theme::BONE, 0.28 * wa));
                    ticks(&mut segs, wp.c, wp.r + 3.0 * sc * g.scale, 24, 1.5 * sc * g.scale, 0, theme::BONE, 0.3 * wa, lws);
                }
            }

            let show = g.hover.max(g.ping.map_or(0.0, |t| ((1.0 - t / PING_SECS) * 4.0).min(1.0)));
            if show > 0.01 && g.form > 0.5 {
                let state = if g.waiting { "waiting" } else if g.busy { "busy" } else { "idle" };
                let workers = match g.workers.iter().filter(|w| !w.leaving).count() {
                    0 => String::new(),
                    1 => format!(" · {}", g.workers[0].kind),
                    n => format!(" · {n} workers"),
                };
                label = Some(Label { text: format!("{} · {state}{workers}", g.label), at: [p.c[0], p.c[1] - p.r], alpha: show });
            }
        }
        globes.truncate(crate::globes::MAX_GLOBES);
        segs.truncate(vitals::PANEL_SEGS);
        let mut h = std::hash::DefaultHasher::new();
        for s in &segs {
            std::hash::Hash::hash(&bytemuck::bytes_of(s), &mut h);
        }
        Some(Frame { globes, segs, key: std::hash::Hasher::finish(&h), label })
    }
}

/// Fine ticks round a globe, every `major`th one 2.2 times longer; the first at the top.
#[allow(clippy::too_many_arguments)]
fn ticks(out: &mut Vec<Seg>, c: [f32; 2], r: f32, n: usize, len: f32, major: usize, col: Rgb, a: f32, w: f32) {
    for i in 0..n {
        let ang = -PI / 2.0 + i as f32 / n as f32 * TAU;
        let l = if major > 0 && i % major == 0 { len * 2.2 } else { len };
        let (s, co) = ang.sin_cos();
        out.push(vitals::seg([c[0] + co * r, c[1] + s * r], [c[0] + co * (r + l), c[1] + s * (r + l)], col, a, w));
    }
}

fn ring(out: &mut Vec<Seg>, c: [f32; 2], r: f32, col: Rgb, a: f32, w: f32) {
    let n = 64;
    for i in 0..n {
        let (a0, a1) = (i as f32 / n as f32 * TAU, (i + 1) as f32 / n as f32 * TAU);
        out.push(vitals::seg([c[0] + a0.cos() * r, c[1] + a0.sin() * r], [c[0] + a1.cos() * r, c[1] + a1.sin() * r], col, a, w));
    }
}

/// A braid's strand recipe, after Loom 4's `KIND` table: lengths in prototype pixels at
/// 5120 (scaled by `SC`), twist rate in radians a second.
struct Braid {
    strands: u32,
    fibres: u32,
    lam: f32,
    amp: f32,
    om: f32,
    width: f32,
    alpha: f32,
    col: Rgb,
}

/// The session braid from the knot's nucleus to a globe. When the knot pulls a session in,
/// the nucleus sends it out as a feeler: it creeps from the centre, its tip closed and
/// seeking, latches on the globe, and hauls it in. It holds while the session stays near,
/// and on letting go flickers and unravels back into the knot.
fn session_braid(out: &mut Vec<Seg>, area: &Area, g: &Globe, p: &Placed, reduced: bool, clock: f32) {
    let q = |secs: f32| if reduced { REDUCED } else { secs };
    let (reach, alpha) = match g.phase {
        Phase::Pulling => (e_out(g.t / q(GROW)), 1.0),
        Phase::Docked => (1.0, 1.0),
        Phase::Releasing if g.t < q(FLICKER) => (1.0, if reduced { 1.0 } else { 0.55 + 0.45 * (g.t * 60.0).sin().abs() }),
        Phase::Releasing => (1.0 - ((g.t - q(FLICKER)) / q(UNRAVEL)).clamp(0.0, 1.0), 1.0),
        Phase::Dissolving if g.dim > 0.7 => (g.form, 1.0),
        _ => return,
    };
    if reach <= 0.0 {
        return;
    }
    let (a0, a1) = (area.knot, p.c);
    // Waiting on a program, the braid greys a little and breathes slowly.
    let breath = 0.5 - 0.5 * (TAU * g.breath / BREATH).cos();
    let alpha = alpha * (1.0 - BREATH_DIM * g.wait * breath);
    let col = grey(theme::EMBER, WAIT_GREY * g.wait);
    let spec = Braid { strands: 6, fibres: 4, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col };
    let s = phase(g.seed);
    let mut bow = if g.seed & 1 == 0 { 0.1 } else { -0.1 };
    if !reduced {
        // The prototype's slow sway of the path, and the feeler's search while it reaches.
        bow += 0.02 * (clock * 0.35 + s).sin();
        if g.phase == Phase::Pulling {
            bow += 0.16 * (1.0 - reach) * (clock * 1.3 + s * 3.0).sin();
        }
    }
    let feeling = g.phase == Phase::Pulling && reach < 1.0;
    let tip = braid(out, a0, a1, bow, reach, feeling, alpha, &spec, area.u, g.twist, g.seed, reduced);
    if feeling {
        // A hot spark rides the feeler's tip.
        out.push(vitals::glow(tip[0], tip[1], 26.0 * area.u, theme::HOT, 0.9));
    }
}

/// `c` with share `k` of its colour drained toward a grey of the same luma.
fn grey(c: Rgb, k: f32) -> Rgb {
    let y = 0.299 * f32::from(c.0) + 0.587 * f32::from(c.1) + 0.114 * f32::from(c.2);
    let ch = |x: u8| (f32::from(x) + (y - f32::from(x)) * k.clamp(0.0, 1.0)).round() as u8;
    Rgb(ch(c.0), ch(c.1), ch(c.2))
}

/// Prototype `eOut`: a cubic ease out over 0..1.
fn e_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// A point on the gentle bow from `a0` to `a1`; `bow` bends it sideways by that share of
/// its length.
fn bez(a0: [f32; 2], a1: [f32; 2], bow: f32, t: f32) -> [f32; 2] {
    let m = [(a0[0] + a1[0]) * 0.5 - (a1[1] - a0[1]) * bow, (a0[1] + a1[1]) * 0.5 + (a1[0] - a0[0]) * bow];
    let u = 1.0 - t;
    [u * u * a0[0] + 2.0 * u * t * m[0] + t * t * a1[0], u * u * a0[1] + 2.0 * u * t * m[1] + t * t * a1[1]]
}

/// Loom 4's braid along a bow, as `drawEdge` draws it: twisting strands spaced evenly round
/// the path, loose fibres wider and slower, and a faint underglow. The envelope closes at
/// both ends of the whole path; strands dim on the far side of each twist. Drawn up to
/// `reach` of its length; a `feeling` braid also closes at its growing tip. Returns the tip.
#[allow(clippy::too_many_arguments)]
fn braid(out: &mut Vec<Seg>, a0: [f32; 2], a1: [f32; 2], bow: f32, reach: f32, feeling: bool, alpha: f32, b: &Braid, u: f32, twist: f32, seed: u32, reduced: bool) -> [f32; 2] {
    let sc = 2.0 * u;
    let lws = (1.6 * u).max(1.0);
    // Points along the bow, one every 14 prototype pixels, with arc length and unit normal.
    let chord = ((a1[0] - a0[0]).powi(2) + (a1[1] - a0[1]).powi(2)).sqrt();
    let n = ((chord * 1.05 / (14.0 * u).max(4.0)).ceil() as usize).clamp(8, 160);
    let pts: Vec<[f32; 2]> = (0..=n).map(|i| bez(a0, a1, bow, i as f32 / n as f32)).collect();
    let mut gl = vec![0.0f32; n + 1];
    for i in 1..=n {
        gl[i] = gl[i - 1] + ((pts[i][0] - pts[i - 1][0]).powi(2) + (pts[i][1] - pts[i - 1][1]).powi(2)).sqrt();
    }
    let total = gl[n].max(1.0);
    let normal = |i: usize| {
        let (p, q) = (pts[i.saturating_sub(1)], pts[(i + 1).min(n)]);
        let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
        let d = (dx * dx + dy * dy).sqrt().max(1e-4);
        [-dy / d, dx / d]
    };
    let ph = if reduced { phase(seed) * 0.3 } else { twist * b.om };
    let end = reach.clamp(0.0, 1.0) * total;
    let last = gl.iter().position(|&l| l >= end).unwrap_or(n).max(1);
    let taper = 40.0 * sc;
    let strand = |out: &mut Vec<Seg>, amp: f32, lam: f32, off: f32, a: f32, w: f32| {
        let at = |i: usize| {
            let s = gl[i] / total;
            let mut tap = (PI * s).sin().max(0.0).powf(0.7);
            if feeling {
                let x = ((end - gl[i]) / taper).clamp(0.0, 1.0);
                tap *= x * x * (3.0 - 2.0 * x);
            }
            let wv = TAU * gl[i] / lam - ph + off + phase(seed);
            let nn = normal(i);
            ([pts[i][0] + nn[0] * amp * tap * wv.sin(), pts[i][1] + nn[1] * amp * tap * wv.sin()], wv.cos())
        };
        for i in 0..last {
            let ((p0, d0), (p1, d1)) = (at(i), at(i + 1));
            let depth = ((d0 + d1) * 0.25 + 0.5).clamp(0.0, 1.0);
            out.push(vitals::seg(p0, p1, b.col, a * (0.5 + 0.5 * depth), w));
        }
    };
    let (amp, lam) = (b.amp * sc, b.lam * sc);
    // Underglow: one wide, faint band along the path. Soft strokes peak at a third of a
    // hard stroke's mean coverage, so the band carries three times the prototype's alpha.
    for i in 0..last {
        out.push(vitals::soft(pts[i], pts[i + 1], b.col, 0.15 * alpha, 2.0 * amp + 6.0 * sc));
    }
    for k in 0..b.strands {
        strand(out, amp, lam, k as f32 * TAU / b.strands as f32, b.alpha * alpha, b.width * lws);
    }
    for k in 0..b.fibres {
        let kf = k as f32;
        strand(out, amp * (1.7 + 0.35 * kf), lam * (1.45 + 0.2 * kf), (kf + 0.5) * TAU / b.fibres as f32, 0.3 * b.alpha * alpha, 0.7 * lws);
    }
    pts[last]
}

/// Canned sessions for screenshots: one busy with two workers, two idle.
pub fn canned() -> Vec<SessionInfo> {
    let s = |id: &str, label: &str, busy: bool, workers: usize| SessionInfo {
        id: id.into(),
        label: label.into(),
        cwd: format!("F:/Projects/{label}"),
        harness: "claude".into(),
        status: if busy { SessionStatus::Busy } else { SessionStatus::Idle },
        started_unix_ms: vitals::CANNED_T0 - 600_000,
        workers: (0..workers)
            .map(|i| erebus_protocol::WorkerInfo { id: format!("{id}-w{i}"), label: format!("w{}", i + 1), kind: "Explore".into(), started_unix_ms: vitals::CANNED_T0 })
            .collect(),
        pending: None,
    };
    vec![s("aabbb6cd", "erebus", true, 2), s("31616aa1", "witchbottle", false, 0), s("17764bb2", "stratum", false, 0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_761_700_000_000;

    fn area() -> Area {
        Area::new(&vitals::column(5120, 1440, 1.0).unwrap(), 5120, 1440, 270.0)
    }

    fn info(id: &str, busy: bool, workers: usize) -> SessionInfo {
        let mut s = canned().remove(0);
        s.id = id.into();
        s.label = id.into();
        s.status = if busy { SessionStatus::Busy } else { SessionStatus::Idle };
        s.workers.truncate(workers);
        s
    }

    fn net() -> Net {
        let mut n = Net::new();
        n.set_area(area());
        n
    }

    fn run(n: &mut Net, secs: f32, now: &mut u64) {
        for _ in 0..(secs * 60.0) as u32 {
            *now += 16;
            n.step(1.0 / 60.0, false, *now);
        }
    }

    fn get<'a>(n: &'a Net, id: &str) -> Option<&'a Globe> {
        n.globes.iter().find(|g| g.id == id)
    }

    fn phase(n: &Net, id: &str) -> Option<Phase> {
        get(n, id).map(|g| g.phase)
    }

    fn dist(n: &Net, id: &str) -> f32 {
        len(get(n, id).unwrap().p)
    }

    #[test]
    fn first_snapshot_places_at_once_near_and_far() {
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1), info("b", false, 0)], T0);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
        assert_eq!(phase(&n, "b"), Some(Phase::Resting));
        assert!(dist(&n, "a") < dist(&n, "b"), "busy sits nearer the knot");
        assert!(n.globes.iter().all(|g| g.form == 1.0));
        assert_eq!(n.globes[0].workers[0].form, 1.0);
    }

    #[test]
    fn busy_is_hauled_in_and_long_idle_drifts_out() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0)], now);
        let far = dist(&n, "a");
        n.ingest_sessions(&[info("a", true, 0)], now);
        assert_eq!(phase(&n, "a"), Some(Phase::Pulling));
        run(&mut n, GROW * 0.9, &mut now);
        assert!(dist(&n, "a") > far * 0.95, "the globe waits while the feeler reaches");
        run(&mut n, 2.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Pulling), "the haul is slow");
        run(&mut n, 10.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
        assert!(dist(&n, "a") < far * 0.7);
        n.ingest_sessions(&[info("a", false, 0)], now);
        run(&mut n, 2.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked), "a short idle keeps it near");
        now += RELEASE_AFTER;
        n.step(0.0, false, now);
        assert_eq!(phase(&n, "a"), Some(Phase::Releasing));
        run(&mut n, 6.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Resting));
        assert!(dist(&n, "a") > far * 0.8);
    }

    #[test]
    fn the_pull_runs_smooth_into_the_dock() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0)], now);
        n.ingest_sessions(&[info("a", true, 0)], now);
        let (mut v, mut twist, mut docked) = (get(&n, "a").unwrap().v, get(&n, "a").unwrap().twist, false);
        let mut acc = [0.0f32; 2];
        for i in 0..60 * 16 {
            now += 16;
            if i == 60 * 6 {
                // Busy flips mid-haul; the twist keeps its pace.
                n.ingest_sessions(&[info("a", false, 0)], now);
            }
            n.step(1.0 / 60.0, false, now);
            let g = get(&n, "a").unwrap();
            // A snap shows as the acceleration changing at once between steps.
            let a = [g.v[0] - v[0], g.v[1] - v[1]];
            let jerk = len([a[0] - acc[0], a[1] - acc[1]]);
            assert!(jerk < 0.002, "the pull snaps by {jerk} at step {i}, phase {:?}", g.phase);
            acc = a;
            assert!((g.twist - twist - TWIST_PACE / 60.0).abs() < 1e-4, "twist jumps at step {i}");
            (v, twist) = (g.v, g.twist);
            docked |= g.phase == Phase::Docked;
        }
        assert!(docked);
    }

    #[test]
    fn a_waiting_session_is_held_near_and_its_braid_breathes() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0)], now);
        let mut s = info("a", false, 0);
        s.status = SessionStatus::Waiting;
        n.ingest_sessions(&[s], now);
        run(&mut n, GROW + HAUL_MAX + 1.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked), "waiting pulls the session in");
        assert_eq!(get(&n, "a").unwrap().wait, 1.0);
        // The braid's mean alpha over one breath: bright, dim at the half, bright again,
        // with no step between frames.
        let alpha = |n: &Net| {
            let f = n.frame(&area(), false).unwrap();
            let c = theme::EMBER.srgb_f32();
            f.segs.iter().filter(|x| x.width[1] == 0.0 && (x.color[0] / x.color[3].max(1e-6) - c[0]).abs() > 0.02).map(|x| x.color[3]).sum::<f32>()
        };
        let (mut series, mut at) = (Vec::new(), Vec::new());
        for _ in 0..(BREATH * 60.0) as u32 {
            series.push(alpha(&n));
            at.push(get(&n, "a").unwrap().breath);
            run(&mut n, 1.0 / 60.0, &mut now);
        }
        let (hi, lo) = (series.iter().cloned().fold(0.0, f32::max), series.iter().cloned().fold(f32::MAX, f32::min));
        assert!(lo < hi * 0.7, "the braid breathes: {lo} vs {hi}");
        let dimmest = series.iter().position(|&x| x == lo).unwrap();
        assert!((at[dimmest] - BREATH * 0.5).abs() < 0.3, "dimmest at the half breath, not {}", at[dimmest]);
        for w in series.windows(2) {
            assert!((w[1] - w[0]).abs() < hi * 0.02, "the breath steps");
        }
        // Back to idle, the colour and breath ease out.
        n.ingest_sessions(&[info("a", false, 0)], now);
        run(&mut n, 6.0, &mut now);
        assert_eq!(get(&n, "a").unwrap().wait, 0.0);
    }

    #[test]
    fn feed_activity_holds_a_session_near() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.ingest_sessions(&[info("a", false, 0)], now);
        now += RELEASE_AFTER - 1000;
        let ev = FeedEvent { unix_ms: now, kind: erebus_protocol::FeedKind::Tool, phrase: "a · Read x".into(), session: "a".into(), worker: None, backlog: false };
        n.note_activity(&[ev]);
        now += 2000;
        n.step(0.0, false, now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
    }

    #[test]
    fn globes_keep_apart_and_shrink_when_crowded() {
        let mut now = T0;
        let mut n = net();
        let few: Vec<SessionInfo> = (0..3).map(|i| info(&format!("s{i}"), true, 0)).collect();
        n.ingest_sessions(&few, now);
        let roomy = n.globes[0].scale;
        let many: Vec<SessionInfo> = (0..9).map(|i| info(&format!("s{i}"), true, 0)).collect();
        n.ingest_sessions(&many, now);
        run(&mut n, 8.0, &mut now);
        assert!(n.globes[0].scale < roomy * 0.8, "crowded globes shrink: {} vs {roomy}", n.globes[0].scale);
        for (i, a) in n.globes.iter().enumerate() {
            assert!(len(a.p) > KNOT_CLEAR, "clear of the knot");
            for b in &n.globes[i + 1..] {
                let d = len([a.p[0] - b.p[0], a.p[1] - b.p[1]]);
                assert!(d > (a.body() + b.body()) * 0.8, "{} and {} overlap: {d}", a.id, b.id);
            }
        }
    }

    #[test]
    fn globes_stay_inside_the_area() {
        let mut now = T0;
        let mut n = net();
        let all: Vec<SessionInfo> = (0..12).map(|i| info(&format!("s{i}"), i % 3 == 0, 0)).collect();
        n.ingest_sessions(&all, now);
        run(&mut n, 6.0, &mut now);
        let a = area();
        for g in &n.globes {
            let c = a.px(g.p);
            assert!(c[0] > a.x[0] && c[0] < a.x[1] && c[1] > a.band[0] && c[1] < a.band[1], "{} at {c:?}", g.id);
        }
    }

    #[test]
    fn ended_sessions_dissolve_and_go() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0), info("b", false, 0)], now);
        n.ingest_sessions(&[info("b", false, 0)], now);
        assert_eq!(phase(&n, "a"), Some(Phase::Dissolving));
        run(&mut n, 1.5, &mut now);
        assert_eq!(phase(&n, "a"), None);
    }

    #[test]
    fn workers_condense_beside_a_docked_parent_and_dissolve_when_done() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.ingest_sessions(&[info("a", true, 1)], now);
        assert_eq!(n.globes[0].workers[0].form, 0.0);
        run(&mut n, 1.0, &mut now);
        assert_eq!(n.globes[0].workers[0].form, 1.0);
        n.ingest_sessions(&[info("a", true, 0)], now);
        run(&mut n, 1.0, &mut now);
        assert!(n.globes[0].workers.is_empty());
    }

    #[test]
    fn a_new_session_condenses() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0)], now);
        n.ingest_sessions(&[info("a", false, 0), info("b", false, 0)], now);
        assert_eq!(get(&n, "b").unwrap().form, 0.0);
        run(&mut n, 1.0, &mut now);
        assert_eq!(get(&n, "b").unwrap().form, 1.0);
    }
}
