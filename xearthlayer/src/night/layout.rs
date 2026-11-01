//! The layout of lit DDS files: the minimal black DDS (phase 1), and
//! `LitLayout`, the full mip chain geometry and all-zero bytes of a lit
//! footprint (phase 2a).

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

/// The shape of one lit DDS: blank levels for the fade, then the visible ones.
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

    /// Bytes `[offset, offset + len)` of the lit texture: the header, zero
    /// blank levels, then `payload` (BC1 visible levels). A payload of the
    /// wrong length is ignored and the texture reads black (same size, R7).
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
        if end > payload_start {
            let from = (payload_start - start) as usize;
            out[from..].copy_from_slice(&payload[..(end - payload_start) as usize]);
        }
        Bytes::from(out)
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

    #[test]
    fn lit_reads_compose_header_zeros_and_payload() {
        let l = LitLayout::new(18, 4); // top 512, first visible 256
        let payload = Bytes::from(vec![7u8; l.payload_len() as usize]);
        let start = l.payload_start();
        assert_eq!(&l.read_lit(0, 4, &payload)[..], b"DDS ");
        assert!(
            l.read_lit(128, 64, &payload).iter().all(|&b| b == 0),
            "blank level"
        );
        let across = l.read_lit(start - 2, 4, &payload);
        assert_eq!(&across[..], &[0, 0, 7, 7]);
        let all: usize = (0..l.total_len)
            .step_by(1 << 16)
            .map(|o| l.read_lit(o, 1 << 16, &payload).len())
            .sum();
        assert_eq!(all as u64, l.total_len);
        assert!(l.read_lit(l.total_len, 8, &payload).is_empty());
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
