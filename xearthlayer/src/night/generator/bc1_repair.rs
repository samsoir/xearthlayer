//! Repair of BC1 endpoints that tint dim lit texels green or purple.
//!
//! BC1 stores a block's two endpoints as RGB565. Near black that grid has
//! no neutral colour between 8 and 16 in red and blue, while green also has
//! 4 and 12, and the ISPC encoder quantises each channel on its own: in a
//! block mixing a dim lamp with black it puts a near-white `(9, 7, 5)` on
//! `(8, 4, 8)`, lavender. After ISPC, every block whose decode has a tint its
//! source does not have gets its endpoints searched one 565 step at a time
//! and is replaced only when the error drops. Over 20 KCCR ZL16 payloads
//! and 5 tops this took the tints introduced at a margin of 2 from 30,458
//! purple and 3,009 green to none, keeping 0.999 of the light (flight 9
//! probe, 2026-11-14).

use image::RgbaImage;

use super::BC1_FLOOR;

/// Stored-value margin of a tint: green when G exceeds both R and B by more
/// than this, purple when R and B both exceed G by more. A margin of 4 misses
/// the commonest codes, `(8, 4, 8)` and `(16, 12, 16)`, which differ by 4 yet
/// carry 4.6 and 1.9 times the green's light in red and blue.
pub const TINT_MARGIN: i32 = 2;

/// Weight of a texel's chroma error (in R - G and B - G) against its
/// luminance error.
pub const CHROMA_WEIGHT: f32 = 4.0;

/// Error added for a texel that decodes with a tint its source does not
/// have: larger than any colour error near black, so a repair removes the
/// tint first.
pub const TINT_PENALTY: f32 = 1000.0;

/// Most sweeps of the endpoint search over one block. Every kept step
/// strictly lowers the block's error, so the search ends anyway; the cap
/// bounds its cost. Over the flight 9 probe's 25 encodes it never bound.
pub const MAX_SWEEPS: usize = 64;

/// Bytes of one BC1 block.
const BLOCK_BYTES: usize = 8;

/// Rec.709 luminance weights, applied to stored-value differences.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// One block's source texels, row by row; `None` past a level's edge.
type Texels = [Option<[u8; 3]>; 16];

/// A 565 endpoint as its three channels: red 0..32, green 0..64, blue 0..32.
type Endpoint = [i32; 3];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tint {
    None,
    Green,
    Purple,
}

fn tint([r, g, b]: [u8; 3]) -> Tint {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    if g > r.max(b) + TINT_MARGIN {
        Tint::Green
    } else if r.min(b) > g + TINT_MARGIN {
        Tint::Purple
    } else {
        Tint::None
    }
}

/// Whether `decoded` is lit (its largest channel at least [`BC1_FLOOR`]) and
/// tinted green or purple where `source` is not tinted the same way.
pub fn introduced(decoded: [u8; 3], source: [u8; 3]) -> bool {
    let t = tint(decoded);
    decoded.iter().max().is_some_and(|&m| m >= BC1_FLOOR) && t != Tint::None && t != tint(source)
}

/// The BC1 bytes of a level of `width` x `height` texels.
pub fn level_len(width: u32, height: u32) -> usize {
    (width.div_ceil(4) * height.div_ceil(4)) as usize * BLOCK_BYTES
}

/// Repairs, in place, the blocks of one BC1 level (`blocks`, as the encoder
/// wrote them) whose decode tints a texel of `level` (the image they were
/// encoded from) green or purple.
pub fn repair_level(blocks: &mut [u8], level: &RgbaImage) {
    let per_row = level.width().div_ceil(4);
    for (i, block) in blocks.chunks_exact_mut(BLOCK_BYTES).enumerate() {
        let block: &mut [u8; BLOCK_BYTES] = block.try_into().expect("8-byte chunk");
        if below_floor(block) {
            continue;
        }
        let (bx, by) = (i as u32 % per_row, i as u32 / per_row);
        repair_block(block, &texels(level, bx, by));
    }
}

/// Whether every colour of `block` is below [`BC1_FLOOR`], so none can be
/// an introduced tint: interpolated colours never exceed their endpoints,
/// and most blocks of a lit texture are black.
fn below_floor(block: &[u8; BLOCK_BYTES]) -> bool {
    let (c0, c1) = endpoints(block);
    let dim = |c: u16| {
        let [r, g, b] = unpack(c);
        expand5(r).max(expand6(g)).max(expand5(b)) < BC1_FLOOR
    };
    dim(c0) && dim(c1)
}

