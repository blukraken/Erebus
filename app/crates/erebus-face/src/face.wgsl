// Ground, then shapes. Colours are sRGB values written to a non-sRGB target, so blending
// happens in sRGB space as it does in the prototype's canvas.

struct Globals {
    size: vec2f,
    centre: vec2f,
    radius: f32,
    vignette: f32,
    _pad: vec2f,
    ink: vec4f,
    light: vec4f,
};

@group(0) @binding(0) var<uniform> g: Globals;

// Ground

@vertex
fn ground_vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let uv = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(uv * 2.0 - 1.0, 0.0, 1.0);
}

fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

// Two uniform values in 0..1, fixed per pixel so every frame and screenshot match.
fn noise2(p: vec2u) -> vec2f {
    let a = pcg(p.x + pcg(p.y));
    let b = pcg(a);
    return vec2f(f32(a), f32(b)) / 4294967295.0;
}

@fragment
fn ground_fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    // Ground-light falloff: smooth top, ~0.2 at half radius, 0 at the edge (canvas stops 0/.5/1).
    let t = distance(pos.xy, g.centre) / g.radius;
    let e1 = exp(-6.44);
    let w = max((exp(-6.44 * t * t) - e1) / (1.0 - e1), 0.0);
    var c = g.ink.rgb + g.light.rgb * w;

    let q = pos.xy / g.size * 2.0 - 1.0;
    c *= 1.0 - g.vignette * smoothstep(0.45, 1.0, length(q) * 0.7071);

    // Triangular dither, ±1 LSB of the 8-bit target.
    let n = noise2(vec2u(pos.xy));
    c += (n.x + n.y - 1.0) / 255.0;
    return vec4f(c, 1.0);
}

// Shapes: instanced quads. kind 0 = solid rect, kind 1 = link bead, kind 2 = close button.

struct ShapeIn {
    @location(0) rect: vec4f,
    @location(1) color: vec4f,
    @location(2) color2: vec4f,
    @location(3) params: vec4f,
};

struct ShapeOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) color: vec4f,
    @location(2) @interpolate(flat) color2: vec4f,
    @location(3) @interpolate(flat) params: vec4f,
};

@vertex
fn shape_vs(@builtin(vertex_index) i: u32, s: ShapeIn) -> ShapeOut {
    let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
    let px = s.rect.xy + corner * s.rect.zw;
    var out: ShapeOut;
    out.pos = vec4f(px.x / g.size.x * 2.0 - 1.0, 1.0 - px.y / g.size.y * 2.0, 0.0, 1.0);
    out.local = (corner - 0.5) * s.rect.zw;
    out.color = s.color;
    out.color2 = s.color2;
    out.params = s.params;
    return out;
}

// Pixel coverage of a disc of radius r at distance d.
fn disc(d: f32, r: f32) -> f32 {
    return clamp(r - d + 0.5, 0.0, 1.0);
}

// Coverage of a stroke of half-width hw at distance d from its centre line.
fn stroke(d: f32, hw: f32) -> f32 {
    return clamp(hw - d + 0.5, 0.0, 1.0);
}

// Premultiplied `top` over `under`.
fn over(top: vec4f, under: vec4f) -> vec4f {
    return top + under * (1.0 - top.a);
}

// One stroke of the ×: two strands twisting tightly about the diagonal `dir`, crossing
// at the centre, mid-arm and both tips. Returns each strand's coverage.
fn braid_arm(p: vec2f, dir: vec2f, a: f32, amp: f32, hw: f32) -> vec2f {
    let s = dot(p, dir);
    let o = dot(p, vec2f(-dir.y, dir.x));
    let sc = clamp(s, -a, a);
    let wave = amp * sin(6.2831853 * sc / a);
    let cap = max(abs(s) - a, 0.0);
    return vec2f(stroke(length(vec2f(cap, o - wave)), hw), stroke(length(vec2f(cap, o + wave)), hw));
}

