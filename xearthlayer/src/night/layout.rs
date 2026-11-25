//! The layout of lit DDS files: the minimal black DDS (phase 1), and
//! `LitLayout`, the full mip chain geometry of a lit footprint and the bytes
//! of its levels (phase 2a): the header, the black levels X-Plane drops, the
//! top level it uploads and the generated payload.
//!
//! What is lit, and which levels are blank, follows X-Plane's texture
//! setting and lives here; the DDS size arithmetic (header, level and file
//! lengths, chain levels) is [`crate::dds::layout`]'s.

use std::sync::OnceLock;

use bytes::Bytes;

use crate::dds::layout::{self, full_chain_levels};
use crate::dds::{DdsFormat, DdsHeader};

/// The lit format: lit DDS files are BC1.
const LIT_FORMAT: DdsFormat = DdsFormat::BC1;

/// First visible level of a ZL16 lit texture: about 7.5 m per texel.
pub const FIRST_VISIBLE_ZL16: u32 = 1024;

/// Largest top level a lit DDS declares. At the lowest texture setting the
/// uncapped top would be 32768 px, unverified in X-Plane; the cap shrinks the
/// first visible level instead (design: Lit DDS layout, Risk).
pub const MAX_TOP_LEVEL: u32 = 16384;

/// Zeros a blank read is served from (#434): a read wholly inside the
/// blank levels, at most this long, is a slice of [`ZEROS`] and allocates
/// nothing. A FUSE read is at most 1 MiB.
const STATIC_ZEROS_LEN: usize = 1 << 20;

/// All zeros, in the binary's zero-initialised data: no file bytes, and no
/// memory until a read touches it.
static ZEROS: [u8; STATIC_ZEROS_LEN] = [0; STATIC_ZEROS_LEN];

/// Whether `bytes` is a slice of [`ZEROS`].
#[cfg(test)]
fn is_static_zeros(bytes: &Bytes) -> bool {
    let zeros = ZEROS.as_ptr_range();
    let range = bytes.as_ptr_range();
    zeros.start <= range.start && range.end <= zeros.end
}

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
        let levels = full_chain_levels(top, top);
        let header = Bytes::from(DdsHeader::new(top, top, levels, LIT_FORMAT).to_bytes());
        let total_len = layout::file_len(top, top, levels, LIT_FORMAT) as u64;
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
    /// then zero blocks (black, adds nothing to the base colour). A read
    /// past the header of at most `STATIC_ZEROS_LEN` bytes is a slice of a
    /// static zero buffer, not a new one.
    pub fn read_black(&self, offset: u64, len: u32) -> Bytes {
        let (start, end) = self.clamp(offset, len);
        self.compose(start, end, None)
    }

    /// `[offset, offset + len)` clamped to the file.
    fn clamp(&self, offset: u64, len: u32) -> (u64, u64) {
        let start = offset.min(self.total_len);
        (
            start,
            start.saturating_add(u64::from(len)).min(self.total_len),
        )
    }

    /// Bytes `[start, end)` (within the file) of the header, zeros and,
    /// from `payload_start`, `payload` (the visible levels, of their full
    /// length) or zeros without one. Zeros alone come from the static
    /// buffer when they fit; anything else is one buffer, written once.
    fn compose(&self, start: u64, end: u64, payload: Option<&Bytes>) -> Bytes {
        let header_len = self.header.len() as u64;
        let payload_start = self.payload_start();
        let in_payload = payload.is_some() && end > payload_start;
        let n = (end - start) as usize;
        if start >= header_len && !in_payload && n <= STATIC_ZEROS_LEN {
            return Bytes::from_static(&ZEROS[..n]);
        }
        let mut out = vec![0u8; n];
        if start < header_len {
            let k = (header_len.min(end) - start) as usize;
            out[..k].copy_from_slice(&self.header[start as usize..start as usize + k]);
        }
        if let Some(payload) = payload.filter(|_| in_payload) {
            let from = payload_start.max(start);
            out[(from - start) as usize..].copy_from_slice(
                &payload[(from - payload_start) as usize..(end - payload_start) as usize],
            );
        }
        Bytes::from(out)
    }

    /// Bytes of the visible levels (first visible level down to 1 x 1).
    pub fn payload_len(&self) -> u64 {
        let size = self.first_visible;
        layout::chain_len(size, size, full_chain_levels(size, size), LIT_FORMAT) as u64
    }

    /// Where the visible levels start: after the header and the blank levels.
    pub fn payload_start(&self) -> u64 {
        self.total_len - self.payload_len()
    }

    /// Whether bytes `[offset, offset + len)` overlap the visible levels,
    /// the only generated part: everything before them is black.
    pub fn reaches_payload(&self, offset: u64, len: u32) -> bool {
        let (start, end) = self.clamp(offset, len);
        start < end && end > self.payload_start()
    }

    /// Bytes `[offset, offset + len)` of the lit texture: the header, zero
    /// levels above the visible ones (those the texture setting drops and the
    /// top X-Plane uploads, which its objects light), then `payload` (BC1
    /// visible levels). An absent payload, or one of the wrong length, reads
    /// black (same size, R7).
    pub fn read_lit(&self, offset: u64, len: u32, payload: Option<&Bytes>) -> Bytes {
        let Some(payload) = payload.filter(|p| p.len() as u64 == self.payload_len()) else {
            return self.read_black(offset, len);
        };
        let payload_start = self.payload_start();
        let (start, end) = self.clamp(offset, len);
        if start >= payload_start {
            // Entirely inside the payload: a refcount slice, no copy.
            return payload.slice((start - payload_start) as usize..(end - payload_start) as usize);
        }
        self.compose(start, end, Some(payload))
    }
}

