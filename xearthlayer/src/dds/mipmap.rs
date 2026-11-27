//! Mipmap chain generation for DDS textures.

use std::sync::LazyLock;

use image::RgbaImage;

/// How a mip level averages each 2×2 block of the level above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MipFilter {
    /// The stored bytes, averaged and rounded half to even (#394): the
    /// filter before #467. Implemented on the GPU only (`dds::gpu_mips`),
    /// as a tested option: no chain uses it.
    StoredValues,
    /// R, G and B averaged in linear light, alpha as stored (#467):
    /// [`MipmapGenerator::downsample_box_2x`].
    LinearLight,
}

/// White in the 16-bit linear light the filter averages in.
const LINEAR_WHITE: f64 = u16::MAX as f64;

/// sRGB value (0 to 1) to linear light (0 to 1), IEC 61966-2-1.
fn srgb_decode(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light (0 to 1) to sRGB value (0 to 1), IEC 61966-2-1.
fn srgb_encode(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Every sRGB byte as 16-bit linear light, rounded to nearest.
static SRGB_TO_LINEAR: LazyLock<[u16; 256]> = LazyLock::new(|| {
    std::array::from_fn(|v| (srgb_decode(v as f64 / 255.0) * LINEAR_WHITE).round() as u16)
});

/// Every 16-bit linear light value as the nearest sRGB byte. 16 bits, not
/// fewer: near black one sRGB step is only about 20 of 65,535 linear steps.
static LINEAR_TO_SRGB: LazyLock<Box<[u8]>> = LazyLock::new(|| {
    (0..=u16::MAX)
        .map(|l| (srgb_encode(f64::from(l) / LINEAR_WHITE) * 255.0).round() as u8)
        .collect()
});

/// [`SRGB_TO_LINEAR`] as u32, so the four values of a block add with no
/// conversion.
static SRGB_TO_LINEAR_WIDE: LazyLock<[u32; 256]> =
    LazyLock::new(|| std::array::from_fn(|v| u32::from(SRGB_TO_LINEAR[v])));

const SUM_TABLE_LEN: usize = 1 << 18;

/// [`LINEAR_TO_SRGB`] indexed by the sum of four linear values, the rounding
/// of their mean ([`quarter_half_to_even`]) folded in: one load where the
/// filter did rounding arithmetic and a load. A sum is at most `4 * 65535`;
/// the table is 2^18 entries (256 KiB) so that masking a sum to 18 bits
/// needs no bounds check, and the unreachable tail is clamped to white.
static SUM_TO_SRGB: LazyLock<Box<[u8; SUM_TABLE_LEN]>> = LazyLock::new(|| {
    (0..SUM_TABLE_LEN as u32)
        .map(|sum| LINEAR_TO_SRGB[(quarter_half_to_even(sum) as usize).min(usize::from(u16::MAX))])
        .collect::<Box<[u8]>>()
        .try_into()
        .expect("the table has SUM_TABLE_LEN entries")
});

/// The filter's decode table: every sRGB byte as 16-bit linear light. The
/// GPU's linear-light shader reads the same table (`dds::gpu_mips`).
pub(crate) fn srgb_to_linear_table() -> &'static [u16; 256] {
    &SRGB_TO_LINEAR
}

/// The filter's encode table: every 16-bit linear light value as the
/// nearest sRGB byte. The GPU's linear-light shader reads the same table.
pub(crate) fn linear_to_srgb_table() -> &'static [u8] {
    &LINEAR_TO_SRGB
}

/// Mipmap generator.
pub struct MipmapGenerator;

impl MipmapGenerator {
    /// The filter of every mip chain XEarthLayer encodes. It describes the
    /// CPU chain ([`downsample_box_2x`](Self::downsample_box_2x)), which
    /// does not read it: a test (`the_filter_describes_the_cpu_chain`)
    /// holds the value to what the chain does. The GPU worker's chain
    /// reads it (#424, `dds::gpu_mips::gpu_filter`), so a tile's levels are
    /// the same bytes on either path. Linear light since #467.
    pub const FILTER: MipFilter = MipFilter::LinearLight;

    /// Generate full mipmap chain from source image.
    ///
    /// Continues downsampling by 2× until reaching 1×1 or a minimum size.
    ///
    /// # Arguments
    ///
    /// * `source` - Source image (must have power-of-2 dimensions)
    ///
    /// # Returns
    ///
    /// Vector of images: [original, half-size, quarter-size, ...]
    pub fn generate_chain(source: &RgbaImage) -> Vec<RgbaImage> {
        let mut mipmaps = vec![source.clone()];

        let mut current = source.clone();
        while current.width() > 1 && current.height() > 1 {
            current = Self::downsample_box_2x(&current);
            mipmaps.push(current.clone());
        }

        mipmaps
    }

    /// Generate mipmap chain up to a specific count.
    ///
    /// # Arguments
    ///
    /// * `source` - Source image
    /// * `count` - Number of mipmap levels to generate (including original)
    ///
    /// # Returns
    ///
    /// Vector of `count` images
    pub fn generate_chain_with_count(source: &RgbaImage, count: usize) -> Vec<RgbaImage> {
        let mut mipmaps = vec![source.clone()];

        let mut current = source.clone();
        for _ in 1..count {
            if current.width() <= 1 || current.height() <= 1 {
                break;
            }
            current = Self::downsample_box_2x(&current);
            mipmaps.push(current.clone());
        }

        mipmaps
    }

    /// Count the total number of mipmap levels from full size down to 1×1.
    ///
    /// `floor(log2(min(width, height))) + 1`, from
    /// [`layout::full_chain_levels`](crate::dds::layout::full_chain_levels).
    /// Matches the behavior of `generate_chain()` which stops when
    /// `width <= 1 || height <= 1`.
    pub fn full_chain_count(width: u32, height: u32) -> usize {
        crate::dds::layout::full_chain_levels(width, height) as usize
    }

