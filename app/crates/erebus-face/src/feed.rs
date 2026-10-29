//! Live feed: session events at the bottom of the left column under the vitals. After Loom
//! 4's `#feed` and `.fr` rows (`v3/concept-loom4.html`).
//!
//! Six rows show, newest at the bottom; the feed keeps the last [`KEEP`]. Each row is a clock
//! line over a short phrase, with a left rule in its kind's colour. A new row rises 10 px into
//! place and fades in over 0.6 s while the rows above ease up a slot, and the top row fades as
//! it crosses the feed's top edge. With reduced motion rows only cross-fade. History replayed
//! on connect shows at once.
//!
//! The wheel scrolls the history smoothly, with no bar. Scrolled up, the view holds still as
//! new rows arrive below it; once the wheel rests, a "↓ live" chip offers the way back.

use erebus_protocol::{FeedEvent, FeedKind};
use erebus_theme::{self as theme, Rgb};
use std::collections::VecDeque;

/// Rows shown at rest, and rows kept for scrolling back.
pub const ROWS: usize = 6;
pub const KEEP: usize = 300;
/// Steady opacity by slot, top to bottom.
const OPACITY: [f32; ROWS] = [0.84, 0.87, 0.9, 0.93, 0.96, 1.0];
const ENTER: f32 = 0.6;
const REDUCED: f32 = 0.15;
/// Rows ease toward their slots, and the view toward its scroll, with these time constants.
const SETTLE: f32 = 0.12;
const GLIDE: f32 = 0.09;
/// The live chip shows once the wheel has rested this long, and fades in over the next.
const CHIP_AFTER: f32 = 0.4;
const CHIP_FADE: f32 = 0.25;
/// Prototype CSS pixels: `.fr` padding, the clock and phrase sizes, the gap between rows.
const PAD_Y: f32 = 4.0;
const PAD_L: f32 = 10.0;
const PAD_R: f32 = 6.0;
pub const TIME_SIZE: f32 = 12.5;
pub const PHRASE_SIZE: f32 = 13.0;
pub const CHIP_SIZE: f32 = 12.0;
const GAP: f32 = 5.0;

/// A turn row's wash: peak opacity, and a mono digit's advance in ems.
const WASH: f32 = 0.22;
const WASH_ADVANCE: f32 = 0.59;

/// Clock line colour, the prototype's `.ft`.
pub const CLOCK: Rgb = Rgb::hex(0xcfc6b3);
/// Row ground under the cursor.
const HOVER: Rgb = Rgb::hex(0x17140f);

/// The prototype's `KCOL` for each kind; a turn's end reads as settled verdigris.
pub fn color(kind: FeedKind) -> Rgb {
    match kind {
        FeedKind::Tool => theme::EMBER,
        FeedKind::Result | FeedKind::Turn => theme::VERD,
        FeedKind::Error | FeedKind::Perm => theme::VERM,
        FeedKind::Session | FeedKind::Spawn => theme::BONE,
        FeedKind::Compact => theme::LILAC,
        FeedKind::Skill => theme::BONE_2,
    }
}

#[derive(Clone, Debug)]
struct Row {
    id: u64,
    ev: FeedEvent,
    clock: String,
    /// Feed time it arrived; `None` shows it settled at once.
    born: Option<f64>,
    /// Drawn slot above the newest, in rows; eases toward its index.
    y: f32,
}

/// Rows, the scroll and their motion.
#[derive(Clone, Debug, Default)]
pub struct Feed {
    /// Oldest first.
    rows: VecDeque<Row>,
    /// Feed time, seconds; runs only while something moves.
    clock: f64,
    next_id: u64,
    /// The next batch is history: it shows at once.
    replay: bool,
    /// Rows scrolled back from the live end: drawn, and where the wheel has sent it.
    scroll: f32,
    target: f32,
    /// Seconds since the wheel last turned, and rows that arrived while scrolled back.
    rest: f32,
    unseen: usize,
    pub hover: Option<u64>,
    pub chip_hover: bool,
}

impl Feed {
    pub fn new() -> Self {
        Feed { replay: true, ..Default::default() }
    }

    /// Forgets every row; the core replays its history on reconnect.
    pub fn clear(&mut self) {
        *self = Feed { next_id: self.next_id, ..Feed::new() };
    }

    /// Scrolled back from the live end.
    pub fn scrolled(&self) -> bool {
        self.target > 0.0
    }

