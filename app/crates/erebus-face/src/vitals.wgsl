// Vitals: one braid quad per row, then the instruments as instanced segments. Both add
// light, as the prototype's `lighter` canvas does. The palette constants come first; see
// `vitals::shader`.

struct Row {
    // Braid x, y, width (to the instrument), height; physical pixels.
    rect: vec4f,
    // Strand colour, hot-speck threshold.
    color: vec4f,
    // Load now 0..1, age of the newest sample in s, unused, unit q (height / 66).
    live: vec4f,
    // Mote phase, mote count, dot size, one CSS pixel.
    motes: vec4f,
};

struct Uniform {
    rows: array<Row, 3>,
    // Braid phase, wall seconds mod 60, reduced motion, unused.
    clock: vec4f,
    size: vec4f,
};

@group(0) @binding(0) var<uniform> u: Uniform;
// Per row, 64 one-second loads 0..1, the newest last.
@group(0) @binding(1) var<storage, read> hist: array<f32, 192>;

const PI2 = 6.2831853;
const SPAN = 60.0;

fn clip(p: vec2f) -> vec4f {
    return vec4f(p.x / u.size.x * 2.0 - 1.0, 1.0 - p.y / u.size.y * 2.0, 0.0, 1.0);
}

// Load at braid x, blurred over ±2 s with 1-4-6-4-1 weights so a load step swells the
// braid over a few seconds instead of standing up as a wall.
fn load_at(r: u32, x: f32) -> f32 {
    let age = SPAN * (1.0 - x / u.rows[r].rect.z);
    let a = max(age, 2.0);
    let edge = raw_at(r, age);
    let blur = (raw_at(r, a - 2.0) + 4.0 * raw_at(r, a - 1.0) + 6.0 * raw_at(r, a) + 4.0 * raw_at(r, a + 1.0) + raw_at(r, a + 2.0)) / 16.0;
    // The last 2 s near the instrument blend back to the exact reading, so the braid meets
    // the instrument at the value it shows.
    return mix(edge, blur, clamp(age / 2.0, 0.0, 1.0));
}

// Load at a given age: history scrolls right to left, and the last stretch eases from the
// newest sample into the load now at the instrument.
fn raw_at(r: u32, age: f32) -> f32 {
    let row = u.rows[r];
    let a0 = row.live.y;
    let base = r * 64u;
    if age <= a0 {
        return mix(row.live.x, hist[base + 63u], clamp(age / max(a0, 1e-3), 0.0, 1.0));
    }
    let f = clamp(63.0 - (age - a0), 0.0, 63.0);
    let i0 = u32(floor(f));
    let i1 = min(i0 + 1u, 63u);
    return mix(hist[base + i0], hist[base + i1], fract(f));
}

// Coverage of a stroke of half-width hw at distance d from its centre line.
fn stroke(d: f32, hw: f32) -> f32 {
    return clamp(hw - d + 0.5, 0.0, 1.0);
}

// Coverage of an axis-aligned square of half-size s centred at c.
fn square(p: vec2f, c: vec2f, s: f32) -> f32 {
    let d = abs(p - c);
    return clamp(s - d.x + 0.5, 0.0, 1.0) * clamp(s - d.y + 0.5, 0.0, 1.0);
}

struct BraidOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) row: u32,
};

@vertex
fn braid_vs(@builtin(vertex_index) i: u32, @builtin(instance_index) r: u32) -> BraidOut {
    let rect = u.rows[r].rect;
    let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
    var out: BraidOut;
    out.pos = clip(rect.xy + corner * rect.zw);
    out.local = corner * rect.zw;
    out.row = r;
    return out;
}

// Strands: amplitude ×, phase, wavelength ×, loose fibre.
const STRANDS = array<vec4f, 5>(
    vec4f(1.0, 0.0, 1.0, 0.0),
    vec4f(1.0, 2.0943951, 1.0, 0.0),
    vec4f(1.0, 4.1887902, 1.0, 0.0),
    vec4f(1.55, 0.7, 1.6, 1.0),
    vec4f(1.8, 2.4, 2.1, 1.0),
);