    /// Downsample image by 2× using a box filter in linear light.
    ///
    /// Each output pixel is the average of a 2×2 block of input pixels.
    /// R, G and B are sRGB-encoded, so they are averaged as light: each value
    /// is taken to 16-bit linear light (IEC 61966-2-1, rounded to nearest),
    /// the four are averaged and the mean is taken back to the nearest sRGB
    /// byte. Averaging the stored values instead loses light wherever
    /// neighbours differ (black beside white gives 128, about a fifth of the
    /// light, where half the light is 188), so towns, rock, coast and snow
    /// grew darker with every level. Alpha is
    /// coverage, not light, and keeps the mean of its stored values. Each
    /// mean is rounded half to even ([`quarter_half_to_even`], #394), so the
    /// rounding has no bias to accumulate down the chain.
    /// An odd last column or row has no partner and is dropped, so a `w×h`
    /// source gives `w/2 × h/2` (a 1-wide or 1-high source gives an empty
    /// image).
    ///
    /// Works on the raw RGBA bytes, one output row at a time, each output
    /// pixel from a pair of pixels of each of its two source rows, with no
    /// per-pixel bounds checks (#389), in [`downsample_row`]. The output is
    /// the only allocation. Cost of the whole chain of a 4096×4096 tile in
    /// this allocating form, one thread, release build, glibc's default
    /// malloc, Ryzen 7 9800X3D: about 14 ms, against 4.7 ms for the
    /// stored-value average it replaced (`time_the_4096_mip_chain`).
    ///
    /// The CPU backends, ISPC and software, run the same arithmetic through
    /// [`MipmapStream`] for every level of every tile, in place
    /// ([`downsample_box_2x_in_place`](Self::downsample_box_2x_in_place)). The GPU worker builds
    /// its levels in a shader with the same filter and tables
    /// (`dds::gpu_mips`, [`Self::FILTER`]).
    pub(crate) fn downsample_box_2x(source: &RgbaImage) -> RgbaImage {
        let new_width = source.width() / 2;
        let new_height = source.height() / 2;
        if new_width == 0 || new_height == 0 {
            return RgbaImage::new(new_width, new_height);
        }

        let src_row = source.width() as usize * CHANNELS;
        let out_row = new_width as usize * CHANNELS;
        let mut output = vec![0u8; out_row * new_height as usize];

        // `chunks_exact` drops an odd last source row, as the halved
        // dimensions require.
        for (out, rows) in output
            .chunks_exact_mut(out_row)
            .zip(source.as_raw().chunks_exact(2 * src_row))
        {
            let (top, bottom) = rows.split_at(src_row);
            downsample_row(out, top, bottom);
        }

        RgbaImage::from_raw(new_width, new_height, output)
            .expect("output buffer is sized from the halved dimensions")
    }

    /// [`downsample_box_2x`](Self::downsample_box_2x), the same bytes,
    /// written over the start of the source's own buffer, which is cut to
    /// the new length (`truncate`, not `shrink_to_fit`: the buffer stays
    /// held to the end of the chain, so a chain holds level 0's buffer
    /// instead of level 0 and level 1, and no level gets a buffer of its
    /// own). The one allocation is a copy of the first two source rows
    /// (32 KiB for a 4096 level), for row 0.
    ///
    /// The point is the allocator, not the arithmetic. With
    /// `M_MMAP_THRESHOLD` at 1 MiB (XEarthLayer's setting on glibc unless
    /// the environment overrides it, #227) the buffers of levels 1 to 3 of a 4096 tile (16, 4 and 1 MiB)
    /// were each a new mapping whose pages fault in on first write.
    /// `time_the_4096_mip_chain` (a noise tile) with that threshold: 18.4 ms
    /// allocating, 15.2 ms in place; with glibc's default malloc, which
    /// serves the levels from the heap, 14.4 ms allocating and 15.2 ms in
    /// place. Real tiles, the same filter in a benchmark outside this
    /// repository: 16.2 to 17.4 ms down to 13.1 to 13.6 ms with the
    /// threshold, 13.1 to 13.7 ms either way without it.
    ///
    /// Output row `y` is `out_row = 4 * (w / 2)` bytes at `y * out_row`; its
    /// source rows start at `2 * y * 4 * w`. Since `out_row <= 2 * w` (equal
    /// to half a source row for even `w`, less for odd `w`), the output row
    /// ends at `(y + 1) * out_row <= (y + 1) * 2 * w`, which is at most
    /// `2 * y * 4 * w` for every `y >= 1`. So every row after the first lies
    /// wholly before its source rows and the two are split apart with
    /// `split_at_mut`; row 0 overlaps its own source rows and is built from
    /// a copy of them.
    pub(crate) fn downsample_box_2x_in_place(image: &mut RgbaImage) {
        let new_width = image.width() / 2;
        let new_height = image.height() / 2;
        let src_row = image.width() as usize * CHANNELS;
        let out_row = new_width as usize * CHANNELS;

        let mut raw = std::mem::replace(image, RgbaImage::new(0, 0)).into_raw();
        if new_width == 0 || new_height == 0 {
            raw.clear();
        } else {
            // Row 0: its output bytes overlap its own source rows.
            let rows = raw[..2 * src_row].to_vec();
            let (top, bottom) = rows.split_at(src_row);
            downsample_row(&mut raw[..out_row], top, bottom);

            for y in 1..new_height as usize {
                let (done, rest) = raw.split_at_mut(2 * y * src_row);
                let (top, rest) = rest.split_at(src_row);
                downsample_row(
                    &mut done[y * out_row..(y + 1) * out_row],
                    top,
                    &rest[..src_row],
                );
            }
            raw.truncate(out_row * new_height as usize);
        }
        *image = RgbaImage::from_raw(new_width, new_height, raw)
            .expect("buffer is cut to the halved dimensions");
    }
}

