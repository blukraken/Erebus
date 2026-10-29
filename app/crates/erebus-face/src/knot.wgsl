// The knot. Knot light adds, as the prototype's `lighter` canvas pass does, so draw order
// inside the knot does not matter. File glyphs blend over it as solid beads; their dash
// trails add. Colours are sRGB values on a unorm target.
//
// The palette (BONE, PEARL, PEARL_HI, PEARL_DIM, EMBER, HOT, HOT_CORE) is prepended from
// `erebus_theme` by `knot::shader`. Pearl is the knot; ember and hot mark the centre and
// events.

struct Knot {
    frame: vec4f,   // width, height, centre x, centre y
    scale: vec4f,   // radius, dot scale, line width, tick unit
    clock: vec4f,   // rotation, jitter, twinkle, orbit
    state: vec4f,   // live, lod, reduced, haze count
    flare: vec4f,   // ring, pulse, shimmer progress (1 = idle), file hover fade
    files: vec4f,   // event clock, hovered glyph (-1 none), glyph count, merge flash on
    aux: vec4f,     // head phase, head spin
};

// One changed-file glyph; see `files.rs`.
struct File {
    p: vec4f,   // rest point in knot radii, phase seed
    c: vec4f,   // status colour, conflict pulse
    t: vec4f,   // born, leave start, kind (0 listed, 1 discard, 2 commit), rotation at the leave
};

@group(0) @binding(0) var<uniform> k: Knot;
@group(0) @binding(1) var<storage, read> files: array<File>;

const PI = 3.14159265;
const TAU = 6.2831853;

// File timings, seconds; they match `files.rs`.
const GROW = 0.5;
const FADE = 0.6;
const DASH = 0.5;
const TRAIL = 1.2;
const FLASH = 0.5;
// A dash covers distance as progress to this power: it accelerates.
const DASH_EASE = 2.2;

fn e_out(t: f32) -> f32 {
    let u = 1.0 - clamp(t, 0.0, 1.0);
    return 1.0 - u * u * u;
}

fn to_ndc(px: vec2f) -> vec4f {
    return vec4f(px.x / k.frame.x * 2.0 - 1.0, 1.0 - px.y / k.frame.y * 2.0, 0.0, 1.0);
}

// Depth cue: the far side is at most a quarter dimmer and a little smaller, with no step.
fn depth_dim(depth: f32) -> f32 {
    return mix(0.75, 1.0, clamp(depth, 0.0, 1.0));
}

fn depth_size(depth: f32) -> f32 {
    return mix(0.85, 1.0, clamp(depth, 0.0, 1.0));
}

// The shell turn: about the vertical, tilted 0.38 rad. Returns screen offset in radii and
// depth 0..1 for a point at radius `len`.
fn shell_turn(p: vec3f, rot: f32, len: f32) -> vec3f {
    let cr = cos(rot);
    let sr = sin(rot);
    let x = p.x * cr - p.z * sr;
    let z = p.x * sr + p.z * cr;
    let ct = cos(0.38);
    let st = sin(0.38);
    let y = p.y * ct - z * st;
    let zz = p.y * st + z * ct;
    return vec3f(x, y, (zz / max(len, 1e-3) + 1.0) * 0.5);
}

// Trefoil curve and its tangent.
fn trefoil(t: f32) -> vec3f {
    return vec3f(sin(t) + 2.0 * sin(2.0 * t), cos(t) - 2.0 * cos(2.0 * t), -sin(3.0 * t));
}

fn trefoil_d(t: f32) -> vec3f {
    return vec3f(cos(t) + 4.0 * cos(2.0 * t), -sin(t) + 4.0 * sin(2.0 * t), -3.0 * cos(3.0 * t));
}

// A point `off` out from the curve at phase t, at angle `ang` about it, in knot units.
fn tube(t: f32, off: f32, ang: f32) -> vec3f {
    let tg = normalize(trefoil_d(t));
    let u = normalize(cross(tg, vec3f(0.0, 0.0, 1.0)));
    let v = cross(tg, u);
    return (trefoil(t) + off * (cos(ang) * u + sin(ang) * v)) / 3.25;
}