@fragment
fn braid_fs(in: BraidOut) -> @location(0) vec4f {
    let r = in.row;
    let row = u.rows[r];
    let p = in.local;
    let w = row.rect.z;
    let h = row.rect.w;
    let q = row.live.w;
    let px = row.motes.w;
    let my = h * 0.46;
    let lam = h * 0.95;
    let ph = u.clock.x;
    let now = row.live.x;
    let col = row.color.rgb;

    // Amplitude follows load: a few pixels idle, most of the height at full load.
    let v = load_at(r, p.x);
    let dv = load_at(r, p.x + 1.0) - v;
    let amp = h * (0.025 + 0.36 * v);
    let damp = h * 0.36 * dv;

    // Fades in from the left: 0 at the far end, 0.4 at 22%, 0.95 at the instrument.
    let fx = p.x / w;
    let ramp = select(mix(0.4, 0.95, (fx - 0.22) / 0.78), 0.4 * fx / 0.22, fx < 0.22);

    var c = vec3f(0.0);
    var strands = STRANDS;
    for (var s = 0u; s < 5u; s++) {
        let sb = strands[s];
        let loose = sb.w > 0.5;
        let k = PI2 / (lam * sb.z);
        let arg = k * p.x - ph * select(1.0, 0.6, loose) + sb.y;
        let a = min(h * 0.44, amp * sb.x);
        let y = my + a * sin(arg);
        let slope = a * k * cos(arg) + damp * sb.x * sin(arg);
        let d = abs(p.y - y) / sqrt(1.0 + slope * slope);
        let hw = max(px, select(1.1, 0.7, loose) * q) * 0.5;
        // Low load lights one or two tight strands, high load all three.
        let lit = select(clamp(now * 3.0 - f32(s) + 0.9, 0.3, 1.0), 0.32, loose);
        c += col * stroke(d, hw) * ramp * lit;
    }

    let dd = row.motes.z;

    // Hot specks where load ran high, on a fixed grid over the newest 80%.
    let step = 7.0 * q;
    let x0 = w * 0.2;
    let xs = x0 + round((p.x - x0) / step) * step;
    if xs >= x0 && xs <= w {
        let vs = load_at(r, xs);
        if vs > row.color.w {
            let ys = my + h * (0.025 + 0.36 * vs) * sin(PI2 * xs / lam - ph);
            c += HOT * 0.85 * square(p, vec2f(xs, ys), dd * 0.6);
        }
    }

    // Motes ride the braid into the instrument; count and speed follow load now.
    if u.clock.z < 0.5 {
        let n = row.motes.y;
        for (var i = 0u; i < 9u; i++) {
            if f32(i) >= n {
                break;
            }
            let t = 1.0 - fract(row.motes.x + f32(i) / n);
            let x = w * (0.2 + 0.8 * t);
            let y = my + h * (0.025 + 0.36 * load_at(r, x)) * sin(PI2 * x / lam - ph + f32(i % 3u) * PI2 / 3.0);
            c += HOT * 0.85 * t * square(p, vec2f(x, y), dd * 0.7);
        }
    }

    // Brass ruler along the bottom: a tick every 5 s, longer every 30 s, scrolling with time.
    var ruler = stroke(abs(p.y - (h - 0.5 * px)), 0.5 * px);
    let wall = u.clock.y;
    let age = SPAN * (1.0 - p.x / w);
    let n5 = round((wall - age) / 5.0);
    let tick_age = wall - n5 * 5.0;
    let tx = w * (1.0 - tick_age / SPAN);
    if tick_age >= 0.0 && tick_age <= SPAN {
        let big = (i32(n5) % 6 + 6) % 6 == 0;
        let len = select(3.0, 6.0, big) * q;
        let on_x = stroke(abs(p.x - tx), 0.5 * px);
        let on_y = clamp(p.y - (h - len) + 0.5, 0.0, 1.0);
        ruler = max(ruler, on_x * on_y);
    }
    c += BRASS * 0.5 * ruler;

    return vec4f(c, 0.0);
}

// Instruments: straight butt-ended strokes. A square dot is a stroke as long as it is wide.

struct SegIn {
    @location(0) ab: vec4f,
    // Premultiplied.
    @location(1) color: vec4f,
    @location(2) width: vec4f,
};

struct SegOut {
    @builtin(position) pos: vec4f,
    // Along and across the stroke, pixels.
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) color: vec4f,
    // Length, half-width, kind: 0 stroke, 1 round dot, 2 soft glow.
    @location(2) @interpolate(flat) dims: vec3f,
};

@vertex
fn seg_vs(@builtin(vertex_index) i: u32, s: SegIn) -> SegOut {
    let a = s.ab.xy;
    let d = s.ab.zw - a;
    let len = length(d);
    let dir = select(vec2f(1.0, 0.0), d / len, len > 1e-4);
    let n = vec2f(-dir.y, dir.x);
    let hw = s.width.x * 0.5;
    let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
    // Dots and glows cap at their radius; soft strokes overlap each neighbour by half a length.
    var cap = select(1.0, hw + 1.0, s.width.y > 0.5);
    if s.width.y > 2.5 {
        cap = len * 0.5;
    }
    let along = mix(-cap, len + cap, corner.x);
    let across = mix(-hw - 1.0, hw + 1.0, corner.y);
    var out: SegOut;
    out.pos = clip(a + dir * along + n * across);
    out.local = vec2f(along, across);
    out.color = s.color;
    out.dims = vec3f(len, hw, s.width.y);
    return out;
}

@fragment
fn seg_fs(in: SegOut) -> @location(0) vec4f {
    // Soft strokes fall off smoothly across, and ramp in and out along over a whole length,
    // so neighbours along a path sum to an even glow with no corners.
    if in.dims.z > 2.5 {
        let q = clamp(1.0 - abs(in.local.y) / max(in.dims.y, 1.0), 0.0, 1.0);
        let len = max(in.dims.x, 1e-3);
        let along = clamp(min(in.local.x + len * 0.5, len * 1.5 - in.local.x) / len, 0.0, 1.0);
        return vec4f(in.color.rgb * q * q * along, 0.0);
    }
    // Dots and glows are measured from the centre, so they come out round.
    if in.dims.z > 0.5 {
        let r = length(vec2f(in.local.x - in.dims.x * 0.5, in.local.y));
        var k = clamp(in.dims.y - r + 0.5, 0.0, 1.0);
        if in.dims.z > 1.5 {
            // Gaussian, shifted so it reaches zero at the quad's edge.
            let g = exp(-r * r / (2.0 * pow(in.dims.y * 0.4, 2.0)));
            let edge = exp(-1.0 / (2.0 * 0.16));
            k = max(g - edge, 0.0) / (1.0 - edge);
        }
        return vec4f(in.color.rgb * k, 0.0);
    }
    let across = stroke(abs(in.local.y), in.dims.y);
    let along = clamp(min(in.local.x, in.dims.x - in.local.x) + 0.5, 0.0, 1.0);
    return vec4f(in.color.rgb * across * along, 0.0);
}
