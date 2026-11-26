//! Mipmap chain generation for DDS textures.

use image::RgbaImage;

/// How a mip level averages each 2×2 block of the level above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MipFilter {
    /// The stored bytes, averaged and rounded half to even (#394):
    /// [`MipmapGenerator::downsample_box_2x`].
    StoredValues,
    /// R, G and B averaged in linear light, alpha as stored (#467, after
    /// #458). Implemented on the GPU only (`dds::gpu_mips`), as a tested
    /// option: no chain uses it until the CPU chain does.
    LinearLight,
}

/// Mipmap generator.
pub struct MipmapGenerator;

impl MipmapGenerator {
    /// The filter of every mip chain XEarthLayer encodes. It describes the
    /// CPU chain ([`downsample_box_2x`](Self::downsample_box_2x)), which
    /// does not read it: a test (`the_filter_describes_the_cpu_chain`)
    /// holds the value to what the chain does. The GPU worker's chain
    /// reads it (#424, `dds::gpu_mips::gpu_filter`), so a tile's levels are
    /// the same bytes on either path. #467 changes this one value, with the
    /// CPU's filter, to switch both.
    pub const FILTER: MipFilter = MipFilter::StoredValues;

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

    /// Downsample image by 2× using box filter (simple average).
    ///
    /// Each output pixel is the average of a 2×2 block of input pixels, per
    /// channel, alpha included: `(a + b + c + d) / 4` rounded to the nearest
    /// step, ties to even ([`block_average`], #394), so the chain does not
    /// darken or brighten level by level. An odd last column or row has no
    /// partner and is dropped, so a `w×h` source gives `w/2 × h/2` (a 1-wide
    /// or 1-high source gives an empty image).
    ///
    /// Works on the raw RGBA bytes, one output row at a time and in three
    /// passes, with no per-pixel bounds checks (#389, #394):
    ///
    /// 1. the row's two source rows are added byte by byte into a `u16`
    ///    scratch row (a plain element-wise loop the compiler vectorises);
    /// 2. each 2×2 block's sum, per channel, is the sum of two neighbouring
    ///    entries of that row, one pixel (4 entries) apart, written to a
    ///    second `u16` scratch row with an entry per output byte;
    /// 3. that row is averaged into the output byte by byte by
    ///    [`block_average`], again a plain element-wise loop the compiler
    ///    vectorises.
    ///
    /// The rounding is a pass of its own because, folded into pass 2, the
    /// compiler vectorises it across the one-pixel stride with so many
    /// shuffles that the 4096×4096 chain took twice as long as with the
    /// truncating average it replaced; as its own pass it adds about a tenth.
    ///
    /// The two scratch rows are the only allocations besides the output:
    /// `Vec<u16>`s with an entry per byte of a source row (less an odd last
    /// pixel) and per byte of an output row, allocated once per call and
    /// reused for every row. Both encoder backends run this through
    /// [`MipmapStream`] for every level of every tile.
    pub(crate) fn downsample_box_2x(source: &RgbaImage) -> RgbaImage {
        const CHANNELS: usize = 4;

        let new_width = source.width() / 2;
        let new_height = source.height() / 2;
        if new_width == 0 || new_height == 0 {
            // `chunks_exact` refuses a zero chunk size.
            return RgbaImage::new(new_width, new_height);
        }

        let src_row = source.width() as usize * CHANNELS;
        let out_row = new_width as usize * CHANNELS;
        let mut output = vec![0u8; out_row * new_height as usize];
        // Column sums of the source pixels that have a horizontal partner:
        // an odd last source pixel is left out here, as the halved width
        // requires.
        let mut column_sums = vec![0u16; 2 * out_row];
        // The 2×2 block sums of one output row, an entry per output byte.
        let mut block_sums = vec![0u16; out_row];

        // `chunks_exact` drops an odd last source row, as the halved height
        // requires.
        for (out, rows) in output
            .chunks_exact_mut(out_row)
            .zip(source.as_raw().chunks_exact(2 * src_row))
        {
            let (top, bottom) = rows.split_at(src_row);
            for ((sum, &t), &b) in column_sums.iter_mut().zip(top).zip(bottom) {
                *sum = u16::from(t) + u16::from(b);
            }

            for (block, pair) in block_sums
                .chunks_exact_mut(CHANNELS)
                .zip(column_sums.chunks_exact(2 * CHANNELS))
            {
                for c in 0..CHANNELS {
                    block[c] = pair[c] + pair[c + CHANNELS];
                }
            }

            for (byte, &sum) in out.iter_mut().zip(&block_sums) {
                *byte = block_average(sum);
            }
        }

        RgbaImage::from_raw(new_width, new_height, output)
            .expect("output buffer is sized from the halved dimensions")
    }
}

