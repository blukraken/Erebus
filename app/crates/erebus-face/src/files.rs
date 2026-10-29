//! Changed files as rhombus glyphs that turn with the knot.
//!
//! Each file sits at a 3D point outside the tick ring, hashed from its path, so it keeps its
//! place across updates. The knot shader turns these points with the shell and draws them
//! from one storage buffer, written only when the file list changes.
//!
//! A file that leaves the list either fades in place (a discard) or, when the head moved on
//! the same branch, dashes straight to the core (a commit). Dash starts are hashed from the
//! path and the new head, so the same commit always plays the same way. Every time here is
//! on the event clock, in seconds; the buffer stores them relative to [`Files::epoch`].

use bytemuck::{Pod, Zeroable};
use erebus_protocol::{ChangedFile, FileStatus};
use erebus_theme as theme;
use std::f64::consts::TAU;

/// Glyph slots: a full list plus a full list leaving.
pub const CAP: usize = 2 * erebus_protocol::MAX_CHANGED_FILES;
pub const GROW_SECS: f64 = 0.5;
pub const FADE_SECS: f64 = 0.6;
pub const DASH_SECS: f64 = 0.9;
pub const TRAIL_SECS: f64 = 1.6;
/// The merge flash at the core outlives each arrival by this much.
pub const FLASH_SECS: f64 = 0.5;
/// The last dash starts within this long of the commit.
const SPREAD_SECS: f64 = 2.0;
/// Closest two glyph centres may sit, in knot radii.
const MIN_GAP: f64 = 0.34;

const KIND_LIVE: f32 = 0.0;
const KIND_DISCARD: f32 = 1.0;
const KIND_COMMIT: f32 = 2.0;

/// One glyph as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct FileInst {
    /// Rest point in knot radii, and a phase seed.
    pub p: [f32; 4],
    /// Status colour, and 1 for a conflict pulse.
    pub c: [f32; 4],
    /// Born, leave start, kind, shell rotation at the leave start.
    pub t: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leave {
    Stay,
    Discard(f64),
    /// Dash start and the shell rotation then.
    Commit(f64, f32),
}

#[derive(Clone, Debug)]
struct Glyph {
    path: String,
    status: FileStatus,
    pos: [f64; 3],
    seed: f64,
    born: f64,
    leave: Leave,
}

impl Glyph {
    /// When nothing more of this glyph is drawn.
    fn gone(&self) -> Option<f64> {
        match self.leave {
            Leave::Stay => None,
            Leave::Discard(t) => Some(t + FADE_SECS),
            Leave::Commit(t, _) => Some(t + DASH_SECS + TRAIL_SECS.max(FLASH_SECS)),
        }
    }
}

/// What an update did, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Change {
    pub added: usize,
    pub dashed: usize,
    pub faded: usize,
    /// First dash arrival after the update, seconds from now.
    pub first_arrival: Option<f64>,
}

/// The shell's turn, so a dash can start from where its glyph is drawn then.
#[derive(Clone, Copy, Debug)]
pub struct Spin {
    /// Rotation now, as the uniform carries it.
    pub rot: f64,
    /// Radians per second of event time.
    pub rate: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Files {
    glyphs: Vec<Glyph>,
    /// Bumped whenever the buffer contents change.
    pub generation: u64,
    pub epoch: f64,
    inst: Vec<FileInst>,
    /// Events run until then.
    pub busy_until: f64,
    /// The merge flash runs until then.
    pub flash_until: f64,
    /// Listed files, and the core's full count.
    pub listed: usize,
    pub changed: u32,
}

/// FNV-1a over `parts`, finished with splitmix64.
fn hash(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        h = (h ^ 0xff).wrapping_mul(0x100_0000_01b3);
    }
    let mut z = h.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Four uniform values in 0..1 from one hash.
fn unit4(h: u64) -> [f64; 4] {
    let h2 = hash(&[&h.to_le_bytes()]);
    [
        (h >> 32) as f64 / 4_294_967_296.0,
        (h & 0xffff_ffff) as f64 / 4_294_967_296.0,
        (h2 >> 32) as f64 / 4_294_967_296.0,
        (h2 & 0xffff_ffff) as f64 / 4_294_967_296.0,
    ]
}

/// Candidate rest point `k` for a path: a direction kept off the poles, 1.45 to 2.0 radii out.
fn candidate(path: &str, k: u32) -> ([f64; 3], f64) {
    let [a, b, c, d] = unit4(hash(&[path.as_bytes(), &k.to_le_bytes()]));
    let y = (2.0 * a - 1.0) * 0.62;
    let th = b * TAU;
    let q = (1.0 - y * y).sqrt();
    let rad = 1.45 + 0.55 * c.powf(0.8);
    ([th.cos() * q * rad, y * rad, th.sin() * q * rad], d * TAU)
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

pub fn status_rgb(s: FileStatus) -> [f32; 3] {
    match s {
        FileStatus::Modified => theme::EMBER,
        FileStatus::Added => theme::VERD,
        FileStatus::Deleted | FileStatus::Conflicted => theme::VERM,
        FileStatus::Renamed => theme::LILAC,
        FileStatus::Untracked => theme::BONE_2,
    }
    .srgb_f32()
}

/// Dash start offsets for `n` files in hash order: some leave together in twos and threes,
/// some a few milliseconds apart, some after a long pause. Gaps shrink to fit the spread.
fn dash_offsets(keys: &[u64]) -> Vec<f64> {
    let mut gaps = Vec::with_capacity(keys.len());
    let mut cluster = 1;
    for (i, &k) in keys.iter().enumerate() {
        if i == 0 {
            gaps.push(0.0);
            continue;
        }
        let [u, v, ..] = unit4(k);
        let g = if u < 0.32 && cluster < 3 {
            cluster += 1;
            v * 0.004
        } else if u < 0.7 {
            cluster = 1;
            0.03 + v * 0.12
        } else {
            cluster = 1;
            0.45 + v * 0.3
        };
        gaps.push(g);
    }
    let total: f64 = gaps.iter().sum();
    let squeeze = if total > SPREAD_SECS { SPREAD_SECS / total } else { 1.0 };
    let mut t = 0.0;
    gaps.iter()
        .map(|g| {
            t += g * squeeze;
            t
        })
        .collect()
}

/// Projects a rest point turned by `rot`, as `knot.wgsl` does: offset in radii and depth 0..1.
pub fn project(p: [f32; 3], rot: f32) -> ([f32; 2], f32) {
    let (sr, cr) = rot.sin_cos();
    let (st, ct) = 0.38f32.sin_cos();
    let x = p[0] * cr - p[2] * sr;
    let z = p[0] * sr + p[2] * cr;
    let y = p[1] * ct - z * st;
    let zz = p[1] * st + z * ct;
    let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-3);
    ([x, y], (zz / len + 1.0) * 0.5)
}

impl Files {
    pub fn instances(&self) -> &[FileInst] {
        &self.inst
    }

