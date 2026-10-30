//! The approval card, after Loom 4's `#approval` surface: it unfolds just below the held gap
//! in the braid that asked, names what is asked and who waits, and shows the verdict before
//! folding away. Drawn through the panel host (`panel::Frame`): rects, strokes and text.
//!
//! Erebus only shows the ask. The answer is given in the Claude Code terminal.

use crate::panel::{self, clip_rect, clip_seg, Item, Text};
use crate::vitals::{self, Seg};
use erebus_theme::{self as theme, Rgb};

/// The card's ground and its code block's.
const GROUND: Rgb = Rgb::hex(0x120b09);
const CODE: Rgb = Rgb::hex(0x1c1310);
/// Header text on the vermilion and verdigris washes.
const HEAD: Rgb = Rgb::hex(0xffcfc5);
/// Code lines at most, and the card's width in CSS pixels. Kept small: the network carries
/// the alarm, the card only says what is asked.
const CODE_LINES: usize = 2;
const WIDTH: f32 = 280.0;
/// The card's scale against the rest of the network's type.
const SCALE: f32 = 0.9;
/// Header band, and one code line, in CSS pixels.
const HEAD_H: f32 = 40.0;
const LINE: f32 = 16.0;

/// What the card says.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// Who waits: `stratum` or `stratum·w2`.
    pub who: String,
    pub tool: String,
    pub summary: Option<String>,
    pub since_ms: u64,
}