/// The source texels of block `(bx, by)`.
fn texels(level: &RgbaImage, bx: u32, by: u32) -> Texels {
    std::array::from_fn(|i| {
        let (x, y) = (bx * 4 + i as u32 % 4, by * 4 + i as u32 / 4);
        (x < level.width() && y < level.height()).then(|| {
            let [r, g, b, _] = level.get_pixel(x, y).0;
            [r, g, b]
        })
    })
}

/// Repairs one block (see the module doc) when its decode has an
/// introduced tint: the same endpoints with indices chosen anew, then
/// coordinate descent over each endpoint's R, G and B, one 565 step at a
/// time, keeping every step that lowers the block's error, until none does
/// or [`MAX_SWEEPS`] sweeps have run. The block keeps its mode (four colours
/// while `c0 > c1`, three and transparent black otherwise) and changes only
/// when its error drops.
fn repair_block(block: &mut [u8; BLOCK_BYTES], texels: &Texels) {
    let (c0, c1) = endpoints(block);
    let colours = palette(c0, c1);
    let tinted = texels
        .iter()
        .enumerate()
        .any(|(i, s)| s.is_some_and(|s| introduced(colours[index(block, i)], s)));
    if !tinted {
        return;
    }
    let four_colour = c0 > c1;
    let in_range =
        |e: Endpoint| (0..32).contains(&e[0]) && (0..64).contains(&e[1]) && (0..32).contains(&e[2]);
    let mut best_error = block_error(block, texels);
    let mut best = None;
    let (error, indices) = fit(c0, c1, texels);
    if error < best_error {
        (best_error, best) = (error, Some((c0, c1, indices)));
    }
    let mut ends = [unpack(c0), unpack(c1)];
    for _ in 0..MAX_SWEEPS {
        let mut improved = false;
        for channel in 0..6 {
            for step in [-1, 1] {
                let mut next = ends;
                next[channel / 3][channel % 3] += step;
                let (n0, n1) = (pack(next[0]), pack(next[1]));
                if !in_range(next[0]) || !in_range(next[1]) || (n0 > n1) != four_colour {
                    continue;
                }
                let (error, indices) = fit(n0, n1, texels);
                if error < best_error {
                    (best_error, best, ends, improved) =
                        (error, Some((n0, n1, indices)), next, true);
                }
            }
        }
        if !improved {
            break;
        }
    }
    if let Some((n0, n1, indices)) = best {
        block[..2].copy_from_slice(&n0.to_le_bytes());
        block[2..4].copy_from_slice(&n1.to_le_bytes());
        block[4..].copy_from_slice(&indices.to_le_bytes());
    }
}

/// The least error of endpoints `c0` and `c1` over `texels`, each texel on
/// its nearest colour, and those indices packed as a block stores them.
/// Three-colour mode's fourth entry is transparent, so it is never chosen:
/// a lit texture stays opaque. Texels past a level's edge take index 0.
fn fit(c0: u16, c1: u16, texels: &Texels) -> (f32, u32) {
    let colours = palette(c0, c1);
    let usable = if c0 > c1 { 4 } else { 3 };
    let mut total = 0.0;
    let mut indices = 0u32;
    for (i, s) in texels.iter().enumerate() {
        let Some(s) = *s else { continue };
        let (k, error) = colours[..usable]
            .iter()
            .map(|&c| texel_error(c, s))
            .enumerate()
            .min_by(|x, y| x.1.total_cmp(&y.1))
            .expect("at least three colours");
        total += error;
        indices |= (k as u32) << (2 * i);
    }
    (total, indices)
}

/// The two endpoints of a block.
fn endpoints(block: &[u8; BLOCK_BYTES]) -> (u16, u16) {
    (
        u16::from_le_bytes([block[0], block[1]]),
        u16::from_le_bytes([block[2], block[3]]),
    )
}

/// The palette index of texel `i`.
fn index(block: &[u8; BLOCK_BYTES], i: usize) -> usize {
    let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    ((indices >> (2 * i)) & 3) as usize
}

fn unpack(c: u16) -> Endpoint {
    let c = i32::from(c);
    [(c >> 11) & 31, (c >> 5) & 63, c & 31]
}

fn pack([r, g, b]: Endpoint) -> u16 {
    ((r << 11) | (g << 5) | b) as u16
}

