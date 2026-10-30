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

use crate::approval;
use crate::codex;
use crate::reach;
use crate::globes::GlobeInst;
use crate::vitals::{self, Seg};
use erebus_protocol::{FeedEvent, Pending, SessionInfo, SessionStatus, TaskKind};
use erebus_theme::{self as theme, Rgb};
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};

/// Idle this long, a busy-placed session is let go, milliseconds.
pub const RELEASE_AFTER: u64 = 5 * 60 * 1000;
/// The nucleus feels out to a globe for this long before it hauls; letting go, the braid
/// flickers, then unravels back to the knot.
const GROW: f32 = 2.6;
const FLICKER: f32 = 0.3;
const UNRAVEL: f32 = 0.8;
const CONDENSE: f32 = 1.5;
/// On the first snapshot, each globe starts condensing this long after the one before.
const STAGGER: f32 = 0.35;
/// A worker glides out of its parent's nucleus to its slot over this long, growing and
/// condensing as it goes; done, it turns to dust where it stands over `WORKER_DISSOLVE`.
const WORKER_EMERGE: f32 = 4.0;
const WORKER_DISSOLVE: f32 = 3.0;
/// A pulse runs a session braid, globe to core, in this long, and a worker braid, worker to
/// parent, in this long, seconds.
const PULSE_SECS: f32 = 2.2;
const WORKER_PULSE_SECS: f32 = 1.4;
/// Pulses on one braid launch at least this far apart, seconds, so events from one instant
/// run one behind another in feed order.
const PULSE_GAP: f32 = 0.35;
/// Pulses travelling on one braid at once, and waiting behind them.
const PULSE_LIVE: usize = 6;
const PULSE_QUEUE: usize = 24;
/// A pulse's lit tail along the fibres, prototype pixels.
const PULSE_TAIL: f32 = 110.0;

/// A task's reach feels out from the globe's rim over this long and unravels back over
/// this long, seconds; it runs this many globe radii out.
const REACH_GROW: f32 = 1.2;
const REACH_UNRAVEL: f32 = 1.5;
const REACH_LEN: f32 = 1.4;

/// A permission ask holds its braid over this long, and lets go as long, seconds; the card
/// unfolds over `CARD_OPEN`, shows the verdict for `VERDICT_SECS`, then folds.
const HOLD_SECS: f32 = 0.6;
const CARD_OPEN: f32 = 0.42;
const VERDICT_SECS: f32 = 2.6;
/// While an ask holds the network, everything else dims to this, easing with `FOCUS_TAU`.
const FOCUS_DIM: f32 = 0.26;
const FOCUS_TAU: f32 = 0.25;
/// The held gap's half length, as a share of the braid.
const GAP_HALF: f32 = 0.07;

/// Workers glide to new places in the fan with this time constant, seconds.
const FAN_TAU: f32 = 0.7;
const DISSOLVE: f32 = 1.5;
const REDUCED: f32 = 0.15;
/// Idle globes: opacity and size.
const REST_DIM: f32 = 0.4;
const REST_SCALE: f32 = 0.8;
/// Hover growth and its time constant, seconds.
const HOVER_GROW: f32 = 1.14;
const HOVER_TAU: f32 = 0.11;
const PING_SECS: f32 = 2.4;
/// A name chip whose words change cross-fades old to new over this long, seconds; a ping's
/// chip fades in over `PING_RISE`.
const WORDS_SWAP: f32 = 0.25;
const PING_RISE: f32 = 0.15;

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
/// A headless Codex run's globe, as a share of a session's.
const HEADLESS_SIZE: f32 = 0.75;
const PUSH: f32 = 40.0;
/// Bodies nudge each other at this share of `PUSH`, so a newcomer eases its neighbours aside.
const NUDGE: f32 = 0.5;
/// Braids push more gently and only once touched: a thin half width and a soft give. Reach
/// threads push harder than braids, softer than bodies. In knot radii.
const BRAID_HALF: f32 = 0.05;
const BRAID_GIVE: f32 = 0.25;
const REACH_HALF: f32 = 0.03;
const REACH_GIVE: f32 = 0.4;
const REACH_END_R: f32 = 0.09;
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

/// Pulses on one braid: those travelling, and those queued behind them in feed order.
#[derive(Clone, Debug)]
struct Pulses {
    /// Seconds each has run, and its colour.
    live: Vec<(f32, Rgb)>,
    queue: VecDeque<Rgb>,
    /// Seconds since the last launch.
    since: f32,
}

impl Default for Pulses {
    fn default() -> Self {
        Pulses { live: Vec::new(), queue: VecDeque::new(), since: PULSE_GAP }
    }
}

impl Pulses {
    /// Queues one; false when the queue is full and it was dropped.
    fn push(&mut self, col: Rgb) -> bool {
        if self.queue.len() >= PULSE_QUEUE {
            return false;
        }
        self.queue.push_back(col);
        true
    }

    fn step(&mut self, dt: f32, secs: f32) {
        self.since += dt;
        for p in &mut self.live {
            p.0 += dt;
        }
        self.live.retain(|p| p.0 < secs);
        if self.since >= PULSE_GAP && self.live.len() < PULSE_LIVE {
            if let Some(col) = self.queue.pop_front() {
                self.live.push((0.0, col));
                self.since = 0.0;
            }
        }
    }

    fn busy(&self) -> bool {
        !self.live.is_empty() || !self.queue.is_empty()
    }

    /// Where each travelling pulse is lit, for `braid`.
    fn lit(&self, secs: f32, reduced: bool) -> Vec<Lit> {
        self.live
            .iter()
            .map(|&(t, col)| {
                let k = (t / secs).clamp(0.0, 1.0);
                let rise = (t / 0.3).clamp(0.0, 1.0);
                let fall = ((secs - t) / 0.25).clamp(0.0, 1.0);
                let light = rise * rise * (3.0 - 2.0 * rise) * fall * fall * (3.0 - 2.0 * fall);
                Lit { head: 1.0 - e_in_out(k), light, col, whole: reduced }
            })
            .collect()
    }
}

/// A pulse as `braid` draws it: its head, 1 at the far end down to 0 at the near end, how
/// lit it is, and its colour. A `whole` pulse lights every fibre at once, for reduced motion.
#[derive(Clone, Copy, Debug)]
struct Lit {
    head: f32,
    light: f32,
    col: Rgb,
    whole: bool,
}

/// A name chip's words: the last ones set, the ones before, and how far the change has
/// cross-faded, 0..1.
#[derive(Clone, Debug)]
struct Words {
    now: String,
    was: String,
    swap: f32,
}

impl Default for Words {
    fn default() -> Self {
        Words { now: String::new(), was: String::new(), swap: 1.0 }
    }
}

impl Words {
    /// Takes the chip's words. A change while the chip shows cross-fades from the old words.
    fn set(&mut self, text: String, shown: bool) {
        if text == self.now {
            return;
        }
        self.was = std::mem::replace(&mut self.now, text);
        self.swap = if shown && !self.was.is_empty() { 0.0 } else { 1.0 };
    }

    fn step(&mut self, dt: f32, span: f32) {
        self.swap = (self.swap + dt / span).min(1.0);
    }

    fn busy(&self) -> bool {
        self.swap < 1.0
    }

    /// Adds the chip for `text` at `at` with opacity `show`. While a change runs, the old words
    /// fade out over the chip's steady ground as the new ones fade in.
    fn push(&self, out: &mut Vec<Label>, text: String, at: [f32; 2], show: f32) {
        let k = if text == self.now { smooth(self.swap) } else { 1.0 };
        if k < 1.0 && !self.was.is_empty() && self.was != text {
            out.push(Label { text: self.was.clone(), at, alpha: show * (1.0 - k), ground: show * (1.0 - k) });
        }
        out.push(Label { text, at, alpha: show * k, ground: show });
    }
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A chip's opacity as its owner's form passes `at`: it fades across a fifth of the form.
fn formed(form: f32, at: f32) -> f32 {
    smooth((form - at) / 0.2 + 0.5)
}

/// A ping's chip: it fades in over `PING_RISE`, holds, and fades out by `PING_SECS`.
fn ping_show(ping: Option<f32>) -> f32 {
    ping.map_or(0.0, |t| smooth(t / PING_RISE) * ((1.0 - t / PING_SECS) * 4.0).min(1.0))
}

/// A background shell or monitor, reaching out from its session's globe.
#[derive(Clone, Debug)]
struct Reach {
    id: String,
    kind: TaskKind,
    label: String,
    words: Words,
    /// 0..1: grown out, or what is left as it unravels.
    form: f32,
    /// Its angle round the globe, easing toward its slot.
    ang: f32,
    /// -1 above the globe, 1 below; kept for life, so a reach never swings across.
    side: f32,
    hover: f32,
    leaving: bool,
    /// Seconds since it appeared, for its creep or ripples.
    age: f32,
}

impl Worker {
    fn words(&self, parent: &str) -> String {
        format!("{parent}·{} · {}", self.label, self.kind)
    }

    fn show(&self) -> f32 {
        self.hover.max(ping_show(self.ping))
    }
}

impl Reach {
    fn words(&self, parent: &str) -> String {
        let what = if self.kind == TaskKind::Monitor { "monitor" } else { "shell" };
        format!("{parent} · {what} {}", self.label)
    }
}

#[derive(Clone, Debug)]
struct Worker {
    id: String,
    /// "w1", and the agent type, for its name chip.
    label: String,
    kind: String,
    words: Words,
    hover: f32,
    /// Its angle in the fan, radians, easing toward its slot as workers come and go.
    ang: f32,
    /// Seconds since a feed click pinged it, while the ping lasts.
    ping: Option<f32>,
    pulses: Pulses,
    /// Seconds of twist on its braid, held still while it waits on a person.
    twist: f32,
    /// 0..1 along the glide out of the parent's nucleus, linear in time.
    emerge: f32,
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
    words: Words,
    hover: f32,
    /// Seconds since a feed click pinged it, while the ping lasts.
    ping: Option<f32>,
    /// Seconds of braid twist, advanced at one steady rate so the twist never jumps.
    twist: f32,
    /// A Codex session, drawn as a frost wireframe; a headless one is its hollow shell.
    codex: bool,
    headless: bool,
    /// Seconds since it appeared, for the Codex globe's stepped turn.
    age: f32,
    /// Ended while held near: it dissolves where it is, its braid unravelling with it.
    ended_near: bool,
    pulses: Pulses,
    tasks: Vec<Reach>,
    cwd: String,
    /// The permission prompt waiting on a person, as the core reports it.
    pending: Option<Pending>,
    /// 0..1: how held its braid is (the asker's: the session's own, or a worker's).
    hold: f32,
    /// The worker that asked, or none for the session; kept while the hold lets go.
    asker: Option<String>,
    /// The last verdict seen, so each one pulses once.
    decided: Option<String>,
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
    /// Physical pixels per CSS pixel times the type scale, for text the network draws.
    pub k: f32,
}

impl Area {
    /// Right of the feed column, inside the window's padding.
    pub fn new(col: &vitals::Column, w: u32, h: u32, knot_r: f32) -> Area {
        let x0 = col.right + 24.0 * col.u;
        Area { x: [x0, w as f32 - 14.0 * col.k], band: col.band, u: col.u, knot: [w as f32 * 0.5, h as f32 * 0.5], knot_r, k: col.k }
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
    /// The approval card, while one shows, and how much the rest of the network is dimmed.
    card: Option<Card>,
    focus: f32,
    /// Wall time at the last step, milliseconds.
    now_ms: u64,
}

/// The approval card: the globe and call it belongs to, what it says, and how far along.
#[derive(Clone, Debug)]
struct Card {
    globe: String,
    /// The worker that asked, or none for the session; the card stays on its braid.
    worker: Option<String>,
    /// Where the card sits, fixed by the braid's middle on its first frame.
    at: std::cell::Cell<Option<[f32; 2]>>,
    tool_use_id: String,
    card: approval::Card,
    open: f32,
    verdict: Option<bool>,
    /// Seconds since the verdict came.
    shown: f32,
    closing: bool,
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
    /// The session chip's words: its name, its state and its workers.
    fn words(&self) -> String {
        let state = if self.waiting { "waiting" } else if self.busy { "busy" } else { "idle" };
        let mut staying = self.workers.iter().filter(|w| !w.leaving);
        let workers = match (staying.next(), staying.count()) {
            (None, _) => String::new(),
            (Some(w), 0) => format!(" · {}", w.kind),
            (Some(_), n) => format!(" · {} workers", n + 1),
        };
        format!("{} · {state}{workers}", self.label)
    }

