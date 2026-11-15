//! The layout of lit DDS files: the minimal black DDS (phase 1), and
//! `LitLayout`, the full mip chain geometry of a lit footprint and the bytes
//! of its levels (phase 2a): the header, the black levels X-Plane drops, the
//! top level it uploads (built from the first visible level) and the
//! generated payload.

use std::sync::OnceLock;

use bytes::Bytes;

use crate::cache::integrity::full_chain_bc1_dds_size;
use crate::dds::{DdsFormat, DdsHeader};

/// First visible level of a ZL16 lit texture: about 7.5 m per texel.
pub const FIRST_VISIBLE_ZL16: u32 = 1024;

/// Largest top level a lit DDS declares. At the lowest texture setting the
/// uncapped top would be 32768 px, unverified in X-Plane; the cap shrinks the
/// first visible level instead (design: Lit DDS layout, Risk).
pub const MAX_TOP_LEVEL: u32 = 16384;

/// The shape of one lit DDS: the levels above the first visible one (the
/// `blank_levels`, all black but the last, which X-Plane uploads), then the
/// visible ones.
#[derive(Debug, Clone)]
pub struct LitLayout {
    pub top: u32,
    pub levels: u32,
    pub blank_levels: u32,
    pub first_visible: u32,
    pub total_len: u64,
    header: Bytes,
}

impl LitLayout {
    /// The layout for a texture at `zoom` under X-Plane texture setting `t` (R15).
    pub fn new(zoom: u8, texture_setting: u8) -> Self {
        let t = u32::from(texture_setting.min(4));
        let blank_levels = (4 - t) + 1;
        let wanted_first = if zoom >= 16 {
            (FIRST_VISIBLE_ZL16 >> (zoom - 16)).max(4)
        } else {
            (FIRST_VISIBLE_ZL16 << (16 - zoom)).min(MAX_TOP_LEVEL)
        };
        let top = (wanted_first << blank_levels).min(MAX_TOP_LEVEL);
        let first_visible = top >> blank_levels;
        let levels = top.ilog2() + 1;
        let header = Bytes::from(DdsHeader::new(top, top, levels, DdsFormat::BC1).to_bytes());
        let total_len = full_chain_bc1_dds_size(top, top) as u64;
        Self {
            top,
            levels,
            blank_levels,
            first_visible,
            total_len,
            header,
        }
    }

    /// Bytes `[offset, offset + len)` of the all-black texture: the header,
    /// then zero blocks (black, adds nothing to the base colour).
    pub fn read_black(&self, offset: u64, len: u32) -> Bytes {
        let start = offset.min(self.total_len);
        let end = start.saturating_add(u64::from(len)).min(self.total_len);
        let mut out = vec![0u8; (end - start) as usize];
        let header_len = self.header.len() as u64;
        if start < header_len {
            let n = (header_len.min(end) - start) as usize;
            out[..n].copy_from_slice(&self.header[start as usize..start as usize + n]);
        }
        Bytes::from(out)
    }

    /// Bytes of the visible levels (first visible level down to 1 x 1).
    pub fn payload_len(&self) -> u64 {
        full_chain_bc1_dds_size(self.first_visible, self.first_visible) as u64
            - self.header.len() as u64
    }

    /// Where the visible levels start: after the header and the blank levels.
    pub fn payload_start(&self) -> u64 {
        self.total_len - self.payload_len()
    }

    /// Where the top level X-Plane uploads starts: the last blank level,
    /// twice the first visible level's size. The levels above it are the
    /// ones the texture setting drops at load.
    pub fn uploaded_top_start(&self) -> u64 {
        let blocks = u64::from(self.first_visible * 2).div_ceil(4);
        self.payload_start() - blocks * blocks * BC1_BLOCK_LEN as u64
    }

    /// Whether bytes `[offset, offset + len)` reach the uploaded top level
    /// or the visible levels, so that serving them needs the payload.
    pub fn reads_light(&self, offset: u64, len: u32) -> bool {
        offset < self.total_len && offset.saturating_add(u64::from(len)) > self.uploaded_top_start()
    }

