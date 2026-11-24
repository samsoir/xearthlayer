//! DDS file geometry: how many bytes a DDS file, its header and each of its
//! mip levels take (#391).
//!
//! Not to be confused with `cache::layout`, which says where the cache tiers
//! live on disk. This module says nothing about where a file is stored; it is
//! the one owner of the arithmetic of the DDS format itself, and every site
//! that sizes a DDS (the header, the compressors, the GPU worker, FUSE
//! `st_size`, the FUSE read gate and DDS disk validator through
//! `EXPECTED_DDS_SIZE`, the lit layout, the BC1 repair pass, the magenta
//! placeholder) takes its numbers from here.
//!
//! A DDS file written by XEarthLayer is [`HEADER_LEN`] bytes of header (the
//! 4-byte magic and the 124-byte DX9 header, no DX10 extension) followed by
//! its levels, largest first. Level `i` of a `width` x `height` texture is
//! `max(width >> i, 1)` x `max(height >> i, 1)` texels, stored as whole
//! 4 x 4 blocks of [`DdsFormat::block_bytes`] each, so the 2 x 2 and 1 x 1
//! levels at the tail of a chain still take one block.
//!
//! Every function is `const` and pure, so constants such as
//! `EXPECTED_DDS_SIZE` are computed at compile time from the same code that
//! sizes files at run time.

use super::types::DdsFormat;

/// Bytes before the first level: the `"DDS "` magic and the 124-byte header.
pub const HEADER_LEN: usize = 128;

/// Edge of a compression block, in texels. Every block-compressed format
/// XEarthLayer can name (BC1, BC3) uses 4 x 4 blocks.
pub const BLOCK_DIM: u32 = 4;

impl DdsFormat {
    /// Bytes of one 4 x 4 block in this format: 8 for BC1, 16 for BC3.
    ///
    /// A format fact, true whether or not the format is supported: BC3 is
    /// named but rejected by every encoder (#395).
    pub const fn block_bytes(self) -> usize {
        match self {
            DdsFormat::BC1 => 8,
            DdsFormat::BC3 => 16,
        }
    }
}

/// Bytes of one level of `width` x `height` texels: whole blocks, rounded up
/// on each axis.
pub const fn level_len(width: u32, height: u32, format: DdsFormat) -> usize {
    width.div_ceil(BLOCK_DIM) as usize * height.div_ceil(BLOCK_DIM) as usize * format.block_bytes()
}

/// Levels in a full chain for a `width` x `height` texture: halving both
/// dimensions until either reaches 1, so `floor(log2(min(width, height))) + 1`
/// (13 for 4096 x 4096). A zero dimension counts one level.
///
/// The chain stops at the smaller dimension, as the encoders'
/// `MipmapStream` does, which for a square texture is the chain down to
/// 1 x 1.
pub const fn full_chain_levels(width: u32, height: u32) -> u32 {
    let smaller = if width < height { width } else { height };
    if smaller == 0 {
        1
    } else {
        smaller.ilog2() + 1
    }
}

/// Levels written when `requested` levels are asked for: the full chain for
/// `None`, else the request clamped to `1..=full_chain_levels(width, height)`.
///
/// A file must never declare more levels than its dimensions allow: the
/// encoder would stop at the last level while the header still declared the
/// request. Both the encoder ([`DdsEncoder`](super::DdsEncoder)) and the size
/// it reports for FUSE `st_size` (`DdsTextureEncoder::expected_size`) resolve
/// their level count here, so the two cannot disagree.
pub const fn chain_levels(width: u32, height: u32, requested: Option<u32>) -> u32 {
    let full = full_chain_levels(width, height);
    match requested {
        None => full,
        Some(0) => 1,
        Some(n) if n > full => full,
        Some(n) => n,
    }
}

/// Bytes of the first `levels` levels of a `width` x `height` chain, without
/// the header.
///
/// Each level halves the one above, never below 1 texel on either axis.
/// `levels` is taken as given: pass at most
/// [`full_chain_levels`]`(width, height)`, or [`chain_levels`] for a request.
pub const fn chain_len(width: u32, height: u32, levels: u32, format: DdsFormat) -> usize {
    let (mut w, mut h) = (width, height);
    let mut total = 0;
    let mut level = 0;
    while level < levels {
        total += level_len(w, h, format);
        w = if w > 1 { w / 2 } else { 1 };
        h = if h > 1 { h / 2 } else { 1 };
        level += 1;
    }
    total
}