    /// Takes events, oldest first. Backlog and the first batch after a clear settle at once.
    pub fn ingest(&mut self, events: Vec<FeedEvent>) {
        let replay = std::mem::replace(&mut self.replay, false);
        for ev in events {
            let instant = replay || ev.backlog;
            self.next_id += 1;
            let clock = clock_text(ev.unix_ms);
            if self.scrolled() {
                // The view holds still: every row and the scroll step up together.
                for r in &mut self.rows {
                    r.y += 1.0;
                }
                self.scroll += 1.0;
                self.target += 1.0;
                self.unseen += 1;
            }
            self.rows.push_back(Row { id: self.next_id, ev, clock, born: (!instant).then_some(self.clock), y: 0.0 });
            if self.rows.len() > KEEP {
                self.rows.pop_front();
                self.target = self.target.min(self.max_scroll());
                self.scroll = self.scroll.min(self.max_scroll());
            }
            if instant {
                self.settle_slots();
            }
        }
    }

    fn max_scroll(&self) -> f32 {
        self.rows.len().saturating_sub(ROWS) as f32
    }

    /// Puts every row straight onto its slot.
    fn settle_slots(&mut self) {
        let n = self.rows.len();
        for (i, r) in self.rows.iter_mut().enumerate() {
            r.y = (n - 1 - i) as f32;
            r.born = None;
        }
    }

    /// Turns the wheel by `rows`: positive scrolls back into history.
    pub fn scroll_by(&mut self, rows: f32) {
        self.target = (self.target + rows).clamp(0.0, self.max_scroll());
        self.rest = 0.0;
        if self.target == 0.0 {
            self.unseen = 0;
        }
    }

    /// The session a row came from.
    pub fn session_of(&self, id: u64) -> Option<&str> {
        self.rows.iter().find(|r| r.id == id).map(|r| r.ev.session.as_str())
    }

    /// Glides back to the live end.
    pub fn go_live(&mut self) {
        self.target = 0.0;
        self.unseen = 0;
        self.chip_hover = false;
    }

    /// Opacity of the live chip.
    fn chip_alpha(&self) -> f32 {
        if self.scrolled() { ((self.rest - CHIP_AFTER) / CHIP_FADE).clamp(0.0, 1.0) } else { 0.0 }
    }

    /// Something moves.
    pub fn busy(&self) -> bool {
        let n = self.rows.len();
        let chip = self.scrolled() && self.rest < CHIP_AFTER + CHIP_FADE;
        chip || (self.scroll - self.target).abs() > 0.001
            || self.rows.iter().enumerate().any(|(i, r)| r.born.is_some() || (r.y - (n - 1 - i) as f32).abs() > 0.002)
    }

    pub fn step(&mut self, dt: f32, reduced: bool) {
        if !self.busy() {
            return;
        }
        let dt = dt.max(0.0);
        self.clock += f64::from(dt);
        self.rest += dt;
        let now = self.clock;
        let enter = if reduced { REDUCED } else { ENTER };
        let ease = |tau: f32| if reduced { 1.0 } else { 1.0 - (-dt / tau).exp() };
        let n = self.rows.len();
        let k = ease(SETTLE);
        for (i, r) in self.rows.iter_mut().enumerate() {
            let t = (n - 1 - i) as f32;
            r.y += (t - r.y) * k;
            if (t - r.y).abs() < 0.002 {
                r.y = t;
            }
            if r.born.is_some_and(|b| now - b >= f64::from(enter)) {
                r.born = None;
            }
        }
        self.scroll += (self.target - self.scroll) * ease(GLIDE);
        if (self.target - self.scroll).abs() < 0.001 {
            self.scroll = self.target;
        }
    }

    /// Finishes all motion at once.
    pub fn snap(&mut self) {
        self.settle_slots();
        self.scroll = self.target;
    }
}

/// `HH:MM:SS` in local time.
fn clock_text(unix_ms: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;
    // FILETIME counts 100 ns ticks from 1601.
    let ticks = (unix_ms + 11_644_473_600_000) * 10_000;
    let utc = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: all three structs are ours and live across the calls.
    let ok = unsafe { FileTimeToLocalFileTime(&utc, &mut local) != 0 && FileTimeToSystemTime(&local, &mut st) != 0 };
    if !ok {
        let s = unix_ms / 1000;
        return format!("{:02}:{:02}:{:02}", s / 3600 % 24, s / 60 % 60, s % 60);
    }
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

/// The feed's box, physical pixels: the column's width, bottom-aligned on the band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// `[x, y, w, h]`.
    pub rect: [f32; 4],
    /// Row box left and width, and the text's left edge.
    pub row_x: f32,
    pub row_w: f32,
    pub text_x: f32,
    /// One row's height and the distance between row tops.
    pub row_h: f32,
    pub pitch: f32,
    /// Type scale.
    pub k: f32,
    pub px: f32,
}