/// The four colours a block with endpoints `c0` and `c1` decodes to, with
/// a D3D-exact decoder's arithmetic (bcdec's): interpolated in 565 units.
/// In three-colour mode the fourth is transparent black.
fn palette(c0: u16, c1: u16) -> [[u8; 3]; 4] {
    let (a, b) = (unpack(c0), unpack(c1));
    let expand = |e: Endpoint| [expand5(e[0]), expand6(e[1]), expand5(e[2])];
    if c0 > c1 {
        // Two thirds of `p` and one of `q`.
        let third = |p: Endpoint, q: Endpoint| {
            [
                (((2 * p[0] + q[0]) * 351 + 61) >> 7) as u8,
                (((2 * p[1] + q[1]) * 2763 + 1039) >> 11) as u8,
                (((2 * p[2] + q[2]) * 351 + 61) >> 7) as u8,
            ]
        };
        [expand(a), expand(b), third(a, b), third(b, a)]
    } else {
        let half = [
            (((a[0] + b[0]) * 1053 + 125) >> 8) as u8,
            (((a[1] + b[1]) * 4145 + 1019) >> 11) as u8,
            (((a[2] + b[2]) * 1053 + 125) >> 8) as u8,
        ];
        [expand(a), expand(b), half, [0; 3]]
    }
}

fn expand5(v: i32) -> u8 {
    ((v * 527 + 23) >> 6) as u8
}

fn expand6(v: i32) -> u8 {
    ((v * 259 + 33) >> 6) as u8
}

/// The error of `decoded` standing for `source` (see the module constants).
fn texel_error(decoded: [u8; 3], source: [u8; 3]) -> f32 {
    let d: [f32; 3] = std::array::from_fn(|c| f32::from(decoded[c]) - f32::from(source[c]));
    let chroma = (d[0] - d[1]).powi(2) + (d[2] - d[1]).powi(2);
    let luma = LUMA[0] * d[0] + LUMA[1] * d[1] + LUMA[2] * d[2];
    let penalty = if introduced(decoded, source) {
        TINT_PENALTY
    } else {
        0.0
    };
    CHROMA_WEIGHT * chroma + luma * luma + penalty
}