    /// How much the session chip shows, from hover or a ping.
    fn show(&self) -> f32 {
        self.hover.max(ping_show(self.ping))
    }

    /// Work under way: a turn, a live worker, or a background task, even one silently
    /// waiting on a long job.
    fn working(&self) -> bool {
        self.busy || self.workers.iter().any(|w| !w.leaving) || self.tasks.iter().any(|r| !r.leaving)
    }

    /// Held near the knot: hauled in, arrived, or about to be let go.
    fn near(&self) -> bool {
        match self.phase {
            Phase::Pulling => self.t >= GROW,
            Phase::Docked => true,
            Phase::Releasing => self.t < FLICKER,
            Phase::Dissolving => self.ended_near,
            Phase::Resting => false,
        }
    }

    /// Where its spring pulls, swaying slowly on two unrelated periods.
    fn anchor(&self, clock: f32, reduced: bool) -> [f32; 2] {
        self.anchor_at(self.near(), clock, reduced)
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
        SESSION_R * self.scale * if self.headless { HEADLESS_SIZE } else { 1.0 }
    }

    /// What this globe carries, in knot radii: its own circle, its workers and reach ends,
    /// and the braids and reach threads that join them.
    fn shapes(&self) -> Body {
        let mut b = Body::default();
        if self.form <= 0.0 {
            return b;
        }
        let r = self.body();
        b.circles.push((self.p, r));
        // Its braid, from the knot's centre, while drawn.
        if matches!(self.phase, Phase::Pulling | Phase::Docked | Phase::Releasing) {
            b.segments.push(([0.0, 0.0], self.p, BRAID_HALF, BRAID_GIVE));
        }
        for w in self.workers.iter().filter(|w| w.form > 0.0) {
            let e = e_in_out(w.emerge);
            let ang = if w.ang.is_nan() { 0.0 } else { w.ang };
            let d = WORKER_D * self.scale * e;
            let c = [self.p[0] + d * ang.cos(), self.p[1] + d * ang.sin()];
            b.circles.push((c, WORKER_R * self.scale * (0.12 + 0.88 * e)));
            b.segments.push((self.p, c, BRAID_HALF * 0.7 * self.scale, BRAID_GIVE));
        }
        for t in self.tasks.iter().filter(|t| t.form > 0.0) {
            let ang = if t.ang.is_nan() { reach_slot(t.side, 0) } else { t.ang };
            let (s, c) = ang.sin_cos();
            let out = r + REACH_LEN * r.max(SESSION_R * 0.5) * t.form;
            let rim = [self.p[0] + c * r, self.p[1] + s * r];
            let end = [self.p[0] + c * out, self.p[1] + s * out];
            b.segments.push((rim, end, REACH_HALF, REACH_GIVE));
            b.circles.push((end, REACH_END_R));
        }
        b
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
                        // Below zero waits its turn to condense.
                        form: if instant { -STAGGER / CONDENSE * self.globes.len() as f32 } else { 0.0 },
                        active_ms: s.started_unix_ms,
                        workers: Vec::new(),
                        words: Words::default(),
                        hover: 0.0,
                        ping: None,
                        twist: 0.0,
                        codex: s.harness == "codex",
                        headless: s.headless,
                        age: 0.0,
                        ended_near: false,
                        pulses: Pulses::default(),
                        tasks: Vec::new(),
                        cwd: String::new(),
                        pending: None,
                        hold: 0.0,
                        asker: None,
                        decided: None,
                    };
                    g.p = g.anchor(self.clock, true);
                    self.globes.push(g);
                    self.globes.len() - 1
                }
            };
            let g = &mut self.globes[i];
            g.label.clone_from(&s.label);
            g.cwd.clone_from(&s.cwd);
            g.pending.clone_from(&s.pending);
            if let Some(p) = &s.pending {
                g.asker.clone_from(&p.worker);
            }
            if let Some(d) = s.decided.as_ref().filter(|d| g.decided.as_deref() != Some(&d.tool_use_id)) {
                g.decided = Some(d.tool_use_id.clone());
                // The answer runs home along the asker's braid: green for yes, red for no.
                let col = if d.granted { theme::VERD } else { theme::VERM };
                let worker = g.asker.as_ref().and_then(|id| g.workers.iter_mut().find(|w| &w.id == id));
                match worker {
                    Some(w) => {
                        w.pulses.push(col);
                    }
                    None => {
                        g.pulses.push(col);
                    }
                }
                if let Some(c) = self.card.as_mut().filter(|c| c.tool_use_id == d.tool_use_id) {
                    c.verdict = Some(d.granted);
                }
            }
            g.busy = busy;
            g.waiting = waiting;
            if busy {
                g.active_ms = g.active_ms.max(now_ms);
            }
            for w in &s.workers {
                if !g.workers.iter().any(|x| x.id == w.id && !x.leaving) {
                    g.workers.push(Worker { id: w.id.clone(), label: w.label.clone(), kind: w.kind.clone(), words: Words::default(), hover: 0.0, ang: f32::NAN, ping: None, pulses: Pulses::default(), twist: g.twist, emerge: 0.0, form: 0.0, leaving: false });
                }
            }
            for x in &mut g.workers {
                if !x.leaving && !s.workers.iter().any(|w| w.id == x.id) {
                    // A worker finishing is work too: the release clock starts over.
                    x.leaving = true;
                    g.active_ms = g.active_ms.max(now_ms);
                }
            }
            if !s.workers.is_empty() {
                g.active_ms = g.active_ms.max(now_ms);
            }
            for t in &s.tasks {
                if !g.tasks.iter().any(|x| x.id == t.id && !x.leaving) {
                    // The side with fewer reaches, above first.
                    let count = |side: f32| g.tasks.iter().filter(|x| !x.leaving && x.side == side).count();
                    let side = if count(-1.0) <= count(1.0) { -1.0 } else { 1.0 };
                    g.tasks.push(Reach { id: t.id.clone(), kind: t.kind, label: t.label.clone(), words: Words::default(), form: 0.0, ang: f32::NAN, side, hover: 0.0, leaving: false, age: 0.0 });
                }
            }
            for x in &mut g.tasks {
                if !x.leaving && !s.tasks.iter().any(|t| t.id == x.id) {
                    x.leaving = true;
                    g.active_ms = g.active_ms.max(now_ms);
                }
            }
        }
        for g in &mut self.globes {
            if g.phase != Phase::Dissolving && !sessions.iter().any(|s| s.id == g.id) {
                g.ended_near = g.near();
                g.phase = Phase::Dissolving;
                g.t = 0.0;
            }
        }
        self.settle(now_ms);
        if instant {
            self.snap();
        }
    }

    /// Live feed events pulse the braids in feed order: a worker's event runs its braid into
    /// its parent, a session's runs its braid into the core. History and braids not drawn
    /// get nothing.
    pub fn pulse(&mut self, events: &[FeedEvent]) {
        for e in events.iter().filter(|e| !e.backlog) {
            let Some(g) = self.globes.iter_mut().find(|g| g.id == e.session && g.phase != Phase::Dissolving) else { continue };
            let col = crate::feed::pulse_color(e);
            let queued = match e.worker.as_deref() {
                Some(id) => match g.workers.iter_mut().find(|w| w.id == id && !w.leaving && w.emerge > 0.5) {
                    Some(w) => w.pulses.push(col),
                    None => continue,
                },
                None if g.form > 0.5 && matches!(g.phase, Phase::Pulling | Phase::Docked | Phase::Releasing) => g.pulses.push(col),
                None => continue,
            };
            if !queued {
                log::info!("pulse queue full on {}; dropped {:?}", g.label, e.kind);
            }
        }
    }

    /// The approval card follows the oldest ask: it unfolds, shows the verdict, then folds;
    /// an ask that vanishes unanswered folds it at once. The rest of the network dims while
    /// it shows.
    fn step_card(&mut self, dt: f32, reduced: bool) {
        let quick = |secs: f32| if reduced { REDUCED } else { secs };
        if self.card.is_none() {
            let oldest = self
                .globes
                .iter()
                .filter(|g| g.form > 0.5 && g.phase != Phase::Dissolving)
                .filter_map(|g| g.pending.as_ref().map(|p| (p.since_unix_ms, g, p)))
                .min_by_key(|(t, g, _)| (*t, g.id.clone()));
            if let Some((_, g, p)) = oldest {
                let worker = p.worker.as_ref().and_then(|id| g.workers.iter().find(|w| &w.id == id));
                self.card = Some(Card {
                    globe: g.id.clone(),
                    worker: p.worker.clone(),
                    at: std::cell::Cell::new(None),
                    tool_use_id: p.tool_use_id.clone(),
                    card: approval::Card {
                        who: worker.map_or_else(|| g.label.clone(), |w| format!("{}·{}", g.label, w.label)),
                        tool: p.tool.clone(),
                        summary: p.summary.clone(),
                        since_ms: p.since_unix_ms,
                    },
                    open: 0.0,
                    verdict: None,
                    shown: 0.0,
                    closing: false,
                });
            }
        }
        if let Some(c) = &mut self.card {
            let still = self.globes.iter().find(|g| g.id == c.globe).and_then(|g| g.pending.as_ref()).is_some_and(|p| p.tool_use_id == c.tool_use_id);
            if c.verdict.is_some() {
                c.shown += dt;
                c.closing |= c.shown >= quick(VERDICT_SECS);
            } else if !still {
                c.closing = true;
            }
            let step = dt / quick(CARD_OPEN);
            c.open = if c.closing { (c.open - step).max(0.0) } else { (c.open + step).min(1.0) };
            if c.closing && c.open <= 0.0 {
                self.card = None;
            }
        }
        let target = if self.card.as_ref().is_some_and(|c| !c.closing) { 1.0 } else { 0.0 };
        self.focus = if reduced { target } else { self.focus + (target - self.focus) * (1.0 - (-dt / FOCUS_TAU).exp()) };
        if (target - self.focus).abs() < 0.001 {
            self.focus = target;
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
            let working = g.working();
            let idle_long = !working && now_ms.saturating_sub(g.active_ms) >= RELEASE_AFTER;
            let next = match g.phase {
                Phase::Resting | Phase::Releasing if g.working() => Some(Phase::Pulling),
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
        self.card.is_some() || self.focus > 0.0 || self.globes.iter().any(|g| {
            matches!(g.phase, Phase::Pulling | Phase::Releasing | Phase::Dissolving)
                || g.form < 1.0
                || len(g.v) > 0.01
                || g.workers.iter().any(|w| w.leaving || w.form < 1.0)
                || g.ping.is_some()
                || g.workers.iter().any(|w| w.ping.is_some() || w.pulses.busy())
                || g.pulses.busy()
                || g.hold > 0.0
                || g.tasks.iter().any(|r| r.leaving || r.form < 1.0)
                || g.words.busy()
                || g.workers.iter().any(|w| w.words.busy())
                || g.tasks.iter().any(|r| r.words.busy() || (r.hover - if self.hover.as_deref() == Some(&r.id) { 1.0 } else { 0.0 }).abs() > 0.001)
                || (g.hover - if self.hover.as_deref() == Some(&g.id) { 1.0 } else { 0.0 }).abs() > 0.001
                || g.workers.iter().any(|w| (w.hover - if self.hover.as_deref() == Some(&w.id) { 1.0 } else { 0.0 }).abs() > 0.001)
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
                // A held session braid stops still; a worker's ask only slows its parent's.
                let held = g.hold * if g.asker.is_none() { 1.0 } else { 0.4 };
                g.twist += dt * TWIST_PACE * (1.0 - (1.0 - WAIT_TWIST) * g.wait) * (1.0 - held);
                for w in &mut g.workers {
                    let held = if g.asker.as_deref() == Some(&w.id) { g.hold } else { 0.0 };
                    w.twist += dt * TWIST_PACE * (1.0 - held);
                }
                g.age += dt;
            }
            let target = if g.pending.is_some() { 1.0 } else { 0.0 };
            let step = dt / quick(HOLD_SECS);
            g.hold = if target > g.hold { (g.hold + step).min(target) } else { (g.hold - step).max(target) };
            if g.hold <= 0.0 && g.pending.is_none() {
                g.asker = None;
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
            g.form = if target_form > g.form { (g.form + dt / quick(span)).min(target_form) } else { (g.form - dt / quick(span)).max(target_form) };
            let near = g.near();
            let (dim, scale) = if near { (1.0, near_scale) } else { (REST_DIM, far_scale) };
            g.dim += (dim - g.dim) * ease(0.6);
            g.scale += (scale - g.scale) * ease(0.6);
            let h = if hover.as_deref() == Some(&g.id) { 1.0 } else { 0.0 };
            g.hover += (h - g.hover) * ease(HOVER_TAU);
            if (h - g.hover).abs() < 0.001 {
                g.hover = h;
            }
            g.pulses.step(dt, PULSE_SECS);
            for w in &mut g.workers {
                w.pulses.step(dt, WORKER_PULSE_SECS);
                if let Some(p) = &mut w.ping {
                    *p += dt;
                    if *p >= PING_SECS {
                        w.ping = None;
                    }
                }
            }
            if let Some(p) = &mut g.ping {
                *p += dt;
                if *p >= PING_SECS {
                    g.ping = None;
                }
            }
            // Workers wait for their parent to finish condensing.
            let shown = g.phase == Phase::Docked && g.form >= 1.0;
            let staying = g.workers.iter().filter(|w| !w.leaving).count();
            let mut k = 0;
            for w in &mut g.workers {
                if !w.leaving {
                    let target = fan(k, staying);
                    k += 1;
                    w.ang = if w.ang.is_nan() { target } else { w.ang + (target - w.ang) * ease(FAN_TAU) };
                }
                let h = if hover.as_deref() == Some(&w.id) { 1.0 } else { 0.0 };
                w.hover += (h - w.hover) * ease(HOVER_TAU);
                if (h - w.hover).abs() < 0.001 {
                    w.hover = h;
                }
                if w.leaving {
                    // Done: dust where it stands.
                    w.form = (w.form - dt / quick(WORKER_DISSOLVE)).max(0.0);
                } else {
                    // Out of the nucleus while the parent is docked, back into it otherwise.
                    let step = dt / quick(WORKER_EMERGE);
                    w.emerge = if shown { (w.emerge + step).min(1.0) } else { (w.emerge - 2.0 * step).max(0.0) };
                    w.form = w.emerge;
                }
            }
            g.workers.retain(|w| !(w.leaving && w.form <= 0.0));

            // Reaches feel out while the globe is held near and formed, and draw back in
            // when it is let go or the task ends.
            let held = g.form >= 1.0 && matches!(g.phase, Phase::Pulling | Phase::Docked);
            let mut ranks = [0usize; 2];
            for r in &mut g.tasks {
                r.age += dt;
                if !r.leaving {
                    let rank = &mut ranks[usize::from(r.side > 0.0)];
                    let target = reach_slot(r.side, *rank);
                    *rank += 1;
                    r.ang = if r.ang.is_nan() { target } else { r.ang + (target - r.ang) * ease(FAN_TAU) };
                }
                let h = if hover.as_deref() == Some(&r.id) { 1.0 } else { 0.0 };
                r.hover += (h - r.hover) * ease(HOVER_TAU);
                if (h - r.hover).abs() < 0.001 {
                    r.hover = h;
                }
                r.form = if held && !r.leaving { (r.form + dt / quick(REACH_GROW)).min(1.0) } else { (r.form - dt / quick(REACH_UNRAVEL)).max(0.0) };
            }
            g.tasks.retain(|r| !(r.leaving && r.form <= 0.0));

            // Name chips cross-fade when their words change while they show.
            let swap = quick(WORDS_SWAP);
            for w in &mut g.workers {
                let (text, shown) = (w.words(&g.label), w.show() > 0.01);
                w.words.set(text, shown);
                w.words.step(dt, swap);
            }
            for r in &mut g.tasks {
                let (text, shown) = (r.words(&g.label), r.hover > 0.01);
                r.words.set(text, shown);
                r.words.step(dt, swap);
            }
            let (text, shown) = (g.words(), g.show() > 0.01);
            g.words.set(text, shown);
            g.words.step(dt, swap);
        }
        self.globes.retain(|g| !(g.phase == Phase::Dissolving && g.form <= 0.0));
        if let Some(area) = self.area {
            self.physics(dt, reduced, &area);
        }
        self.now_ms = now_ms;
        self.step_card(dt, reduced);
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
        let bodies: Vec<Body> = self.globes.iter().map(Globe::shapes).collect();
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
            // Everything a globe carries pushes on everything another globe carries: globes,
            // workers and reach ends as circles, braids and reach threads as segments.
            for j in i + 1..n {
                for (push, on_i, on_j) in contacts(&bodies[i], &bodies[j]) {
                    force[i][0] += push[0] * on_i;
                    force[i][1] += push[1] * on_i;
                    force[j][0] -= push[0] * on_j;
                    force[j][1] -= push[1] * on_j;
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

    /// Every globe and worker fully formed at once. Only for stills, which have no time to
    /// condense in.
    pub fn finish(&mut self) {
        for g in &mut self.globes {
            g.form = 1.0;
            let docked = g.phase == Phase::Docked;
            let n = g.workers.len();
            for (k, w) in g.workers.iter_mut().enumerate() {
                (w.form, w.emerge) = if docked { (1.0, 1.0) } else { (0.0, 0.0) };
                w.ang = fan(k, n);
            }
        }
    }

    /// Finishes every move at once and settles the bodies apart. Forms are left alone: a globe
    /// still condensing goes on condensing.
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
            g.p = g.anchor(self.clock, true);
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
    /// Rings the worker a feed row came from, or its session when the session said it.
    pub fn ping(&mut self, session: &str, worker: Option<&str>) {
        let Some(g) = self.globes.iter_mut().find(|g| g.id == session) else { return };
        match worker.and_then(|id| g.workers.iter_mut().find(|w| w.id == id && !w.leaving)) {
            Some(w) => w.ping = Some(0.0),
            None => g.ping = Some(0.0),
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

/// What the cursor or a ping labels: the words, the point above the globe, and the
/// opacity of the words and of the chip's ground.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub text: String,
    pub at: [f32; 2],
    pub alpha: f32,
    pub ground: f32,
}

pub struct Frame {
    pub globes: Vec<GlobeInst>,
    pub segs: Vec<Seg>,
    /// Names `segs`; the painter uploads them when it changes.
    pub key: u64,
    /// Name chips, each fading on its own, so one can fade out as the next fades in.
    pub labels: Vec<Label>,
    /// The approval card, drawn through the panel host while it shows.
    pub card: Option<crate::panel::Frame>,
}

/// One globe's drawn place and size in pixels.
struct Placed {
    c: [f32; 2],
    r: f32,
    a: f32,
}

impl Net {
    fn place(&self, g: &Globe, area: &Area) -> Placed {
        let size = if g.headless { HEADLESS_SIZE } else { 1.0 };
        let r = SESSION_R * area.knot_r * g.scale * size * (1.0 + (HOVER_GROW - 1.0) * g.hover);
        let dim = match &self.card {
            Some(c) if c.globe != g.id => 1.0 + (FOCUS_DIM - 1.0) * self.focus,
            _ => 1.0,
        };
        Placed { c: area.px(g.p), r, a: g.dim * dim }
    }

    /// A worker at angle `ang`, `e` of the way along its eased glide from the parent's centre
    /// to its slot, growing from a speck as it goes.
    fn worker_place(parent: &Placed, scale: f32, ang: f32, e: f32, area: &Area) -> Placed {
        let ang = if ang.is_nan() { 0.0 } else { ang };
        let d = WORKER_D * area.knot_r * scale * e;
        let grow = 0.12 + 0.88 * e;
        Placed { c: [parent.c[0] + d * ang.cos(), parent.c[1] + d * ang.sin()], r: WORKER_R * area.knot_r * scale * grow, a: parent.a }
    }

    /// Where a task's reach leaves its globe's rim and where it ends.
    fn reach_ends(p: &Placed, r: &Reach, area: &Area) -> ([f32; 2], [f32; 2]) {
        let ang = if r.ang.is_nan() { reach_slot(r.side, 0) } else { r.ang };
        let (s, c) = ang.sin_cos();
        let len = REACH_LEN * p.r.max(SESSION_R * area.knot_r * 0.5);
        ([p.c[0] + c * p.r, p.c[1] + s * p.r], [p.c[0] + c * (p.r + len), p.c[1] + s * (p.r + len)])
    }

    /// The globe or worker under `(x, y)`, by id. Workers sit on top of their parents.
    pub fn hit(&self, area: &Area, x: f32, y: f32) -> Option<String> {
        let inside = |c: [f32; 2], r: f32| (x - c[0]).powi(2) + (y - c[1]).powi(2) < r * r;
        self.globes.iter().rev().find_map(|g| {
            let p = self.place(g, area);
            let worker = g.workers.iter().find_map(|w| {
                let wp = Self::worker_place(&p, g.scale, w.ang, e_in_out(w.emerge), area);
                (w.form > 0.5 && !w.leaving && inside(wp.c, wp.r * HOVER_GROW)).then(|| w.id.clone())
            });
            let task = g.tasks.iter().find_map(|r| {
                let (_, end) = Self::reach_ends(&p, r, area);
                (r.form > 0.8 && !r.leaving && inside(end, 10.0 * area.u * 2.0)).then(|| r.id.clone())
            });
            worker.or(task).or_else(|| (g.form > 0.5 && inside(p.c, p.r)).then(|| g.id.clone()))
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
        let mut labels = Vec::new();
        // The middle of the card's braid, held or not, for its tether.
        let mut held_gap: Option<[f32; 2]> = None;
        let card_on = |g: &Globe, worker: Option<&str>| self.card.as_ref().is_some_and(|c| c.globe == g.id && c.worker.as_deref() == worker);
        for g in &self.globes {
            let p = self.place(g, area);
            let a = p.a * g.form.clamp(0.0, 1.0).sqrt();
            if g.codex {
                let leaving = g.phase == Phase::Dissolving;
                let ball = codex::Ball { c: p.c, r: p.r, alpha: p.a, secs: g.age, phase: phase(g.seed), headless: g.headless, form: g.form, leaving, u, reduced };
                codex::ball(&mut segs, &ball);
            } else {
                let [er, eg, eb] = theme::EMBER.srgb_f32();
                globes.push(GlobeInst {
                    pos: [p.c[0], p.c[1], p.r, self.rot + phase(g.seed) * 0.37],
                    color: [er, eg, eb, p.a],
                    form: [0.0, g.form.clamp(0.0, 1.0), if g.busy { 0.16 } else { 0.06 }, (g.seed % 997) as f32],
                });
            }
            if a > 0.01 && !g.codex {
                segs.push(vitals::glow(p.c[0], p.c[1], 7.2 * p.r, theme::EMBER, 0.5 * a));
                segs.push(vitals::glow(p.c[0], p.c[1], 1.2 * p.r, theme::HOT, 0.55 * a));
                ticks(&mut segs, p.c, p.r + 5.0 * sc * g.scale, 72, 1.8 * sc * g.scale, 6, theme::EMBER, 0.32 * a, lws);
            }
            if let Some(t) = g.ping {
                ring(&mut segs, p.c, p.r + 12.0 * sc, theme::BONE, 0.9 * ping_light(t, reduced), 1.6 * lws);
            }
            if let Some(mid) = session_braid(&mut segs, area, g, &p, reduced, self.clock) {
                if card_on(g, None) {
                    held_gap = Some(mid);
                }
            }

            // A worker's or reach's chip covers its session's.
            let mut cover: f32 = 0.0;
            for (i, r) in g.tasks.iter().enumerate() {
                if r.form <= 0.0 {
                    continue;
                }
                let (from, to) = Self::reach_ends(&p, r, area);
                let growing = !r.leaving && r.form < 1.0;
                let span = if growing { e_out(r.form) } else { r.form };
                let spec = Braid {
                    strands: 2,
                    fibres: 0,
                    lam: 46.0,
                    amp: 2.6,
                    om: if r.kind == TaskKind::Monitor { 0.7 } else { 1.6 },
                    width: 0.8,
                    alpha: if r.kind == TaskKind::Monitor { 0.74 } else { 0.55 },
                    col: reach::color(r.kind),
                };
                braid(&mut segs, from, to, 0.0, span, span < 1.0, p.a, &spec, &[], 0.0, area.u, r.age, g.seed + 101 * i as u32, reduced);
                let secs = r.age + phase(g.seed + i as u32);
                reach::end(&mut segs, &reach::End { kind: r.kind, from, to, form: r.form, secs, hover: r.hover, alpha: p.a, u: area.u, reduced });
                let show = r.hover * formed(r.form, 0.8);
                cover = cover.max(show);
                if show > 0.01 {
                    r.words.push(&mut labels, r.words(&g.label), [to[0], to[1] - 14.0 * sc], show);
                }
            }

            for (i, w) in g.workers.iter().enumerate() {
                let e = e_in_out(w.emerge);
                let mut wp = Self::worker_place(&p, g.scale, w.ang, e, area);
                wp.r *= 1.0 + (HOVER_GROW - 1.0) * w.hover;
                let grow = 0.12 + 0.88 * e;
                if let Some(t) = w.ping {
                    ring(&mut segs, wp.c, wp.r + 6.0 * sc * g.scale, theme::BONE, 0.9 * ping_light(t, reduced), 1.4 * lws);
                }
                let show = w.show() * formed(w.form, 0.5);
                cover = cover.max(show);
                if show > 0.01 {
                    w.words.push(&mut labels, w.words(&g.label), [wp.c[0], wp.c[1] - wp.r], show);
                }
                let wa = wp.a * w.form.sqrt();
                let held = if g.asker.as_deref() == Some(&w.id) { g.hold } else { 0.0 };
                let tone = mix(theme::BONE, theme::VERM, held);
                let [br, bg, bb] = tone.srgb_f32();
                globes.push(GlobeInst {
                    pos: [wp.c[0], wp.c[1], wp.r, self.rot * 1.3 + phase(g.seed + i as u32)],
                    color: [br, bg, bb, wp.a],
                    form: [1.0, w.form, 0.0, ((g.seed + 31 * i as u32) % 997) as f32],
                });
                if wa > 0.01 {
                    // The worker braid trails out of the parent's nucleus behind the worker and
                    // fades as it turns to dust.
                    let spec = Braid { strands: 3, fibres: 3, lam: 104.0, amp: 6.0 * g.scale, om: 3.0, width: 1.1, alpha: 0.8, col: tone };
                    let lit = w.pulses.lit(WORKER_PULSE_SECS, reduced);
                    braid(&mut segs, p.c, wp.c, 0.08, 1.0, false, wa, &spec, &lit, held, area.u, w.twist, g.seed + i as u32, reduced);
                    segs.push(vitals::glow(wp.c[0], wp.c[1], 6.0 * wp.r, tone, 0.28 * wa));
                    let mid = bez(p.c, wp.c, 0.08, 0.5);
                    if held > 0.0 {
                        let edges = [bez(p.c, wp.c, 0.08, 0.5 - GAP_HALF), bez(p.c, wp.c, 0.08, 0.5 + GAP_HALF)];
                        hold_marks(&mut segs, mid, edges, wp.c, wp.r, held, self.clock, area.u, reduced);
                    }
                    if card_on(g, Some(&w.id)) {
                        held_gap = Some(mid);
                    }
                    ticks(&mut segs, wp.c, wp.r + 3.0 * sc * g.scale * grow, 24, 1.5 * sc * g.scale * grow, 0, theme::BONE, 0.3 * wa, lws);
                }
            }

            let show = g.show() * formed(g.form, 0.5) * (1.0 - cover);
            if show > 0.01 {
                g.words.push(&mut labels, g.words(), [p.c[0], p.c[1] - p.r], show);
            }
        }
        globes.truncate(crate::globes::MAX_GLOBES);
        segs.truncate(vitals::PANEL_SEGS);
        let mut h = std::hash::DefaultHasher::new();
        for s in &segs {
            std::hash::Hash::hash(&bytemuck::bytes_of(s), &mut h);
        }
        let card = self.card.as_ref().and_then(|c| {
            let gap = held_gap.or_else(|| {
                let g = self.globes.iter().find(|g| g.id == c.globe)?;
                Some(self.place(g, area).c)
            })?;
            if c.at.get().is_none() {
                c.at.set(Some(gap));
            }
            let view = approval::View {
                card: &c.card,
                gap,
                at: c.at.get().unwrap_or(gap),
                open: c.open,
                verdict: c.verdict,
                now_ms: self.now_ms.max(c.card.since_ms),
                clock: self.clock,
                x: area.x,
                band: area.band,
                k: area.k,
                u: area.u,
                reduced,
            };
            Some(approval::frame(&view))
        });
        Some(Frame { globes, segs, key: std::hash::Hasher::finish(&h), labels, card })
    }
}

/// A ping ring's light `t` seconds in: it fades in over a quarter second, pulses, and
/// fades out by `PING_SECS`.
fn ping_light(t: f32, reduced: bool) -> f32 {
    let rise = (t / 0.25).clamp(0.0, 1.0);
    let br = if reduced { 1.0 } else { 0.6 + 0.4 * (t * 8.0).sin() };
    rise * rise * (3.0 - 2.0 * rise) * (1.0 - t / PING_SECS) * br
}

/// The angle of the `k`th reach on one side of a globe (-1 above, 1 below), fanning outward
/// from just past straight up or down, clear of the knot's braid and the workers.
fn reach_slot(side: f32, k: usize) -> f32 {
    side * (100.0 + 24.0 * k as f32).to_radians()
}

/// The angle of slot `k` of `n` in a worker fan, ±40° about the parent's right.
fn fan(k: usize, n: usize) -> f32 {
    if n <= 1 { 0.0 } else { (-40.0 + 80.0 * k as f32 / (n - 1) as f32).to_radians() }
}

/// A globe's collision shapes in knot radii: circles `(centre, radius)`, and segments
/// `(from, to, half width, give)`. A segment's give is how softly it pushes, 1 for full.
#[derive(Default)]
struct Body {
    circles: Vec<([f32; 2], f32)>,
    segments: Vec<([f32; 2], [f32; 2], f32, f32)>,
}

/// The nearest point to `c` on the segment `a`–`b`, and how far along it lies.
fn nearest(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> ([f32; 2], f32) {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let l2 = (dx * dx + dy * dy).max(1e-8);
    let t = (((c[0] - a[0]) * dx + (c[1] - a[1]) * dy) / l2).clamp(0.0, 1.0);
    ([a[0] + dx * t, a[1] + dy * t], t)
}

/// Pushes between two globes' bodies: each is the push on the first, away from the second,
/// with how much of it the first and the second take. Circles push both equally; a segment
/// pushes the circle it touches, with the segment's give, and its own globe takes half.
fn contacts(a: &Body, b: &Body) -> Vec<([f32; 2], f32, f32)> {
    let mut out = Vec::new();
    let push = |from: [f32; 2], to: [f32; 2], over: f32, give: f32, on_from: f32, on_to: f32, out: &mut Vec<_>| {
        if over <= 0.0 {
            return;
        }
        let d = [from[0] - to[0], from[1] - to[1]];
        let dl = len(d).max(1e-4);
        out.push(([d[0] / dl * over * PUSH * give, d[1] / dl * over * PUSH * give], on_from, on_to));
    };
    for &(ca, ra) in &a.circles {
        for &(cb, rb) in &b.circles {
            push(ca, cb, ra + rb + GAP - len([ca[0] - cb[0], ca[1] - cb[1]]), NUDGE, 1.0, 1.0, &mut out);
        }
        for &(s0, s1, w, give) in &b.segments {
            let (q, _) = nearest(s0, s1, ca);
            push(ca, q, ra + w - len([ca[0] - q[0], ca[1] - q[1]]), give, 1.0, 0.5, &mut out);
        }
    }
    for &(cb, rb) in &b.circles {
        for &(s0, s1, w, give) in &a.segments {
            let (q, _) = nearest(s0, s1, cb);
            // The push is on `a`, away from `b`: here `a`'s segment is pushed from the circle.
            push(q, cb, rb + w - len([cb[0] - q[0], cb[1] - q[1]]), give, 0.5, 1.0, &mut out);
        }
    }
    out
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
/// Draws it, and returns its middle, where an ask holds it.
fn session_braid(out: &mut Vec<Seg>, area: &Area, g: &Globe, p: &Placed, reduced: bool, clock: f32) -> Option<[f32; 2]> {
    let q = |secs: f32| if reduced { REDUCED } else { secs };
    let (reach, alpha) = match g.phase {
        Phase::Pulling => (e_out(g.t / q(GROW)), 1.0),
        // Docked as it condenses (a session already working at startup), the braid feels
        // out of the nucleus while the globe forms.
        Phase::Docked => (e_out(g.form), 1.0),
        Phase::Releasing if g.t < q(FLICKER) => (1.0, if reduced { 1.0 } else { 0.55 + 0.45 * (g.t * 60.0).sin().abs() }),
        Phase::Releasing => (1.0 - ((g.t - q(FLICKER)) / q(UNRAVEL)).clamp(0.0, 1.0), 1.0),
        Phase::Dissolving if g.ended_near => (g.form, 1.0),
        _ => return None,
    };
    if reach <= 0.0 {
        return None;
    }
    let (a0, a1) = (area.knot, p.c);
    // Waiting on a program, the braid greys a little and breathes slowly.
    let breath = 0.5 - 0.5 * (TAU * g.breath / BREATH).cos();
    let alpha = alpha * (1.0 - BREATH_DIM * g.wait * breath);
    let held = if g.asker.is_none() { g.hold } else { 0.0 };
    let col = mix(grey(if g.codex { theme::FROST } else { theme::EMBER }, WAIT_GREY * g.wait), theme::VERM, held);
    // A headless run keeps one loose fibre, so its pulses have a thread to ride.
    let (strands, fibres) = if g.headless { (3, 1) } else { (6, 4) };
    let spec = Braid { strands, fibres, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col };
    let s = phase(g.seed);
    let mut bow = if g.seed & 1 == 0 { 0.1 } else { -0.1 };
    if !reduced {
        // The prototype's slow sway of the path, and the feeler's search while it reaches.
        bow += 0.02 * (clock * 0.35 + s).sin();
        if g.phase == Phase::Pulling {
            bow += 0.16 * (1.0 - reach) * (clock * 1.3 + s * 3.0).sin();
        }
    }
    // Any end on the move, reaching out or unravelling back, closes to a point.
    let feeling = reach < 1.0;
    let lit = g.pulses.lit(PULSE_SECS, reduced);
    let tip = braid(out, a0, a1, bow, reach, feeling, alpha, &spec, &lit, held, area.u, g.twist, g.seed, reduced);
    if feeling && matches!(g.phase, Phase::Pulling | Phase::Docked) {
        // A hot spark rides the feeler's tip as it reaches out.
        out.push(vitals::glow(tip[0], tip[1], 26.0 * area.u, theme::HOT, 0.9));
    }
    let gap = bez(a0, a1, bow, 0.5);
    if held > 0.0 {
        let edges = [bez(a0, a1, bow, 0.5 - GAP_HALF), bez(a0, a1, bow, 0.5 + GAP_HALF)];
        hold_marks(out, gap, edges, p.c, p.r, held, clock, area.u, reduced);
    }
    Some(gap)
}

/// `a` moved `t` of the way to `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// What marks a held braid, after the prototype's suspended gap and approval veil: a soft
/// vermilion veil over the gap, a breathing cap and cross-tick at each frayed edge, and a
/// breathing ring round the globe that asked.
#[allow(clippy::too_many_arguments)]
fn hold_marks(out: &mut Vec<Seg>, gap: [f32; 2], edges: [[f32; 2]; 2], c: [f32; 2], r: f32, held: f32, clock: f32, u: f32, reduced: bool) {
    let (sc, lws) = (2.0 * u, (1.6 * u).max(1.0));
    let br = if reduced { 1.0 } else { 0.65 + 0.35 * (clock * 4.5).sin() };
    out.push(vitals::glow(gap[0], gap[1], 160.0 * sc.max(0.8), theme::VERM, 0.5 * held * (0.8 + 0.2 * br)));
    let (dx, dy) = (edges[1][0] - edges[0][0], edges[1][1] - edges[0][1]);
    let d = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (nx, ny) = (-dy / d, dx / d);
    for e in edges {
        out.push(vitals::glow(e[0], e[1], 44.0 * sc, theme::VERM, br * held));
        let t = 10.0 * sc;
        out.push(vitals::seg([e[0] + nx * t, e[1] + ny * t], [e[0] - nx * t, e[1] - ny * t], theme::VERM, 0.95 * held, 2.0 * lws));
    }
    ring(out, c, r + (10.0 + 4.0 * br) * sc, theme::VERM, 0.8 * held * br, 1.8 * lws);
}

/// `c` with share `k` of its colour drained toward a grey of the same luma.
fn grey(c: Rgb, k: f32) -> Rgb {
    let y = 0.299 * f32::from(c.0) + 0.587 * f32::from(c.1) + 0.114 * f32::from(c.2);
    let ch = |x: u8| (f32::from(x) + (y - f32::from(x)) * k.clamp(0.0, 1.0)).round() as u8;
    Rgb(ch(c.0), ch(c.1), ch(c.2))
}

/// Prototype `eInOut`: a cubic ease in and out over 0..1.
fn e_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
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
///
/// The loose fibres flow toward `a0`, the core or parent end, and the `lit` pulses ride
/// them there. The strands and the underglow never carry a pulse.
///
/// A `gap` above 0 holds the braid open at its middle: strands fray outward at its edges
/// and nothing is drawn across it.
#[allow(clippy::too_many_arguments)]
fn braid(out: &mut Vec<Seg>, a0: [f32; 2], a1: [f32; 2], bow: f32, reach: f32, feeling: bool, alpha: f32, b: &Braid, lit: &[Lit], gap: f32, u: f32, twist: f32, seed: u32, reduced: bool) -> [f32; 2] {
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
    // A moving end closes to a point over this length, strands and underglow alike.
    let taper = 40.0 * sc;
    let closing = |l: f32| {
        let x = if feeling { ((end - l) / taper).clamp(0.0, 1.0) } else { 1.0 };
        x * x * (3.0 - 2.0 * x)
    };
    let tail = PULSE_TAIL * sc;
    // Segments inside the held gap are not drawn.
    let held = |i: usize| gap > 0.01 && ((gl[i] + gl[i + 1]) * 0.5 / total - 0.5).abs() < GAP_HALF * gap;
    // `flow` is the wave's direction: +1 runs toward `a1`, -1 toward `a0`.
    let strand = |out: &mut Vec<Seg>, amp: f32, lam: f32, off: f32, a: f32, w: f32, flow: f32, lit: &[Lit]| {
        let at = |i: usize| {
            let s = gl[i] / total;
            let fray = 2.6 * gap * ((-((s - 0.5 + GAP_HALF) / 0.035).powi(2)).exp() + (-((s - 0.5 - GAP_HALF) / 0.035).powi(2)).exp());
            let tap = ((PI * s).sin().max(0.0).powf(0.7) + fray) * closing(gl[i]);
            let wv = TAU * gl[i] / lam - flow * ph + off + phase(seed);
            let nn = normal(i);
            ([pts[i][0] + nn[0] * amp * tap * wv.sin(), pts[i][1] + nn[1] * amp * tap * wv.sin()], wv.cos())
        };
        for i in 0..last {
            if held(i) {
                continue;
            }
            let ((p0, d0), (p1, d1)) = (at(i), at(i + 1));
            let depth = ((d0 + d1) * 0.25 + 0.5).clamp(0.0, 1.0);
            out.push(vitals::seg(p0, p1, b.col, a * (0.5 + 0.5 * depth), w));
            for l in lit {
                // Lit behind the head (toward `a1`, where it came from), brightest at the head.
                let behind = (gl[i] + gl[i + 1]) * 0.5 - l.head * total;
                let f = if l.whole {
                    1.0
                } else if behind >= 0.0 {
                    (1.0 - behind / tail).max(0.0).powf(1.5)
                } else {
                    (1.0 + behind / (8.0 * sc)).max(0.0)
                };
                if f > 0.0 {
                    // The lit thread, and a soft halo hugging the same fibre.
                    let a = l.light * f * (0.6 + 0.4 * depth);
                    out.push(vitals::soft(p0, p1, l.col, 0.45 * a, 7.0 * sc));
                    out.push(vitals::seg(p0, p1, l.col, a, w * 2.6));
                }
                if !l.whole && gl[i] <= l.head * total && l.head * total < gl[i + 1] {
                    out.push(vitals::glow(p0[0], p0[1], 16.0 * sc, l.col, 0.75 * l.light));
                    out.push(vitals::bead(p0[0], p0[1], 3.6 * lws, theme::HOT, 0.9 * l.light));
                }
            }
        }
    };
    let (amp, lam) = (b.amp * sc, b.lam * sc);
    // Underglow: one wide, faint band along the path. Soft strokes peak at a third of a
    // hard stroke's mean coverage, so the band carries three times the prototype's alpha.
    for i in 0..last {
        if held(i) {
            continue;
        }
        let k = closing(gl[i + 1].min(end));
        out.push(vitals::soft(pts[i], pts[i + 1], b.col, 0.15 * alpha * k, (2.0 * amp + 6.0 * sc) * (0.3 + 0.7 * k)));
    }
    for k in 0..b.strands {
        strand(out, amp, lam, k as f32 * TAU / b.strands as f32, b.alpha * alpha, b.width * lws, 1.0, &[]);
    }
    for k in 0..b.fibres {
        let kf = k as f32;
        strand(out, amp * (1.7 + 0.35 * kf), lam * (1.45 + 0.2 * kf), (kf + 0.5) * TAU / b.fibres as f32, 0.3 * b.alpha * alpha, 0.7 * lws, -1.0, lit);
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
        headless: false,
        status: if busy { SessionStatus::Busy } else { SessionStatus::Idle },
        started_unix_ms: vitals::CANNED_T0 - 600_000,
        workers: (0..workers)
            .map(|i| erebus_protocol::WorkerInfo { id: format!("{id}-w{i}"), label: format!("w{}", i + 1), kind: "Explore".into(), started_unix_ms: vitals::CANNED_T0 })
            .collect(),
        pending: None,
        tasks: Vec::new(),
        decided: None,
    };
    let mut erebus = s("aabbb6cd", "erebus", true, 2);
    erebus.tasks = vec![
        erebus_protocol::TaskInfo { id: "toolu_shell".into(), kind: TaskKind::Shell, label: "cargo".into(), started_unix_ms: vitals::CANNED_T0 },
        erebus_protocol::TaskInfo { id: "toolu_mon".into(), kind: TaskKind::Monitor, label: "build log".into(), started_unix_ms: vitals::CANNED_T0 },
    ];
    vec![erebus, s("31616aa1", "witchbottle", false, 0), s("17764bb2", "stratum", false, 0)]
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
        s.tasks.clear();
        s.id = id.into();
        s.label = id.into();
        s.status = if busy { SessionStatus::Busy } else { SessionStatus::Idle };
        let w0 = s.workers[0].clone();
        s.workers = (0..workers)
            .map(|i| erebus_protocol::WorkerInfo { id: format!("{id}-w{i}"), label: format!("w{}", i + 1), ..w0.clone() })
            .collect();
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

    /// The chip that shows most.
    fn chip(f: &Frame) -> Option<Label> {
        f.labels.iter().max_by(|a, b| a.alpha.total_cmp(&b.alpha)).cloned()
    }

    fn chip_of<'a>(f: &'a Frame, prefix: &str) -> Option<&'a Label> {
        f.labels.iter().find(|l| l.text.starts_with(prefix))
    }

    #[test]
    fn moving_between_globes_cross_fades_their_chips() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0), info("b", true, 0)], now);
        n.finish();
        n.hover = Some("a".into());
        run(&mut n, 1.0, &mut now);
        assert_eq!(chip(&n.frame(&area(), false).unwrap()).unwrap().text, "a · busy");
        n.hover = Some("b".into());
        run(&mut n, 0.05, &mut now);
        let f = n.frame(&area(), false).unwrap();
        let (a, b) = (chip_of(&f, "a ·").unwrap(), chip_of(&f, "b ·").unwrap());
        assert!(a.alpha > 0.05 && a.alpha < 0.95, "a fades out: {}", a.alpha);
        assert!(b.alpha > 0.05 && b.alpha < 0.95, "b fades in: {}", b.alpha);
        run(&mut n, 1.0, &mut now);
        let f = n.frame(&area(), false).unwrap();
        assert!(chip_of(&f, "a ·").is_none());
        assert_eq!(chip_of(&f, "b ·").unwrap().alpha, 1.0);
    }

    #[test]
    fn a_chip_whose_words_change_cross_fades_in_place() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.finish();
        n.hover = Some("a".into());
        run(&mut n, 1.0, &mut now);
        n.ingest_sessions(&[info("a", false, 0)], now);
        run(&mut n, 0.1, &mut now);
        let f = n.frame(&area(), false).unwrap();
        let old = f.labels.iter().find(|l| l.text == "a · busy").expect("old words still show");
        let new = f.labels.iter().find(|l| l.text == "a · idle").expect("new words show");
        assert_eq!(old.at, new.at, "in place");
        assert!(old.alpha > 0.05 && old.alpha < 0.95, "old fades out: {}", old.alpha);
        assert!(new.alpha > 0.05 && new.alpha < 0.95, "new fades in: {}", new.alpha);
        assert_eq!(new.ground, 1.0, "the ground holds steady");
        run(&mut n, 0.5, &mut now);
        let f = n.frame(&area(), false).unwrap();
        assert_eq!(f.labels.len(), 1);
        assert_eq!(f.labels[0].text, "a · idle");
    }

    #[test]
    fn a_pinged_chip_fades_in() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.finish();
        n.ping("a", None);
        assert!(n.frame(&area(), false).unwrap().labels.is_empty(), "nothing at once");
        run(&mut n, 0.05, &mut now);
        let a = chip(&n.frame(&area(), false).unwrap()).unwrap().alpha;
        assert!(a > 0.0 && a < 0.95, "{a}");
    }

    #[test]
    fn a_workers_chip_fades_its_sessions_out() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], now);
        n.finish();
        n.hover = Some("a".into());
        run(&mut n, 1.0, &mut now);
        n.hover = Some("a-w0".into());
        run(&mut n, 0.05, &mut now);
        let f = n.frame(&area(), false).unwrap();
        let (g, w) = (chip_of(&f, "a · ").unwrap(), chip_of(&f, "a·w1").unwrap());
        assert!(g.alpha > 0.05 && g.alpha < 0.95, "session fades out: {}", g.alpha);
        assert!(w.alpha > 0.05 && w.alpha < 0.95, "worker fades in: {}", w.alpha);
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
        // Nothing pops in: they condense one after another, then the worker glides out.
        assert!(n.globes.iter().all(|g| g.form <= 0.0));
        let mut now = T0;
        run(&mut n, CONDENSE * 0.5, &mut now);
        let (a, b) = (get(&n, "a").unwrap().form, get(&n, "b").unwrap().form);
        assert!(a > 0.3 && a < 0.7 && b < a - 0.1, "staggered: {a} {b}");
        assert_eq!(n.globes[0].workers[0].form, 0.0, "the worker waits for its parent");
        run(&mut n, CONDENSE * 0.5 + STAGGER + 0.1, &mut now);
        assert!(n.globes.iter().all(|g| g.form == 1.0));
        run(&mut n, WORKER_EMERGE + 0.1, &mut now);
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
    fn a_codex_session_is_a_frost_wireframe_and_headless_is_smaller() {
        let now = T0;
        let mut n = net();
        let mut cx = info("cx", true, 0);
        cx.harness = "codex".into();
        let mut ex = info("ex", true, 0);
        ex.harness = "codex".into();
        ex.headless = true;
        n.ingest_sessions(&[cx, ex], now);
        n.finish();
        let f = n.frame(&area(), false).unwrap();
        assert!(f.globes.is_empty(), "no particle globe for Codex");
        let frost = theme::FROST.srgb_f32();
        let is = |s: &Seg, c: [f32; 3]| (s.color[0] / s.color[3].max(1e-6) - c[0]).abs() < 0.01 && (s.color[2] / s.color[3].max(1e-6) - c[2]).abs() < 0.01;
        assert!(f.segs.iter().filter(|s| s.width[1] == 0.0 && is(s, frost)).count() > 120, "frost wireframe and braid");
        assert!(!f.segs.iter().any(|s| is(s, theme::EMBER.srgb_f32())), "nothing ember");
        let (a, b) = (n.place(&n.globes[0], &area()).r, n.place(&n.globes[1], &area()).r);
        let (busy, headless) = if n.globes[0].headless { (b, a) } else { (a, b) };
        assert!(headless < busy * 0.9);
    }

    #[test]
    fn a_docked_session_that_ends_dissolves_where_it_is() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.finish();
        let d0 = dist(&n, "a");
        n.ingest_sessions(&[], now);
        let mut prev = 1.0;
        while n.globes.iter().any(|g| g.id == "a") {
            run(&mut n, 1.0 / 60.0, &mut now);
            let Some(g) = get(&n, "a") else { break };
            assert!((dist(&n, "a") - d0).abs() < 0.15, "it drifts while dissolving");
            assert!(g.dim > 0.95, "it dims away instead of dissolving");
            assert!(g.form <= prev && prev - g.form < 0.02);
            prev = g.form;
        }
        assert!(prev < 0.05, "gone only once dissolved");
    }

    #[test]
    fn a_moving_braid_end_closes_to_a_point() {
        let spec = Braid { strands: 6, fibres: 4, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col: theme::EMBER };
        let (a0, a1) = ([0.0, 0.0], [1000.0, 0.0]);
        let mut out = Vec::new();
        let tip = braid(&mut out, a0, a1, 0.0, 0.5, true, 1.0, &spec, &[], 0.0, 1.0, 0.3, 7, false);
        // The underglow narrows and fades into the tip, and no strand strays from the path
        // near it.
        let glow: Vec<&Seg> = out.iter().filter(|s| s.width[1] > 2.5).collect();
        let (first, last) = (glow[glow.len() / 4], glow[glow.len() - 1]);
        assert!(last.color[3] < first.color[3] * 0.2 && last.width[0] < first.width[0] * 0.5);
        for s in out.iter().filter(|s| s.width[1] == 0.0) {
            if s.ab[2] > tip[0] - 10.0 {
                assert!(s.ab[3].abs() < 3.0, "a strand ends wide of the tip: {:?}", s.ab);
            }
        }
    }

    /// Worker `id` of globe 0: centre relative to its parent, and its form.
    fn worker(n: &Net, id: &str) -> Option<([f32; 2], f32)> {
        let g = &n.globes[0];
        let p = n.place(g, &area());
        let w = g.workers.iter().find(|w| w.id == id)?;
        let wp = Net::worker_place(&p, g.scale, w.ang, e_in_out(w.emerge), &area());
        Some(([wp.c[0] - p.c[0], wp.c[1] - p.c[1]], w.form))
    }

    #[test]
    fn when_a_worker_leaves_it_dusts_in_place_and_the_rest_glide_over() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 4)], now);
        n.finish();
        run(&mut n, 0.1, &mut now);
        let ids: Vec<String> = n.globes[0].workers.iter().map(|w| w.id.clone()).collect();
        let (gone0, _) = worker(&n, &ids[1]).unwrap();
        let mut s = info("a", true, 4);
        s.workers.remove(1);
        n.ingest_sessions(&[s], now);
        let start = worker(&n, &ids[2]).unwrap().0;
        let mut last = worker(&n, &ids[2]).unwrap().0;
        for i in 0..(WORKER_DISSOLVE * 60.0) as u32 + 10 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let c = worker(&n, &ids[2]).unwrap().0;
            assert!(len([c[0] - last[0], c[1] - last[1]]) < 2.0, "the others jump at {i}");
            last = c;
            if let Some((c, f)) = worker(&n, &ids[1]) {
                assert!(len([c[0] - gone0[0], c[1] - gone0[1]]) < 0.5, "the leaving one moves");
                if (i as f32) < WORKER_DISSOLVE * 60.0 * 0.8 {
                    assert!(f > 0.1, "it is gone too soon, at {i}");
                }
            }
        }
        assert!(worker(&n, &ids[1]).is_none());
        assert!(len([last[0] - start[0], last[1] - start[1]]) > 5.0, "the others close the gap");
    }

    #[test]
    fn a_worker_can_be_hovered_and_names_itself() {
        let now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], now);
        n.finish();
        let mut t = now;
        run(&mut n, 0.1, &mut t);
        let g = &n.globes[0];
        let p = n.place(g, &area());
        let w = &g.workers[0];
        let wp = Net::worker_place(&p, g.scale, w.ang, 1.0, &area());
        let id = w.id.clone();
        assert_eq!(n.hit(&area(), wp.c[0], wp.c[1]), Some(id.clone()));
        n.hover = Some(id);
        run(&mut n, 1.0, &mut t);
        let label = chip(&n.frame(&area(), false).unwrap()).unwrap();
        assert!(label.text.starts_with("a·w1 · "), "{}", label.text);
    }

    #[test]
    fn a_workers_feed_row_rings_the_worker_not_its_parent() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 2)], now);
        n.finish();
        n.ping("a", Some("a-w1"));
        assert!(n.globes[0].ping.is_none());
        assert!(n.globes[0].workers[1].ping.is_some() && n.globes[0].workers[0].ping.is_none());
        run(&mut n, 0.5, &mut now);
        let label = chip(&n.frame(&area(), false).unwrap()).unwrap();
        assert!(label.text.starts_with("a·w2"), "{}", label.text);
        // An unknown worker falls back to the session.
        n.ping("a", Some("gone"));
        assert!(n.globes[0].ping.is_some());
        // The ring fades in.
        assert!(ping_light(0.0, false) < 0.01 && ping_light(0.3, false) > 0.5);
    }

    use erebus_protocol::FeedKind;

    fn ev(session: &str, worker: Option<&str>, kind: FeedKind, phrase: &str, backlog: bool) -> FeedEvent {
        FeedEvent { unix_ms: T0, kind, phrase: phrase.into(), session: session.into(), worker: worker.map(str::to_owned), backlog }
    }

    fn docked_with_worker() -> (Net, u64) {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], now);
        n.finish();
        run(&mut n, 0.1, &mut now);
        (n, now)
    }

    #[test]
    fn events_pulse_the_braid_they_came_from_and_history_does_not() {
        let (mut n, _) = docked_with_worker();
        n.pulse(&[ev("a", None, FeedKind::Tool, "a · Bash cargo", true)]);
        assert!(!n.globes[0].pulses.busy(), "backlog is history");
        n.pulse(&[ev("a", Some("a-w0"), FeedKind::Tool, "a·w1 · Read x", false)]);
        assert!(n.globes[0].workers[0].pulses.busy() && !n.globes[0].pulses.busy());
        n.pulse(&[ev("a", None, FeedKind::Error, "a · Bash failed", false)]);
        assert!(n.globes[0].pulses.busy());
        let mut idle = net();
        idle.ingest_sessions(&[info("b", false, 0)], T0);
        idle.finish();
        idle.pulse(&[ev("b", None, FeedKind::Tool, "b · Read", false)]);
        assert!(!idle.globes[0].pulses.busy(), "a resting session has no braid to pulse");
    }

    #[test]
    fn a_pulse_runs_globe_to_core_smoothly_and_rises_and_falls() {
        let mut p = Pulses::default();
        p.push(theme::VERM);
        p.step(0.0, PULSE_SECS);
        let (mut head, mut light) = (1.0f32, 0.0f32);
        let mut peak = 0.0f32;
        for i in 1..(PULSE_SECS * 60.0) as u32 {
            p.step(1.0 / 60.0, PULSE_SECS);
            let Some(l) = p.lit(PULSE_SECS, false).first().copied() else { break };
            assert!(l.head <= head + 1e-6 && head - l.head < 0.03, "the head steps at {i}");
            assert!((l.light - light).abs() < 0.15, "the light pops at {i}");
            (head, light) = (l.head, l.light);
            peak = peak.max(light);
        }
        assert!(head < 0.02 && light < 0.1 && peak > 0.95);
    }

    #[test]
    fn pulses_ride_only_the_loose_fibres_in_the_events_colour() {
        let spec = Braid { strands: 6, fibres: 4, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col: theme::EMBER };
        let lit = [Lit { head: 0.5, light: 1.0, col: theme::VERM, whole: false }];
        let (mut plain, mut pulsed) = (Vec::new(), Vec::new());
        braid(&mut plain, [0.0, 0.0], [800.0, 0.0], 0.1, 1.0, false, 1.0, &spec, &[], 0.0, 1.0, 0.4, 3, false);
        braid(&mut pulsed, [0.0, 0.0], [800.0, 0.0], 0.1, 1.0, false, 1.0, &spec, &lit, 0.0, 1.0, 0.4, 3, false);
        let red: Vec<&Seg> = pulsed.iter().filter(|s| s.color[3] > 0.0 && (s.color[0] / s.color[3] - theme::VERM.srgb_f32()[0]).abs() < 0.01 && (s.color[1] / s.color[3] - theme::VERM.srgb_f32()[1]).abs() < 0.01).collect();
        assert!(!red.is_empty());
        // Every ember segment of the plain braid is still there, untouched: the pulse only adds.
        let ember = |v: &Vec<Seg>| v.iter().filter(|s| (s.color[1] / s.color[3].max(1e-6) - theme::EMBER.srgb_f32()[1]).abs() < 0.01).count();
        assert_eq!(ember(&plain), ember(&pulsed));
        // Lit only near the head and behind it, toward the globe end.
        for s in red.iter().filter(|s| s.width[1] == 0.0) {
            let x = (s.ab[0] + s.ab[2]) / 2.0;
            assert!(x > 400.0 - 20.0 && x < 400.0 + PULSE_TAIL * 2.0 + 20.0, "lit away from the head at {x}");
        }
        // Each lit segment lies on a fibre: wider than any strand's offset at that point.
        let strand_reach = 9.0 * 2.0;
        assert!(red.iter().filter(|s| s.width[1] == 0.0).any(|s| s.ab[1].abs() > strand_reach), "pulses ride the outer fibres");
    }

    #[test]
    fn the_fibres_flow_toward_the_core_and_the_strands_do_not() {
        // A crest's position along the path, a little later.
        let spec = Braid { strands: 1, fibres: 1, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col: theme::EMBER };
        let wave = |twist: f32, fibre: bool| {
            let mut out = Vec::new();
            braid(&mut out, [0.0, 0.0], [2000.0, 0.0], 0.0, 1.0, false, 1.0, &spec, &[], 0.0, 1.0, twist, 0, false);
            let segs: Vec<&Seg> = out.iter().filter(|s| s.width[1] == 0.0).collect();
            let half = segs.len() / 2;
            let part = if fibre { &segs[half..] } else { &segs[..half] };
            // Every crest along the middle of the path.
            part.windows(2).filter(|w| w[0].ab[0] > 600.0 && w[0].ab[0] < 1400.0 && w[0].ab[1] < w[0].ab[3] && w[1].ab[1] >= w[1].ab[3]).map(|w| w[0].ab[2]).collect::<Vec<f32>>()
        };
        // Follow one crest: the one nearest where it was a moment before.
        let moved = |fibre: bool| {
            let c0 = wave(0.0, fibre)[0];
            let c1 = wave(0.02, fibre).into_iter().min_by(|a, b| (a - c0).abs().total_cmp(&(b - c0).abs())).unwrap();
            c1 - c0
        };
        assert!(moved(true) < 0.0, "the fibre's crest moves toward the core");
        assert!(moved(false) > 0.0, "the strand's crest moves toward the globe");
    }

    #[test]
    fn same_instant_events_queue_in_feed_order() {
        let (mut n, mut now) = docked_with_worker();
        let burst = [
            ev("a", None, FeedKind::Tool, "a · Bash cargo", false),
            ev("a", None, FeedKind::Error, "a · Bash failed", false),
            ev("a", None, FeedKind::Result, "a · Bash ok", false),
        ];
        n.pulse(&burst);
        let mut order = Vec::new();
        let mut t = 0.0;
        while order.len() < 3 && t < 3.0 {
            run(&mut n, 1.0 / 60.0, &mut now);
            t += 1.0 / 60.0;
            for &(age, col) in &n.globes[0].pulses.live {
                if age <= 1.0 / 60.0 + 1e-4 && !order.iter().any(|(c, _)| *c == col) {
                    order.push((col, t));
                }
            }
        }
        assert_eq!(order.iter().map(|o| o.0).collect::<Vec<_>>(), vec![theme::EMBER, theme::VERM, theme::VERD]);
        for w in order.windows(2) {
            assert!(w[1].1 - w[0].1 >= PULSE_GAP - 0.02, "launched too close");
        }
    }

    #[test]
    fn the_queue_caps_and_drops_the_newest() {
        let mut p = Pulses::default();
        for _ in 0..PULSE_QUEUE {
            assert!(p.push(theme::EMBER));
        }
        assert!(!p.push(theme::VERM));
        assert_eq!(p.queue.len(), PULSE_QUEUE);
        assert!(p.queue.iter().all(|c| *c == theme::EMBER));
    }

    #[test]
    fn a_granted_permission_pulses_green() {
        assert_eq!(crate::feed::pulse_color(&ev("a", None, FeedKind::Perm, "a · granted Bash", false)), theme::VERD);
        assert_eq!(crate::feed::pulse_color(&ev("a", None, FeedKind::Perm, "a · denied Bash", false)), theme::VERM);
    }

    #[test]
    fn a_quiet_worker_holds_its_idle_parent_near() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], now);
        n.finish();
        // The session goes idle while its worker waits on a long job, saying nothing.
        n.ingest_sessions(&[info("a", false, 1)], now);
        now += RELEASE_AFTER * 3;
        n.step(0.0, false, now);
        run(&mut n, 2.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
        assert_eq!(n.globes[0].workers[0].form, 1.0, "the worker stays out");
        // The worker finishing starts the release clock over; after that the idle lets it go.
        n.ingest_sessions(&[info("a", false, 0)], now);
        n.step(0.0, false, now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
        now += RELEASE_AFTER;
        n.step(0.0, false, now);
        assert_eq!(phase(&n, "a"), Some(Phase::Releasing));
    }

    fn with_tasks(id: &str, kinds: &[TaskKind]) -> SessionInfo {
        let mut s = info(id, true, 0);
        s.tasks = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| erebus_protocol::TaskInfo { id: format!("{id}-t{i}"), kind: *k, label: format!("job{i}"), started_unix_ms: T0 })
            .collect();
        s
    }

    /// Where reach `id` of globe 0 ends, relative to its globe, and how grown it is.
    fn reach_at(n: &Net, id: &str) -> Option<([f32; 2], f32)> {
        let g = &n.globes[0];
        let p = n.place(g, &area());
        let r = g.tasks.iter().find(|r| r.id == id)?;
        let (_, to) = Net::reach_ends(&p, r, &area());
        Some(([to[0] - p.c[0], to[1] - p.c[1]], r.form))
    }

    #[test]
    fn a_task_reaches_out_slowly_and_unravels_when_done() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.finish();
        n.ingest_sessions(&[with_tasks("a", &[TaskKind::Shell])], now);
        let mut prev = 0.0;
        for i in 0..(REACH_GROW * 60.0) as u32 + 5 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let (_, f) = reach_at(&n, "a-t0").unwrap();
            assert!(f >= prev && f - prev < 0.03, "a step at {i}");
            prev = f;
        }
        assert_eq!(prev, 1.0);
        n.ingest_sessions(&[info("a", true, 0)], now);
        run(&mut n, REACH_UNRAVEL * 0.5, &mut now);
        let (_, f) = reach_at(&n, "a-t0").unwrap();
        assert!(f > 0.3 && f < 0.7, "unravelling, not gone: {f}");
        run(&mut n, REACH_UNRAVEL, &mut now);
        assert!(reach_at(&n, "a-t0").is_none());
    }

    #[test]
    fn reaches_fan_above_and_below_and_glide_when_one_ends() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[with_tasks("a", &[TaskKind::Shell, TaskKind::Monitor, TaskKind::Shell])], now);
        n.finish();
        run(&mut n, REACH_GROW + 0.2, &mut now);
        let (a, b) = (reach_at(&n, "a-t0").unwrap().0, reach_at(&n, "a-t1").unwrap().0);
        assert!(a[1] < 0.0 && b[1] > 0.0, "one above, one below");
        // t0 and t2 share the upper side; t0 ending frees the slot nearest the top.
        let mut s = with_tasks("a", &[TaskKind::Shell, TaskKind::Monitor, TaskKind::Shell]);
        s.tasks.remove(0);
        n.ingest_sessions(&[s], now);
        let start = reach_at(&n, "a-t2").unwrap().0;
        let mut last = start;
        for i in 0..90 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let c = reach_at(&n, "a-t2").unwrap().0;
            assert!(len([c[0] - last[0], c[1] - last[1]]) < 3.0, "a jump at {i}");
            last = c;
        }
        assert!(len([last[0] - start[0], last[1] - start[1]]) > 5.0, "it glides to the freed slot");
    }

    #[test]
    fn a_reach_can_be_hovered_and_names_its_task() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[with_tasks("a", &[TaskKind::Monitor])], now);
        n.finish();
        run(&mut n, REACH_GROW + 0.2, &mut now);
        let g = &n.globes[0];
        let p = n.place(g, &area());
        let (_, to) = Net::reach_ends(&p, &g.tasks[0], &area());
        assert_eq!(n.hit(&area(), to[0], to[1]).as_deref(), Some("a-t0"));
        n.hover = Some("a-t0".into());
        run(&mut n, 1.0, &mut now);
        assert_eq!(chip(&n.frame(&area(), false).unwrap()).unwrap().text, "a · monitor job0");
    }

    #[test]
    fn a_running_task_pulls_its_idle_session_in_and_holds_it() {
        let mut now = T0;
        let mut n = net();
        let mut s = with_tasks("a", &[TaskKind::Shell]);
        s.status = SessionStatus::Idle;
        n.ingest_sessions(&[info("a", false, 0)], now);
        n.finish();
        n.ingest_sessions(&[s], now);
        run(&mut n, GROW + HAUL_MAX + 1.0, &mut now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked));
        now += RELEASE_AFTER * 2;
        n.step(0.0, false, now);
        assert_eq!(phase(&n, "a"), Some(Phase::Docked), "held while the shell runs");
    }

    /// Two docked sessions, the first carrying `workers` and `tasks`, with the second dropped
    /// at `at` (knot radii), relative to the first. Returns how far the second ends up from
    /// that spot after `secs`, and the most it moved in one frame.
    fn shove(workers: usize, tasks: &[TaskKind], at: impl Fn(&Net) -> [f32; 2], secs: f32) -> (f32, f32) {
        let mut now = T0;
        let mut n = net();
        let mut a = with_tasks("a", tasks);
        a.workers = info("a", true, workers).workers;
        n.ingest_sessions(&[a, info("b", true, 0)], now);
        n.finish();
        run(&mut n, REACH_GROW + 0.3, &mut now);
        let spot = at(&n);
        let start = [n.globes[0].p[0] + spot[0], n.globes[0].p[1] + spot[1]];
        n.globes[1].p = start;
        n.globes[1].v = [0.0; 2];
        let mut last = start;
        let mut most = 0.0f32;
        for _ in 0..(secs * 60.0) as u32 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let p = n.globes[1].p;
            most = most.max(len([p[0] - last[0], p[1] - last[1]]));
            last = p;
        }
        (len([last[0] - start[0], last[1] - start[1]]), most)
    }

    #[test]
    fn a_worker_nudges_another_globe_aside_gently() {
        let worker = |n: &Net| {
            let g = &n.globes[0];
            let w = &g.workers[0];
            [WORKER_D * g.scale * w.ang.cos(), WORKER_D * g.scale * w.ang.sin()]
        };
        // The worker is part of its globe's body, as its own circle and its braid.
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], T0);
        n.finish();
        let g = &n.globes[0];
        let body = g.shapes();
        let w = worker(&n);
        let at = [g.p[0] + w[0], g.p[1] + w[1]];
        assert!(body.circles.iter().any(|(c, r)| len([c[0] - at[0], c[1] - at[1]]) < 1e-4 && (*r - WORKER_R * g.scale).abs() < 1e-4));
        assert_eq!(body.segments.len(), 2, "the knot braid and the worker braid");
        let (moved, most) = shove(1, &[], worker, 1.0);
        assert!(moved > 0.15, "pushed off the worker: {moved}");
        assert!(most < 0.05, "a shove, not a nudge: {most}");
    }

    #[test]
    fn a_reach_end_nudges_another_globe_aside() {
        let end = |n: &Net| {
            let g = &n.globes[0];
            let r = g.body();
            let t = &g.tasks[0];
            let out = r + REACH_LEN * r.max(SESSION_R * 0.5);
            [out * t.ang.cos(), out * t.ang.sin()]
        };
        let (moved, _) = shove(0, &[TaskKind::Shell], end, 1.0);
        assert!(moved > 0.1, "pushed off the reach's end: {moved}");
    }

    #[test]
    fn a_braid_gives_way_more_gently_than_a_globe() {
        // The same depth of overlap: a globe against a braid, and against another globe.
        let r = SESSION_R;
        let depth = 0.05;
        let globe = Body { circles: vec![([0.0, 0.0], r)], segments: vec![] };
        let braid = Body { circles: vec![], segments: vec![([-5.0, r + BRAID_HALF - depth], [5.0, r + BRAID_HALF - depth], BRAID_HALF, BRAID_GIVE)] };
        let other = Body { circles: vec![([0.0, 2.0 * r + GAP - depth], r)], segments: vec![] };
        let size = |c: Vec<([f32; 2], f32, f32)>| c.iter().map(|(p, on_a, _)| len(*p) * on_a).sum::<f32>();
        let (off_braid, off_globe) = (size(contacts(&globe, &braid)), size(contacts(&globe, &other)));
        assert!(off_braid > 0.0, "the braid still pushes");
        assert!(off_braid < off_globe * 0.6, "but more gently: {off_braid} vs {off_globe}");
        // A braid only pushes once touched.
        let clear = Body { circles: vec![], segments: vec![([-5.0, r + BRAID_HALF + 0.01], [5.0, r + BRAID_HALF + 0.01], BRAID_HALF, BRAID_GIVE)] };
        assert!(contacts(&globe, &clear).is_empty());
    }

    fn asking(id: &str, worker: Option<&str>, since: u64) -> SessionInfo {
        let mut s = info(id, true, 1);
        s.pending = Some(Pending {
            tool_use_id: format!("{id}-ask"),
            tool: "Bash".into(),
            worker: worker.map(str::to_owned),
            since_unix_ms: since,
            summary: Some("cargo test".into()),
        });
        s
    }

    fn answered(id: &str, granted: bool, at: u64) -> SessionInfo {
        let mut s = info(id, true, 1);
        s.decided = Some(erebus_protocol::Decided { tool_use_id: format!("{id}-ask"), granted, unix_ms: at });
        s
    }

    fn docked(sessions: &[SessionInfo]) -> (Net, u64) {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(sessions, now);
        n.finish();
        run(&mut n, 0.1, &mut now);
        (n, now)
    }

    #[test]
    fn an_ask_holds_the_braid_and_its_twist_stops_smoothly() {
        let (mut n, mut now) = docked(&[info("a", true, 1)]);
        n.ingest_sessions(&[asking("a", None, now)], now);
        let (mut hold, mut twist) = (0.0, n.globes[0].twist);
        let mut rate = f32::MAX;
        for i in 0..(HOLD_SECS * 60.0) as u32 + 10 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let g = &n.globes[0];
            assert!(g.hold >= hold && g.hold - hold < 0.05, "the hold steps at {i}");
            let r = g.twist - twist;
            assert!(r <= rate + 1e-6, "the twist speeds up while holding at {i}");
            (hold, twist, rate) = (g.hold, g.twist, r);
        }
        assert_eq!(hold, 1.0);
        assert!(rate < 1e-6, "held still");
        // Answered, it lets go and twists again.
        n.ingest_sessions(&[answered("a", true, now)], now);
        run(&mut n, HOLD_SECS + 0.1, &mut now);
        assert_eq!(n.globes[0].hold, 0.0);
        let t = n.globes[0].twist;
        run(&mut n, 0.5, &mut now);
        assert!(n.globes[0].twist > t);
    }

    #[test]
    fn the_gap_is_open_only_while_held() {
        let spec = Braid { strands: 6, fibres: 4, lam: 118.0, amp: 9.0, om: 3.4, width: 1.25, alpha: 0.9, col: theme::EMBER };
        let mid = |gap: f32| {
            let mut out = Vec::new();
            braid(&mut out, [0.0, 0.0], [1000.0, 0.0], 0.0, 1.0, false, 1.0, &spec, &[], gap, 1.0, 0.3, 1, false);
            out.iter().filter(|s| s.width[1] < 1.5 && ((s.ab[0] + s.ab[2]) / 2.0 - 500.0).abs() < 30.0).count()
        };
        assert!(mid(0.0) > 0, "a free braid crosses its middle");
        assert_eq!(mid(1.0), 0, "a held one does not");
    }

    #[test]
    fn the_oldest_ask_gets_the_card_and_the_rest_dims() {
        let (mut n, mut now) = docked(&[info("a", true, 1), info("b", true, 0), info("c", true, 0)]);
        n.ingest_sessions(&[asking("a", None, now + 500), asking("b", None, now), info("c", true, 0)], now);
        run(&mut n, 1.0, &mut now);
        let card = n.card.as_ref().expect("a card");
        assert_eq!(card.globe, "b", "the oldest ask");
        assert!(card.open > 0.99);
        let a = |id: &str| n.place(get(&n, id).unwrap(), &area()).a;
        assert!(a("c") < 0.3 * a("b"), "others dim: {} vs {}", a("c"), a("b"));
        let f = n.frame(&area(), false).unwrap();
        let card = f.card.expect("the card is drawn");
        assert!(card.text.items.iter().any(|i| i.text == "cargo test"));
    }

    #[test]
    fn a_verdict_shows_then_the_card_folds_and_the_focus_lifts() {
        let (mut n, mut now) = docked(&[info("a", true, 1)]);
        n.ingest_sessions(&[asking("a", Some("a-w0"), now)], now);
        run(&mut n, 1.0, &mut now);
        assert!(n.globes[0].workers[0].twist > 0.0);
        n.ingest_sessions(&[answered("a", false, now)], now);
        assert_eq!(n.card.as_ref().unwrap().verdict, Some(false));
        assert!(n.globes[0].workers[0].pulses.busy(), "the verdict runs home along the worker's braid");
        run(&mut n, VERDICT_SECS * 0.5, &mut now);
        assert!(n.card.is_some(), "the verdict shows a while");
        run(&mut n, VERDICT_SECS * 0.5 + CARD_OPEN + 0.2, &mut now);
        assert!(n.card.is_none());
        run(&mut n, 2.0, &mut now);
        assert_eq!(n.focus, 0.0);
    }

    #[test]
    fn the_card_stays_on_the_braid_after_the_answer() {
        let (mut n, mut now) = docked(&[info("a", true, 1)]);
        n.ingest_sessions(&[asking("a", None, now)], now);
        run(&mut n, 1.0, &mut now);
        let at = |n: &Net| n.frame(&area(), false).unwrap().card.map(|c| c.rects[0].0);
        let before = at(&n).unwrap();
        n.ingest_sessions(&[answered("a", true, now)], now);
        // It stays exactly where it unfolded, through the sway, the answer and the fold.
        for i in 0..((HOLD_SECS + VERDICT_SECS) * 60.0) as u32 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let Some(r) = at(&n) else { break };
            assert_eq!(r[..2], before[..2], "the card moves at {i}");
        }
    }

    #[test]
    fn an_ask_that_vanishes_unanswered_folds_the_card() {
        let (mut n, mut now) = docked(&[info("a", true, 1)]);
        n.ingest_sessions(&[asking("a", None, now)], now);
        run(&mut n, 1.0, &mut now);
        n.ingest_sessions(&[info("a", true, 1)], now);
        run(&mut n, 0.1, &mut now);
        let c = n.card.as_ref().unwrap();
        assert!(c.closing && c.verdict.is_none());
        run(&mut n, CARD_OPEN + 0.1, &mut now);
        assert!(n.card.is_none());
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
        n.finish();
        n.ingest_sessions(&[info("a", true, 1)], now);
        assert_eq!(n.globes[0].workers[0].form, 0.0);
        run(&mut n, WORKER_EMERGE * 0.5, &mut now);
        assert!(n.globes[0].workers[0].form < 1.0, "emergence is slow");
        run(&mut n, WORKER_EMERGE * 0.5 + 0.1, &mut now);
        assert_eq!(n.globes[0].workers[0].form, 1.0);
        n.ingest_sessions(&[info("a", true, 0)], now);
        run(&mut n, WORKER_DISSOLVE + 0.1, &mut now);
        assert!(n.globes[0].workers.is_empty());
    }

    /// Where worker 0 of globe 0 is drawn, relative to its parent's centre, and its radius.
    fn worker_at(n: &Net) -> ([f32; 2], f32) {
        let g = &n.globes[0];
        let p = n.place(g, &area());
        let w = &g.workers[0];
        let wp = Net::worker_place(&p, g.scale, w.ang, e_in_out(w.emerge), &area());
        ([wp.c[0] - p.c[0], wp.c[1] - p.c[1]], wp.r)
    }

    #[test]
    fn a_worker_glides_out_of_the_nucleus_without_steps() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 0)], now);
        n.finish();
        n.ingest_sessions(&[info("a", true, 1)], now);
        let (c0, r0) = worker_at(&n);
        assert!(len(c0) < 1e-3, "starts in the nucleus");
        let (mut c, mut r, mut v) = (c0, r0, [0.0f32; 2]);
        for _ in 0..(WORKER_EMERGE * 60.0) as u32 + 6 {
            run(&mut n, 1.0 / 60.0, &mut now);
            let (c1, r1) = worker_at(&n);
            let v1 = [c1[0] - c[0], c1[1] - c[1]];
            assert!(len([v1[0] - v[0], v1[1] - v[1]]) < 0.2, "the glide steps");
            assert!(r1 >= r - 1e-4, "it only grows");
            (c, r, v) = (c1, r1, v1);
        }
        assert!(r > r0 * 5.0, "a speck grows to full size");
        assert_eq!(n.globes[0].workers[0].emerge, 1.0);
    }

    #[test]
    fn a_done_worker_turns_to_dust_where_it_stands() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", true, 1)], now);
        n.finish();
        let (c0, _) = worker_at(&n);
        n.ingest_sessions(&[info("a", true, 0)], now);
        run(&mut n, WORKER_DISSOLVE * 0.5, &mut now);
        let w = &n.globes[0].workers[0];
        assert!(w.form > 0.3 && w.form < 0.7, "dissolving, not gone");
        let (c1, _) = worker_at(&n);
        assert!(len([c1[0] - c0[0], c1[1] - c0[1]]) < 1.0, "it holds its place");
        run(&mut n, WORKER_DISSOLVE, &mut now);
        assert!(n.globes[0].workers.is_empty());
    }

    #[test]
    fn a_new_session_condenses() {
        let mut now = T0;
        let mut n = net();
        n.ingest_sessions(&[info("a", false, 0)], now);
        n.ingest_sessions(&[info("a", false, 0), info("b", false, 0)], now);
        assert_eq!(get(&n, "b").unwrap().form, 0.0);
        run(&mut n, CONDENSE * 0.5, &mut now);
        assert!(get(&n, "b").unwrap().form < 0.6, "condensing is slow");
        run(&mut n, CONDENSE * 0.5 + 0.1, &mut now);
        assert_eq!(get(&n, "b").unwrap().form, 1.0);
    }
}