// Trail light. x is the fraction of a loop behind a head: 0 at the head, 1 one loop on.
//   rise(x)  = smoothstep(0, 0.015, x)          soft front just behind the head
//   body(x)  = 1 - 0.3 smoothstep(0, 0.25, x)   bright and dense for a quarter loop
//   fade(x)  = 1 - smoothstep(0.25, 1, x)       then a long slow fade
//   trail(x) = rise(x) body(x) fade(x)
// The brighter of the two heads' trails sits on a faint floor. Each head cuts a dark gap in
// front of itself, black for 1% of a loop and opening by 6%:
//   gap(x) = smoothstep(0.01, 0.06, 1 - x)
//   b = (0.12 + 0.8 max(trail(x0), trail(x1))) gap(x0) gap(x1)
// So b runs 0.92 at the head, 0.68 a quarter loop on, 0.54 at half, 0.26 at three
// quarters and 0.12 a loop on, then dark just ahead of the head. A max, not a sum, keeps
// the two trails apart where they overlap.
const TRAIL_FLOOR = 0.12;
const TRAIL_SPAN = 0.8;
const GAP_DARK = 0.01;
const GAP_OPEN = 0.06;

fn trail_light(x: f32) -> f32 {
    let rise = smoothstep(0.0, 0.015, x);
    let body = 1.0 - 0.3 * smoothstep(0.0, 0.25, x);
    let fade = 1.0 - smoothstep(0.25, 1.0, x);
    return rise * body * fade;
}

// Particles: one quad per dot.

struct PIn {
    @location(0) p: vec4f,
    @location(1) q: vec4f,   // group, index, flag or variance, kept in Glance
};

struct POut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) half_glow: vec2f,
    @location(2) @interpolate(flat) color: vec4f,
};

fn cull() -> POut {
    var o: POut;
    o.pos = vec4f(2.0, 2.0, 0.0, 1.0);
    o.local = vec2f(0.0);
    o.half_glow = vec2f(0.0);
    o.color = vec4f(0.0);
    return o;
}