    /// Bytes `[offset, offset + len)` of the lit texture: the header, the
    /// black levels X-Plane drops, the uploaded top level, then `payload`
    /// (BC1 visible levels). The uploaded top level repeats the first
    /// visible level at twice the size, each block of it four blocks with the
    /// same colours: X-Plane loads it for the textures nearest the aircraft
    /// and shows it over the whole texture, so a black top level left the
    /// ground under the 3D scenery unlit (KCCR, 2026-11-14). It is built per
    /// read and never stored. A payload of the wrong length is ignored and
    /// the texture reads black (same size, R7).
    pub fn read_lit(&self, offset: u64, len: u32, payload: &Bytes) -> Bytes {
        if payload.len() as u64 != self.payload_len() {
            return self.read_black(offset, len);
        }
        let payload_start = self.payload_start();
        let start = offset.min(self.total_len);
        let end = start.saturating_add(u64::from(len)).min(self.total_len);
        if start >= payload_start {
            // Entirely inside the payload: a refcount slice, no copy.
            return payload.slice((start - payload_start) as usize..(end - payload_start) as usize);
        }
        let mut out = self.read_black(start, (end - start) as u32).to_vec();
        let top = self.uploaded_top_start();
        let (from, to) = (start.max(top), end.min(payload_start));
        if from < to {
            upsample_bc1(
                payload,
                self.first_visible,
                from - top,
                &mut out[(from - start) as usize..(to - start) as usize],
            );
        }
        if end > payload_start {
            let from = (payload_start - start) as usize;
            out[from..].copy_from_slice(&payload[..(end - payload_start) as usize]);
        }
        Bytes::from(out)
    }
}

/// Bytes of one BC1 block: two RGB565 colours, then sixteen 2-bit indices.
const BC1_BLOCK_LEN: usize = 8;

/// Writes bytes `[at, at + out.len())` of the BC1 level twice the size of
/// `first`, a BC1 level `side` texels wide, with each texel repeated over
/// 2 x 2. A block of `first` becomes four blocks with its two colours, each
/// taking the indices of one quadrant, so the light and colour are the same,
/// block for block, and nothing is decoded or encoded.
fn upsample_bc1(first: &[u8], side: u32, at: u64, out: &mut [u8]) {
    let first_row = u64::from(side).div_ceil(4);
    let row = first_row * 2;
    let len = BC1_BLOCK_LEN as u64;
    let mut block = at / len;
    let mut written = 0;
    while written < out.len() {
        let (bx, by) = (block % row, block / row);
        let source = ((by / 2) * first_row + bx / 2) as usize * BC1_BLOCK_LEN;
        let source = &first[source..source + BC1_BLOCK_LEN];
        let indices = u32::from_le_bytes([source[4], source[5], source[6], source[7]]);
        let (qx, qy) = ((bx % 2) as u32 * 2, (by % 2) as u32 * 2);
        let mut quadrant = 0u32;
        for y in 0..4 {
            for x in 0..4 {
                let from = 2 * ((qy + y / 2) * 4 + qx + x / 2);
                quadrant |= ((indices >> from) & 3) << (2 * (y * 4 + x));
            }
        }
        let mut bytes = [0u8; BC1_BLOCK_LEN];
        bytes[..4].copy_from_slice(&source[..4]);
        bytes[4..].copy_from_slice(&quadrant.to_le_bytes());
        let skip = if written == 0 { (at % len) as usize } else { 0 };
        let n = (BC1_BLOCK_LEN - skip).min(out.len() - written);
        out[written..written + n].copy_from_slice(&bytes[skip..skip + n]);
        written += n;
        block += 1;
    }
}

/// The texture served wherever nothing is lit: 4 x 4 DXT1 with its full
/// chain (4, 2 and 1 px levels, one 8-byte block each), all blocks zero,
/// which decodes to black and adds nothing to the base colour. 152 bytes.
pub fn minimal_black_dds() -> Bytes {
    static DDS: OnceLock<Bytes> = OnceLock::new();
    DDS.get_or_init(|| {
        let mut bytes = DdsHeader::new(4, 4, 3, DdsFormat::BC1).to_bytes();
        bytes.resize(bytes.len() + 3 * 8, 0);
        Bytes::from(bytes)
    })
    .clone()
}

#[cfg(test)]
mod lit_tests {
    use super::*;

