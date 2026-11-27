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

/// Largest first visible level of a texture below ZL16 (#431). Fixed in
/// metres, the first visible level doubles per zoom below 16 until the top
/// cap holds it: 8192 px at ZL12 and ZL13, 64 times a ZL16 generation's work
/// and memory, for one texture. At 2048 the largest generation is 4 times a
/// ZL16 one and the top X-Plane uploads at the full setting is 4096 px, the
/// size of the ortho texture beneath it. The cost is density: ZL14, ZL13 and
/// ZL12 get a half, a quarter and an eighth of ZL16's, and since the level
/// X-Plane switches at follows the uploaded top's texel size, their lit layer
/// starts farther from the aircraft by the same factor. ZL15 (2048) and
/// every zoom from 16 up are below it at every setting, so unchanged.
pub const MAX_FIRST_VISIBLE_BELOW_ZL16: u32 = 2048;

/// Most road raster samples per output texel side (#431): a capped texture
/// has its roads rastered at up to this many times its linear resolution.
/// 2 gives ZL14 exactly ZL16's road density, and ZL13 and ZL12 a 4096 px
/// raster for their 2048 px texture.
pub const MAX_ROAD_SUPERSAMPLE: u32 = 2;

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
    /// See [`LitLayout::road_supersample`].
    road_supersample: u32,
    header: Bytes,
}

impl LitLayout {
    /// The layout for a texture at `zoom` under X-Plane texture setting `t` (R15).
    /// Below ZL16 the first visible level is at most
    /// [`MAX_FIRST_VISIBLE_BELOW_ZL16`] (#431).
    pub fn new(zoom: u8, texture_setting: u8) -> Self {
        let t = u32::from(texture_setting.min(4));
        let blank_levels = (4 - t) + 1;
        // The first visible level fixed in metres, and the one served.
        let (fixed_first, wanted_first) = if zoom >= 16 {
            let fixed = (FIRST_VISIBLE_ZL16 >> (zoom - 16)).max(4);
            (fixed, fixed)
        } else {
            let fixed = (FIRST_VISIBLE_ZL16 << (16 - zoom)).min(MAX_TOP_LEVEL);
            (fixed, fixed.min(MAX_FIRST_VISIBLE_BELOW_ZL16))
        };
        let (top, first_visible) = top_and_first_visible(wanted_first, blank_levels);
        let (_, uncapped_first) = top_and_first_visible(fixed_first, blank_levels);
        let road_supersample = (uncapped_first / first_visible).min(MAX_ROAD_SUPERSAMPLE);
        let levels = full_chain_levels(top, top);
        let header = Bytes::from(DdsHeader::new(top, top, levels, LIT_FORMAT).to_bytes());
        let total_len = layout::file_len(top, top, levels, LIT_FORMAT) as u64;
        Self {
            top,
            levels,
            blank_levels,
            first_visible,
            total_len,
            road_supersample,
            header,
        }
    }

    /// Road raster samples per side of a first visible texel (#431): the
    /// first visible level the density fixed in metres asks for over the one
    /// served, at most [`MAX_ROAD_SUPERSAMPLE`]. 1 wherever the cap below
    /// ZL16 changed nothing, so every uncapped texture, ZL15 and up at every
    /// setting, rasters its roads as before. A road's glow is never narrower
    /// than a texel, so on a capped texture lamps and roads would widen into
    /// thick lines; rastered at the uncapped density and averaged down, they
    /// keep their width on the ground.
    pub fn road_supersample(&self) -> u32 {
        self.road_supersample
    }

    /// Bytes `[offset, offset + len)` of the all-black texture: the header,
    /// then zero blocks (black, adds nothing to the base colour). A read
    /// past the header of at most `STATIC_ZEROS_LEN` bytes is a slice of a
    /// static zero buffer, not a new one.
    pub fn read_black(&self, offset: u64, len: u32) -> Bytes {
        let (start, end) = self.clamp(offset, len);
        self.compose(start, end, &LitParts::default())
    }

    /// `[offset, offset + len)` clamped to the file.
    fn clamp(&self, offset: u64, len: u32) -> (u64, u64) {
        let start = offset.min(self.total_len);
        (
            start,
            start.saturating_add(u64::from(len)).min(self.total_len),
        )
    }