const CHANNELS: usize = 4;

/// One output row of [`MipmapGenerator::downsample_box_2x`] from its two
/// source rows: no per-pixel bounds checks (#389). R, G and B go to linear
/// light through [`SRGB_TO_LINEAR_WIDE`] and back through [`SUM_TO_SRGB`],
/// which takes the sum of four and returns the sRGB byte of their mean.
/// An odd last source pixel has no partner and is ignored (the output row
/// sets how many pixels are written).
#[inline(always)]
fn downsample_row(out: &mut [u8], top: &[u8], bottom: &[u8]) {
    let to_linear = &*SRGB_TO_LINEAR_WIDE;
    let to_srgb = &**SUM_TO_SRGB;
    for ((pixel, t), b) in out
        .chunks_exact_mut(CHANNELS)
        .zip(top.chunks_exact(2 * CHANNELS))
        .zip(bottom.chunks_exact(2 * CHANNELS))
    {
        for c in 0..3 {
            let light = |v: u8| to_linear[usize::from(v)];
            let sum = light(t[c]) + light(t[c + CHANNELS]) + light(b[c]) + light(b[c + CHANNELS]);
            // A sum is below 2^18; the table folds in the rounded mean.
            pixel[c] = to_srgb[sum as usize & (SUM_TABLE_LEN - 1)];
        }
        let alpha = [t[3], t[7], b[3], b[7]].map(u32::from);
        // The mean of four bytes is a byte.
        pixel[3] = quarter_half_to_even(alpha.iter().sum()) as u8;
    }
}

/// `sum / 4`, a 2×2 block's mean from its sum, rounded to the nearest step
/// with ties to even (#394).
///
/// The sum leaves a remainder of 0, 1, 2 or 3 over 4 about equally often on
/// imagery, and each level is built from the one above, so any bias in this
/// rounding accumulates down the chain. Truncating (`sum / 4`) lost 0.375 of
/// a step a level, darkening distant terrain; rounding half up
/// (`(sum + 2) / 4`) would gain 0.125 a level. Ties to even has no bias:
/// a remainder of 2 rounds down when the quotient is even and up when it is
/// odd.
///
/// `(sum >> 2) & 1` is the quotient's parity, so adding it to `sum + 1`
/// carries into the quotient exactly for a remainder of 3, or of 2 with an
/// odd quotient. An opaque block stays opaque: an alpha sum of 1020 gives
/// 255, and a linear sum of four whites gives white.
#[inline(always)]
const fn quarter_half_to_even(sum: u32) -> u32 {
    (sum + 1 + ((sum >> 2) & 1)) >> 2
}

/// Streaming mipmap generator that lends one level at a time.
///
/// Takes ownership of the source image and builds the levels in its buffer:
/// each [`next_level`](Self::next_level) after the first writes the next
/// level over the start of the previous one
/// ([`MipmapGenerator::downsample_box_2x_in_place`]), so the previous level
/// is gone once the next is lent.
///
/// # Memory
///
/// One buffer, the size of level 0, held until the stream is dropped: the
/// chain never holds a second level, and after the first no level is a new
/// allocation.
pub struct MipmapStream {
    /// The level lent last, or the source before the first call.
    image: RgbaImage,
    /// Number of levels remaining to lend.
    remaining: usize,
    /// Whether the first (original) image has been lent.
    lent_first: bool,
}

impl MipmapStream {
    /// Create a new mipmap stream from an owned source image.
    ///
    /// # Arguments
    ///
    /// * `image` - Source image (ownership transferred, no clone)
    /// * `count` - Total number of mipmap levels to lend (including the original)
    pub fn new(image: RgbaImage, count: usize) -> Self {
        Self {
            image,
            remaining: count,
            lent_first: false,
        }
    }

