//! The layout of lit DDS files: the minimal black DDS (phase 1), and
//! `LitLayout`, the full mip chain geometry of a lit footprint and the bytes
//! of its levels (phase 2a): the header, the black levels X-Plane drops, the
//! top level it uploads and the generated payload.

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

    /// Bytes of the top level X-Plane uploads: the last blank level, twice
    /// the first visible level's size.
    pub fn top_len(&self) -> u64 {
        let blocks = u64::from(self.first_visible * 2).div_ceil(4);
        blocks * blocks * BC1_BLOCK_LEN
    }

    /// Where the top level X-Plane uploads starts. The levels above it are
    /// the ones the texture setting drops at load.
    pub fn uploaded_top_start(&self) -> u64 {
        self.payload_start() - self.top_len()
    }

    /// The generated parts bytes `[offset, offset + len)` overlap: the
    /// uploaded top level, the payload, both or neither.
    pub fn reach(&self, offset: u64, len: u32) -> LitReach {
        let start = offset.min(self.total_len);
        let end = start.saturating_add(u64::from(len)).min(self.total_len);
        let payload_start = self.payload_start();
        LitReach {
            top: start < end && start < payload_start && end > self.uploaded_top_start(),
            payload: start < end && end > payload_start,
        }
    }

    /// Bytes `[offset, offset + len)` of the lit texture: the header, the
    /// black levels X-Plane drops, the uploaded `top` level, then `payload`
    /// (BC1 visible levels). Each part is generated separately; one that is
    /// absent or of the wrong length reads as zeros (black, same size, R7).
    pub fn read_lit(
        &self,
        offset: u64,
        len: u32,
        top: Option<&Bytes>,
        payload: Option<&Bytes>,
    ) -> Bytes {
        let start = offset.min(self.total_len);
        let end = start.saturating_add(u64::from(len)).min(self.total_len);
        let parts = [
            (
                self.uploaded_top_start(),
                top.filter(|t| t.len() as u64 == self.top_len()),
            ),
            (
                self.payload_start(),
                payload.filter(|p| p.len() as u64 == self.payload_len()),
            ),
        ];
        // Entirely inside one part: a refcount slice, no copy.
        for (at, part) in parts {
            if let Some(part) = part {
                if start >= at && end <= at + part.len() as u64 {
                    return part.slice((start - at) as usize..(end - at) as usize);
                }
            }
        }
        let mut out = self.read_black(start, (end - start) as u32).to_vec();
        for (at, part) in parts {
            let Some(part) = part else { continue };
            let (from, to) = (start.max(at), end.min(at + part.len() as u64));
            if from < to {
                out[(from - start) as usize..(to - start) as usize]
                    .copy_from_slice(&part[(from - at) as usize..(to - at) as usize]);
            }
        }
        Bytes::from(out)
    }
}

/// Which generated parts of a lit texture a read overlaps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LitReach {
    /// The top level X-Plane uploads.
    pub top: bool,
    /// The visible levels.
    pub payload: bool,
}