/// The average of a 2×2 block's four channel values, from their sum
/// (`0..=1020`), rounded to the nearest step with ties to even (#394).
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
/// odd quotient. Alpha goes through it too, and an opaque block stays
/// opaque: 1020 gives 255.
#[inline(always)]
const fn block_average(sum: u16) -> u8 {
    ((sum + 1 + ((sum >> 2) & 1)) >> 2) as u8
}

/// Streaming mipmap generator that yields one level at a time.
///
/// Takes ownership of the source image and produces mipmap levels
/// incrementally. Each call to `next()` returns the next downsampled
/// level. The previous level is consumed during downsampling, so only
/// one uncompressed level is held in memory at a time.
///
/// # Memory
///
/// Peak memory is the size of the current level plus the downsampled
/// output — roughly 1.25× the current level, not the entire chain.
pub struct MipmapStream {
    /// The next image to yield (or downsample from).
    current: Option<RgbaImage>,
    /// Number of levels remaining to yield.
    remaining: usize,
    /// Whether the first (original) image has been yielded.
    yielded_first: bool,
}

impl MipmapStream {
    /// Create a new mipmap stream from an owned source image.
    ///
    /// # Arguments
    ///
    /// * `image` - Source image (ownership transferred, no clone)
    /// * `count` - Total number of mipmap levels to yield (including the original)
    pub fn new(image: RgbaImage, count: usize) -> Self {
        Self {
            current: if count > 0 { Some(image) } else { None },
            remaining: count,
            yielded_first: false,
        }
    }
}

impl Iterator for MipmapStream {
    type Item = RgbaImage;