    /// The next level, largest first: the source on the first call, then
    /// each half the size of the last. `None` after `count` levels, or when
    /// the last was not both wider and higher than 1. The level is valid
    /// until the next call.
    pub fn next_level(&mut self) -> Option<&RgbaImage> {
        if self.remaining == 0 {
            return None;
        }
        if self.lent_first {
            if self.image.width() <= 1 || self.image.height() <= 1 {
                self.remaining = 0;
                return None;
            }
            MipmapGenerator::downsample_box_2x_in_place(&mut self.image);
        }
        self.lent_first = true;
        self.remaining -= 1;
        Some(&self.image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sum / 4` rounded to the nearest whole step, a tie going to the even
    /// one: round half to even, by its definition, in exact arithmetic on the
    /// rational `sum / 4 = q + r/4` (no bit tricks, so it is independent of
    /// the formula the fast path uses).
    fn round_half_to_even_quarter(sum: u32) -> u32 {
        let (q, r) = (sum / 4, sum % 4);
        // r/4 against 1/2, compared as 2r against 4.
        match (2 * r).cmp(&4) {
            std::cmp::Ordering::Less => q,
            std::cmp::Ordering::Greater => q + 1,
            std::cmp::Ordering::Equal if q % 2 == 0 => q,
            std::cmp::Ordering::Equal => q + 1,
        }
    }

    /// sRGB value (0 to 1) to linear light (0 to 1), IEC 61966-2-1, written
    /// out here so the tests do not lean on the production curve.
    fn iec_decode(v: f64) -> f64 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Linear light (0 to 1) to sRGB value (0 to 1), IEC 61966-2-1.
    fn iec_encode(l: f64) -> f64 {
        if l <= 0.003_130_8 {
            l * 12.92
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        }
    }

    /// An sRGB byte as linear light in 1/65535 steps, rounded to nearest.
    fn linear_steps(byte: u8) -> u32 {
        (iec_decode(f64::from(byte) / 255.0) * 65535.0).round() as u32
    }

    /// Linear light in 1/65535 steps as the nearest sRGB byte.
    fn nearest_byte(steps: u32) -> u8 {
        (iec_encode(f64::from(steps) / 65535.0) * 255.0).round() as u8
    }

    /// The definition of the filter, pixel by pixel: R, G and B of each 2×2
    /// block averaged as light, each value taken to 16-bit linear light, the
    /// mean rounded half to even and taken to the nearest sRGB byte; alpha
    /// the stored values' mean rounded half to even (#394). The fast path
    /// must match it byte for byte.
    fn downsample_box_2x_reference(source: &RgbaImage) -> RgbaImage {
        let new_width = source.width() / 2;
        let new_height = source.height() / 2;

        let mut output = RgbaImage::new(new_width, new_height);

        for y in 0..new_height {
            for x in 0..new_width {
                let block = [
                    source.get_pixel(x * 2, y * 2),
                    source.get_pixel(x * 2 + 1, y * 2),
                    source.get_pixel(x * 2, y * 2 + 1),
                    source.get_pixel(x * 2 + 1, y * 2 + 1),
                ];
                let avg = image::Rgba(std::array::from_fn(|c| {
                    if c == 3 {
                        let sum = block.iter().map(|p| u32::from(p[c])).sum();
                        round_half_to_even_quarter(sum) as u8
                    } else {
                        let sum = block.iter().map(|p| linear_steps(p[c])).sum();
                        nearest_byte(round_half_to_even_quarter(sum))
                    }
                }));
                output.put_pixel(x, y, avg);
            }
        }

        output
    }

    /// An endless deterministic byte stream from SplitMix64.
    fn noise_bytes(seed: u64) -> impl Iterator<Item = u8> {
        let mut state = seed;
        std::iter::repeat_with(move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        })
        .flat_map(u64::to_le_bytes)
    }

    /// Deterministic noise: every byte of every channel drawn from SplitMix64,
    /// so all 256 values of each channel occur and every 2×2 sum is exercised.
    fn noise(width: u32, height: u32, seed: u64) -> RgbaImage {
        let len = width as usize * height as usize * 4;
        let bytes = noise_bytes(seed).take(len).collect();
        RgbaImage::from_raw(width, height, bytes).expect("buffer sized for the image")
    }

    /// Noise whose every byte is uniform over `0..=254`: the bytes of
    /// [`noise_bytes`] with each 255 skipped. Each channel's mean is then
    /// close to a whole step, 127, where [`noise`]'s is close to 127.5.
    fn noise_with_whole_step_mean(width: u32, height: u32, seed: u64) -> RgbaImage {
        let len = width as usize * height as usize * 4;
        let bytes = noise_bytes(seed).filter(|&b| b != 255).take(len).collect();
        RgbaImage::from_raw(width, height, bytes).expect("buffer sized for the image")
    }

    fn solid(width: u32, height: u32, colour: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, image::Rgba(colour))
    }

    /// Assert two images have the same dimensions and bytes, naming the first
    /// differing pixel rather than printing megabytes of buffer.
    fn assert_same_image(actual: &RgbaImage, expected: &RgbaImage, what: &str) {
        assert_eq!(
            actual.dimensions(),
            expected.dimensions(),
            "{what}: dimensions differ"
        );
        if let Some(i) = actual
            .as_raw()
            .iter()
            .zip(expected.as_raw().iter())
            .position(|(a, e)| a != e)
        {
            let pixel = i / 4;
            let width = expected.width().max(1) as usize;
            panic!(
                "{what}: first difference at pixel ({}, {}) channel {}: got {}, expected {}",
                pixel % width,
                pixel / width,
                i % 4,
                actual.as_raw()[i],
                expected.as_raw()[i]
            );
        }
        assert_eq!(
            actual.as_raw().len(),
            expected.as_raw().len(),
            "{what}: buffer lengths differ"
        );
    }

    fn assert_matches_reference(source: &RgbaImage, what: &str) {
        let expected = downsample_box_2x_reference(source);
        let actual = MipmapGenerator::downsample_box_2x(source);
        let what = format!("{what} {}x{}", source.width(), source.height());
        assert_same_image(&actual, &expected, &what);
    }

    /// 4×4 black and white checkerboard.
    fn checkerboard_4x4() -> RgbaImage {
        let mut source = RgbaImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                let color = if (x + y) % 2 == 0 {
                    image::Rgba([0, 0, 0, 255])
                } else {
                    image::Rgba([255, 255, 255, 255])
                };
                source.put_pixel(x, y, color);
            }
        }
        source
    }

    /// 4×4 grey diagonal gradient.
    fn gradient_4x4() -> RgbaImage {
        let mut source = RgbaImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                let val = ((x + y) * 255 / 6) as u8;
                source.put_pixel(x, y, image::Rgba([val, val, val, 255]));
            }
        }
        source
    }

    /// 4×4 image with varying RGB and a constant alpha of 200.
    fn constant_alpha_4x4() -> RgbaImage {
        let mut source = RgbaImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                source.put_pixel(x, y, image::Rgba([x as u8 * 60, y as u8 * 60, 128, 200]));
            }
        }
        source
    }

    /// 2×2 image with one distinct primary in each corner.
    fn corners_2x2() -> RgbaImage {
        let mut source = RgbaImage::new(2, 2);
        source.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        source.put_pixel(1, 0, image::Rgba([100, 0, 0, 255]));
        source.put_pixel(0, 1, image::Rgba([0, 100, 0, 255]));
        source.put_pixel(1, 1, image::Rgba([0, 0, 100, 255]));
        source
    }

    /// The fast path's average is round half to even of `sum / 4` for every
    /// sum four 16-bit linear values can make, `0..=4 * 65535` (which
    /// covers the 1021 sums of four alpha bytes), against the definition.
    #[test]
    fn the_block_average_rounds_half_to_even_for_every_sum() {
        for sum in 0..=4 * u32::from(u16::MAX) {
            assert_eq!(
                quarter_half_to_even(sum),
                round_half_to_even_quarter(sum),
                "sum {sum}"
            );
        }
    }

    /// The filter's own tables are the shared ones, widened or folded: the
    /// u32 decode table holds the u16 table's values, and the encode table
    /// indexed by a sum of four holds, for every sum four 16-bit values can
    /// make, what rounding the mean and encoding it gives.
    #[test]
    fn the_filter_tables_are_the_shared_tables_widened_and_folded() {
        for v in 0..=u8::MAX {
            assert_eq!(
                SRGB_TO_LINEAR_WIDE[usize::from(v)],
                u32::from(SRGB_TO_LINEAR[usize::from(v)]),
                "byte {v}"
            );
        }
        for sum in 0..=4 * u32::from(u16::MAX) {
            assert_eq!(
                SUM_TO_SRGB[sum as usize],
                LINEAR_TO_SRGB[quarter_half_to_even(sum) as usize],
                "sum {sum}"
            );
        }
    }

    /// [`MipmapGenerator::FILTER`] describes the CPU chain, which does not
    /// read it: the chain's levels are what the filter it names gives,
    /// computed here by its definition. A filter the CPU does not implement
    /// fails here.
    #[test]
    fn the_filter_describes_the_cpu_chain() {
        let source = noise(64, 48, 7);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 4);
        for (level, pair) in chain.windows(2).enumerate() {
            let expected = match MipmapGenerator::FILTER {
                MipFilter::LinearLight => downsample_box_2x_reference(&pair[0]),
                MipFilter::StoredValues => panic!("the CPU chain does not average stored values"),
            };
            assert_same_image(&pair[1], &expected, &format!("level {}", level + 1));
        }
    }

    /// Every byte survives the way to linear light and back, so a uniform
    /// colour keeps its value at every level.
    #[test]
    fn every_byte_round_trips_through_linear_light() {
        for v in 0..=u8::MAX {
            let light = SRGB_TO_LINEAR[usize::from(v)];
            assert_eq!(LINEAR_TO_SRGB[usize::from(light)], v, "byte {v}");
        }
    }

    #[test]
    fn downsample_matches_reference_on_noise_of_every_small_shape() {
        // Every width and height from 0 to 9: odd, non-square, the 2×2 and
        // 1×1 tail of a chain, and the degenerate shapes that halve to an
        // empty image.
        for width in 0..=9 {
            for height in 0..=9 {
                for seed in 0..3 {
                    let source = noise(width, height, seed);
                    assert_matches_reference(&source, &format!("noise seed {seed}"));
                }
            }
        }
    }

    #[test]
    fn downsample_matches_reference_on_noise_of_larger_and_non_square_shapes() {
        for (width, height) in [
            (64, 64),
            (256, 128),
            (128, 256),
            (257, 129),
            (129, 257),
            (1023, 511),
            (4096, 2),
            (2, 4096),
            (4097, 3),
        ] {
            assert_matches_reference(&noise(width, height, 7), "noise");
        }
    }

    #[test]
    fn downsample_matches_reference_on_solid_colours() {
        for colour in [
            [0, 0, 0, 0],
            [255, 255, 255, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 128],
            [1, 2, 3, 4],
            [254, 253, 252, 251],
            [128, 64, 32, 16],
        ] {
            for (width, height) in [(1, 1), (2, 2), (4, 4), (5, 3), (3, 5), (64, 64)] {
                assert_matches_reference(&solid(width, height, colour), &format!("{colour:?}"));
            }
        }
    }

    #[test]
    fn downsample_matches_reference_on_the_fixtures() {
        assert_matches_reference(&checkerboard_4x4(), "checkerboard");
        assert_matches_reference(&gradient_4x4(), "gradient");
        assert_matches_reference(&constant_alpha_4x4(), "constant alpha");
        assert_matches_reference(&corners_2x2(), "corners");
    }

    /// The production chain of a 4096×4096 tile, every level through
    /// [`MipmapStream`], against the reference applied level by level.
    #[test]
    fn full_4096_chain_matches_reference_at_every_level() {
        let source = noise(4096, 4096, 389);
        let count = MipmapGenerator::full_chain_count(4096, 4096);

        let mut expected = source.clone();
        let mut stream = MipmapStream::new(source, count);
        let mut levels = 0;
        while let Some(actual) = stream.next_level() {
            if levels > 0 {
                expected = downsample_box_2x_reference(&expected);
            }
            assert_same_image(actual, &expected, &format!("level {levels}"));
            levels += 1;
        }
        assert_eq!(levels, 13);
    }

    /// The mean light of each of R, G and B, and the mean stored alpha, over
    /// every pixel of `image`, expressed in sRGB steps (the light's mean
    /// encoded back), so a drift reads in the steps a viewer sees.
    fn light_means(image: &RgbaImage) -> [f64; 4] {
        let means = channel_light_sums(image);
        let pixels = f64::from(image.width()) * f64::from(image.height());
        std::array::from_fn(|c| {
            let mean = means[c] / pixels;
            if c == 3 {
                mean
            } else {
                iec_encode(mean) * 255.0
            }
        })
    }

    /// Per channel, the sum over every pixel of its linear light (R, G, B)
    /// or its stored value (alpha).
    fn channel_light_sums(image: &RgbaImage) -> [f64; 4] {
        let mut sums = [0f64; 4];
        for pixel in image.as_raw().chunks_exact(4) {
            for (c, (sum, &value)) in sums.iter_mut().zip(pixel).enumerate() {
                *sum += if c == 3 {
                    f64::from(value)
                } else {
                    iec_decode(f64::from(value) / 255.0)
                };
            }
        }
        sums
    }

    /// The chain must not change the brightness of a tile with distance
    /// (#394): at every level of a 4096×4096
    /// chain, the mean light of R, G and B, encoded back to sRGB, stays
    /// within half a step of the source's, and so does the mean alpha.
    /// Averaging the stored sRGB values instead loses light wherever
    /// neighbours differ: on this noise level 1 is already about 17 steps
    /// darker.
    ///
    /// The source's bytes are uniform over `0..=254`, so the stored alpha
    /// mean is close to a whole step. The 1×1 level is one pixel, a whole
    /// step, and a source mean half way between two steps would leave it
    /// half a step off however the chain rounds.
    #[test]
    fn every_level_keeps_the_source_light_within_half_a_step() {
        let source = noise_with_whole_step_mean(4096, 4096, 394);
        let source_means = light_means(&source);
        let count = MipmapGenerator::full_chain_count(4096, 4096);

        let mut stream = MipmapStream::new(source, count);
        let mut levels = 0;
        while let Some(image) = stream.next_level() {
            let level = levels;
            let means = light_means(image);
            for c in 0..4 {
                let drift = means[c] - source_means[c];
                assert!(
                    drift.abs() <= 0.5,
                    "level {level} ({}x{}) channel {c}: mean {:.3} against the source's {:.3}, \
                     {drift:+.3} of a step",
                    image.width(),
                    image.height(),
                    means[c],
                    source_means[c],
                );
            }
            levels += 1;
        }
        assert_eq!(levels, 13);
    }

    /// Half black and half white is half the light, sRGB 188, not the 128
    /// that averaging the stored values gives (about a fifth of the light).
    #[test]
    fn a_black_and_white_block_averages_to_half_the_light() {
        let downsampled = MipmapGenerator::downsample_box_2x(&checkerboard_4x4());
        for pixel in downsampled.pixels() {
            assert_eq!(pixel.0, [188, 188, 188, 255]);
        }
    }

    /// The 16-bit linear steps cost at most one step against averaging in
    /// exact f64 light: noise of every value, against the exact mean.
    #[test]
    fn downsample_is_within_one_step_of_the_exact_light_average() {
        let source = noise(256, 256, 5);
        let downsampled = MipmapGenerator::downsample_box_2x(&source);
        for y in 0..downsampled.height() {
            for x in 0..downsampled.width() {
                for c in 0..3 {
                    let light: f64 = [(0, 0), (1, 0), (0, 1), (1, 1)]
                        .iter()
                        .map(|&(dx, dy)| {
                            let v = source.get_pixel(2 * x + dx, 2 * y + dy)[c];
                            iec_decode(f64::from(v) / 255.0)
                        })
                        .sum::<f64>()
                        / 4.0;
                    let exact = iec_encode(light) * 255.0;
                    let got = f64::from(downsampled.get_pixel(x, y)[c]);
                    assert!(
                        (got - exact).abs() <= 1.0,
                        "({x}, {y}) channel {c}: {got} against exact {exact:.3}"
                    );
                }
            }
        }
    }

    /// Pins where each 2×2 block's average lands and which source pixels feed
    /// it, independent of the reference: a 7×5 source (odd in both directions,
    /// so the last column and row are dropped). R, G and B are uniform within
    /// each block, so its average is that colour however the light is
    /// averaged, and differ from block to block (and in the dropped column and
    /// row); alpha is a linear ramp whose block sums are multiples of 4, so
    /// the expected values hold whatever the rounding of the average.
    #[test]
    fn downsample_puts_each_block_average_at_its_own_position() {
        let mut source = RgbaImage::new(7, 5);
        for y in 0..5u32 {
            for x in 0..7u32 {
                let (bx, by) = (x / 2, y / 2);
                source.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        (40 * bx + 10) as u8,
                        (60 * by + 20) as u8,
                        (30 * bx + 50 * by + 5) as u8,
                        (250 - 10 * x - 30 * y) as u8,
                    ]),
                );
            }
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.dimensions(), (3, 2));
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            10, 20, 5, 230,    50, 20, 35, 210,    90, 20, 65, 190,
            10, 80, 55, 170,   50, 80, 85, 150,    90, 80, 115, 130,
        ];
        assert_eq!(downsampled.as_raw(), &expected);
    }

    /// Release-build timing of the full 4096×4096 chain, the raw-slice
    /// filter and its in-place form against the per-pixel reference, single
    /// thread. The three alternate run by run so all see the same machine
    /// load. Run with `cargo test --release -p xearthlayer --lib
    /// time_the_4096_mip_chain -- --ignored --nocapture`. The in-place gain
    /// shows only with `MALLOC_MMAP_THRESHOLD_=1048576` in front of the
    /// command, XEarthLayer's own setting on glibc (#227): glibc's default
    /// threshold adapts upward and serves the levels from the heap anyway.
    #[test]
    #[ignore]
    fn time_the_4096_mip_chain() {
        const RUNS: usize = 21;
        let source = noise(4096, 4096, 389);

        // (whole chain, 4096 -> 2048 step) of one run.
        let run = |downsample: fn(&RgbaImage) -> RgbaImage| {
            let started = std::time::Instant::now();
            let mut level = downsample(&source);
            let first = started.elapsed();
            while level.width() > 1 && level.height() > 1 {
                level = downsample(&level);
            }
            let chain = started.elapsed();
            std::hint::black_box(&level);
            (chain, first)
        };
        // The same, in place: each run gets its own copy of the source, made
        // before the clock starts.
        let run_in_place = || {
            let mut level = source.clone();
            let started = std::time::Instant::now();
            MipmapGenerator::downsample_box_2x_in_place(&mut level);
            let first = started.elapsed();
            while level.width() > 1 && level.height() > 1 {
                MipmapGenerator::downsample_box_2x_in_place(&mut level);
            }
            let chain = started.elapsed();
            std::hint::black_box(&level);
            (chain, first)
        };

        let mut reference = Vec::with_capacity(RUNS);
        let mut raw = Vec::with_capacity(RUNS);
        let mut in_place = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            reference.push(run(downsample_box_2x_reference));
            raw.push(run(MipmapGenerator::downsample_box_2x));
            in_place.push(run_in_place());
        }
        let summary = |runs: &[(std::time::Duration, std::time::Duration)]| {
            let mut chain: Vec<_> = runs.iter().map(|r| r.0).collect();
            let mut first: Vec<_> = runs.iter().map(|r| r.1).collect();
            chain.sort();
            first.sort();
            format!(
                "chain median {:.2?} (min {:.2?}), 4096->2048 median {:.2?}",
                chain[RUNS / 2],
                chain[0],
                first[RUNS / 2]
            )
        };
        println!(
            "4096 chain, {RUNS} runs each: reference {}; raw slices {}; in place {}",
            summary(&reference),
            summary(&raw),
            summary(&in_place)
        );
    }

    #[test]
    fn test_downsample_256_to_128() {
        let source = RgbaImage::new(256, 256);
        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 128);
        assert_eq!(downsampled.height(), 128);
    }