/// The error of a block as encoded, over the texels it stands for.
fn block_error(block: &[u8; BLOCK_BYTES], texels: &Texels) -> f32 {
    let (c0, c1) = endpoints(block);
    let colours = palette(c0, c1);
    texels
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| texel_error(colours[index(block, i)], s)))
        .sum()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::dds::{DdsFormat, ImageCompressor, IspcCompressor};

    /// The top-left block of the flight 9 probe's lamp crop at the top
    /// level: dim near-white road lamps beside black.
    const LAMP: [[u8; 3]; 16] = [
        [9, 7, 5],
        [9, 7, 5],
        [9, 7, 5],
        [8, 6, 5],
        [9, 7, 5],
        [9, 7, 5],
        [8, 7, 5],
        [0, 0, 0],
        [8, 6, 5],
        [8, 6, 5],
        [8, 6, 5],
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
    ];

    fn image(texels: &[[u8; 3]; 16]) -> RgbaImage {
        RgbaImage::from_fn(4, 4, |x, y| {
            let [r, g, b] = texels[(y * 4 + x) as usize];
            image::Rgba([r, g, b, 255])
        })
    }

    fn ispc(img: &RgbaImage) -> Vec<u8> {
        IspcCompressor.compress(img, DdsFormat::BC1).unwrap()
    }

    fn decode(block: &[u8]) -> [[u8; 3]; 16] {
        let mut rgba = [0u8; 64];
        bcdec_rs::bc1(block, &mut rgba, 16);
        std::array::from_fn(|i| [rgba[4 * i], rgba[4 * i + 1], rgba[4 * i + 2]])
    }

    /// Texels of BC1 level `encoded` that decode with a tint `level`, the
    /// image it was encoded from, does not have.
    pub(crate) fn introduced_tints(encoded: &[u8], level: &RgbaImage) -> usize {
        let (width, height) = level.dimensions();
        let per_row = width.div_ceil(4) as usize;
        let mut count = 0;
        for (i, block) in encoded.chunks_exact(BLOCK_BYTES).enumerate() {
            let (bx, by) = ((i % per_row) as u32, (i / per_row) as u32);
            for (t, d) in decode(block).into_iter().enumerate() {
                let (x, y) = (bx * 4 + t as u32 % 4, by * 4 + t as u32 / 4);
                if x < width && y < height {
                    let [r, g, b, _] = level.get_pixel(x, y).0;
                    count += usize::from(introduced(d, [r, g, b]));
                }
            }
        }
        count
    }

    fn introduced_count(block: &[u8], texels: &[[u8; 3]; 16]) -> usize {
        introduced_tints(block, &image(texels))
    }

    fn some(texels: &[[u8; 3]; 16]) -> Texels {
        texels.map(Some)
    }

    #[test]
    fn the_palette_is_the_decoders() {
        for step in 0..4096u32 {
            let h = super::super::noise::mix(u64::from(step));
            let (c0, c1) = (h as u16, (h >> 16) as u16);
            let mut block = [0u8; BLOCK_BYTES];
            block[..2].copy_from_slice(&c0.to_le_bytes());
            block[2..4].copy_from_slice(&c1.to_le_bytes());
            block[4..].copy_from_slice(&0b11_10_01_00u32.to_le_bytes());
            let got = palette(c0, c1);
            let want = decode(&block);
            assert_eq!(
                got,
                [want[0], want[1], want[2], want[3]],
                "{c0:04x} {c1:04x}"
            );
        }
    }

    #[test]
    fn a_dim_lamp_that_ispc_tints_purple_is_repaired() {
        let mut block = ispc(&image(&LAMP));
        // Pinned: if ISPC stops tinting this block, the test must say so
        // rather than pass without repairing anything.
        assert!(
            decode(&block).contains(&[8, 4, 8]),
            "ISPC decodes the lamp {:?}",
            decode(&block)
        );
        assert!(introduced_count(&block, &LAMP) > 0);
        repair_level(&mut block, &image(&LAMP));
        assert_eq!(
            introduced_count(&block, &LAMP),
            0,
            "repaired {:?}",
            decode(&block)
        );
    }

    #[test]
    fn a_block_without_an_introduced_tint_is_left_byte_for_byte() {
        let warm = [[200, 120, 50]; 16];
        let sodium: [[u8; 3]; 16] = std::array::from_fn(|i| {
            let v = 40 + 8 * i as u8;
            [v, v / 5 * 3, v / 4]
        });
        let mut lamp_on_black = [[0u8; 3]; 16];
        lamp_on_black[5] = [180, 170, 160];
        for texels in [warm, sodium, lamp_on_black, [[0; 3]; 16]] {
            let encoded = ispc(&image(&texels));
            assert_eq!(introduced_count(&encoded, &texels), 0, "{texels:?}");
            let mut block = encoded.clone();
            repair_level(&mut block, &image(&texels));
            assert_eq!(block, encoded, "{texels:?}");
        }
    }

    /// A hand-made block: endpoints `c0` and `c1`, texel `i` at `indices[i]`.
    fn block_of(c0: Endpoint, c1: Endpoint, indices: [usize; 16]) -> [u8; BLOCK_BYTES] {
        let mut block = [0u8; BLOCK_BYTES];
        block[..2].copy_from_slice(&pack(c0).to_le_bytes());
        block[2..4].copy_from_slice(&pack(c1).to_le_bytes());
        let bits = indices
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &k)| acc | (k as u32) << (2 * i));
        block[4..].copy_from_slice(&bits.to_le_bytes());
        block
    }

    /// Texels whose decode has alpha 0: three-colour mode's fourth entry.
    fn transparent(block: &[u8]) -> usize {
        let mut rgba = [0u8; 64];
        bcdec_rs::bc1(block, &mut rgba, 16);
        rgba.chunks_exact(4).filter(|p| p[3] == 0).count()
    }

    /// Repairs `block`, which stands for 16 texels of the lamp's
    /// `(8, 6, 5)` and whose decode the lamp shows tinted, and asserts that
    /// it keeps its mode, no texel decodes transparent and none is tinted.
    /// Its endpoints are one 565 step apart, so a single step can equalise
    /// or swap them: unguarded, the best repair of either block found here
    /// changes its mode.
    fn assert_repaired_in_its_mode(mut block: [u8; BLOCK_BYTES]) {
        let lamp = [[8, 6, 5]; 16];
        let (c0, c1) = endpoints(&block);
        let four_colour = c0 > c1;
        assert!(introduced_count(&block, &lamp) > 0, "{:?}", decode(&block));
        repair_block(&mut block, &some(&lamp));
        let (n0, n1) = endpoints(&block);
        assert_eq!(
            n0 > n1,
            four_colour,
            "mode kept: {:?} {:?}",
            unpack(n0),
            unpack(n1)
        );
        assert_eq!(transparent(&block), 0, "{:?}", decode(&block));
        assert_eq!(introduced_count(&block, &lamp), 0, "{:?}", decode(&block));
    }

    #[test]
    fn a_four_colour_block_one_step_from_three_colours_stays_four_colour() {
        // (0, 2, 0) decodes (0, 8, 0), green over the lamp; c1 is one green
        // step below it.
        assert_repaired_in_its_mode(block_of([0, 2, 0], [0, 1, 0], [0; 16]));
    }

    #[test]
    fn a_three_colour_block_one_step_from_four_colours_stays_three_colour() {
        // c0 < c1 by one red step; (1, 0, 1) decodes (8, 0, 8), purple over
        // the lamp's second half.
        assert_repaired_in_its_mode(block_of(
            [0, 0, 1],
            [1, 0, 1],
            std::array::from_fn(|i| usize::from(i >= 8)),
        ));
    }

    #[test]
    fn a_repaired_block_decodes_no_worse_than_ispcs() {
        let mut repaired = 0;
        for n in 0..2000u64 {
            // Dim near-white or warm texels beside black, as lamps are.
            let texels: [[u8; 3]; 16] = std::array::from_fn(|i| {
                let h = super::super::noise::mix(n * 16 + i as u64);
                if h % 3 == 0 {
                    return [0; 3];
                }
                let v = 8 + (h >> 8) as u8 % 24;
                let (dg, db) = ((h >> 16) as u8 % 4, (h >> 24) as u8 % 6);
                [v, v.saturating_sub(dg), v.saturating_sub(db)]
            });
            let encoded = ispc(&image(&texels));
            let before: [u8; BLOCK_BYTES] = encoded[..].try_into().unwrap();
            let mut after = before;
            repair_block(&mut after, &some(&texels));
            let (e0, e1) = (
                block_error(&before, &some(&texels)),
                block_error(&after, &some(&texels)),
            );
            assert!(e1 <= e0, "block {n}: {e0} to {e1}");
            repaired += usize::from(after != before);
        }
        assert!(repaired > 0, "some blocks need a repair");
    }

    #[test]
    fn each_block_is_repaired_against_its_own_texels() {
        // An 8 x 12 level, 2 x 3 blocks: the lamp at block (1, 2), the lamp
        // mirrored left to right at block (0, 1), sodium at block (0, 0),
        // black elsewhere. A wrong block or texel mapping judges a block
        // against texels it does not stand for.
        let mirrored: [[u8; 3]; 16] = std::array::from_fn(|i| LAMP[i / 4 * 4 + 3 - i % 4]);
        let level = RgbaImage::from_fn(8, 12, |x, y| {
            let t = (y % 4 * 4 + x % 4) as usize;
            let [r, g, b] = match (x / 4, y / 4) {
                (1, 2) => LAMP[t],
                (0, 1) => mirrored[t],
                (0, 0) => [200, 120, 50],
                _ => [0; 3],
            };
            image::Rgba([r, g, b, 255])
        });
        let encoded = ispc(&level);
        let block = |bytes: &[u8], i: usize| bytes[i * BLOCK_BYTES..(i + 1) * BLOCK_BYTES].to_vec();
        // Pinned: ISPC tints both lamps.
        for i in [2, 5] {
            assert!(
                decode(&block(&encoded, i)).contains(&[8, 4, 8]),
                "block {i}"
            );
        }
        assert!(introduced_tints(&encoded, &level) > 0);
        let mut repaired = encoded.clone();
        repair_level(&mut repaired, &level);
        assert_eq!(introduced_tints(&repaired, &level), 0);
        // Each lamp is still where its texels are: black exactly where the
        // source is black.
        for (i, texels) in [(2, mirrored), (5, LAMP)] {
            let decoded = decode(&block(&repaired, i));
            for t in 0..16 {
                assert_eq!(
                    decoded[t] == [0; 3],
                    texels[t] == [0; 3],
                    "block {i} texel {t}"
                );
            }
        }
        for i in [0, 1, 3, 4] {
            assert_eq!(block(&repaired, i), block(&encoded, i), "block {i}");
        }
    }

    #[test]
    fn texels_past_the_edge_of_a_small_level_are_ignored() {
        // A 2 x 2 level is one block whose other 12 texels are padding.
        let level = RgbaImage::from_pixel(2, 2, image::Rgba([8, 6, 5, 255]));
        let mut block = block_of([1, 1, 1], [0, 0, 0], [0; 16]);
        repair_level(&mut block, &level);
        let decoded = decode(&block);
        for i in [0, 1, 4, 5] {
            assert!(!introduced(decoded[i], [8, 6, 5]), "{decoded:?}");
        }
    }
}