pub fn layout(col: &crate::vitals::Column) -> Layout {
    let [x, y, w, h] = col.feed;
    let (k, px) = (col.k, col.px);
    let row_h = ((PAD_Y * 2.0 + TIME_SIZE * 1.3 + PHRASE_SIZE * 1.3) * k).round();
    let row_x = x + col.pad_l;
    let row_w = w - col.pad_l - 10.0 * k;
    Layout {
        rect: [x, y, w, h],
        row_x,
        row_w,
        text_x: (row_x + 2.0 * px + PAD_L * k).round(),
        row_h,
        pitch: row_h + (GAP * k).round(),
        k,
        px,
    }
}

impl Layout {
    /// Text width left for a row's words.
    pub fn text_w(&self) -> f32 {
        self.row_x + self.row_w - PAD_R * self.k - self.text_x
    }

    /// The live chip: centred on the feed, just under its bottom edge.
    pub fn chip(&self) -> [f32; 4] {
        let (w, h) = (130.0 * self.k, (CHIP_SIZE * 1.3 + 8.0) * self.k);
        let [x, y, fw, fh] = self.rect;
        [(self.row_x + (fw - (self.row_x - x)) * 0.5 - w * 0.5).round(), (y + fh + 6.0 * self.k).round(), w.round(), h.round()]
    }
}

/// One drawn row.
#[derive(Clone, Debug, PartialEq)]
pub struct RowFrame {
    /// Names the row; its words never change.
    pub id: u64,
    pub clock: String,
    pub phrase: String,
    /// Row box `[x, y, w, h]`.
    pub rect: [f32; 4],
    pub alpha: f32,
    pub color: Rgb,
    pub hover: bool,
    /// A turn's end: a wash of its colour fades out from the rule.
    pub wash: bool,
}

/// The live chip's words and opacity.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub text: String,
    pub alpha: f32,
    pub hover: bool,
}

pub struct Frame {
    pub layout: Layout,
    pub rows: Vec<RowFrame>,
    /// Row rules, the hovered row's ground and the chip's box: `[x, y, w, h]`, colour, opacity.
    pub rects: Vec<([f32; 4], Rgb, f32)>,
    /// Turn rows' washes: like `rects`, fading out to the right.
    pub washes: Vec<([f32; 4], Rgb, f32)>,
    pub chip: Option<Chip>,
}

impl RowFrame {
    pub fn clock_top(&self, l: &Layout) -> f32 {
        (self.rect[1] + PAD_Y * l.k).round()
    }

    pub fn phrase_top(&self, l: &Layout) -> f32 {
        (self.rect[1] + (PAD_Y + TIME_SIZE * 1.3) * l.k).round()
    }
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// What the cursor is over in the feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Row(u64),
    Chip,
    /// The feed's box, between rows.
    Ground,
}

impl Feed {
    pub fn hit(&self, col: &crate::vitals::Column, x: f32, y: f32) -> Option<Spot> {
        let f = self.frame(col, false)?;
        let inside = |[rx, ry, rw, rh]: [f32; 4]| x >= rx && x < rx + rw && y >= ry && y < ry + rh;
        if f.chip.as_ref().is_some_and(|c| c.alpha > 0.5) && inside(f.layout.chip()) {
            return Some(Spot::Chip);
        }
        if let Some(r) = f.rows.iter().find(|r| r.alpha > 0.5 && inside(r.rect)) {
            return Some(Spot::Row(r.id));
        }
        inside(f.layout.rect).then_some(Spot::Ground)
    }