    /// Takes a new file list. `commit` names the new head when the head moved on the same
    /// branch; files that left then dash if `dash`, and fade otherwise.
    pub fn update(&mut self, list: &[ChangedFile], changed: u32, commit: Option<&str>, dash: bool, now: f64, spin: Spin) -> Change {
        self.glyphs.retain(|g| g.gone().is_none_or(|t| t > now));
        let mut change = Change::default();

        // Files that left.
        let mut leaving: Vec<(u64, usize)> = Vec::new();
        for (i, g) in self.glyphs.iter_mut().enumerate() {
            if g.leave != Leave::Stay || list.iter().any(|f| f.path == g.path) {
                continue;
            }
            match commit.filter(|_| dash) {
                Some(head) => leaving.push((hash(&[g.path.as_bytes(), head.as_bytes()]), i)),
                None => {
                    g.leave = Leave::Discard(now);
                    change.faded += 1;
                }
            }
        }
        leaving.sort_unstable();
        let keys: Vec<u64> = leaving.iter().map(|l| l.0).collect();
        for (off, (_, i)) in dash_offsets(&keys).into_iter().zip(&leaving) {
            let start = now + off;
            let rot = (spin.rot + spin.rate * off) as f32;
            self.glyphs[*i].leave = Leave::Commit(start, rot);
            change.dashed += 1;
            let arrive = off + DASH_SECS;
            change.first_arrival = Some(change.first_arrival.map_or(arrive, |a: f64| a.min(arrive)));
            self.flash_until = self.flash_until.max(start + DASH_SECS + FLASH_SECS);
        }

        // Status changes and new files.
        for f in list {
            if let Some(g) = self.glyphs.iter_mut().find(|g| g.leave == Leave::Stay && g.path == f.path) {
                g.status = f.status;
                continue;
            }
            let (pos, seed) = self.place(&f.path);
            self.glyphs.push(Glyph { path: f.path.clone(), status: f.status, pos, seed, born: now, leave: Leave::Stay });
            change.added += 1;
        }
        // A full buffer drops the oldest leavers first.
        while self.glyphs.len() > CAP {
            match self.glyphs.iter().position(|g| g.leave != Leave::Stay) {
                Some(i) => self.glyphs.remove(i),
                None => self.glyphs.remove(0),
            };
        }

        self.listed = list.len();
        self.changed = changed;
        let ends = self.glyphs.iter().map(|g| g.gone().unwrap_or(g.born + GROW_SECS));
        self.busy_until = ends.fold(self.busy_until, f64::max);
        self.rebuild(now);
        change
    }

    /// The first candidate that keeps its distance from every glyph still shown; failing
    /// that, the roomiest of the first 32.
    fn place(&self, path: &str) -> ([f64; 3], f64) {
        let mut best = (candidate(path, 0), -1.0);
        for k in 0..32 {
            let c = candidate(path, k);
            let room = self.glyphs.iter().map(|g| dist(g.pos, c.0)).fold(f64::INFINITY, f64::min);
            if room >= MIN_GAP {
                return c;
            }
            if room > best.1 {
                best = (c, room);
            }
        }
        best.0
    }