    /// Bytes `[start, end)` (within the file) of the header, zeros and the
    /// present `parts` of the visible levels, each at its place (zeros where
    /// a part is absent). Zeros alone come from the static buffer when they
    /// fit; anything else is one buffer, written once.
    fn compose(&self, start: u64, end: u64, parts: &LitParts) -> Bytes {
        let header_len = self.header.len() as u64;
        let overlapped: Vec<_> = PayloadPart::ALL
            .into_iter()
            .filter_map(|part| {
                let range = self.part_range(part);
                let bytes = parts.get(part)?;
                (start < range.end && end > range.start).then_some((range, bytes))
            })
            .collect();
        let n = (end - start) as usize;
        if start >= header_len && overlapped.is_empty() && n <= STATIC_ZEROS_LEN {
            return Bytes::from_static(&ZEROS[..n]);
        }
        let mut out = vec![0u8; n];
        if start < header_len {
            let k = (header_len.min(end) - start) as usize;
            out[..k].copy_from_slice(&self.header[start as usize..start as usize + k]);
        }
        for (range, bytes) in overlapped {
            let (from, to) = (range.start.max(start), range.end.min(end));
            out[(from - start) as usize..(to - start) as usize].copy_from_slice(
                &bytes[(from - range.start) as usize..(to - range.start) as usize],
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

    /// Bytes of `part` of the visible levels (#432): the first visible level
    /// for the head, every level below it for the tail.
    pub fn part_len(&self, part: PayloadPart) -> u64 {
        let size = self.first_visible;
        let head = layout::level_len(size, size, LIT_FORMAT) as u64;
        match part {
            PayloadPart::Head => head,
            PayloadPart::Tail => self.payload_len() - head,
        }
    }

    /// Where `part` lies in the file: the head from the start of the
    /// visible levels, the tail after it to the end of the file.
    pub fn part_range(&self, part: PayloadPart) -> std::ops::Range<u64> {
        let tail_start = self.payload_start() + self.part_len(PayloadPart::Head);
        match part {
            PayloadPart::Head => self.payload_start()..tail_start,
            PayloadPart::Tail => tail_start..self.total_len,
        }
    }

    /// `part` of `payload` (the visible levels, of their full length), a
    /// slice of it: sharing its allocation, so not for keeping apart.
    pub fn part_of(&self, payload: &Bytes, part: PayloadPart) -> Bytes {
        let at = self.payload_start();
        let range = self.part_range(part);
        payload.slice((range.start - at) as usize..(range.end - at) as usize)
    }

    /// Both parts of `payload` as slices of it (see [`Self::part_of`]), or
    /// none when it is not of the payload's length (it then reads black).
    pub fn parts_of(&self, payload: &Bytes) -> LitParts {
        let mut parts = LitParts::default();
        if payload.len() as u64 == self.payload_len() {
            for part in PayloadPart::ALL {
                parts.set(part, self.part_of(payload, part));
            }
        }
        parts
    }

    /// Whether bytes `[offset, offset + len)` overlap the visible levels,
    /// the only generated part: everything before them is black.
    pub fn reaches_payload(&self, offset: u64, len: u32) -> bool {
        PayloadPart::ALL
            .into_iter()
            .any(|part| self.reaches(part, offset, len))
    }

    /// Whether bytes `[offset, offset + len)` overlap `part`.
    pub fn reaches(&self, part: PayloadPart, offset: u64, len: u32) -> bool {
        let (start, end) = self.clamp(offset, len);
        let range = self.part_range(part);
        start < end && start < range.end && end > range.start
    }

    /// Bytes `[offset, offset + len)` of the lit texture: the header, zero
    /// levels above the visible ones (those the texture setting drops and the
    /// top X-Plane uploads, which its objects light), then the visible
    /// levels from `parts` (BC1), each part from its own buffer (#432). A
    /// part absent, or of the wrong length, reads black (same size, R7), so
    /// any split of a read gives the bytes of one read, whatever the parts.
    pub fn read_lit(&self, offset: u64, len: u32, parts: &LitParts) -> Bytes {
        let parts = parts.checked(self);
        let (start, end) = self.clamp(offset, len);
        for part in PayloadPart::ALL {
            let range = self.part_range(part);
            if let Some(bytes) = parts
                .get(part)
                .filter(|_| start >= range.start && end <= range.end)
            {
                // Entirely inside one part: a refcount slice, no copy.
                return bytes.slice((start - range.start) as usize..(end - range.start) as usize);
            }
        }
        self.compose(start, end, &parts)
    }
}

/// The two parts of a lit payload, kept apart in memory (#432): the first
/// visible level (the head, 512 KiB at ZL16) and every level below it (the
/// tail, 170.7 KiB at ZL16). X-Plane reads the small levels first, and most
/// textures never read their first visible level, so the tails of a whole
/// working set fit where its whole payloads did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadPart {
    Head,
    Tail,
}

impl PayloadPart {
    /// Both, in file order.
    pub const ALL: [Self; 2] = [Self::Head, Self::Tail];
}

/// The parts of a lit payload a read is served from, each present or not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LitParts {
    head: Option<Bytes>,
    tail: Option<Bytes>,
}

impl LitParts {
    pub fn get(&self, part: PayloadPart) -> Option<&Bytes> {
        match part {
            PayloadPart::Head => self.head.as_ref(),
            PayloadPart::Tail => self.tail.as_ref(),
        }
    }

