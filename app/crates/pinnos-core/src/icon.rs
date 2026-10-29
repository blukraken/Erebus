//! Tray icon art. An ember disc with a soft glow on a transparent ground, like the face's link bead.

use pinnos_theme::EMBER;

/// Sizes the tray picks from, matching the shell's small-icon sizes at 100%, 125%, 150% and 200% DPI.
pub const SIZES: [u32; 4] = [16, 20, 24, 32];

const DISC: f32 = 0.28;
const GLOW_PEAK: f32 = 0.55;
const SAMPLES: u32 = 4;

/// Straight-alpha RGBA, row-major from the top, `size * size * 4` bytes.
pub fn render(size: u32) -> Vec<u8> {
    let s = size as f32;
    let (r, outer, mid) = (s * DISC, s * 0.5, s * 0.5); // disc radius, glow radius, centre
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut inside, mut dist) = (0u32, 0.0f32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let fx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - mid;
                    let fy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - mid;
                    let d = fx.hypot(fy);
                    dist += d;
                    inside += u32::from(d <= r);
                }
            }
            let disc = inside as f32 / (SAMPLES * SAMPLES) as f32;
            let d = dist / (SAMPLES * SAMPLES) as f32;
            let t = ((outer - d) / (outer - r)).clamp(0.0, 1.0);
            let alpha = disc + (1.0 - disc) * GLOW_PEAK * t * t;
            px.extend([EMBER.0, EMBER.1, EMBER.2, (alpha * 255.0).round() as u8]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(px: &[u8], size: u32, x: u32, y: u32) -> u8 {
        px[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn disc_is_ember_and_opaque_with_a_transparent_corner() {
        for size in SIZES {
            let px = render(size);
            assert_eq!(px.len(), (size * size * 4) as usize);
            let c = size / 2;
            let i = ((c * size + c) * 4) as usize;
            assert_eq!(&px[i..i + 4], &[EMBER.0, EMBER.1, EMBER.2, 255], "size {size}");
            assert_eq!(alpha(&px, size, 0, 0), 0, "size {size}");
            assert_eq!(alpha(&px, size, size - 1, size - 1), 0, "size {size}");
        }
    }

    #[test]
    fn glow_fades_outward() {
        let px = render(32);
        let row: Vec<u8> = (16..32).map(|x| alpha(&px, 32, x, 16)).collect();
        assert!(row.windows(2).all(|w| w[0] >= w[1]), "{row:?}");
        assert!(row[10] > 0 && row[10] < 255, "{row:?}");
    }

    /// Writes the 32 px icon as a PNG to the path in `PINNOS_ICON_PNG`; a no-op when unset.
    #[test]
    fn dump_png() {
        let Some(path) = std::env::var_os("PINNOS_ICON_PNG") else { return };
        std::fs::write(path, png(32, 32, &render(32))).unwrap();
    }

    /// Minimal RGBA PNG using stored (uncompressed) deflate blocks.
    fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut c = !0u32;
            for &b in data {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            out.extend((body.len() as u32).to_be_bytes());
            let mut tagged = kind.to_vec();
            tagged.extend(body);
            out.extend(&tagged);
            out.extend(crc(&tagged).to_be_bytes());
        }
        let mut raw = Vec::new();
        for row in rgba.chunks((w * 4) as usize) {
            raw.push(0);
            raw.extend(row);
        }
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push(u8::from(i + 1 == blocks.len()));
            z.extend((b.len() as u16).to_le_bytes());
            z.extend((!(b.len() as u16)).to_le_bytes());
            z.extend(*b);
        }
        let (mut a, mut s) = (1u32, 0u32);
        for &b in &raw {
            a = (a + u32::from(b)) % 65_521;
            s = (s + a) % 65_521;
        }
        z.extend(((s << 16) | a).to_be_bytes());

        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}