pub struct View<'a> {
    pub card: &'a Card,
    /// Where the braid is held now, physical pixels, for the tether; and where it was when
    /// the card unfolded, which places the card, so the card stays still as the braid sways.
    pub gap: [f32; 2],
    pub at: [f32; 2],
    /// 0..1 unfolded.
    pub open: f32,
    /// The answer, once given: granted or not.
    pub verdict: Option<bool>,
    pub now_ms: u64,
    /// Seconds on a free clock, for the flag's blink.
    pub clock: f32,
    /// Network area: left and right, band top and bottom; type scale and prototype `U`.
    pub x: [f32; 2],
    pub band: [f32; 2],
    pub k: f32,
    pub u: f32,
    pub reduced: bool,
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `m:ss` since `since`.
fn waited(since: u64, now: u64) -> String {
    let s = now.saturating_sub(since) / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Splits `text` into lines of at most `cols` characters, at most `max` of them, the last
/// ending in an ellipsis when cut.
fn wrap(text: &str, cols: usize, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = chars.chunks(cols.max(8)).map(|c| c.iter().collect()).collect();
    if out.len() > max {
        out.truncate(max);
        if let Some(last) = out.last_mut() {
            last.pop();
            last.push('…');
        }
    }
    out
}

/// The card's place: below the gap and a little left of it, kept inside the area.
pub fn rect(v: &View, lines: usize) -> [f32; 4] {
    let k = v.k * SCALE;
    let w = (WIDTH * k).min(v.x[1] - v.x[0] - 12.0 * k);
    let h = (HEAD_H + 12.0 + LINE * lines as f32 + 30.0) * k;
    let x = (v.at[0] - w * 0.22).clamp(v.x[0], v.x[1] - w).round();
    let y = (v.at[1] + 40.0 * v.u).clamp(v.band[0] + 6.0, v.band[1] - h - 6.0).round();
    [x, y, w.round(), h.round()]
}

/// One frame of the card.
pub fn frame(v: &View) -> panel::Frame {
    let k = v.k * SCALE;
    let c = v.card;
    let cols = ((WIDTH - 32.0) / (11.0 * 0.6)) as usize;
    let code = wrap(c.summary.as_deref().unwrap_or(&c.tool), cols, CODE_LINES);
    let [x, y, w, h] = rect(v, code.len());
    let tone = match v.verdict {
        Some(true) => theme::VERD,
        _ => theme::VERM,
    };
    let unfold = if v.reduced { 1.0 } else { 1.0 - (1.0 - v.open.clamp(0.0, 1.0)).powi(3) };
    let front = x + w * unfold;
    let a = smooth(v.open / 0.6);

    let mut rects = Vec::new();
    let mut add = |r: [f32; 4], col: Rgb, alpha: f32| {
        if let Some(r) = clip_rect(r, front) {
            rects.push((r, col, alpha));
        }
    };
    add([x, y, w, h], GROUND, 0.96 * a);
    add([x, y, w, HEAD_H * k], tone, 0.16 * a);
    let blink = if v.reduced || v.verdict.is_some() { 1.0 } else { 0.62 + 0.38 * (v.clock * std::f32::consts::TAU / 1.4).cos() };
    add([x + 12.0 * k, y + 11.0 * k, 6.0 * k, 6.0 * k], tone, blink * a);
    let code_top = HEAD_H + 8.0;
    add([x + 10.0 * k, y + (code_top - 4.0) * k, w - 20.0 * k, (LINE * code.len() as f32 + 8.0) * k], CODE, 0.95 * a);

    let lws = (1.6 * v.u).max(1.0);
    let mut segs = Vec::new();
    let stroke = |p: [f32; 2], q: [f32; 2], col: Rgb, alpha: f32, width: f32, segs: &mut Vec<Seg>| {
        if let Some((p, q)) = clip_seg(p, q, front) {
            segs.push(vitals::seg(p, q, col, alpha, width));
        }
    };
    for (p, q) in [([x, y], [x + w, y]), ([x, y + h], [x + w, y + h]), ([x, y], [x, y + h]), ([x + w, y], [x + w, y + h])] {
        stroke(p, q, tone, 0.9 * a, lws, &mut segs);
    }
    stroke([x, y + HEAD_H * k], [x + w, y + HEAD_H * k], tone, 0.45 * a, lws, &mut segs);
    // The tether: a bow from the gap down to the card's top edge.
    let anchor = [v.gap[0].clamp(x + 12.0 * k, x + w - 12.0 * k), y];
    let mid = [(v.gap[0] + anchor[0]) * 0.5 - 18.0 * v.u, (v.gap[1] + anchor[1]) * 0.5];
    let bez = |t: f32| {
        let s = 1.0 - t;
        [s * s * v.gap[0] + 2.0 * s * t * mid[0] + t * t * anchor[0], s * s * v.gap[1] + 2.0 * s * t * mid[1] + t * t * anchor[1]]
    };
    for i in 0..12 {
        let (t0, t1) = (i as f32 / 12.0, (i + 1) as f32 / 12.0);
        if t1 <= unfold.max(0.05) {
            segs.push(vitals::seg(bez(t0), bez(t1), tone, 0.7 * a, lws));
        }
    }
    segs.push(vitals::bead(anchor[0], anchor[1], 3.0 * lws, tone, 0.9 * a));

    let flag = match v.verdict {
        None => "APPROVAL REQUIRED",
        Some(true) => "APPROVED",
        Some(false) => "DENIED",
    };
    let it = |dx: f32, dy: f32, size: f32, bold: bool, text: String, color: Rgb| Item { x: x + dx * k, y: y + dy * k, right: false, size, mono: true, bold, text, color };
    let mut items = vec![
        it(24.0, 6.0, 11.0, true, flag.into(), HEAD),
        it(12.0, 22.0, 10.0, false, format!("{} · {}", c.who, c.tool.to_uppercase()), theme::BONE_2),
        Item { x: x + w - 12.0 * k, y: y + 6.0 * k, right: true, size: 10.0, mono: true, bold: false, text: waited(c.since_ms, v.now_ms), color: HEAD },
    ];
    for (i, line) in code.iter().enumerate() {
        items.push(it(16.0, code_top + LINE * i as f32, 11.0, false, line.clone(), theme::BONE));
    }
    if v.verdict.is_none() {
        items.push(it(12.0, code_top + LINE * code.len() as f32 + 10.0, 9.5, false, "answer in the terminal".into(), theme::BONE_2));
    }

    let mut hsh = std::hash::DefaultHasher::new();
    for s in &segs {
        std::hash::Hash::hash(&bytemuck::bytes_of(s), &mut hsh);
    }
    panel::Frame {
        rects,
        segs,
        key: std::hash::Hasher::finish(&hsh),
        text: Text { items, k },
        alpha: a,
        clip: front,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(card: &Card, open: f32, verdict: Option<bool>) -> View<'_> {
        View { card, gap: [3000.0, 500.0], at: [3000.0, 500.0], open, verdict, now_ms: 61_000, clock: 0.0, x: [1200.0, 5000.0], band: [100.0, 1340.0], k: 1.45, u: 1.0, reduced: false }
    }

    fn card() -> Card {
        Card { who: "stratum·w2".into(), tool: "Bash".into(), summary: Some("cargo test --all".into()), since_ms: 1_000 }
    }

    #[test]
    fn the_card_names_the_ask_and_counts_the_wait() {
        let c = card();
        let f = frame(&view(&c, 1.0, None));
        let texts: Vec<&str> = f.text.items.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"APPROVAL REQUIRED"));
        assert!(texts.contains(&"1:00"));
        assert!(texts.contains(&"stratum·w2 · BASH") && texts.contains(&"cargo test --all"));
        assert!(texts.contains(&"answer in the terminal"));
        assert!(f.text.items.len() <= 5, "a few words, no more");
    }

    #[test]
    fn it_is_small() {
        let c = card();
        let [_, _, w, h] = rect(&view(&c, 1.0, None), 1);
        assert!(w <= WIDTH * SCALE * 1.45 + 0.5 && h < 140.0 * SCALE * 1.45, "{w} x {h}");
    }

    #[test]
    fn it_sits_below_the_gap_inside_the_area() {
        let c = card();
        let v = view(&c, 1.0, None);
        let [x, y, w, h] = rect(&v, 1);
        assert!(y > v.gap[1] && x >= v.x[0] && x + w <= v.x[1] && y + h <= v.band[1]);
        assert!(x < v.gap[0] && x + w > v.gap[0], "under the gap");
    }

    #[test]
    fn it_unfolds_from_the_left_without_a_pop() {
        let c = card();
        let width = |open: f32| frame(&view(&c, open, None)).rects.first().map_or(0.0, |r| r.0[2]);
        assert_eq!(width(0.0), 0.0);
        let mut prev = 0.0;
        for i in 1..=60 {
            let w = width(i as f32 / 60.0);
            assert!(w >= prev && w - prev < 0.08 * width(1.0), "a jump at {i}");
            prev = w;
        }
    }

    #[test]
    fn a_verdict_turns_it_and_drops_the_terminal_hint() {
        let c = card();
        let ok = frame(&view(&c, 1.0, Some(true)));
        let no = frame(&view(&c, 1.0, Some(false)));
        assert!(ok.text.items.iter().any(|i| i.text == "APPROVED"));
        assert!(no.text.items.iter().any(|i| i.text == "DENIED"));
        assert!(ok.rects.iter().any(|r| r.1 == theme::VERD) && !no.rects.iter().any(|r| r.1 == theme::VERD));
        assert!(!ok.text.items.iter().any(|i| i.text.starts_with("answer in")));
    }

    #[test]
    fn long_requests_wrap_and_cut() {
        let lines = wrap(&"x".repeat(500), 50, CODE_LINES);
        assert_eq!(lines.len(), CODE_LINES);
        assert!(lines[CODE_LINES - 1].ends_with('…'));
    }
}