@vertex
fn particle_vs(@builtin(vertex_index) vi: u32, d: PIn) -> POut {
    let r = k.scale.x;
    let ds = k.scale.y;
    let live = k.state.x;
    let lod = k.state.y;
    let reduced = k.state.z > 0.5;
    let l = mix(0.45, 1.0, live);
    let rot = k.clock.x;
    let group = u32(d.q.x + 0.5);
    let idx = d.q.y;
    let pulse = 1.0 - k.flare.y;
    let swell = select(sin(PI * clamp(k.flare.y, 0.0, 1.0)) * step(0.001, pulse), 0.0, reduced);

    var off = vec2f(0.0);
    var size = 1.0;
    var rgb = vec3f(0.0);
    var a = 0.0;
    var glow = 0.0;

    switch group {
        // Shell: a faint globe turning slowly, with a slow shimmer.
        case 0u: {
            let s = shell_turn(d.p.xyz, rot, 1.0);
            off = s.xy * r * (1.0 + 0.05 * swell);
            let shimmer = select(0.78 + 0.22 * sin(k.clock.z * 0.5 + d.q.z * TAU), 0.9, reduced);
            size = 1.35 * ds * depth_size(s.z);
            a = (0.5 * l + 0.14) * 0.36 * depth_dim(s.z) * shimmer;
            rgb = mix(PEARL_DIM, PEARL, live);
        }
        // Trefoil: two heads run the curve in opposite directions. Trail dots light by
        // how far each head has run past them; head dots ride with their head.
        case 1u: {
            let h0 = k.aux.x;
            let h1 = PI - k.aux.x;
            let head = d.p.w;
            var t = d.p.x;
            var tube_off = d.p.y;
            var tube_ang = d.p.z;
            var b = 0.0;
            var hot = 0.0;
            var grow = 1.0;
            if head < -0.5 {
                if reduced {
                    b = 0.42;
                } else {
                    // Light from both heads; see `trail_light` for the curve.
                    let behind0 = fract((h0 - t) / TAU);
                    let behind1 = fract((t - h1) / TAU);
                    let u = max(trail_light(behind0), trail_light(behind1));
                    let gap0 = smoothstep(GAP_DARK, GAP_OPEN, 1.0 - behind0);
                    let gap1 = smoothstep(GAP_DARK, GAP_OPEN, 1.0 - behind1);
                    b = (TRAIL_FLOOR + TRAIL_SPAN * u) * gap0 * gap1;
                    let fresh = min(behind0, behind1);
                    grow = 1.0 + 0.3 * (1.0 - smoothstep(0.0, 0.25, fresh));
                    hot = 0.6 * pow(1.0 - fresh, 12.0);
                }
            } else {
                if reduced {
                    return cull();
                }
                // Head cluster: dots ahead and behind their head, jittering about the strand.
                let dir = select(-1.0, 1.0, head < 0.5);
                let h = select(h1, h0, head < 0.5);
                let lead = d.p.x;
                t = h + dir * lead;
                let jit = sin(k.clock.y * 3.0 + d.q.z * 40.0);
                tube_off = tube_off * (1.0 + 0.4 * jit);
                // Spin rates are sixteenths, so the 16-turn spin clock wraps without a seam.
                let spin = 1.0 + round(32.0 * d.q.z) / 16.0;
                tube_ang = tube_ang + k.aux.y * spin;
                let core = exp(-lead * lead / 0.0025);
                b = 0.7 + 1.1 * core;
                hot = core;
            }
            let p = tube(t, tube_off, tube_ang);

            // The knot's own turn: about a tipping axis, faster than the shell.
            let r2 = rot * 1.7;
            let cr = cos(r2);
            let sr = sin(r2);
            let tl = 0.5 + sin(rot * 0.9) * 0.15;
            let ct = cos(tl);
            let st = sin(tl);
            let x = p.x * cr + p.z * sr;
            let z1 = -p.x * sr + p.z * cr;
            let y = p.y * ct - z1 * st;
            let z = p.y * st + z1 * ct;
            let depth = clamp(0.5 + z * 0.55, 0.0, 1.0);

            off = vec2f(x, y) * r * 0.82 * (1.0 + 0.08 * swell);
            let bb = min(b, 1.6);
            size = (1.2 + 1.2 * min(bb, 1.0)) * grow * (0.8 + 0.4 * d.q.z) * ds * depth_size(depth);
            a = min(bb, 1.0) * (0.9 * l + 0.1) * depth_dim(depth);
            let base = mix(PEARL_DIM, PEARL, live);
            let heat = mix(PEARL, PEARL_HI, live);
            rgb = mix(base, heat, hot);
            glow = select(0.0, 1.0, bb > 0.95);
        }
        // Hot core: a turning ball of hot and white dots that breathes; live only.
        case 2u: {
            let r3 = rot * 3.0;
            let s = shell_turn(d.p.xyz, r3, max(length(d.p.xyz), 0.05));
            let breathe = select(0.04 * sin(k.clock.y), 0.0, reduced);
            let tw = select(0.8 + 0.2 * sin(k.clock.y * 2.0 + d.p.w * 40.0), 0.9, reduced);
            off = vec2f(s.x, d.p.y) * r * 0.3 * (1.0 + breathe) * (1.0 + 0.35 * swell);
            size = 1.7 * ds;
            a = live * (0.35 + 0.55 * d.p.w) * tw * depth_dim(s.z);
            rgb = select(HOT, vec3f(1.0), d.q.z > 0.5);
        }
        // Haze: pearl dust in a slow drift outside the shell; live only.
        default: {
            if idx >= k.state.w {
                return cull();
            }
            let tw = select(0.5 + 0.5 * sin(k.clock.z * d.p.w + d.p.z), 0.7, reduced);
            let t = d.p.x + k.clock.w;
            let rr = r * d.p.y;
            off = vec2f(cos(t) * rr, sin(t) * rr * 0.92);
            size = 1.5 * ds;
            a = 0.22 * tw * (1.4 - d.p.y * 0.45) * live;
            rgb = PEARL;
        }
    }

    if d.q.w < 0.5 {
        if lod <= 0.0 {
            return cull();
        }
        a *= lod;
    }

    // Changed shimmer: a faint bright front runs out through shell and haze. Reduced motion
    // lifts them evenly instead.
    let sh = k.flare.z;
    if sh < 1.0 && (group == 0u || group >= 3u) {
        let fade = 1.0 - sh;
        var boost = 0.5 * fade;
        if !reduced {
            let front = 0.2 + 1.9 * sh;
            let x = (length(off) / r - front) / 0.2;
            boost = 1.5 * fade * exp(-x * x);
        }
        a *= 1.0 + boost;
        rgb = mix(rgb, HOT, 0.4 * boost);
    }

    a = min(a, 1.0);
    if a <= 0.002 {
        return cull();
    }

    let half = size * 0.5;
    let ext = select(half + 1.5, half * 4.0 + 1.5, glow > 0.5);
    let corner = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let local = corner * ext;
    var o: POut;
    o.pos = to_ndc(k.frame.zw + off + local);
    o.local = local;
    o.half_glow = vec2f(half, glow);
    o.color = vec4f(rgb * a, a);
    return o;
}