    #[test]
    fn payload_follows_header_and_blank_levels() {
        let l = LitLayout::new(16, 4); // top 2048, first visible 1024
        assert_eq!(
            l.payload_len(),
            full_chain_bc1_dds_size(1024, 1024) as u64 - 128
        );
        assert_eq!(l.payload_start(), 128 + 2048 * 2048 / 2);
        assert_eq!(l.payload_start() + l.payload_len(), l.total_len);
    }

    /// A payload whose every block differs: endpoints and indices from a
    /// hash of the byte offset.
    fn varied_payload(l: &LitLayout) -> Bytes {
        (0..l.payload_len())
            .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect::<Vec<u8>>()
            .into()
    }

    /// The colour endpoints and 2-bit index of texel `(x, y)` of a BC1
    /// level `width` texels wide.
    fn texel(level: &[u8], width: u32, x: u32, y: u32) -> ([u8; 4], u8) {
        let block = ((y / 4) * width.div_ceil(4) + x / 4) as usize * 8;
        let b = &level[block..block + 8];
        let indices = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let at = 2 * ((y % 4) * 4 + x % 4);
        ([b[0], b[1], b[2], b[3]], ((indices >> at) & 3) as u8)
    }

    #[test]
    fn lit_reads_compose_header_zeros_and_payload() {
        let l = LitLayout::new(18, 3); // top 1024: 1024 dropped, 512 uploaded, 256 visible
        let payload = Bytes::from(vec![7u8; l.payload_len() as usize]);
        let start = l.payload_start();
        assert_eq!(&l.read_lit(0, 4, &payload)[..], b"DDS ");
        assert!(
            l.read_lit(128, 64, &payload).iter().all(|&b| b == 0),
            "the level X-Plane drops"
        );
        // The last block of the uploaded top is the bottom-right quadrant of
        // a block whose index bytes are all 0b00_00_01_11: texels 2 and 3 of
        // each row, index 0. Its colours are the payload's, 7.
        let across = l.read_lit(start - 2, 4, &payload);
        assert_eq!(&across[..], &[0, 0, 7, 7], "uploaded top, then payload");
        assert!(l.read_lit(l.total_len, 8, &payload).is_empty());
    }