/// The texture served wherever nothing is lit: 4 x 4 DXT1 with its full
/// chain (4, 2 and 1 px levels, one 8-byte block each), all blocks zero,
/// which decodes to black and adds nothing to the base colour. 152 bytes.
pub fn minimal_black_dds() -> Bytes {
    static DDS: OnceLock<Bytes> = OnceLock::new();
    DDS.get_or_init(|| {
        let levels = full_chain_levels(4, 4);
        let mut bytes = DdsHeader::new(4, 4, levels, LIT_FORMAT).to_bytes();
        bytes.resize(layout::file_len(4, 4, levels, LIT_FORMAT), 0);
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
            layout::file_len(1024, 1024, full_chain_levels(1024, 1024), DdsFormat::BC1) as u64
                - 128
        );
        assert_eq!(l.payload_start(), 128 + 2048 * 2048 / 2);
        assert_eq!(l.payload_start() + l.payload_len(), l.total_len);
    }

    /// `len` bytes that differ from one another: a hash of the offset.
    fn varied(len: u64) -> Bytes {
        (0..len)
            .map(|i| ((i ^ 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect::<Vec<u8>>()
            .into()
    }

    #[test]
    fn lit_reads_compose_header_zeros_and_payload() {
        let l = LitLayout::new(18, 3); // top 1024: 1024 dropped, 512 uploaded, 256 visible
        let payload = Bytes::from(vec![7u8; l.payload_len() as usize]);
        let read = |offset, len| l.read_lit(offset, len, Some(&payload));
        assert_eq!(&read(0, 4)[..], b"DDS ");
        let start = l.payload_start();
        assert_eq!(
            &read(start - 2, 4)[..],
            &[0, 0, 7, 7],
            "black, then payload"
        );
        assert!(read(l.total_len, 8).is_empty());
    }

    #[test]
    fn every_level_above_the_visible_ones_is_black() {
        // The levels X-Plane drops and the top it uploads, which X-Plane's
        // objects light.
        let l = LitLayout::new(16, 2); // 8192 and 4096 dropped, 2048 uploaded
        let payload = varied(l.payload_len());
        let file = l.read_lit(0, u32::MAX, Some(&payload));
        let above = &file[128..l.payload_start() as usize];
        assert_eq!(above.len(), (8192 * 8192 + 4096 * 4096 + 2048 * 2048) / 2);
        assert!(above.iter().all(|&b| b == 0));
        assert_eq!(&file[l.payload_start() as usize..], &payload[..]);
    }

    #[test]
    fn any_split_of_a_lit_read_gives_the_same_bytes() {
        // FUSE reads arrive in ranges of up to 1 MiB at any offset.
        let l = LitLayout::new(18, 3);
        let payload = varied(l.payload_len());
        let whole = l.read_lit(0, u32::MAX, Some(&payload));
        assert_eq!(whole.len() as u64, l.total_len);
        for step in [1 << 16, 999, 8 * 1000 + 3] {
            let pieces: Vec<u8> = (0..l.total_len)
                .step_by(step)
                .flat_map(|o| l.read_lit(o, step as u32, Some(&payload)).to_vec())
                .collect();
            assert_eq!(pieces, whole.to_vec(), "step {step}");
        }
    }

    #[test]
    fn a_read_reaches_the_payload_only_where_its_range_overlaps_it() {
        let l = LitLayout::new(16, 3);
        let start = l.payload_start();
        assert!(!l.reaches_payload(0, 128), "header");
        assert!(
            !l.reaches_payload(128, (start - 128) as u32),
            "black levels"
        );
        assert!(!l.reaches_payload(start - 8, 8));
        assert!(l.reaches_payload(start - 1, 2));
        assert!(l.reaches_payload(0, u32::MAX));
        assert!(l.reaches_payload(l.total_len - 1, 8));
        assert!(!l.reaches_payload(start + 8, 0), "an empty read");
        assert!(!l.reaches_payload(l.total_len, 8), "at EOF");
    }

    #[test]
    fn a_read_inside_the_blank_levels_allocates_no_buffer() {
        // The header, the blank levels and the payload at ZL16, setting 3.
        let l = LitLayout::new(16, 3);
        let payload = varied(l.payload_len());
        let start = l.payload_start();
        for (offset, len) in [
            (128, 1 << 20),
            (128 + 12_345, 999),
            (4096, 1),
            (start - (1 << 20), 1 << 20),
            (start - 8, 8),
        ] {
            for read in [
                l.read_black(offset, len),
                l.read_lit(offset, len, Some(&payload)),
                l.read_lit(offset, len, None),
            ] {
                assert_eq!(read.len(), len as usize, "{offset} + {len}");
                assert!(is_static_zeros(&read), "{offset} + {len} allocated");
            }
        }
        // Header bytes, the payload or more than a FUSE read: a buffer.
        for (offset, len) in [(0, 200), (127, 2), (start - 1, 2), (128, (1 << 20) + 1)] {
            assert!(!is_static_zeros(&l.read_lit(offset, len, Some(&payload))));
        }
    }

    #[test]
    fn a_missing_or_wrong_size_payload_reads_black() {
        let l = LitLayout::new(18, 4);
        let bad = Bytes::from(vec![7u8; 10]);
        for payload in [None, Some(&bad)] {
            assert_eq!(l.read_lit(0, u32::MAX, payload), l.read_black(0, u32::MAX));
        }
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