// A soft round dot; bright dots add a faint pearl halo.
@fragment
fn particle_fs(in: POut) -> @location(0) vec4f {
    let h = in.half_glow.x;
    let d = length(in.local);
    var c = in.color.rgb * clamp(h - d + 0.5, 0.0, 1.0) * min(1.0, h * 1.6);
    if in.half_glow.y > 0.5 {
        let s = h * 1.7;
        let halo = 0.32 * exp(-d * d / (2.0 * s * s));
        c += mix(in.color.rgb, PEARL_HI * in.color.a, 0.5) * halo;
    }
    return vec4f(c, 0.0);
}

// Field: glows, the tick ring, flare light and the merge flash on one quad around the knot.

struct FOut {
    @builtin(position) pos: vec4f,
    @location(0) p: vec2f,
};

@vertex
fn field_vs(@builtin(vertex_index) vi: u32) -> FOut {
    let ext = k.scale.x * 2.65;
    let corner = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    var o: FOut;
    o.p = corner * ext;
    o.pos = to_ndc(k.frame.zw + o.p);
    return o;
}

// The prototype's glow sprite: a radial gradient with stops at 0, .16, .45 and 1.
fn glow(d: f32, rad: f32, col: vec3f, centre: vec3f, a: f32) -> vec3f {
    let t = d / rad;
    if t >= 1.0 || a <= 0.0 {
        return vec3f(0.0);
    }
    var c: vec3f;
    if t < 0.16 {
        c = mix(centre, col * 0.6, t / 0.16);
    } else if t < 0.45 {
        c = mix(col * 0.6, col * 0.14, (t - 0.16) / 0.29);
    } else {
        c = mix(col * 0.14, vec3f(0.0), (t - 0.45) / 0.55);
    }
    return c * min(a, 1.0);
}

fn stroke(d: f32, hw: f32) -> f32 {
    return clamp(hw - d + 0.5, 0.0, 1.0);
}

@fragment
fn field_fs(in: FOut) -> @location(0) vec4f {
    let r = k.scale.x;
    let lw = k.scale.z;
    let unit = k.scale.w;
    let live = k.state.x;
    let reduced = k.state.z > 0.5;
    let l = mix(0.45, 1.0, live);
    let p = in.p;
    let d = length(p);

    let ring = k.flare.x;
    let pulse = k.flare.y;
    let ring_on = 1.0 - ring;
    let pulse_on = 1.0 - pulse;

    // Wide glow: dim pearl while dormant, pearl while live.
    let wide = mix(PEARL_DIM, PEARL, live) * 0.85;
    var c = glow(d, r * 2.6, wide, wide, 0.18 + 0.3 * l);

    // Centre: hot and breathing while live, a cool bone glow while dormant. A head pulse
    // swells and brightens it.
    let breathe = select(0.05 * sin(k.clock.y), 0.0, reduced);
    let swell = select(sin(PI * pulse) * step(0.001, pulse_on), 0.0, reduced);
    let hot_r = r * (0.6 + breathe) * (1.0 + 0.45 * swell);
    let hot_a = 0.7 * l * live * (1.0 + 1.3 * pulse_on * pulse_on);
    c += glow(d, hot_r, HOT, HOT_CORE, hot_a);
    c += glow(d, r * 0.45, BONE, BONE, 0.4 * (1.0 - live) * (1.0 + 1.3 * pulse_on * pulse_on));

    // Merge flash: each dashed file lights the core briefly as it arrives, tinted by status.
    if k.files.w > 0.5 && d < r * 0.8 {
        let now = k.files.x;
        let n = u32(k.files.z);
        for (var i = 0u; i < n; i++) {
            let f = files[i];
            if f.t.z < 1.5 {
                continue;
            }
            let tau = now - (f.t.y + DASH);
            if tau < 0.0 || tau >= FLASH {
                continue;
            }
            let fa = exp(-tau / 0.12) * (1.0 - tau / FLASH);
            let s = r * (0.07 + 0.3 * tau);
            c += mix(HOT_CORE, f.c.rgb, 0.4) * 0.55 * fa * exp(-d * d / (2.0 * s * s));
        }
    }

    // Tick ring: 120 pearl ticks pointing out from 1.08 r, every tenth longer. Reduced motion
    // shows a branch change by lighting the ring ember instead of sending a ring outward.
    let rr = r * 1.08;
    if d > rr - 2.0 && d < rr + unit * 6.0 {
        let stp = TAU / 120.0;
        let ang = atan2(p.y, p.x) + PI * 0.5;
        let i = round(ang / stp);
        let t = i * stp - PI * 0.5;
        let along = dot(p, vec2f(cos(t), sin(t)));
        let across = abs(dot(p, vec2f(-sin(t), cos(t))));
        let major = ((i32(i) % 10) + 10) % 10 == 0;
        let len = select(2.6, 5.72, major) * unit;
        let tick = stroke(across, lw * 0.5) * clamp(min(along - rr, rr + len - along) + 0.5, 0.0, 1.0);
        var tc = mix(PEARL_DIM, PEARL, live) * 0.27;
        if reduced {
            tc = mix(tc, EMBER * 0.95, ring_on);
        }
        c += tc * tick;
    }

    if !reduced {
        // Branch: a strong ember ring thrown outward with a soft halo.
        if ring_on > 0.0 {
            let fr = r * (1.0 + 1.4 * e_out(ring));
            let o = abs(d - fr);
            let line = stroke(o, lw * 1.5);
            let halo = exp(-o * o / (2.0 * (6.0 * lw) * (6.0 * lw)));
            c += (EMBER * line * 0.95 + EMBER * halo * 0.22) * ring_on;
        }
        // Head: a soft hot wave swelling out from the centre.
        if pulse_on > 0.0 {
            let fr = r * (0.15 + 1.25 * e_out(pulse));
            let s = r * 0.14 * (1.0 + pulse);
            let x = (d - fr) / s;
            c += mix(HOT, EMBER, pulse) * 0.32 * pow(pulse_on, 1.5) * exp(-x * x);
        }
    }
    return vec4f(c, 0.0);
}