    pub fn set(&mut self, part: PayloadPart, bytes: Bytes) {
        *self.slot(part) = Some(bytes);
    }

    fn slot(&mut self, part: PayloadPart) -> &mut Option<Bytes> {
        match part {
            PayloadPart::Head => &mut self.head,
            PayloadPart::Tail => &mut self.tail,
        }
    }

    /// Whether every part is absent.
    pub fn is_empty(&self) -> bool {
        self.head.is_none() && self.tail.is_none()
    }

    /// These parts with any of the wrong length for `layout` left out.
    fn checked(&self, layout: &LitLayout) -> Self {
        let mut out = self.clone();
        for part in PayloadPart::ALL {
            let slot = out.slot(part);
            if slot
                .as_ref()
                .is_some_and(|b| b.len() as u64 != layout.part_len(part))
            {
                *slot = None;
            }
        }
        out
    }
}

/// The top level and the first visible level for a wanted first visible
/// level `wanted` under `blank_levels` blank levels: the top is held to
/// [`MAX_TOP_LEVEL`], which shrinks the first visible level instead.
fn top_and_first_visible(wanted: u32, blank_levels: u32) -> (u32, u32) {
    let top = (wanted << blank_levels).min(MAX_TOP_LEVEL);
    (top, top >> blank_levels)
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
        let parts = l.parts_of(&payload);
        let read = |offset, len| l.read_lit(offset, len, &parts);
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
        let file = l.read_lit(0, u32::MAX, &l.parts_of(&payload));
        let above = &file[128..l.payload_start() as usize];
        assert_eq!(above.len(), (8192 * 8192 + 4096 * 4096 + 2048 * 2048) / 2);
        assert!(above.iter().all(|&b| b == 0));
        assert_eq!(&file[l.payload_start() as usize..], &payload[..]);
    }

    /// `payload`'s parts copied into allocations of their own, as the
    /// payload memory keeps them, with only those of `keep`.
    fn owned_parts(l: &LitLayout, payload: &Bytes, keep: &[PayloadPart]) -> LitParts {
        let mut parts = LitParts::default();
        for &part in keep {
            parts.set(part, Bytes::copy_from_slice(&l.part_of(payload, part)));
        }
        parts
    }

