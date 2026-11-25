//! How much memory the lit encode holds beyond its input (#440).
//!
//! A binary of its own, because measuring needs a counting global allocator
//! (`support/counting_alloc.rs`, shared with `lit_encode_timing`), which
//! this binary installs and no other test binary needs.

#[path = "support/counting_alloc.rs"]
mod counting_alloc;

use counting_alloc::{held_at_peak, CountingAllocator};
use image::RgbaImage;
use xearthlayer::night::generator::encode_visible;

#[global_allocator]
static COUNTING: CountingAllocator = CountingAllocator;

const MIB: usize = 1 << 20;

/// Size of the level encoded: the first visible level of a ZL16 lit texture
/// at the highest texture setting.
const SIZE: u32 = 1024;

/// The most `encode_visible` may hold beyond a 1024 px input. What it must
/// hold: level 1 of the chain in linear light, three `f32`s for each of its
/// 512 x 512 texels (3 MiB), kept for the whole chain and reused in place
/// for every level below it, and the payload it returns (699,064 bytes, the
/// whole chain in BC1), 3.67 MiB together. Every level's bytes are written
/// into the input's own buffer, and the 2 and 1 px levels' padded copies
/// are 64 bytes each. The rest is slack, so that a buffer of the size of
/// one level (1 MiB of level 1's bytes, or a copy of the input) fails the
/// test. The encoder before #440 held 24 MiB: a copy of the input (4 MiB),
/// its linear copy with four channels (16 MiB) and level 1 (4 MiB) at once.
const MAX_HELD_BEYOND_INPUT: usize = 4 * MIB;

/// A lit level that is not opaque black, so it takes the full path: sparse
/// warm lamps and white lines over black, from a hash of the position.
fn lit_level(size: u32) -> RgbaImage {
    RgbaImage::from_fn(size, size, |x, y| {
        let h =
            ((u64::from(y) << 32 | u64::from(x)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as u32;
        let [r, g, b] = if x.is_multiple_of(16) || y.is_multiple_of(16) {
            [228, 236, 255]
        } else if h.is_multiple_of(7) {
            [255, 150 + (h >> 8) as u8 % 70, 30 + (h >> 16) as u8 % 50]
        } else {
            [0; 3]
        };
        image::Rgba([r, g, b, 255])
    })
}

#[test]
fn encoding_a_1024_px_level_holds_at_most_4_mib_beyond_its_input() {
    // Process-wide tables (the decode table, the byte encoder) are built on
    // first use and kept: built here, before measuring.
    encode_visible(lit_level(64)).unwrap();

    let input = lit_level(SIZE);
    let (payload, held) = held_at_peak(|| encode_visible(input).unwrap());
    println!(
        "encode_visible of a {SIZE} px level held {held} bytes ({:.2} MiB) beyond its input",
        held as f64 / MIB as f64
    );
    assert_eq!(payload.len(), 699_064, "the whole chain in BC1");
    assert!(payload.iter().any(|&b| b != 0), "the full path, not black");
    assert!(
        held <= MAX_HELD_BEYOND_INPUT,
        "held {held} bytes, more than {MAX_HELD_BEYOND_INPUT}"
    );
}