    #[test]
    fn the_uploaded_top_level_repeats_the_first_visible_level_at_twice_the_size() {
        // X-Plane loads the top level for the textures nearest the aircraft
        // and shows it over the whole texture, so a black top level left the
        // ground under the 3D scenery unlit (KCCR, 2026-11-14). Each of its
        // texels is the first visible level's texel under it: the same light
        // and colour, block for block, at no generation cost.
        for (zoom, t) in [(18, 4), (17, 3), (16, 4)] {
            let l = LitLayout::new(zoom, t);
            let payload = varied_payload(&l);
            let file = l.read_lit(0, u32::MAX, &payload);
            let size = l.first_visible * 2;
            let top = &file[l.uploaded_top_start() as usize..l.payload_start() as usize];
            assert_eq!(top.len() as u32, size * size / 2);
            for y in 0..size {
                for x in 0..size {
                    assert_eq!(
                        texel(top, size, x, y),
                        texel(&payload, l.first_visible, x / 2, y / 2),
                        "ZL{zoom} t = {t} texel ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn levels_x_plane_drops_stay_black() {
        let l = LitLayout::new(16, 2); // 8192 and 4096 dropped, 2048 uploaded
        let payload = varied_payload(&l);
        let file = l.read_lit(0, u32::MAX, &payload);
        let dropped = &file[128..l.uploaded_top_start() as usize];
        assert_eq!(dropped.len(), (8192 * 8192 + 4096 * 4096) / 2);
        assert!(dropped.iter().all(|&b| b == 0));
    }

    #[test]
    fn any_split_of_a_lit_read_gives_the_same_bytes() {
        // FUSE reads arrive in ranges of up to 1 MiB at any offset.
        let l = LitLayout::new(18, 3);
        let payload = varied_payload(&l);
        let whole = l.read_lit(0, u32::MAX, &payload);
        assert_eq!(whole.len() as u64, l.total_len);
        for step in [1 << 16, 999, 8 * 1000 + 3] {
            let pieces: Vec<u8> = (0..l.total_len)
                .step_by(step)
                .flat_map(|o| l.read_lit(o, step as u32, &payload).to_vec())
                .collect();
            assert_eq!(pieces, whole.to_vec(), "step {step}");
        }
    }

    #[test]
    fn only_reads_from_the_uploaded_top_on_need_the_payload() {
        let l = LitLayout::new(16, 3);
        let top = l.uploaded_top_start();
        assert!(!l.reads_light(0, 128));
        assert!(!l.reads_light(128, (top - 128) as u32));
        assert!(l.reads_light(top - 1, 2));
        assert!(l.reads_light(l.payload_start(), 8));
        assert!(!l.reads_light(l.total_len, 8));
    }

    #[test]
    fn a_wrong_size_payload_reads_black() {
        let l = LitLayout::new(18, 4);
        let bad = Bytes::from(vec![7u8; 10]);
        assert_eq!(l.read_lit(0, u32::MAX, &bad), l.read_black(0, u32::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn minimal_black_dds_is_a_152_byte_4x4_dxt1_with_three_levels() {
        let dds = minimal_black_dds();
        assert_eq!(dds.len(), 152);
        assert_eq!(&dds[0..4], b"DDS ");
        assert_eq!(u32_at(&dds, 12), 4, "height");
        assert_eq!(u32_at(&dds, 16), 4, "width");
        assert_eq!(u32_at(&dds, 28), 3, "mip levels");
        assert_eq!(&dds[84..88], b"DXT1");
        assert!(dds[128..].iter().all(|&b| b == 0), "all blocks black");
    }

    #[test]
    fn zl16_at_full_setting_matches_the_design_table() {
        let l = LitLayout::new(16, 4);
        assert_eq!((l.blank_levels, l.first_visible, l.top), (1, 1024, 2048));
        assert_eq!(l.levels, 12);
        assert_eq!(l.total_len, 2_796_344);
    }

    #[test]
    fn lower_settings_blank_more_levels() {
        let l = LitLayout::new(16, 2);
        assert_eq!((l.blank_levels, l.top), (3, 8192));
        let l = LitLayout::new(16, 3);
        assert_eq!((l.blank_levels, l.top), (2, 4096));
    }

    #[test]
    fn the_fade_distance_does_not_follow_the_texture_setting() {
        // Where the lit layer cuts out is set by the texel size of the
        // blank level X-Plane uploads first. Shrinking the visible level
        // with the setting moved the cut-out 2x farther per notch (flown
        // 2026-10-18), so the first visible level is fixed in metres.
        for zoom in [15, 16, 18] {
            let full = LitLayout::new(zoom, 4);
            for t in 1..=3u8 {
                let l = LitLayout::new(zoom, t);
                if l.top == MAX_TOP_LEVEL {
                    continue; // the cap trades visible resolution instead
                }
                assert_eq!(l.first_visible, full.first_visible, "ZL{zoom} t = {t}");
                assert_eq!(l.blank_levels, u32::from(4 - t) + 1, "ZL{zoom} t = {t}");
            }
        }
    }

    #[test]
    fn first_visible_scales_with_zoom() {
        assert_eq!(LitLayout::new(17, 4).first_visible, 512);
        assert_eq!(LitLayout::new(18, 4).first_visible, 256);
        assert_eq!(LitLayout::new(15, 4).first_visible, 2048);
    }

    #[test]
    fn the_top_level_is_capped() {
        let l = LitLayout::new(16, 0);
        assert_eq!(l.top, MAX_TOP_LEVEL);
        assert_eq!(l.first_visible, MAX_TOP_LEVEL >> l.blank_levels);
    }

    #[test]
    fn black_reads_are_header_then_zeros_and_agree_with_size() {
        let l = LitLayout::new(16, 4);
        let head = l.read_black(0, 200);
        assert_eq!(&head[0..4], b"DDS ");
        assert_eq!(u32_at(&head, 16), 2048, "width");
        assert_eq!(u32_at(&head, 28), 12, "mips");
        assert!(head[128..].iter().all(|&b| b == 0));
        let all: usize = (0..l.total_len)
            .step_by(1 << 20)
            .map(|o| l.read_black(o, 1 << 20).len())
            .sum();
        assert_eq!(all as u64, l.total_len);
        assert!(l.read_black(l.total_len, 8).is_empty());
        assert_eq!(l.read_black(l.total_len - 3, 8).len(), 3);
    }
}