    fn next(&mut self) -> Option<RgbaImage> {
        if self.remaining == 0 {
            return None;
        }

        let current = self.current.take()?;

        if !self.yielded_first {
            // First call: yield the original image (moved, not cloned)
            self.yielded_first = true;
            self.remaining -= 1;

            // Prepare next level if we need more
            if self.remaining > 0 && current.width() > 1 && current.height() > 1 {
                self.current = Some(MipmapGenerator::downsample_box_2x(&current));
            }

            return Some(current);
        }

        // Subsequent calls: yield the pre-computed downsample
        self.remaining -= 1;

        // Prepare the next level if we need more
        if self.remaining > 0 && current.width() > 1 && current.height() > 1 {
            self.current = Some(MipmapGenerator::downsample_box_2x(&current));
        }

        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sum / 4` rounded to the nearest whole step, a tie going to the even
    /// one: round half to even, by its definition, in exact arithmetic on the
    /// rational `sum / 4 = q + r/4` (no bit tricks, so it is independent of
    /// the formula the fast path uses).
    fn round_half_to_even_quarter(sum: u16) -> u8 {
        let (q, r) = (sum / 4, sum % 4);
        // r/4 against 1/2, compared as 2r against 4.
        let rounded = match (2 * r).cmp(&4) {
            std::cmp::Ordering::Less => q,
            std::cmp::Ordering::Greater => q + 1,
            std::cmp::Ordering::Equal if q % 2 == 0 => q,
            std::cmp::Ordering::Equal => q + 1,
        };
        u8::try_from(rounded).expect("a sum of four bytes averages to a byte")
    }

    /// The per-pixel `downsample_box_2x` that the raw-slice loop replaced
    /// (#389), kept as the independent reference the fast path must match
    /// byte for byte. Since #394 each block average is rounded half to even
    /// by [`round_half_to_even_quarter`], the definition, where the fast path
    /// uses a bit trick.
    fn downsample_box_2x_reference(source: &RgbaImage) -> RgbaImage {
        let new_width = source.width() / 2;
        let new_height = source.height() / 2;

        let mut output = RgbaImage::new(new_width, new_height);

        for y in 0..new_height {
            for x in 0..new_width {
                // Average 2×2 block
                let p00 = source.get_pixel(x * 2, y * 2);
                let p10 = source.get_pixel(x * 2 + 1, y * 2);
                let p01 = source.get_pixel(x * 2, y * 2 + 1);
                let p11 = source.get_pixel(x * 2 + 1, y * 2 + 1);

                let avg = image::Rgba(std::array::from_fn(|c| {
                    round_half_to_even_quarter(
                        p00[c] as u16 + p10[c] as u16 + p01[c] as u16 + p11[c] as u16,
                    )
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

    /// The mean of each channel over every pixel of `image`, in steps.
    fn channel_means(image: &RgbaImage) -> [f64; 4] {
        let mut sums = [0u64; 4];
        for pixel in image.as_raw().chunks_exact(4) {
            for (sum, &value) in sums.iter_mut().zip(pixel) {
                *sum += u64::from(value);
            }
        }
        let pixels = f64::from(image.width()) * f64::from(image.height());
        sums.map(|sum| sum as f64 / pixels)
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
    /// sum four bytes can make, all 1021 of `0..=1020`, against the
    /// definition.
    #[test]
    fn the_block_average_rounds_half_to_even_for_every_sum() {
        let sums = 0..=4 * u16::from(u8::MAX);
        assert_eq!(sums.len(), 1021);
        for sum in sums {
            assert_eq!(
                block_average(sum),
                round_half_to_even_quarter(sum),
                "sum {sum}"
            );
        }
    }

    /// [`MipmapGenerator::FILTER`] describes the CPU chain, which does not
    /// read it: the chain's levels are what the filter it names gives,
    /// computed here by its definition. A filter the CPU does not implement
    /// fails here, until the CPU chain implements it (#467).
    #[test]
    fn the_filter_describes_the_cpu_chain() {
        let source = noise(64, 48, 7);
        let chain = MipmapGenerator::generate_chain_with_count(&source, 4);
        for (level, pair) in chain.windows(2).enumerate() {
            let expected = match MipmapGenerator::FILTER {
                MipFilter::StoredValues => downsample_box_2x_reference(&pair[0]),
                MipFilter::LinearLight => panic!("the CPU chain does not average in linear light"),
            };
            assert_same_image(&pair[1], &expected, &format!("level {}", level + 1));
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
        let mut levels = 0;
        for (level, actual) in MipmapStream::new(source, count).enumerate() {
            if level > 0 {
                expected = downsample_box_2x_reference(&expected);
            }
            assert_same_image(&actual, &expected, &format!("level {level}"));
            levels += 1;
        }
        assert_eq!(levels, 13);
    }

    /// The chain must not change the brightness of a tile with distance
    /// (#394): at every level of a 4096×4096 chain, each channel's mean stays
    /// within half a step of the source's. A truncating average loses 0.375
    /// of a step a level and fails at level 2; rounding half up gains 0.125
    /// a level and fails at level 4 or 5; rounding half to even does not
    /// drift.
    ///
    /// The source's bytes are uniform over `0..=254`, so its channel means
    /// are close to a whole step. The 1×1 level is one pixel, a whole step,
    /// and a source mean half way between two steps would leave it half a
    /// step off however the chain rounds.
    #[test]
    fn every_level_keeps_the_source_mean_within_half_a_step() {
        let source = noise_with_whole_step_mean(4096, 4096, 394);
        let source_means = channel_means(&source);
        let count = MipmapGenerator::full_chain_count(4096, 4096);

        let mut levels = 0;
        for (level, image) in MipmapStream::new(source, count).enumerate() {
            let means = channel_means(&image);
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

    /// Pins where each 2×2 block's average lands and which source pixels feed
    /// it, independent of the reference: a 7×5 source (odd in both directions,
    /// so the last column and row are dropped) with every channel a different
    /// linear ramp. Each block sum is a multiple of 4, so the expected values
    /// hold whatever the rounding of the average.
    #[test]
    fn downsample_puts_each_block_average_at_its_own_position() {
        let mut source = RgbaImage::new(7, 5);
        for y in 0..5u32 {
            for x in 0..7u32 {
                source.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        (40 * x) as u8,
                        (40 * y) as u8,
                        (20 * x + 20 * y + 10) as u8,
                        (250 - 10 * x - 30 * y) as u8,
                    ]),
                );
            }
        }

        let downsampled = MipmapGenerator::downsample_box_2x(&source);

        assert_eq!(downsampled.dimensions(), (3, 2));
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            20, 20, 30, 230,   100, 20, 70, 210,    180, 20, 110, 190,
            20, 100, 70, 170,  100, 100, 110, 150,  180, 100, 150, 130,
        ];
        assert_eq!(downsampled.as_raw(), &expected);
    }

    /// Release-build timing of the full 4096×4096 chain, new against the
    /// per-pixel reference, single thread. The two alternate run by run so
    /// both see the same machine load. Run with `cargo test --release -p
    /// xearthlayer --lib time_the_4096_mip_chain -- --ignored --nocapture`.
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

        let mut reference = Vec::with_capacity(RUNS);
        let mut raw = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            reference.push(run(downsample_box_2x_reference));
            raw.push(run(MipmapGenerator::downsample_box_2x));
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
            "4096 chain, {RUNS} runs each: reference {}; raw slices {}",
            summary(&reference),
            summary(&raw)
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

        // Each pixel should be gray (average of black and white)
        // Some pixels have 2 black + 2 white = gray (127-128)
        // Others might have different ratios
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

    #[test]
    fn test_mipmap_stream_yields_correct_count() {
        let image = RgbaImage::new(256, 256);
        let stream = MipmapStream::new(image, 5);
        let levels: Vec<RgbaImage> = stream.collect();
        assert_eq!(levels.len(), 5);
    }

    #[test]
    fn test_mipmap_stream_dimensions_halve_each_level() {
        let image = RgbaImage::new(256, 256);
        let stream = MipmapStream::new(image, 5);
        let levels: Vec<RgbaImage> = stream.collect();

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
        let first = stream.next().unwrap();
        assert_eq!(first.dimensions(), (4096, 4096));
    }

    #[test]
    fn test_mipmap_stream_terminates_early_at_1x1() {
        let image = RgbaImage::new(4, 4);
        // Request 10 levels but only 3 are possible (4 -> 2 -> 1)
        let stream = MipmapStream::new(image, 10);
        let levels: Vec<RgbaImage> = stream.collect();

        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].dimensions(), (4, 4));
        assert_eq!(levels[1].dimensions(), (2, 2));
        assert_eq!(levels[2].dimensions(), (1, 1));
    }

    #[test]
    fn test_mipmap_stream_single_level() {
        let image = RgbaImage::new(256, 256);
        let stream = MipmapStream::new(image, 1);
        let levels: Vec<RgbaImage> = stream.collect();

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
        let first = stream.next().unwrap();

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
        let image = RgbaImage::new(256, 256);
        let stream = MipmapStream::new(image, 0);
        let levels: Vec<RgbaImage> = stream.collect();
        assert_eq!(levels.len(), 0);
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

        // Average: R=(0+100+0+0)/4=25, G=(0+0+100+0)/4=25, B=(0+0+0+100)/4=25
        assert_eq!(avg[0], 25);
        assert_eq!(avg[1], 25);
        assert_eq!(avg[2], 25);
        assert_eq!(avg[3], 255);
    }
}