    #[test]
    fn any_split_of_a_lit_read_gives_the_same_bytes() {
        // FUSE reads arrive in ranges of up to 1 MiB at any offset, and since
        // #432 the visible levels come from two buffers: whatever the split,
        // across the part boundary too, the bytes are one read's.
        let l = LitLayout::new(18, 3);
        let payload = varied(l.payload_len());
        let whole = l.read_lit(0, u32::MAX, &l.parts_of(&payload));
        assert_eq!(whole.len() as u64, l.total_len);
        let both = owned_parts(&l, &payload, &PayloadPart::ALL);
        assert_eq!(
            l.read_lit(0, u32::MAX, &both),
            whole,
            "two buffers, one file"
        );
        let boundary = l.part_range(PayloadPart::Tail).start;
        let read_from = |parts: &LitParts, offsets: &[u64]| -> Vec<u8> {
            offsets
                .windows(2)
                .flat_map(|w| l.read_lit(w[0], (w[1] - w[0]) as u32, parts).to_vec())
                .collect()
        };
        for parts in [l.parts_of(&payload), both.clone()] {
            for step in [1 << 16, 999, 8 * 1000 + 3] {
                let mut offsets: Vec<u64> = (0..l.total_len).step_by(step).collect();
                offsets.push(l.total_len);
                assert_eq!(read_from(&parts, &offsets), whole.to_vec(), "step {step}");
            }
            for cut in [
                boundary - 9,
                boundary - 1,
                boundary,
                boundary + 1,
                boundary + 8,
            ] {
                let offsets = [0, l.payload_start() - 3, cut, l.total_len];
                assert_eq!(read_from(&parts, &offsets), whole.to_vec(), "cut at {cut}");
            }
        }
        // With one part absent, it reads black, and a split still gives one
        // read's bytes.
        for keep in [PayloadPart::Head, PayloadPart::Tail] {
            let parts = owned_parts(&l, &payload, &[keep]);
            let one = l.read_lit(0, u32::MAX, &parts);
            let offsets = [0, boundary - 5, boundary + 5, l.total_len];
            assert_eq!(read_from(&parts, &offsets), one.to_vec(), "{keep:?} alone");
            let kept = l.part_range(keep);
            assert_eq!(
                &one[kept.start as usize..kept.end as usize],
                &whole[kept.start as usize..kept.end as usize]
            );
            let other = l.part_range(if keep == PayloadPart::Head {
                PayloadPart::Tail
            } else {
                PayloadPart::Head
            });
            assert!(one[other.start as usize..other.end as usize]
                .iter()
                .all(|&b| b == 0));
        }
    }

    #[test]
    fn the_parts_are_the_first_visible_level_and_the_levels_below_it() {
        let l = LitLayout::new(16, 4); // first visible 1024
        assert_eq!(l.part_len(PayloadPart::Head), 524_288, "1024 x 1024 BC1");
        assert_eq!(
            l.part_len(PayloadPart::Tail),
            174_776,
            "512 px down to 1 x 1"
        );
        assert_eq!(
            l.part_len(PayloadPart::Head) + l.part_len(PayloadPart::Tail),
            l.payload_len()
        );
        assert_eq!(l.part_range(PayloadPart::Head).start, l.payload_start());
        assert_eq!(
            l.part_range(PayloadPart::Head).end,
            l.part_range(PayloadPart::Tail).start
        );
        assert_eq!(l.part_range(PayloadPart::Tail).end, l.total_len);
        for zoom in 12..=22 {
            for t in 0..=4u8 {
                let l = LitLayout::new(zoom, t);
                assert!(l.part_len(PayloadPart::Tail) > 0, "ZL{zoom} t = {t}");
                assert_eq!(
                    l.part_len(PayloadPart::Head) + l.part_len(PayloadPart::Tail),
                    l.payload_len(),
                    "ZL{zoom} t = {t}"
                );
            }
        }
    }

    #[test]
    fn a_read_reaches_a_part_only_where_its_range_overlaps_it() {
        let l = LitLayout::new(16, 4);
        let (head, tail) = (PayloadPart::Head, PayloadPart::Tail);
        let boundary = l.part_range(tail).start;
        let reached = |offset, len| (l.reaches(head, offset, len), l.reaches(tail, offset, len));
        assert_eq!(reached(0, 128), (false, false), "header");
        assert_eq!(reached(l.payload_start() - 8, 8), (false, false), "the top");
        assert_eq!(reached(l.payload_start() - 8, 9), (true, false));
        assert_eq!(reached(boundary - 8, 8), (true, false));
        assert_eq!(reached(boundary - 1, 2), (true, true));
        assert_eq!(reached(boundary, 8), (false, true));
        assert_eq!(reached(l.total_len - 8, 1 << 20), (false, true));
        assert_eq!(reached(0, u32::MAX), (true, true));
        assert_eq!(reached(boundary, 0), (false, false), "an empty read");
    }