    /// Builds the feed, or `None` while it is empty.
    pub fn frame(&self, col: &crate::vitals::Column, reduced: bool) -> Option<Frame> {
        if self.rows.is_empty() {
            return None;
        }
        let l = layout(col);
        let [_, y, _, h] = l.rect;
        let bottom = y + h;
        let enter = if reduced { REDUCED } else { ENTER };
        let lift = 10.0 * l.k;
        let top_slot = ROWS as f32 - 1.0;

        let mut rows = Vec::with_capacity(ROWS + 2);
        for r in &self.rows {
            // Place in the view, in rows above the bottom one.
            let v = r.y - self.scroll;
            if !(-1.0..ROWS as f32).contains(&v) {
                continue;
            }
            let mut top = bottom - l.row_h - v * l.pitch;
            let at = (top_slot - v).clamp(0.0, top_slot);
            let (i, f) = (at.floor() as usize, at.fract());
            let mut a = OPACITY[i] + (OPACITY[(i + 1).min(ROWS - 1)] - OPACITY[i]) * f;
            // Rows fade as they cross the top or bottom edge.
            a *= (1.0 - (v - top_slot)).clamp(0.0, 1.0) * (1.0 + v).clamp(0.0, 1.0);
            if let Some(b) = r.born {
                let t = ((self.clock - b) as f32 / enter).clamp(0.0, 1.0);
                a *= smooth(t);
                if !reduced {
                    top += lift * (1.0 - smooth(t));
                }
            }
            if a <= 0.0 {
                continue;
            }
            let hover = self.hover == Some(r.id);
            rows.push(RowFrame {
                id: r.id,
                clock: r.clock.clone(),
                phrase: r.ev.phrase.clone(),
                rect: [l.row_x, top.round(), l.row_w, l.row_h],
                alpha: if hover { 1.0 } else { a },
                color: color(r.ev.kind),
                hover,
                wash: r.ev.kind == FeedKind::Turn,
            });
        }

        let mut rects = Vec::with_capacity(rows.len() + 4);
        let mut washes = Vec::new();
        for r in &rows {
            // Rows past an edge are cut there.
            let [rx, ry, rw, rh] = r.rect;
            let (t, b) = (ry.max(y), (ry + rh).min(bottom));
            if b <= t {
                continue;
            }
            if r.hover {
                rects.push(([rx, t, rw, b - t], HOVER, 1.0));
            }
            rects.push(([rx, t, 2.0 * l.px, b - t], r.color, 0.9 * r.alpha));
            if r.wash {
                // From the rule to the clock's first colon: two mono digits in.
                let x0 = rx + 2.0 * l.px;
                let x1 = l.text_x + 2.0 * WASH_ADVANCE * TIME_SIZE * l.k;
                washes.push(([x0, t, x1 - x0, b - t], r.color, WASH * r.alpha));
            }
        }

        let ca = self.chip_alpha();
        let chip = (ca > 0.0).then(|| {
            let c = l.chip();
            let hair = l.px.round().max(1.0);
            let edge = if self.chip_hover { theme::EMBER } else { theme::RULE_2 };
            rects.push((c, edge, ca));
            rects.push(([c[0] + hair, c[1] + hair, c[2] - 2.0 * hair, c[3] - 2.0 * hair], theme::SLAB, ca));
            let text = if self.unseen > 0 { format!("↓ live · {} new", self.unseen) } else { "↓ live".into() };
            Chip { text, alpha: ca, hover: self.chip_hover }
        });
        Some(Frame { layout: l, rows, rects, washes, chip })
    }
}