// Close button. color = ticks and ring, color2 = the ×,
// params = (kind, ring radius, glow 0..1, line width).
fn close_fs(in: ShapeOut) -> vec4f {
    let p = in.local;
    let r = in.params.y;
    let glow = in.params.z;
    let hw = in.params.w * 0.5;
    let d = length(p);
    let inside = disc(d, r);

    // Ink fill, so nothing drawn beneath competes with the glyph.
    var c = vec4f(g.ink.rgb, 1.0) * inside * 0.85;

    // Hover glow: a soft halo outside the ring and a faint wash within.
    let o = max(d - r, 0.0);
    let sigma = r * 0.3;
    let halo = glow * (1.0 - inside) * 0.24 * exp(-o * o / (2.0 * sigma * sigma));
    let wash = glow * inside * 0.1 * (1.0 - d / r);
    c = over(vec4f(in.color.rgb, 1.0) * wash, c) + vec4f(in.color.rgb, 1.0) * halo;

    // 24 fine ticks pointing in from the ring. The four on the diagonals stand longer and
    // aim at the tips of the ×.
    let step = 6.2831853 / 24.0;
    let i = round(atan2(p.y, p.x) / step);
    let t = i * step;
    let along = dot(p, vec2f(cos(t), sin(t)));
    let across = abs(dot(p, vec2f(-sin(t), cos(t))));
    let major = ((i32(i) % 6) + 6) % 6 == 3;
    let len = select(r * 0.14, r * 0.24, major);
    let tick = stroke(across, hw) * clamp(min(along - (r - len), r - along) + 0.5, 0.0, 1.0);
    let ring = stroke(abs(d - r), hw * 0.8) * 0.5;
    let line = max(tick, ring) * in.color.a;
    c = over(vec4f(in.color.rgb, 1.0) * line, c);

    // The ×: one braided stroke per diagonal; the back strand sits a little dimmer.
    let a = r * 0.5;
    let amp = r * 0.045;
    let s1 = braid_arm(p, vec2f(0.70710678, 0.70710678), a, amp, hw);
    let s2 = braid_arm(p, vec2f(0.70710678, -0.70710678), a, amp, hw);
    let x = max(max(s1.x * 0.65, s1.y), max(s2.x * 0.65, s2.y)) * in.color2.a;
    return over(vec4f(in.color2.rgb, 1.0) * x, c);
}

@fragment
fn shape_fs(in: ShapeOut) -> @location(0) vec4f {
    if in.params.x < 0.5 {
        // params.y = 1 fades the rect out to the right, falling with the square; params.z its width.
        var a = in.color.a;
        if in.params.y > 0.5 {
            let t = clamp(in.local.x / in.params.z + 0.5, 0.0, 1.0);
            a *= (1.0 - t) * (1.0 - t);
        }
        return vec4f(in.color.rgb * a, a);
    }
    if in.params.x > 1.5 {
        return close_fs(in);
    }

    // Bead. color = dormant ring, color2 = live fill, params = (kind, radius, link 0..1, glow sigma).
    let d = length(in.local);
    let r = in.params.y;
    let t = in.params.z;
    let sigma = in.params.w;
    let ring_w = max(1.5, r * 0.22);
    let outside = 1.0 - disc(d, r);
    let o = max(d - r, 0.0);

    let glow = t * outside * (0.55 * exp(-o * o / (2.0 * sigma * sigma)) + 0.10 * exp(-o / (r * 1.2)));

    // Ring band and inner fill cover disjoint parts of each pixel, so they add.
    let inner = disc(d, r - ring_w);
    let ring = disc(d, r) - inner;
    let fill = inner * t;
    let body = vec4f(mix(in.color.rgb, in.color2.rgb, t) * ring + in.color2.rgb * fill, ring + fill);
    return body + vec4f(in.color2.rgb * glow, glow) * (1.0 - body.a);
}
