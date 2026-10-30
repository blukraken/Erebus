// Agent globes: session and worker spheres of dots around a core, condensing from dust and
// dissolving back into it. Light adds, as the prototype's `lighter` pass does, so draw order
// does not matter. Colours are sRGB values on a unorm target.
//
// `globes::shader` prepends the palette (EMBER, HOT) and the point counts (S_SHELL, S_CORE,
// W_SHELL, W_CORE). One instance is one dot: session globes come first, then workers, so
// the instance index splits into globe and point by arithmetic.

struct Frame {
    size: vec4f,    // width, height, dot scale, jitter phase
    state: vec4f,   // lod, session globes, worker globes, unused
};

// One globe; see `GlobeInst` in `globes.rs`.
struct Globe {
    pos: vec4f,     // centre x, y, radius, rotation
    color: vec4f,   // tint, opacity
    form: vec4f,    // kind, form, core jitter, seed
    look: vec4f,    // grey, unused
};

@group(0) @binding(0) var<uniform> fr: Frame;
@group(0) @binding(1) var<storage, read> pts: array<vec4f>;
@group(0) @binding(2) var<storage, read> globes: array<Globe>;

const TAU = 6.2831853;

// Dots per globe of each kind. Points sit in that order: session shell and core, then
// worker shell and core.
const S_N = S_SHELL + S_CORE;
const W_N = W_SHELL + W_CORE;

// Each dot starts gathering at its own point in the form, at most this far in.
const STAGGER = 0.4;

fn to_ndc(px: vec2f) -> vec4f {
    return vec4f(px.x / fr.size.x * 2.0 - 1.0, 1.0 - px.y / fr.size.y * 2.0, 0.0, 1.0);
}

// The shell turn of `knot.wgsl`: about the vertical, tilted 0.38 rad. Returns the screen
// offset in radii and depth 0..1.
fn shell_turn(p: vec3f, rot: f32) -> vec3f {
    let cr = cos(rot);
    let sr = sin(rot);
    let x = p.x * cr - p.z * sr;
    let z = p.x * sr + p.z * cr;
    let ct = cos(0.38);
    let st = sin(0.38);
    let y = p.y * ct - z * st;
    let zz = p.y * st + z * ct;
    return vec3f(x, y, (zz + 1.0) * 0.5);
}

// PCG hash, and a hash as 0..1.
fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn unit(h: u32) -> f32 {
    return f32(h >> 8u) / 16777216.0;
}

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) half: f32,
    @location(2) @interpolate(flat) color: vec3f,
};

fn cull() -> VOut {
    var o: VOut;
    o.pos = vec4f(2.0, 2.0, 0.0, 1.0);
    o.local = vec2f(0.0);
    o.half = 0.0;
    o.color = vec3f(0.0);
    return o;
}

@vertex
fn globe_vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
    let sessions = u32(fr.state.y + 0.5);
    let worker = ii >= sessions * S_N;
    var g = ii / S_N;
    var p = ii % S_N;
    var at = p;
    if worker {
        let j = ii - sessions * S_N;
        g = sessions + j / W_N;
        p = j % W_N;
        at = S_N + p;
    }
    let gl = globes[g];
    let pt = pts[at];
    let r = gl.pos.z;
    let rot = gl.pos.w;
    let ds = fr.size.z;

    var off: vec2f;
    var size: f32;
    var a: f32;
    var rgb: vec3f;
    var shell = false;
    if !worker && p < S_SHELL {
        // Session shell: a dotted globe, nearer dots larger and brighter.
        let s = shell_turn(pt.xyz, rot * 0.9);
        off = s.xy * r;
        size = (0.55 + 1.15 * s.z) * ds;
        a = 0.95 * (0.1 + 0.8 * s.z);
        rgb = gl.color.rgb;
        shell = true;
    } else if !worker {
        // Session core: a hot ball that turns flat and jitters with token flow. Every third
        // dot is white.
        let r2 = rot * 1.7;
        let x = pt.x * cos(r2) - pt.z * sin(r2);
        let j = sin(fr.size.w + pt.w * 40.0) * gl.form.z;
        off = vec2f(x, pt.y) * r * 0.42 * (1.0 + j);
        size = 1.6 * ds;
        a = 0.35 + 0.55 * pt.w;
        rgb = select(HOT, vec3f(1.0), (p - S_SHELL) % 3u == 0u);
    } else if p < W_SHELL {
        // Worker shell: a coarser globe turning faster.
        let s = shell_turn(pt.xyz, rot * 1.3);
        off = s.xy * r;
        size = (0.55 + 1.15 * s.z) * 0.95 * ds;
        a = 0.9 * (0.1 + 0.8 * s.z);
        rgb = gl.color.rgb;
        shell = true;
    } else {
        // Worker core: an ember trefoil knotlet, a strand of the parent braid.
        let r2 = rot * 1.6;
        let x = pt.x * cos(r2) + pt.z * sin(r2);
        off = vec2f(x, pt.y) * r * 0.16;
        size = 1.8 * ds;
        a = 0.95;
        rgb = EMBER;
    }

    // Glance fades every other shell dot.
    let lod = fr.state.x;
    if shell && (p & 1u) == 1u && lod < 1.0 {
        if lod <= 0.0 {
            return cull();
        }
        a *= lod;
    }

    // Form: each dot flies in from its own scattered spot, starting at its own threshold,
    // and fades up as it comes.
    let form = clamp(gl.form.y, 0.0, 1.0);
    if form < 1.0 {
        let h = pcg(p ^ pcg(bitcast<u32>(gl.form.w)));
        let h1 = pcg(h);
        let h2 = pcg(h1);
        let fp = clamp((form - STAGGER * unit(h)) / (1.0 - STAGGER), 0.0, 1.0);
        let ang = unit(h1) * TAU;
        let dust = vec2f(cos(ang), sin(ang)) * mix(0.6, 2.2, unit(h2)) * r;
        off = mix(dust, off, smoothstep(0.0, 1.0, fp));
        a *= 1.0 - (1.0 - fp) * (1.0 - fp);
    }

    // A waiting session greys every dot, its hot core included.
    rgb = mix(rgb, vec3f(dot(rgb, vec3f(0.299, 0.587, 0.114))), gl.look.x);

    a *= gl.color.a;
    if a <= 0.002 {
        return cull();
    }

    let half = size * 0.5;
    let corner = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let local = corner * (half + 1.0);
    var o: VOut;
    o.pos = to_ndc(gl.pos.xy + off + local);
    o.local = local;
    o.half = half;
    o.color = rgb * min(a, 1.0);
    return o;
}

// How much of the pixel at `x` the span -h..h covers.
fn cover(x: f32, h: f32) -> f32 {
    return clamp(min(x + 0.5, h) - max(x - 0.5, -h), 0.0, 1.0);
}

// A square dot, box filtered so sub-pixel dots keep their weight.
@fragment
fn globe_fs(in: VOut) -> @location(0) vec4f {
    let c = cover(in.local.x, in.half) * cover(in.local.y, in.half);
    return vec4f(in.color * c, 0.0);
}