/// Canned events for screenshots: a session at work with a worker.
pub fn canned() -> Vec<FeedEvent> {
    let ev = |s: u64, kind, phrase: &str| FeedEvent {
        unix_ms: crate::vitals::CANNED_T0 - 60_000 + s * 1000,
        kind,
        phrase: phrase.into(),
        session: "aabbb6cd".into(),
        worker: None,
        backlog: false,
    };
    vec![
        ev(0, FeedKind::Session, "erebus · started"),
        ev(4, FeedKind::Tool, "erebus · Read panel.rs"),
        ev(9, FeedKind::Spawn, "erebus · spawned Explore"),
        ev(12, FeedKind::Tool, "erebus·w1 · Grep tool.check"),
        ev(18, FeedKind::Tool, "erebus · Bash cargo"),
        ev(26, FeedKind::Turn, "erebus · turn done · 1 running"),
        ev(31, FeedKind::Error, "erebus · Bash failed"),
        ev(36, FeedKind::Perm, "erebus · granted Bash"),
        ev(40, FeedKind::Result, "erebus·w1 · done"),
        ev(41, FeedKind::Turn, "erebus · idle"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(i: u64, backlog: bool) -> FeedEvent {
        FeedEvent {
            unix_ms: 1_761_700_000_000 + i * 1000,
            kind: FeedKind::Tool,
            phrase: format!("erebus · Read f{i}.rs"),
            session: "s".into(),
            worker: None,
            backlog,
        }
    }

    fn col() -> crate::vitals::Column {
        crate::vitals::column(5120, 1440, 1.0).unwrap()
    }

    fn run(f: &mut Feed, secs: f32) {
        for _ in 0..(secs * 60.0) as u32 {
            f.step(1.0 / 60.0, false);
        }
    }

    fn shown(f: &Feed) -> Vec<String> {
        f.frame(&col(), false).unwrap().rows.iter().filter(|r| r.alpha > 0.5).map(|r| r.phrase.clone()).collect()
    }

    #[test]
    fn replay_settles_at_once_and_shows_six() {
        let mut f = Feed::new();
        f.ingest((0..10).map(|i| ev(i, false)).collect());
        assert!(!f.busy());
        let s = shown(&f);
        assert_eq!(s.len(), ROWS);
        assert_eq!(s.first().unwrap(), "erebus · Read f4.rs");
        assert_eq!(s.last().unwrap(), "erebus · Read f9.rs");
    }

    #[test]
    fn a_live_event_enters_and_the_top_row_leaves() {
        let mut f = Feed::new();
        f.ingest((0..6).map(|i| ev(i, false)).collect());
        f.ingest(vec![ev(6, false)]);
        assert!(f.busy());
        f.step(1.0 / 60.0, false);
        let fr = f.frame(&col(), false).unwrap();
        assert!(fr.rows.iter().find(|r| r.phrase.ends_with("f6.rs")).unwrap().alpha < 0.05, "starts transparent");
        run(&mut f, 1.0);
        assert!(!f.busy());
        let s = shown(&f);
        assert_eq!(s.len(), ROWS);
        assert_eq!(s.first().unwrap(), "erebus · Read f1.rs");
    }

    #[test]
    fn scrolling_back_holds_the_view_as_rows_arrive() {
        let mut f = Feed::new();
        f.ingest((0..20).map(|i| ev(i, false)).collect());
        f.scroll_by(5.0);
        run(&mut f, 1.0);
        let before = shown(&f);
        assert_eq!(before.last().unwrap(), "erebus · Read f14.rs");
        f.ingest(vec![ev(20, false), ev(21, false)]);
        run(&mut f, 1.0);
        assert_eq!(shown(&f), before, "the view does not move");
        let chip = f.frame(&col(), false).unwrap().chip.unwrap();
        assert_eq!(chip.text, "↓ live · 2 new");
        f.go_live();
        run(&mut f, 1.0);
        assert_eq!(shown(&f).last().unwrap(), "erebus · Read f21.rs");
        assert!(f.frame(&col(), false).unwrap().chip.is_none());
    }

    #[test]
    fn the_chip_waits_for_the_wheel_to_rest() {
        let mut f = Feed::new();
        f.ingest((0..20).map(|i| ev(i, false)).collect());
        f.scroll_by(3.0);
        f.step(0.1, false);
        assert!(f.frame(&col(), false).unwrap().chip.is_none());
        run(&mut f, 1.0);
        assert_eq!(f.frame(&col(), false).unwrap().chip.unwrap().alpha, 1.0);
    }

    #[test]
    fn scroll_stops_at_both_ends_and_history_is_capped() {
        let mut f = Feed::new();
        f.ingest((0..(KEEP as u64 + 50)).map(|i| ev(i, false)).collect());
        f.scroll_by(-3.0);
        assert!(!f.scrolled());
        f.scroll_by(1e6);
        run(&mut f, 2.0);
        assert_eq!(shown(&f).first().unwrap(), "erebus · Read f50.rs", "oldest kept row at the top");
    }

    #[test]
    fn rows_sit_bottom_up_inside_the_feed() {
        let mut f = Feed::new();
        f.ingest((0..6).map(|i| ev(i, false)).collect());
        let fr = f.frame(&col(), false).unwrap();
        let [_, y, _, h] = fr.layout.rect;
        let last = fr.rows.last().unwrap().rect;
        assert!((last[1] + last[3] - (y + h)).abs() < 1.0, "newest row at the bottom");
        assert!(fr.rows[0].rect[1] >= y, "six rows fit");
    }

    #[test]
    fn clear_then_replay_does_not_animate() {
        let mut f = Feed::new();
        f.ingest(vec![ev(0, false)]);
        f.ingest(vec![ev(1, false)]);
        f.clear();
        assert!(f.frame(&col(), false).is_none());
        f.ingest((0..3).map(|i| ev(i, false)).collect());
        assert!(!f.busy());
    }

    #[test]
    fn clock_is_hh_mm_ss() {
        let c = clock_text(1_761_700_000_000);
        assert_eq!(c.len(), 8);
        assert_eq!(&c[2..3], ":");
    }
}