// File glyphs: rhombus beads that turn with the shell, grow in, fade out or dash home.

struct GOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) half_size: vec2f,
    @location(2) @interpolate(flat) color: vec4f,
    @location(3) @interpolate(flat) hover: f32,
};

fn cull_glyph() -> GOut {
    var o: GOut;
    o.pos = vec4f(2.0, 2.0, 0.0, 1.0);
    o.local = vec2f(0.0);
    o.half_size = vec2f(1.0);
    o.color = vec4f(0.0);
    o.hover = 0.0;
    return o;
}

// Where glyph `f` sits now, in pixels from the centre, and its depth.
fn glyph_at(f: File, now: f32) -> vec3f {
    let r = k.scale.x;
    if f.t.z > 1.5 && now >= f.t.y {
        let o = shell_turn(f.p.xyz, f.t.w, length(f.p.xyz));
        let prog = clamp((now - f.t.y) / DASH, 0.0, 1.0);
        return vec3f(o.xy * r * (1.0 - pow(prog, DASH_EASE)), 1.0);
    }
    let s = shell_turn(f.p.xyz, k.clock.x, length(f.p.xyz));
    return vec3f(s.xy * r, s.z);
}

@vertex
fn glyph_vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GOut {
    let f = files[ii];
    let now = k.files.x;
    let reduced = k.state.z > 0.5;
    let unit = k.scale.w;
    let kind = f.t.z;

    var a = k.state.x * smoothstep(0.0, GROW, now - f.t.x);
    var scale = mix(0.35, 1.0, smoothstep(0.0, GROW, now - f.t.x));
    if kind > 0.5 && kind < 1.5 {
        a *= 1.0 - smoothstep(0.0, FADE, now - f.t.y);
    }
    var bright = 0.0;
    if kind > 1.5 && now >= f.t.y {
        let prog = (now - f.t.y) / DASH;
        if prog >= 1.0 {
            return cull_glyph();
        }
        scale *= 1.0 - 0.75 * smoothstep(0.6, 1.0, prog);
        bright = prog;
    }
    if f.c.w > 0.5 {
        a *= select(0.6 + 0.4 * sin(k.clock.z * 6.0 + f.p.w), 0.85, reduced);
    }
    let at = glyph_at(f, now);
    scale *= depth_size(at.z);
    a *= depth_dim(at.z);
    let hover = select(0.0, k.flare.w, abs(f32(ii) - k.files.y) < 0.5);
    scale *= 1.0 + 0.3 * hover;
    if a <= 0.002 {
        return cull_glyph();
    }

    let hs = vec2f(0.62, 1.0) * unit * 4.4 * scale;
    let ext = hs.y * 2.6 + 1.0;
    let corner = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let local = corner * ext;
    var o: GOut;
    o.pos = to_ndc(k.frame.zw + at.xy + local);
    o.local = local;
    o.half_size = hs;
    o.color = vec4f(mix(f.c.rgb, HOT, 0.35 * bright), min(a, 1.0));
    o.hover = hover;
    return o;
}