/// Bytes of a whole DDS file: [`HEADER_LEN`] plus [`chain_len`].
pub const fn file_len(width: u32, height: u32, levels: u32, format: DdsFormat) -> usize {
    HEADER_LEN + chain_len(width, height, levels, format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsHeader;

    #[test]
    fn the_header_is_magic_plus_124_bytes() {
        assert_eq!(HEADER_LEN, 4 + 124);
        assert_eq!(std::mem::size_of::<DdsHeader>(), HEADER_LEN);
        assert_eq!(
            DdsHeader::new(4096, 4096, 13, DdsFormat::BC1)
                .to_bytes()
                .len(),
            HEADER_LEN
        );
    }

    #[test]
    fn block_bytes_are_format_facts() {
        assert_eq!(DdsFormat::BC1.block_bytes(), 8);
        assert_eq!(DdsFormat::BC3.block_bytes(), 16);
    }

    #[test]
    fn a_level_is_whole_blocks() {
        assert_eq!(level_len(4, 4, DdsFormat::BC1), 8);
        assert_eq!(level_len(256, 256, DdsFormat::BC1), 32_768);
        assert_eq!(level_len(4096, 4096, DdsFormat::BC1), 8_388_608);
        assert_eq!(level_len(4, 4, DdsFormat::BC3), 16);
        assert_eq!(level_len(256, 256, DdsFormat::BC3), 65_536);
        // Below a block, and not a multiple of 4: rounded up per axis.
        assert_eq!(level_len(2, 2, DdsFormat::BC1), 8);
        assert_eq!(level_len(1, 1, DdsFormat::BC1), 8);
        assert_eq!(level_len(100, 100, DdsFormat::BC1), 25 * 25 * 8);
        assert_eq!(level_len(8, 2, DdsFormat::BC1), 2 * 8);
    }

    /// The halving loop every copy of the chain count used before #391.
    fn halving_count(mut w: u32, mut h: u32) -> u32 {
        let mut count = 1;
        while w > 1 && h > 1 {
            w /= 2;
            h /= 2;
            count += 1;
        }
        count
    }

    #[test]
    fn full_chain_levels_match_the_halving_loop() {
        assert_eq!(full_chain_levels(4096, 4096), 13);
        assert_eq!(full_chain_levels(256, 256), 9);
        assert_eq!(full_chain_levels(4, 4), 3);
        assert_eq!(full_chain_levels(1, 1), 1);
        assert_eq!(full_chain_levels(0, 0), 1);
        for w in 0..=70 {
            for h in 0..=70 {
                assert_eq!(full_chain_levels(w, h), halving_count(w, h), "{w}x{h}");
            }
        }
        for size in [1024, 2048, 4096, 8192, 16384, 32768] {
            assert_eq!(full_chain_levels(size, size), halving_count(size, size));
        }
    }

    #[test]
    fn a_request_is_clamped_to_the_chain() {
        assert_eq!(chain_levels(4096, 4096, None), 13);
        assert_eq!(chain_levels(4096, 4096, Some(5)), 5);
        assert_eq!(chain_levels(64, 64, Some(99)), 7);
        assert_eq!(chain_levels(64, 64, Some(0)), 1);
    }

    #[test]
    fn chains_sum_their_levels() {
        // 5 levels of 4096: 4096, 2048, 1024, 512, 256.
        assert_eq!(chain_len(4096, 4096, 5, DdsFormat::BC1), 11_173_888);
        assert_eq!(chain_len(4096, 4096, 5, DdsFormat::BC3), 22_347_776);
        // 256 down to 16.
        assert_eq!(
            chain_len(256, 256, 5, DdsFormat::BC1),
            32_768 + 8_192 + 2_048 + 512 + 128
        );
        // One level: the level alone.
        assert_eq!(chain_len(4, 4, 1, DdsFormat::BC1), 8);
        assert_eq!(chain_len(256, 256, 1, DdsFormat::BC1), 32_768);
        assert_eq!(chain_len(4, 4, 1, DdsFormat::BC3), 16);
        assert_eq!(chain_len(256, 256, 1, DdsFormat::BC3), 65_536);
        // 4 x 4, 2 x 2 and 1 x 1: one block each.
        assert_eq!(chain_len(4, 4, 3, DdsFormat::BC1), 3 * 8);
        assert_eq!(chain_len(4096, 4096, 0, DdsFormat::BC1), 0);
    }

    #[test]
    fn a_full_4096_bc1_tile_is_11_184_952_bytes() {
        assert_eq!(
            file_len(4096, 4096, full_chain_levels(4096, 4096), DdsFormat::BC1),
            11_184_952
        );
        // The retired format's size, as a format fact (#395).
        assert_eq!(
            file_len(4096, 4096, full_chain_levels(4096, 4096), DdsFormat::BC3),
            22_369_776
        );
    }

    #[test]
    fn a_4x4_bc1_full_chain_is_152_bytes() {
        assert_eq!(file_len(4, 4, full_chain_levels(4, 4), DdsFormat::BC1), 152);
    }

    #[test]
    fn non_square_chains_stop_at_the_smaller_dimension() {
        // 64 x 16, 32 x 8, 16 x 4, 8 x 2, 4 x 1.
        assert_eq!(full_chain_levels(64, 16), 5);
        assert_eq!(
            chain_len(64, 16, 5, DdsFormat::BC1),
            (16 * 4 + 8 * 2 + 4 + 2 + 1) * 8
        );
    }

    #[test]
    fn sizes_are_available_at_compile_time() {
        const TILE: usize = file_len(4096, 4096, full_chain_levels(4096, 4096), DdsFormat::BC1);
        assert_eq!(TILE, 11_184_952);
    }
}
