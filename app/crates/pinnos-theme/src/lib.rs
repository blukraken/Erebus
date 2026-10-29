//! Loom palette. Values come from the approved prototype `v3/concept-loom4.html`.
//!
//! Colours are stored as sRGB bytes. Use [`Rgb::linear`] for GPU clear colours and shader
//! uniforms on an sRGB swapchain.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Self {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// sRGB → linear, per channel, in 0.0..=1.0.
    pub fn linear(self) -> [f64; 3] {
        fn ch(c: u8) -> f64 {
            let c = c as f64 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        }
        [ch(self.0), ch(self.1), ch(self.2)]
    }

    pub fn srgb_f32(self) -> [f32; 3] {
        [self.0 as f32 / 255.0, self.1 as f32 / 255.0, self.2 as f32 / 255.0]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub name: &'static str,
    pub role: &'static str,
    pub rgb: Rgb,
}

// Grounds
pub const INK: Rgb = Rgb::hex(0x0b0a09);
pub const SLAB: Rgb = Rgb::hex(0x0d0c0b);
pub const SLAB_2: Rgb = Rgb::hex(0x13110e);

// Neutrals
pub const BONE: Rgb = Rgb::hex(0xece4d4);
pub const BONE_2: Rgb = Rgb::hex(0xc4baa5);
pub const BONE_3: Rgb = Rgb::hex(0x958a75);
pub const DIM: Rgb = Rgb(120, 110, 94);
pub const BRASS: Rgb = Rgb::hex(0x8c7a58);
pub const RULE: Rgb = Rgb::hex(0x3a3328);
pub const RULE_2: Rgb = Rgb::hex(0x4d4436);

// Knot: the system's own colour, neutral so agents stand out against it
pub const PEARL: Rgb = Rgb::hex(0xdedbd3);
pub const PEARL_HI: Rgb = Rgb::hex(0xf5f3ee);

// Energy
pub const EMBER: Rgb = Rgb::hex(0xffb04a);
pub const HOT: Rgb = Rgb(255, 236, 200);
pub const HOT_CORE: Rgb = Rgb(255, 250, 240);
pub const FROST: Rgb = Rgb(168, 208, 232);
pub const ICE: Rgb = Rgb(232, 246, 255);
pub const LILAC: Rgb = Rgb::hex(0xb0a5dc);

// Semantic
pub const VERD: Rgb = Rgb::hex(0x72d9b2);
pub const VERM: Rgb = Rgb::hex(0xff5a3d);

/// Every token in display order, for the palette strip and docs.
pub const TOKENS: &[Token] = &[
    Token { name: "ink", role: "window ground", rgb: INK },
    Token { name: "slab", role: "panel ground", rgb: SLAB },
    Token { name: "slab-2", role: "raised panel", rgb: SLAB_2 },
    Token { name: "rule", role: "hairlines", rgb: RULE },
    Token { name: "rule-2", role: "strong hairlines", rgb: RULE_2 },
    Token { name: "dim", role: "dormant", rgb: DIM },
    Token { name: "brass", role: "ticks, chrome", rgb: BRASS },
    Token { name: "bone-3", role: "tertiary text", rgb: BONE_3 },
    Token { name: "bone-2", role: "secondary text", rgb: BONE_2 },
    Token { name: "bone", role: "primary text, worker braids", rgb: BONE },
    Token { name: "pearl", role: "knot shell and trails", rgb: PEARL },
    Token { name: "pearl-hi", role: "knot heads", rgb: PEARL_HI },
    Token { name: "ember", role: "Claude, active", rgb: EMBER },
    Token { name: "hot", role: "nucleus core", rgb: HOT },
    Token { name: "hot-core", role: "nucleus white point", rgb: HOT_CORE },
    Token { name: "frost", role: "Codex", rgb: FROST },
    Token { name: "ice", role: "Codex highlight", rgb: ICE },
    Token { name: "lilac", role: "memory, unknown relation", rgb: LILAC },
    Token { name: "verd", role: "success", rgb: VERD },
    Token { name: "verm", role: "approval, error", rgb: VERM },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_unpacks() {
        assert_eq!(Rgb::hex(0xffb04a), Rgb(255, 176, 74));
    }

    #[test]
    fn linear_endpoints() {
        assert_eq!(Rgb(0, 0, 0).linear(), [0.0, 0.0, 0.0]);
        assert_eq!(Rgb(255, 255, 255).linear(), [1.0, 1.0, 1.0]);
    }
}
