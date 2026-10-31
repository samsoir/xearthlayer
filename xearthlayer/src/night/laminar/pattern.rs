//! Laminar lit textures decoded to pixels, for the lit generator to sample.

use std::path::Path;

/// One lit pattern: the top level of a Laminar `*_LIT.dds`, as RGB.
#[derive(Debug, Clone, PartialEq)]
pub struct LitPattern {
    /// Width and height in pixels (Laminar's are square).
    pub size: u32,
    /// Row-major RGB, `size * size` entries.
    pub rgb: Vec<[u8; 3]>,
    /// Metres of ground one copy of the pattern covers.
    pub ground_scale_m: f64,
}

impl LitPattern {
    /// Bilinear sample at pattern coordinates `(u, v)` in repeats; the
    /// pattern wraps, so any real `u`, `v` is valid.
    pub fn sample(&self, u: f64, v: f64) -> [f32; 3] {
        let n = self.size as f64;
        let (x, y) = (u * n - 0.5, v * n - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
        let wrap = |i: f64| (i as i64).rem_euclid(self.size as i64) as usize;
        let (xa, xb, ya, yb) = (wrap(x0), wrap(x0 + 1.0), wrap(y0), wrap(y0 + 1.0));
        let at = |x: usize, y: usize| self.rgb[y * self.size as usize + x];
        let mut out = [0.0f32; 3];
        for (ch, o) in out.iter_mut().enumerate() {
            let top = at(xa, ya)[ch] as f32 * (1.0 - fx) + at(xb, ya)[ch] as f32 * fx;
            let bottom = at(xa, yb)[ch] as f32 * (1.0 - fx) + at(xb, yb)[ch] as f32 * fx;
            *o = top * (1.0 - fy) + bottom * fy;
        }
        out
    }
}

/// Decode a DXT1 DDS file's top level to RGB, or `None` when it is not a
/// readable square DXT1 texture.
pub fn decode_dxt1(bytes: &[u8], ground_scale_m: f64) -> Option<LitPattern> {
    let dds = ddsfile::Dds::read(bytes).ok()?;
    if dds.get_d3d_format() != Some(ddsfile::D3DFormat::DXT1) {
        return None;
    }
    let (w, h) = (dds.get_width(), dds.get_height());
    if w != h || w == 0 || w % 4 != 0 {
        return None;
    }
    let size = w as usize;
    let blocks = size / 4;
    let top = dds.data.get(..blocks * blocks * 8)?;
    let mut rgba = vec![0u8; size * size * 4];
    let pitch = size * 4;
    for by in 0..blocks {
        for bx in 0..blocks {
            let block = &top[(by * blocks + bx) * 8..][..8];
            let start = by * 4 * pitch + bx * 4 * 4;
            bcdec_rs::bc1(block, &mut rgba[start..], pitch);
        }
    }
    let rgb = rgba.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect();
    Some(LitPattern {
        size: w,
        rgb,
        ground_scale_m,
    })
}

/// Read and decode a lit texture file.
pub fn load_pattern(path: &Path, ground_scale_m: f64) -> Option<LitPattern> {
    decode_dxt1(&std::fs::read(path).ok()?, ground_scale_m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::{DdsFormat, DdsHeader};

    /// A 4 x 4 DXT1 file whose single block is solid `c0` (indices all 0).
    fn solid(c0: u16) -> Vec<u8> {
        let mut bytes = DdsHeader::new(4, 4, 1, DdsFormat::BC1).to_bytes();
        bytes.extend_from_slice(&c0.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn decodes_a_solid_block() {
        let p = decode_dxt1(&solid(0xF800), 4096.0).unwrap();
        assert_eq!(p.size, 4);
        assert_eq!(p.rgb.len(), 16);
        assert!(p.rgb.iter().all(|&c| c == [255, 0, 0]), "{:?}", p.rgb[0]);
        assert_eq!(p.ground_scale_m, 4096.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_dxt1(b"not a dds", 4096.0).is_none());
    }

    #[test]
    fn sampling_wraps_and_interpolates() {
        let mut p = decode_dxt1(&solid(0xF800), 4096.0).unwrap();
        assert_eq!(p.sample(0.3, 0.7), [255.0, 0.0, 0.0]);
        assert_eq!(p.sample(-5.3, 12.7), [255.0, 0.0, 0.0], "wraps");
        // Left half black, right half white: the middle of the seam is grey.
        for (i, c) in p.rgb.iter_mut().enumerate() {
            *c = if i % 4 < 2 {
                [0, 0, 0]
            } else {
                [200, 200, 200]
            };
        }
        let mid = p.sample(0.5, 0.5);
        assert!((mid[0] - 100.0).abs() < 1.0, "{mid:?}");
    }

    /// The developer's real installation, skipped when absent.
    #[test]
    fn decodes_a_real_laminar_lit_texture() {
        let path = Path::new(
            "/media/FlightSim/X-Plane 12-beta/Resources/default scenery/1000 world terrain/textures12/urban_FAR/uw5_FAR_LIT.dds",
        );
        if !path.is_file() {
            return;
        }
        let p = load_pattern(path, 4096.0).unwrap();
        assert_eq!(p.size, 1024);
        assert!(p.rgb.iter().any(|c| c.iter().any(|&v| v > 64)), "has light");
    }
}