// The bead: four facets lit from the upper left, a soft hot core, a faint status halo.
@fragment
fn glyph_fs(in: GOut) -> @location(0) vec4f {
    let hs = in.half_size;
    let q = abs(in.local) / hs;
    let m = q.x + q.y;
    let grad = length(vec2f(1.0 / hs.x, 1.0 / hs.y));
    let edge = (1.0 - m) / grad;
    let cov = clamp(edge + 0.5, 0.0, 1.0);

    let left = in.local.x < 0.0;
    let up = in.local.y < 0.0;
    let facet = select(select(0.42, 0.62, left), select(0.72, 1.0, left), up);
    let core = exp(-m * m * 6.0);
    let col = in.color.rgb;
    var body = col * (0.38 + 0.5 * facet) + mix(col, HOT_CORE, 0.65) * core * 0.75;
    // A brighter rim along the bead's edge.
    body += col * 0.25 * clamp(1.0 - abs(edge - 1.0), 0.0, 1.0);

    let a = in.color.a;
    let outside = max(-edge, 0.0);
    var halo = col * 0.2 * exp(-outside / (hs.y * 0.45)) * (1.0 - cov);
    // Hover: a thin outline a little outside the bead.
    let ring = clamp(1.0 - abs(outside - hs.y * 0.45) / 0.9, 0.0, 1.0);
    halo += mix(col, HOT_CORE, 0.4) * ring * 0.8 * in.hover;
    return vec4f(body * cov * a + halo * a, cov * a);
}

// Dash trails: a tapered ribbon from where the glyph left to the core. Each point lights as
// the glyph passes it and fades over TRAIL seconds.

struct TOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) len_w: vec2f,
    @location(2) @interpolate(flat) color: vec4f,
    @location(3) @interpolate(flat) since: f32,
};

@vertex
fn trail_vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> TOut {
    let f = files[ii];
    let now = k.files.x;
    let since = now - f.t.y;
    var o: TOut;
    o.pos = vec4f(2.0, 2.0, 0.0, 1.0);
    o.local = vec2f(0.0);
    o.len_w = vec2f(1.0);
    o.color = vec4f(0.0);
    o.since = 0.0;
    if f.t.z < 1.5 || since < 0.0 || since > DASH + TRAIL || k.state.x <= 0.0 {
        return o;
    }
    let r = k.scale.x;
    let start = shell_turn(f.p.xyz, f.t.w, length(f.p.xyz)).xy * r;
    let len = max(length(start), 1.0);
    let along = -start / len;
    let side = vec2f(-along.y, along.x);
    let w = k.scale.w * 2.6;
    let corner = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let s = mix(-w, len + w, corner.x);
    let c = (corner.y * 2.0 - 1.0) * w;
    o.pos = to_ndc(k.frame.zw + start + along * s + side * c);
    o.local = vec2f(s, c);
    o.len_w = vec2f(len, w);
    o.color = vec4f(f.c.rgb, k.state.x);
    o.since = since;
    return o;
}

@fragment
fn trail_fs(in: TOut) -> @location(0) vec4f {
    let len = in.len_w.x;
    let w = in.len_w.y;
    let s = clamp(in.local.x / len, 0.0, 1.0);
    let head = pow(clamp(in.since / DASH, 0.0, 1.0), DASH_EASE);
    if s > head {
        return vec4f(0.0);
    }
    // When the glyph passed this point, and how much of its light is left.
    let passed = DASH * pow(s, 1.0 / DASH_EASE);
    let life = clamp(1.0 - (in.since - passed) / TRAIL, 0.0, 1.0);
    if life <= 0.0 {
        return vec4f(0.0);
    }
    let half = w * (0.2 + 0.8 * life) * 0.5;
    let ends = max(max(-in.local.x, in.local.x - len), 0.0);
    let x = length(vec2f(ends, in.local.y)) / half;
    let a = life * life * 0.75 * exp(-x * x) * in.color.a;
    let rgb = mix(in.color.rgb, HOT_CORE, 0.45 * pow(life, 4.0));
    return vec4f(rgb * a, 0.0);
}