/// Bytes of one BC1 block: two RGB565 colours, then sixteen 2-bit indices.
const BC1_BLOCK_LEN: u64 = 8;

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

    /// `len` bytes that differ from one another: a hash of the offset,
    /// salted so a top and a payload differ too.
    fn varied(len: u64, salt: u64) -> Bytes {
        (0..len)
            .map(|i| ((i ^ salt).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect::<Vec<u8>>()
            .into()
    }

    #[test]
    fn lit_reads_compose_header_zeros_top_and_payload() {
        let l = LitLayout::new(18, 3); // top 1024: 1024 dropped, 512 uploaded, 256 visible
        let top = Bytes::from(vec![5u8; l.top_len() as usize]);
        let payload = Bytes::from(vec![7u8; l.payload_len() as usize]);
        let read = |offset, len| l.read_lit(offset, len, Some(&top), Some(&payload));
        assert_eq!(&read(0, 4)[..], b"DDS ");
        assert!(
            read(128, 64).iter().all(|&b| b == 0),
            "the level X-Plane drops"
        );
        let (top_start, start) = (l.uploaded_top_start(), l.payload_start());
        assert_eq!(
            &read(top_start - 2, 4)[..],
            &[0, 0, 5, 5],
            "dropped, then top"
        );
        assert_eq!(&read(start - 2, 4)[..], &[5, 5, 7, 7], "top, then payload");
        assert!(read(l.total_len, 8).is_empty());
    }

    #[test]
    fn the_uploaded_top_is_twice_the_first_visible_level() {
        for (zoom, t) in [(18, 4), (17, 3), (16, 4), (16, 0)] {
            let l = LitLayout::new(zoom, t);
            let size = u64::from(l.first_visible * 2);
            assert_eq!(l.top_len(), size * size / 2, "ZL{zoom} t = {t}");
            assert_eq!(l.uploaded_top_start() + l.top_len(), l.payload_start());
        }
    }

    #[test]
    fn a_missing_or_wrong_size_top_reads_as_zeros() {
        // As before the top was lit: black, and the payload still serves.
        let l = LitLayout::new(16, 3);
        let payload = varied(l.payload_len(), 1);
        let (from, to) = (l.uploaded_top_start() as usize, l.payload_start() as usize);
        let bad = Bytes::from(vec![5u8; 10]);
        for top in [None, Some(&bad)] {
            let file = l.read_lit(0, u32::MAX, top, Some(&payload));
            assert!(file[from..to].iter().all(|&b| b == 0));
            assert_eq!(&file[to..], &payload[..]);
        }
    }

    #[test]
    fn levels_x_plane_drops_stay_black() {
        let l = LitLayout::new(16, 2); // 8192 and 4096 dropped, 2048 uploaded
        let (top, payload) = (varied(l.top_len(), 2), varied(l.payload_len(), 1));
        let file = l.read_lit(0, u32::MAX, Some(&top), Some(&payload));
        let dropped = &file[128..l.uploaded_top_start() as usize];
        assert_eq!(dropped.len(), (8192 * 8192 + 4096 * 4096) / 2);
        assert!(dropped.iter().all(|&b| b == 0));
        assert_eq!(
            &file[l.uploaded_top_start() as usize..l.payload_start() as usize],
            &top[..]
        );
    }

    #[test]
    fn any_split_of_a_lit_read_gives_the_same_bytes() {
        // FUSE reads arrive in ranges of up to 1 MiB at any offset.
        let l = LitLayout::new(18, 3);
        let (top, payload) = (varied(l.top_len(), 2), varied(l.payload_len(), 1));
        let whole = l.read_lit(0, u32::MAX, Some(&top), Some(&payload));
        assert_eq!(whole.len() as u64, l.total_len);
        for step in [1 << 16, 999, 8 * 1000 + 3] {
            let pieces: Vec<u8> = (0..l.total_len)
                .step_by(step)
                .flat_map(|o| {
                    l.read_lit(o, step as u32, Some(&top), Some(&payload))
                        .to_vec()
                })
                .collect();
            assert_eq!(pieces, whole.to_vec(), "step {step}");
        }
    }

    #[test]
    fn a_read_reaches_the_parts_its_range_overlaps() {
        let l = LitLayout::new(16, 3);
        let (top, start) = (l.uploaded_top_start(), l.payload_start());
        let none = LitReach::default();
        assert_eq!(l.reach(0, 128), none, "header");
        assert_eq!(l.reach(128, (top - 128) as u32), none, "dropped level");
        let only_top = LitReach {
            top: true,
            payload: false,
        };
        assert_eq!(l.reach(top - 1, 2), only_top);
        assert_eq!(l.reach(top + 8, 0), none, "an empty read inside the top");
        assert_eq!(l.reach(start - 8, 8), only_top);
        let both = LitReach {
            top: true,
            payload: true,
        };
        assert_eq!(l.reach(start - 1, 2), both);
        assert_eq!(l.reach(0, u32::MAX), both);
        let only_payload = LitReach {
            top: false,
            payload: true,
        };
        assert_eq!(l.reach(start, 8), only_payload);
        assert_eq!(l.reach(l.total_len - 1, 8), only_payload);
        assert_eq!(l.reach(l.total_len, 8), none, "at EOF");
    }

    #[test]
    fn a_wrong_size_payload_reads_black() {
        let l = LitLayout::new(18, 4);
        let bad = Bytes::from(vec![7u8; 10]);
        assert_eq!(
            l.read_lit(0, u32::MAX, None, Some(&bad)),
            l.read_black(0, u32::MAX)
        );
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