    fn rebuild(&mut self, now: f64) {
        self.epoch = now;
        let rel = |t: f64| (t - now) as f32;
        self.inst = self
            .glyphs
            .iter()
            .map(|g| {
                let [r, gr, b] = status_rgb(g.status);
                let conflict = if g.status == FileStatus::Conflicted { 1.0 } else { 0.0 };
                let (kind, start, rot) = match g.leave {
                    Leave::Stay => (KIND_LIVE, 1e6, 0.0),
                    Leave::Discard(t) => (KIND_DISCARD, rel(t), 0.0),
                    Leave::Commit(t, rot) => (KIND_COMMIT, rel(t), rot),
                };
                FileInst {
                    p: [g.pos[0] as f32, g.pos[1] as f32, g.pos[2] as f32, g.seed as f32],
                    c: [r, gr, b, conflict],
                    t: [rel(g.born), start, kind, rot],
                }
            })
            .collect();
        self.generation += 1;
    }

    /// Index and path of the listed glyph nearest `p` within `reach` pixels. `centre` and
    /// `r` place the knot; `rot` is the shell rotation now.
    pub fn hit(&self, p: [f32; 2], centre: [f32; 2], r: f32, rot: f32, reach: f32) -> Option<(usize, &str)> {
        self.glyphs
            .iter()
            .enumerate()
            .filter(|(_, g)| g.leave == Leave::Stay)
            .map(|(i, g)| {
                let s = self.screen(i, centre, r, rot);
                (i, g, (s[0] - p[0]).hypot(s[1] - p[1]))
            })
            .filter(|(_, _, d)| *d <= reach)
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(i, g, _)| (i, g.path.as_str()))
    }

    /// Screen position of glyph `i` in pixels.
    pub fn screen(&self, i: usize, centre: [f32; 2], r: f32, rot: f32) -> [f32; 2] {
        let g = &self.glyphs[i];
        let ([x, y], _) = project([g.pos[0] as f32, g.pos[1] as f32, g.pos[2] as f32], rot);
        [centre[0] + x * r, centre[1] + y * r]
    }

    /// Index of the listed glyph for `path`.
    pub fn find(&self, path: &str) -> Option<usize> {
        self.glyphs.iter().position(|g| g.leave == Leave::Stay && g.path == path)
    }

    /// The first listed glyph, for probes.
    pub fn first(&self) -> Option<usize> {
        self.glyphs.iter().position(|g| g.leave == Leave::Stay)
    }

    /// `+N` when the core counts more files than it lists.
    pub fn more(&self) -> Option<String> {
        let extra = (self.changed as usize).saturating_sub(self.listed);
        (extra > 0).then(|| format!("+{extra}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str, s: FileStatus) -> ChangedFile {
        ChangedFile { path: p.into(), status: s }
    }

    const SPIN: Spin = Spin { rot: 0.0, rate: 0.16 };

    #[test]
    fn place_is_stable_and_spaced() {
        let list: Vec<ChangedFile> = (0..64).map(|i| f(&format!("src/f{i}.rs"), FileStatus::Modified)).collect();
        let mut a = Files::default();
        a.update(&list, 64, None, true, 0.0, SPIN);
        let mut b = Files::default();
        b.update(&list[..10], 10, None, true, 0.0, SPIN);
        b.update(&list, 64, None, true, 1.0, SPIN);
        for i in 0..10 {
            assert_eq!(a.glyphs[i].pos, b.glyphs[i].pos);
        }
        let tight = a.glyphs.iter().filter(|g| a.glyphs.iter().any(|h| !std::ptr::eq(*g, h) && dist(g.pos, h.pos) < MIN_GAP)).count();
        assert!(tight < 16, "{tight} glyphs crowded");
    }

    #[test]
    fn commit_dashes_and_discard_fades() {
        let list = [f("a", FileStatus::Modified), f("b", FileStatus::Added), f("c", FileStatus::Untracked)];
        let mut s = Files::default();
        s.update(&list, 3, None, true, 0.0, SPIN);
        let c = s.update(&list[2..], 1, Some("beef"), true, 1.0, SPIN);
        assert_eq!((c.dashed, c.faded), (2, 0));
        assert!(c.first_arrival.is_some());
        let c = s.update(&[], 0, None, true, 2.0, SPIN);
        assert_eq!((c.dashed, c.faded), (0, 1));
    }

    #[test]
    fn dash_spread_is_bounded_and_reproducible() {
        let keys: Vec<u64> = (0..64u64).map(|i| hash(&[&i.to_le_bytes(), b"beef"])).collect();
        let a = dash_offsets(&keys);
        assert!(*a.last().unwrap() <= SPREAD_SECS + 1e-9);
        assert_eq!(a, dash_offsets(&keys));
        let gaps: Vec<f64> = a.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.iter().any(|g| *g < 0.005));
    }
}