    #[test]
    fn a_read_inside_one_part_is_a_slice_of_it() {
        let l = LitLayout::new(16, 4);
        let payload = varied(l.payload_len());
        let parts = owned_parts(&l, &payload, &PayloadPart::ALL);
        for part in PayloadPart::ALL {
            let range = l.part_range(part);
            let read = l.read_lit(range.start + 8, 4096, &parts);
            let buffer = parts.get(part).unwrap().as_ptr_range();
            assert!(buffer.contains(&read.as_ptr()), "{part:?}: no copy");
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
        let parts = l.parts_of(&payload);
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
                l.read_lit(offset, len, &parts),
                l.read_lit(offset, len, &LitParts::default()),
            ] {
                assert_eq!(read.len(), len as usize, "{offset} + {len}");
                assert!(is_static_zeros(&read), "{offset} + {len} allocated");
            }
        }
        // Header bytes, the payload or more than a FUSE read: a buffer.
        for (offset, len) in [(0, 200), (127, 2), (start - 1, 2), (128, (1 << 20) + 1)] {
            assert!(!is_static_zeros(&l.read_lit(offset, len, &parts)));
        }
    }

    #[test]
    fn a_missing_or_wrong_size_payload_reads_black() {
        let l = LitLayout::new(18, 4);
        let bad = Bytes::from(vec![7u8; 10]);
        let mut bad_parts = LitParts::default();
        bad_parts.set(PayloadPart::Head, bad.clone());
        bad_parts.set(PayloadPart::Tail, bad.clone());
        for parts in [LitParts::default(), l.parts_of(&bad), bad_parts] {
            assert_eq!(l.read_lit(0, u32::MAX, &parts), l.read_black(0, u32::MAX));
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
        // 2026-10-18), so the first visible level is fixed in metres. The
        // cap below ZL16 holds a zoom's first visible level to one size at
        // every setting, so capped zooms keep the rule too.
        for zoom in 12..=22 {
            let full = LitLayout::new(zoom, 4);
            for t in 1..=3u8 {
                let l = LitLayout::new(zoom, t);
                if l.top == MAX_TOP_LEVEL {
                    continue; // the top cap trades visible resolution instead
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
        for zoom in 12..=22 {
            for t in 0..=4u8 {
                assert!(
                    LitLayout::new(zoom, t).top <= MAX_TOP_LEVEL,
                    "ZL{zoom} t = {t}"
                );
            }
        }
    }

    /// `(top, first_visible, total_len, payload_len())` of `LitLayout::new(zoom, t)`.
    fn shape(zoom: u8, t: u8) -> (u32, u32, u64, u64) {
        let l = LitLayout::new(zoom, t);
        (l.top, l.first_visible, l.total_len, l.payload_len())
    }

    #[test]
    fn every_layout_at_zl16_and_above_is_as_before_the_cap() {
        // Taken before the cap below ZL16 (#431): every served size and
        // payload length at ZL16 and above, at every texture setting.
        #[rustfmt::skip]
        let before: [(u8, u8, u32, u32, u64, u64); 35] = [
            (16, 0, 16384, 512, 178_957_112, 174_776),
            (16, 1, 16384, 1024, 178_957_112, 699_064),
            (16, 2, 8192, 1024, 44_739_384, 699_064),
            (16, 3, 4096, 1024, 11_184_952, 699_064),
            (16, 4, 2048, 1024, 2_796_344, 699_064),
            (17, 0, 16384, 512, 178_957_112, 174_776),
            (17, 1, 8192, 512, 44_739_384, 174_776),
            (17, 2, 4096, 512, 11_184_952, 174_776),
            (17, 3, 2048, 512, 2_796_344, 174_776),
            (17, 4, 1024, 512, 699_192, 174_776),
            (18, 0, 8192, 256, 44_739_384, 43_704),
            (18, 1, 4096, 256, 11_184_952, 43_704),
            (18, 2, 2048, 256, 2_796_344, 43_704),
            (18, 3, 1024, 256, 699_192, 43_704),
            (18, 4, 512, 256, 174_904, 43_704),
            (19, 0, 4096, 128, 11_184_952, 10_936),
            (19, 1, 2048, 128, 2_796_344, 10_936),
            (19, 2, 1024, 128, 699_192, 10_936),
            (19, 3, 512, 128, 174_904, 10_936),
            (19, 4, 256, 128, 43_832, 10_936),
            (20, 0, 2048, 64, 2_796_344, 2_744),
            (20, 1, 1024, 64, 699_192, 2_744),
            (20, 2, 512, 64, 174_904, 2_744),
            (20, 3, 256, 64, 43_832, 2_744),
            (20, 4, 128, 64, 11_064, 2_744),
            (21, 0, 1024, 32, 699_192, 696),
            (21, 1, 512, 32, 174_904, 696),
            (21, 2, 256, 32, 43_832, 696),
            (21, 3, 128, 32, 11_064, 696),
            (21, 4, 64, 32, 2_872, 696),
            (22, 0, 512, 16, 174_904, 184),
            (22, 1, 256, 16, 43_832, 184),
            (22, 2, 128, 16, 11_064, 184),
            (22, 3, 64, 16, 2_872, 184),
            (22, 4, 32, 16, 824, 184),
        ];
        for (zoom, t, top, first, total, payload) in before {
            assert_eq!(
                shape(zoom, t),
                (top, first, total, payload),
                "ZL{zoom} t = {t}"
            );
        }
    }

    #[test]
    fn below_zl16_the_first_visible_level_is_capped_at_2048() {
        // Before the cap, ZL12 and ZL13 were 8192 at t = 4 and 4096 at
        // t = 3, and ZL14 4096 at both: up to 64 times a ZL16 generation.
        // At t <= 2 the top cap already held them to 2048, and ZL15 is
        // 2048 at every setting: those are unchanged.
        #[rustfmt::skip]
        let after: [(u8, u8, u32, u32, u64, u64); 20] = [
            (12, 0, 16384, 512, 178_957_112, 174_776),
            (12, 1, 16384, 1024, 178_957_112, 699_064),
            (12, 2, 16384, 2048, 178_957_112, 2_796_216),
            (12, 3, 8192, 2048, 44_739_384, 2_796_216),
            (12, 4, 4096, 2048, 11_184_952, 2_796_216),
            (13, 0, 16384, 512, 178_957_112, 174_776),
            (13, 1, 16384, 1024, 178_957_112, 699_064),
            (13, 2, 16384, 2048, 178_957_112, 2_796_216),
            (13, 3, 8192, 2048, 44_739_384, 2_796_216),
            (13, 4, 4096, 2048, 11_184_952, 2_796_216),
            (14, 0, 16384, 512, 178_957_112, 174_776),
            (14, 1, 16384, 1024, 178_957_112, 699_064),
            (14, 2, 16384, 2048, 178_957_112, 2_796_216),
            (14, 3, 8192, 2048, 44_739_384, 2_796_216),
            (14, 4, 4096, 2048, 11_184_952, 2_796_216),
            (15, 0, 16384, 512, 178_957_112, 174_776),
            (15, 1, 16384, 1024, 178_957_112, 699_064),
            (15, 2, 16384, 2048, 178_957_112, 2_796_216),
            (15, 3, 8192, 2048, 44_739_384, 2_796_216),
            (15, 4, 4096, 2048, 11_184_952, 2_796_216),
        ];
        for (zoom, t, top, first, total, payload) in after {
            assert_eq!(
                shape(zoom, t),
                (top, first, total, payload),
                "ZL{zoom} t = {t}"
            );
            assert!(first <= MAX_FIRST_VISIBLE_BELOW_ZL16, "ZL{zoom} t = {t}");
        }
    }

    #[test]
    fn roads_are_supersampled_only_where_the_cap_lowered_the_density() {
        // The first visible level the density fixed in metres asks for,
        // over the capped one, at most 2: ZL12 to ZL14 at t = 3 and 4.
        for zoom in 12..=22u8 {
            for t in 0..=4u8 {
                let want = if zoom <= 14 && t >= 3 { 2 } else { 1 };
                assert_eq!(
                    LitLayout::new(zoom, t).road_supersample(),
                    want,
                    "ZL{zoom} t = {t}"
                );
            }
        }
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