    #[test]
    fn test_downsample_4096_to_2048() {
        let source = RgbaImage::new(4096, 4096);
        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 2048);
        assert_eq!(downsampled.height(), 2048);
    }

    #[test]
    fn test_downsample_solid_color() {
        let mut source = RgbaImage::new(4, 4);
        // Fill with solid red
        for pixel in source.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]);
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // Should still be solid red
        for pixel in downsampled.pixels() {
            assert_eq!(pixel[0], 255);
            assert_eq!(pixel[1], 0);
            assert_eq!(pixel[2], 0);
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn test_downsample_checkerboard() {
        let source = checkerboard_4x4();

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // Each 2x2 block holds 2 black and 2 white pixels: gray (188 in
        // linear light, see a_black_and_white_block_averages_to_half_the_light)
        for pixel in downsampled.pixels() {
            // Should be some shade of gray
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 255); // Alpha unchanged
        }
    }

    #[test]
    fn test_generate_chain_256() {
        let source = RgbaImage::new(256, 256);
        let chain = MipmapGenerator::generate_chain(&source);

        // 256 → 128 → 64 → 32 → 16 → 8 → 4 → 2 → 1
        assert_eq!(chain.len(), 9);

        assert_eq!(chain[0].dimensions(), (256, 256));
        assert_eq!(chain[1].dimensions(), (128, 128));
        assert_eq!(chain[2].dimensions(), (64, 64));
        assert_eq!(chain[3].dimensions(), (32, 32));
        assert_eq!(chain[4].dimensions(), (16, 16));
        assert_eq!(chain[5].dimensions(), (8, 8));
        assert_eq!(chain[6].dimensions(), (4, 4));
        assert_eq!(chain[7].dimensions(), (2, 2));
        assert_eq!(chain[8].dimensions(), (1, 1));
    }

    #[test]
    fn test_generate_chain_4096() {
        let source = RgbaImage::new(4096, 4096);
        let chain = MipmapGenerator::generate_chain(&source);

        // 4096 → 2048 → 1024 → 512 → 256 → 128 → 64 → 32 → 16 → 8 → 4 → 2 → 1
        assert_eq!(chain.len(), 13);

        assert_eq!(chain[0].dimensions(), (4096, 4096));
        assert_eq!(chain[1].dimensions(), (2048, 2048));
        assert_eq!(chain[2].dimensions(), (1024, 1024));
        assert_eq!(chain[3].dimensions(), (512, 512));
        assert_eq!(chain[4].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_with_count() {
        let source = RgbaImage::new(4096, 4096);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 5);

        // Should generate exactly 5 levels
        assert_eq!(chain.len(), 5);

        assert_eq!(chain[0].dimensions(), (4096, 4096));
        assert_eq!(chain[1].dimensions(), (2048, 2048));
        assert_eq!(chain[2].dimensions(), (1024, 1024));
        assert_eq!(chain[3].dimensions(), (512, 512));
        assert_eq!(chain[4].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_with_count_exceeds_possible() {
        let source = RgbaImage::new(4, 4);
        // Request 10 levels but can only generate 3 (4→2→1)
        let chain = MipmapGenerator::generate_chain_with_count(&source, 10);

        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].dimensions(), (4, 4));
        assert_eq!(chain[1].dimensions(), (2, 2));
        assert_eq!(chain[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_generate_chain_single_level() {
        let source = RgbaImage::new(256, 256);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 1);

        // Should only have original
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].dimensions(), (256, 256));
    }

    #[test]
    fn test_generate_chain_4x4() {
        let source = RgbaImage::new(4, 4);
        let chain = MipmapGenerator::generate_chain(&source);

        // 4 → 2 → 1
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].dimensions(), (4, 4));
        assert_eq!(chain[1].dimensions(), (2, 2));
        assert_eq!(chain[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_mipmap_preserves_average_color() {
        let source = gradient_4x4();

        let chain = MipmapGenerator::generate_chain(&source);

        // The 1×1 mipmap should approximate the average color
        let final_pixel = chain.last().unwrap().get_pixel(0, 0);

        // Average value should be somewhere in middle range
        assert!(
            final_pixel[0] > 50 && final_pixel[0] < 200,
            "Final mipmap should have mid-range color, got {}",
            final_pixel[0]
        );
    }

    /// Every level a stream lends, copied out.
    fn stream_levels(image: RgbaImage, count: usize) -> Vec<RgbaImage> {
        let mut stream = MipmapStream::new(image, count);
        let mut levels = Vec::new();
        while let Some(level) = stream.next_level() {
            levels.push(level.clone());
        }
        levels
    }

    #[test]
    fn test_mipmap_stream_yields_correct_count() {
        let levels = stream_levels(RgbaImage::new(256, 256), 5);
        assert_eq!(levels.len(), 5);
    }

    #[test]
    fn test_mipmap_stream_dimensions_halve_each_level() {
        let levels = stream_levels(RgbaImage::new(256, 256), 5);

        assert_eq!(levels[0].dimensions(), (256, 256));
        assert_eq!(levels[1].dimensions(), (128, 128));
        assert_eq!(levels[2].dimensions(), (64, 64));
        assert_eq!(levels[3].dimensions(), (32, 32));
        assert_eq!(levels[4].dimensions(), (16, 16));
    }

    #[test]
    fn test_mipmap_stream_first_yield_matches_source_dimensions() {
        let image = RgbaImage::new(4096, 4096);
        let mut stream = MipmapStream::new(image, 3);
        let first = stream.next_level().unwrap();
        assert_eq!(first.dimensions(), (4096, 4096));
    }

    #[test]
    fn test_mipmap_stream_terminates_early_at_1x1() {
        // Request 10 levels but only 3 are possible (4 -> 2 -> 1)
        let levels = stream_levels(RgbaImage::new(4, 4), 10);

        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].dimensions(), (4, 4));
        assert_eq!(levels[1].dimensions(), (2, 2));
        assert_eq!(levels[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_mipmap_stream_single_level() {
        let levels = stream_levels(RgbaImage::new(256, 256), 1);

        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].dimensions(), (256, 256));
    }

    #[test]
    fn test_mipmap_stream_preserves_pixel_data() {
        let mut image = RgbaImage::new(4, 4);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]);
        }

        let mut stream = MipmapStream::new(image, 2);
        let first = stream.next_level().unwrap();

        // First level should have the same pixel data as the source
        for pixel in first.pixels() {
            assert_eq!(pixel[0], 255);
            assert_eq!(pixel[1], 0);
            assert_eq!(pixel[2], 0);
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn test_mipmap_stream_zero_count_yields_nothing() {
        let levels = stream_levels(RgbaImage::new(256, 256), 0);
        assert_eq!(levels.len(), 0);
    }

    #[test]
    fn a_stream_that_has_ended_stays_ended() {
        let mut stream = MipmapStream::new(RgbaImage::new(2, 2), 5);
        assert!(stream.next_level().is_some());
        assert!(stream.next_level().is_some());
        assert!(stream.next_level().is_none());
        assert!(stream.next_level().is_none());
    }

    const IN_PLACE_SHAPES: [(u32, u32); 12] = [
        (7, 5),
        (5, 7),
        (2, 2),
        (1, 4),
        (4, 1),
        (3, 3),
        (1, 1),
        (2, 1),
        (9, 2),
        (256, 256),
        (64, 16),
        (130, 66),
    ];

    #[test]
    fn in_place_is_byte_for_byte_the_allocating_downsample() {
        for (w, h) in IN_PLACE_SHAPES {
            let source = noise(w, h, u64::from(w) * 1000 + u64::from(h));
            let expected = MipmapGenerator::downsample_box_2x(&source);
            let mut actual = source.clone();
            MipmapGenerator::downsample_box_2x_in_place(&mut actual);
            assert_same_image(&actual, &expected, &format!("{w}x{h}"));
        }
    }

    #[test]
    fn the_levels_of_a_stream_are_the_levels_of_generate_chain_with_count() {
        for (w, h) in IN_PLACE_SHAPES {
            for count in [0, 1, 2, 3, 5, 20] {
                let source = noise(w, h, 7 + u64::from(w) + u64::from(h) * 31);
                let expected = MipmapGenerator::generate_chain_with_count(&source, count);
                // `generate_chain_with_count` always holds the source, so a
                // count of 0 gives one level where the stream gives none.
                let expected = &expected[..expected.len().min(count)];
                let actual = stream_levels(source, count);
                assert_eq!(actual.len(), expected.len(), "{w}x{h} count {count}");
                for (level, (a, e)) in actual.iter().zip(expected).enumerate() {
                    assert_same_image(a, e, &format!("{w}x{h} count {count} level {level}"));
                }
            }
        }
    }

    #[test]
    fn test_full_chain_count_square() {
        // 4096×4096: 4096→2048→1024→512→256→128→64→32→16→8→4→2→1 = 13 levels
        assert_eq!(MipmapGenerator::full_chain_count(4096, 4096), 13);
    }

    #[test]
    fn test_full_chain_count_small() {
        // 4×4: 4→2→1 = 3 levels
        assert_eq!(MipmapGenerator::full_chain_count(4, 4), 3);
    }

    #[test]
    fn test_full_chain_count_1x1() {
        // Already at minimum
        assert_eq!(MipmapGenerator::full_chain_count(1, 1), 1);
    }

    #[test]
    fn test_full_chain_count_256() {
        // 256→128→64→32→16→8→4→2→1 = 9 levels
        assert_eq!(MipmapGenerator::full_chain_count(256, 256), 9);
    }

    #[test]
    fn test_full_chain_count_matches_generate_chain() {
        let source = RgbaImage::new(256, 256);
        let chain = MipmapGenerator::generate_chain(&source);
        assert_eq!(MipmapGenerator::full_chain_count(256, 256), chain.len(),);
    }

    #[test]
    fn test_downsample_preserves_alpha() {
        let source = constant_alpha_4x4();

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        // All pixels should have alpha around 200
        for pixel in downsampled.pixels() {
            assert_eq!(pixel[3], 200, "Alpha should be preserved");
        }
    }

    #[test]
    fn test_downsample_averages_correctly() {
        let source = corners_2x2();

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.width(), 1);
        assert_eq!(downsampled.height(), 1);

        let avg = downsampled.get_pixel(0, 0);

        // Each channel is 100 in one pixel and 0 in three: a quarter of the
        // light of sRGB 100 (0.1274 linear, a quarter 0.0319) is sRGB 50,
        // not the 25 that averaging the stored values gives.
        assert_eq!(avg[0], 50);
        assert_eq!(avg[1], 50);
        assert_eq!(avg[2], 50);
        assert_eq!(avg[3], 255);
    }
}
