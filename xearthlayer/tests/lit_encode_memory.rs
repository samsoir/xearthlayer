//! How much memory the lit encode holds beyond its input (#440).
//!
//! A binary of its own, because measuring needs a counting global allocator
//! (`support/counting_alloc.rs`, shared with `lit_encode_timing`), which
//! this binary installs and no other test binary needs.

#[path = "support/counting_alloc.rs"]
mod counting_alloc;
#[path = "support/lit_encode_before.rs"]
mod lit_encode_before;

use counting_alloc::{held_at_peak, CountingAllocator};
use image::RgbaImage;
use lit_encode_before::encode_visible_before;
use xearthlayer::night::generator::encode_visible;

#[global_allocator]
static COUNTING: CountingAllocator = CountingAllocator;

const MIB: usize = 1 << 20;
const KIB: usize = 1 << 10;

/// Size of the level encoded: the first visible level of a ZL16 lit texture
/// at the highest texture setting.
const SIZE: u32 = 1024;

/// Bytes of the payload: the whole chain of a 1024 px level in BC1.
const PAYLOAD: usize = 699_064;

/// What `encode_visible` must hold beyond a 1024 px input: level 1 of the
/// chain in linear light, three `f32`s for each of its 512 x 512 texels
/// (3 MiB), kept for the whole chain and reused in place for every level
/// below it, and the payload it returns. Every level's bytes are written
/// into the input's own buffer.
const MUST_HOLD: usize = 512 * 512 * 3 * 4 + PAYLOAD;

/// The most `encode_visible` may hold beyond a 1024 px input: [`MUST_HOLD`]
/// (3,844,792 bytes) and 16 KiB. Measured on Linux x86_64, it held 64 bytes
/// more, a padded copy of the 2 or 1 px level for its kernel; 16 KiB leaves
/// room for an allocator or a platform that rounds differently, and is a
/// quarter of the smallest level copy that matters (level 3's bytes, 64
/// KiB), so any copy of a level of 128 px or more fails the test.
const MAX_HELD_BEYOND_INPUT: usize = MUST_HOLD + 16 * KIB;

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

/// Builds the process-wide tables (the decode table, the byte encoder) the
/// encode builds on first use and keeps, so a measurement does not count
/// them.
fn warm_up() {
    encode_visible(lit_level(64)).unwrap();
}

/// The payload of `encode_visible` of a 1024 px lit level, and the bytes it
/// held beyond its input.
fn measure_encode_visible() -> (bytes::Bytes, usize) {
    warm_up();
    let input = lit_level(SIZE);
    held_at_peak(|| encode_visible(input).unwrap())
}

#[test]
fn encoding_a_1024_px_level_holds_little_more_than_level_1_and_the_payload() {
    let (payload, held) = measure_encode_visible();
    println!(
        "encode_visible of a {SIZE} px level held {held} bytes ({:.2} MiB) beyond its \
         input, {} more than it must",
        held as f64 / MIB as f64,
        held as isize - MUST_HOLD as isize,
    );
    assert_eq!(payload.len(), PAYLOAD, "the whole chain in BC1");
    assert!(payload.iter().any(|&b| b != 0), "the full path, not black");
    assert!(
        held <= MAX_HELD_BEYOND_INPUT,
        "held {held} bytes, more than {MAX_HELD_BEYOND_INPUT}"
    );
}

#[test]
fn encoding_a_level_in_place_holds_a_fraction_of_what_the_whole_chain_did() {
    let (payload, held) = measure_encode_visible();
    let input = lit_level(SIZE);
    let (before, held_before) = held_at_peak(|| encode_visible_before(&input));
    println!(
        "a {SIZE} px level held {held_before} bytes ({:.2} MiB) beyond its input before \
         #440, {held} ({:.2} MiB) now",
        held_before as f64 / MIB as f64,
        held as f64 / MIB as f64,
    );
    assert!(before[..] == payload[..], "the same payload, byte for byte");
    assert!(
        held * 5 <= held_before,
        "held {held} bytes, more than a fifth of the {held_before} before #440"
    );
}
