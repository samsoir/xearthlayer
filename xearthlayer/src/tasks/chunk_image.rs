//! What counts as a usable imagery chunk (#388).
//!
//! A chunk is usable when its bytes are an image that `image` can read and
//! that is exactly [`CHUNK_PIXELS`] by [`CHUNK_PIXELS`]. The rule is applied
//! at the two places a chunk can turn out to be unusable:
//!
//! - [`check_header`], before a downloaded body is written to the chunk disk
//!   cache. It reads the format and dimensions only, no pixels, so it costs
//!   microseconds. A body that fails it is a failed download, never a chunk
//!   cache entry: an error page from a proxy, a CDN or a captive portal is
//!   retried like a transport error.
//! - [`decode`], when a tile is assembled, for every chunk whether it was
//!   downloaded or read from the chunk cache. A chunk that fails it is painted
//!   magenta and makes the tile incomplete, which is served but never cached
//!   (#180), and is discarded from the chunk cache. Assembly calls it through
//!   [`decode_into`], which gives exactly its result, faster for JPEG (#390).
//!
//! The format is whatever `image` recognises from the bytes, not JPEG
//! specifically: the providers have not been audited for returning JPEG only
//! (see [`crate::cache::integrity::raw_chunk_validator`]).

use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::fmt;
use std::io::Cursor;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Width and height of every chunk, in pixels.
pub(crate) const CHUNK_PIXELS: u32 = 256;

/// Bytes in one row of a decoded chunk: [`CHUNK_PIXELS`] RGBA pixels.
pub(crate) const CHUNK_ROW_BYTES: usize = CHUNK_PIXELS as usize * 4;

/// The bytes `image` recognises a JPEG by: the start-of-image marker and the
/// first byte of the next marker.
const JPEG_SIGNATURE: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// The alpha of every pixel decoded from a JPEG, which has no alpha.
const OPAQUE: u8 = u8::MAX;

/// Why a chunk's bytes are not a usable chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChunkImageError {
    /// The bytes are not an image `image` can read: an unrecognised format,
    /// or a header or pixel data it cannot parse.
    NotAnImage(String),
    /// An image, but not [`CHUNK_PIXELS`] square.
    WrongSize { width: u32, height: u32 },
}

impl fmt::Display for ChunkImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnImage(detail) => write!(f, "not an image ({detail})"),
            Self::WrongSize { width, height } => write!(
                f,
                "a {width}x{height} image, not {CHUNK_PIXELS}x{CHUNK_PIXELS}"
            ),
        }
    }
}

/// Checks that `bytes` start like a usable chunk: a format `image`
/// recognises, with a header it can parse that says [`CHUNK_PIXELS`] square.
///
/// Reads the header only, never the pixels, so a body that passes may still
/// fail [`decode`] (corrupt or truncated pixel data).
pub(crate) fn check_header(bytes: &[u8]) -> Result<(), ChunkImageError> {
    chunk_decoder(bytes).map(drop)
}

/// Decodes a usable chunk to RGBA.
///
/// The size is checked from the header before any pixel is decoded, so a
/// wrong-sized chunk costs no more than [`check_header`].
pub(crate) fn decode(bytes: &[u8]) -> Result<RgbaImage, ChunkImageError> {
    let decoder = chunk_decoder(bytes)?;
    DynamicImage::from_decoder(decoder)
        .map(DynamicImage::into_rgba8)
        .map_err(|e| ChunkImageError::NotAnImage(e.to_string()))
}

/// One decoded chunk: [`CHUNK_PIXELS`] rows of [`CHUNK_ROW_BYTES`] of RGBA,
/// written by [`decode_into`].
///
/// Assembly reuses one for every chunk of a tile, so a tile allocates it
/// once instead of three buffers per chunk (#390).
pub(crate) struct ChunkPixels {
    rgba: Box<[u8]>,
}

impl ChunkPixels {
    pub(crate) fn new() -> Self {
        Self {
            rgba: vec![0; CHUNK_ROW_BYTES * CHUNK_PIXELS as usize].into_boxed_slice(),
        }
    }

    /// The chunk's rows, top to bottom, each [`CHUNK_ROW_BYTES`] long.
    pub(crate) fn rows(&self) -> std::slice::ChunksExact<'_, u8> {
        self.rgba.chunks_exact(CHUNK_ROW_BYTES)
    }
}

/// Decodes a usable chunk into `pixels`, with exactly [`decode`]'s result:
/// the same chunks accepted, decoded to the same bytes, and the same error
/// for every other. After an error `pixels` holds no particular chunk.
///
/// A JPEG in YCbCr or grayscale, which is what imagery providers serve, is
/// decoded by `zune-jpeg` straight to RGBA in `pixels`. That is the decoder
/// `image` runs for JPEG, with `image`'s options, so the two differ only in
/// the colour conversion's output: RGBA, against RGB that [`decode`] then
/// converts to RGBA in another buffer. The RGB arithmetic is the same, and
/// the tests compare the bytes. It saves `image`'s copy of the input and
/// its second header parse, the RGB and RGBA buffers, and the pass between
/// them. Every other chunk goes to [`decode`]: other formats,
/// other JPEG colour spaces (zune has no RGB to RGBA conversion, and
/// converts CMYK and YCCK to RGBA differently from to RGB), and JPEGs zune
/// does not decode whole.
pub(crate) fn decode_into(bytes: &[u8], pixels: &mut ChunkPixels) -> Result<(), ChunkImageError> {
    if decode_jpeg_into(bytes, &mut pixels.rgba) {
        return Ok(());
    }
    let image = decode(bytes)?;
    pixels.rgba.copy_from_slice(image.as_raw());
    Ok(())
}

/// The fast path of [`decode_into`]: decodes a 3 component YCbCr or 1
/// component grayscale JPEG of [`CHUNK_PIXELS`] square into `rgba` and
/// returns whether it did.
///
/// It never rejects a chunk: `false` leaves the chunk to [`decode`], which
/// decides, so every rejection and its error are [`decode`]'s own.
///
/// zune, like `image`, decodes non-strictly: a damaged JPEG is rendered as
/// far as it goes and still returns `Ok`. It fills the rest with byte 128,
/// which in RGBA is an alpha of 128 where [`decode`] has opaque grey, or,
/// after a stray marker and an end of image, leaves the rest unwritten,
/// which here is the previous chunk where [`decode`] has opaque black from
/// its fresh buffer. zune writes whole rows, and every row it converts ends
/// in an opaque pixel, so the last alpha of each row is cleared before
/// decoding: a row that does not read opaque after it was not converted, and
/// the chunk goes to [`decode`].
fn decode_jpeg_into(bytes: &[u8], rgba: &mut [u8]) -> bool {
    if !bytes.starts_with(&JPEG_SIGNATURE) {
        return false;
    }
    // `image`'s options (its `JpegDecoder`), with RGBA out.
    let options = DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    if decoder.decode_headers().is_err() {
        return false;
    }
    let side = CHUNK_PIXELS as usize;
    if decoder.dimensions() != Some((side, side)) {
        return false;
    }
    // Only the colour spaces whose RGBA conversion is their RGB one with an
    // opaque alpha, and only with the components they have: the colour space
    // the headers give is settled again when zune decodes (two YCbCr
    // components become two bands, a four component frame relabelled YCbCr
    // stays four), and zune picks the components to decode by the output
    // colour space, so for any other frame RGBA and RGB need not decode alike.
    let colour_space = decoder.input_colorspace();
    let components = decoder.info().map(|info| usize::from(info.components));
    if !matches!(colour_space, Some(ColorSpace::YCbCr | ColorSpace::Luma))
        || components != colour_space.map(|space| space.num_components())
    {
        return false;
    }

    let last_alpha = CHUNK_ROW_BYTES - 1;
    for row in rgba.chunks_exact_mut(CHUNK_ROW_BYTES) {
        row[last_alpha] = 0;
    }
    decoder.decode_into(rgba).is_ok()
        && rgba
            .chunks_exact(CHUNK_ROW_BYTES)
            .all(|row| row[last_alpha] == OPAQUE)
}

/// A decoder for `bytes` whose header has been read and says the image is
/// [`CHUNK_PIXELS`] square. The format is guessed from the bytes' signature.
fn chunk_decoder(bytes: &[u8]) -> Result<impl ImageDecoder + '_, ChunkImageError> {
    let not_an_image = |e: &dyn fmt::Display| ChunkImageError::NotAnImage(e.to_string());
    let decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| not_an_image(&e))?
        .into_decoder()
        .map_err(|e| not_an_image(&e))?;
    match decoder.dimensions() {
        (CHUNK_PIXELS, CHUNK_PIXELS) => Ok(decoder),
        (width, height) => Err(ChunkImageError::WrongSize { width, height }),
    }
}

/// Encoded chunks for tests.
#[cfg(test)]
pub(crate) mod test_chunks {
    use super::CHUNK_PIXELS;
    use bytes::Bytes;
    use image::codecs::jpeg::JpegEncoder;
    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;
    use std::sync::OnceLock;

    /// The colour of every test chunk.
    pub(crate) const COLOUR: Rgb<u8> = Rgb([40, 120, 60]);

    /// A `width` by `height` image of [`COLOUR`] encoded as `format`.
    pub(crate) fn encoded(width: u32, height: u32, format: ImageFormat) -> Bytes {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, COLOUR));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encoding a test chunk");
        Bytes::from(bytes)
    }

    /// A usable chunk: a 256x256 JPEG, encoded once per test binary.
    pub(crate) fn jpeg() -> Bytes {
        static JPEG: OnceLock<Bytes> = OnceLock::new();
        JPEG.get_or_init(|| encoded(CHUNK_PIXELS, CHUNK_PIXELS, ImageFormat::Jpeg))
            .clone()
    }

    /// A body a proxy or captive portal might answer 200 with.
    pub(crate) fn error_page() -> Bytes {
        Bytes::from_static(b"<html><body>502 Bad Gateway</body></html>")
    }

    // ------------------------------------------------------------------
    // The corpus the fast JPEG path is checked against `decode` on (#390)
    // ------------------------------------------------------------------

    /// JPEG chunks written by libjpeg-turbo 3.2.1, in the modes the `image`
    /// encoder cannot write (it writes baseline 4:4:4 and grayscale only),
    /// and in colour spaces and a coding zune does not take the fast path
    /// with. All are 256x256 and
    /// encode the same synthetic image, made with
    /// `magick -seed 390 -size 256x256 plasma:fractal source.ppm`, then:
    ///
    /// | Fixture | Command |
    /// |---|---|
    /// | `ycc420_q75` | `cjpeg -quality 75 -sample 2x2` |
    /// | `ycc420_q60_restart` | `cjpeg -quality 60 -sample 2x2 -optimize -restart 1` |
    /// | `ycc422_q70` | `cjpeg -quality 70 -sample 2x1` |
    /// | `ycc440_q70` | `cjpeg -quality 70 -sample 1x2` |
    /// | `ycc411_q75` | `cjpeg -quality 75 -sample 4x1` |
    /// | `ycc420_q80_progressive` | `cjpeg -quality 80 -sample 2x2 -progressive` |
    /// | `ycc444_q70_progressive` | `cjpeg -quality 70 -sample 1x1 -progressive` |
    /// | `gray_q85_progressive` | `cjpeg -quality 85 -grayscale -progressive` |
    /// | `rgb_q40` | `cjpeg -quality 40 -rgb` (RGB, not YCbCr) |
    /// | `arithmetic_q75` | `cjpeg -quality 75 -arithmetic` |
    /// | `ycck_q40` | `magick source.ppm -colorspace CMYK -quality 40` (YCCK) |
    pub(crate) const FIXTURES: [(&str, &[u8]); 11] = [
        (
            "ycc420_q75",
            include_bytes!("../../tests/fixtures/chunks/ycc420_q75.jpg"),
        ),
        (
            "ycc420_q60_restart",
            include_bytes!("../../tests/fixtures/chunks/ycc420_q60_restart.jpg"),
        ),
        (
            "ycc422_q70",
            include_bytes!("../../tests/fixtures/chunks/ycc422_q70.jpg"),
        ),
        (
            "ycc440_q70",
            include_bytes!("../../tests/fixtures/chunks/ycc440_q70.jpg"),
        ),
        (
            "ycc411_q75",
            include_bytes!("../../tests/fixtures/chunks/ycc411_q75.jpg"),
        ),
        (
            "ycc420_q80_progressive",
            include_bytes!("../../tests/fixtures/chunks/ycc420_q80_progressive.jpg"),
        ),
        (
            "ycc444_q70_progressive",
            include_bytes!("../../tests/fixtures/chunks/ycc444_q70_progressive.jpg"),
        ),
        (
            "gray_q85_progressive",
            include_bytes!("../../tests/fixtures/chunks/gray_q85_progressive.jpg"),
        ),
        (
            "rgb_q40",
            include_bytes!("../../tests/fixtures/chunks/rgb_q40.jpg"),
        ),
        (
            "arithmetic_q75",
            include_bytes!("../../tests/fixtures/chunks/arithmetic_q75.jpg"),
        ),
        (
            "ycck_q40",
            include_bytes!("../../tests/fixtures/chunks/ycck_q40.jpg"),
        ),
    ];

    /// The fixture called `name`.
    pub(crate) fn fixture(name: &str) -> Bytes {
        FIXTURES
            .iter()
            .find(|(fixture, _)| *fixture == name)
            .map(|(_, bytes)| Bytes::from_static(bytes))
            .unwrap_or_else(|| panic!("no fixture {name}"))
    }

    /// A 256x256 image with detail in every block, unlike [`COLOUR`]'s flat
    /// square: a gradient under noise that `seed` varies, and one corner of
    /// saturated primaries, white and black, whose quantised YCbCr falls
    /// outside RGB so the colour conversion has to clamp.
    pub(crate) fn textured(seed: u32) -> RgbImage {
        const EXTREMES: [Rgb<u8>; 6] = [
            Rgb([255, 0, 0]),
            Rgb([0, 255, 0]),
            Rgb([0, 0, 255]),
            Rgb([255, 255, 255]),
            Rgb([0, 0, 0]),
            Rgb([255, 0, 255]),
        ];
        RgbImage::from_fn(CHUNK_PIXELS, CHUNK_PIXELS, |x, y| {
            if x < 64 && y < 64 {
                return EXTREMES[((x / 8 + y / 8) % 6) as usize];
            }
            let hash = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed)
                .wrapping_mul(2_654_435_761);
            let noise = (hash >> 26) as u8;
            Rgb([
                (x as u8).wrapping_add(noise),
                (y as u8).wrapping_sub(noise),
                ((x + y + seed) / 2) as u8 ^ noise,
            ])
        })
    }

    /// `image` encoded as a JPEG of `quality` by the `image` crate: one
    /// component for a grayscale image, three (YCbCr) for any other.
    pub(crate) fn jpeg_of(image: &DynamicImage, quality: u8) -> Bytes {
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
        // A `DynamicImage`'s pixels are RGBA whatever it holds, so encoding
        // one as it is writes three components even for a grayscale image.
        match image {
            DynamicImage::ImageLuma8(gray) => encoder.encode_image(gray),
            other => encoder.encode_image(&other.to_rgb8()),
        }
        .expect("encoding a test chunk");
        Bytes::from(bytes)
    }

    /// `image` encoded as `format`.
    fn encode_as(image: &DynamicImage, format: ImageFormat) -> Bytes {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encoding a test chunk");
        Bytes::from(bytes)
    }

    /// Damaged copies of the JPEG `bytes`, named after `name`: truncated,
    /// missing its end marker, with stray markers inserted into the entropy
    /// coded data, and with bytes overwritten. `image` decodes JPEG
    /// non-strictly, so most of these still decode, in part.
    pub(crate) fn damaged(name: &str, bytes: &[u8]) -> Vec<(String, Bytes)> {
        // Past the headers of every fixture and every `image` encoded chunk.
        let data_start = 700.min(bytes.len() / 2);
        let at = |percent: usize| data_start + (bytes.len() - data_start) * percent / 100;
        let mut damaged = Vec::new();
        for percent in [20, 60, 95] {
            damaged.push((
                format!("{name} truncated at {percent}%"),
                Bytes::copy_from_slice(&bytes[..at(percent)]),
            ));
        }
        damaged.push((
            format!("{name} without its end marker"),
            Bytes::copy_from_slice(&bytes[..bytes.len() - 2]),
        ));
        let markers: [(&str, &[u8]); 4] = [
            // A marker after an MCU row, then the end of the image: zune
            // stops there and returns Ok, leaving the rows below unwritten.
            ("APP1 and EOI", &[0xFF, 0xE1, 0x00, 0x02, 0xFF, 0xD9]),
            ("COM and EOI", &[0xFF, 0xFE, 0x00, 0x02, 0xFF, 0xD9]),
            ("SOS", &[0xFF, 0xDA]),
            ("EOI", &[0xFF, 0xD9]),
        ];
        for (label, marker) in markers {
            for percent in [30, 70] {
                let mut with_marker = bytes[..at(percent)].to_vec();
                with_marker.extend_from_slice(marker);
                with_marker.extend_from_slice(&bytes[at(percent)..]);
                damaged.push((
                    format!("{name} with {label} at {percent}%"),
                    Bytes::from(with_marker),
                ));
            }
        }
        for (i, percent) in [10, 35, 50, 75, 90].into_iter().enumerate() {
            let mut overwritten = bytes.to_vec();
            overwritten[at(percent)] ^= 0x5A;
            overwritten[at(percent) + 1] = (i as u8).wrapping_mul(0x3D);
            damaged.push((
                format!("{name} overwritten at {percent}%"),
                Bytes::from(overwritten),
            ));
        }
        damaged
    }

    /// Offsets of the restart markers (`FF D0` to `FF D7`) in a JPEG.
    fn restart_markers(bytes: &[u8]) -> Vec<usize> {
        (0..bytes.len() - 1)
            .filter(|&i| bytes[i] == 0xFF && (0xD0..=0xD7).contains(&bytes[i + 1]))
            .collect()
    }

    /// Damaged copies of the JPEG `bytes`, which has restart markers, named
    /// after `name`: damage where zune's restart handling runs, which is
    /// where its decoding can end early.
    pub(crate) fn damaged_restarts(name: &str, bytes: &[u8]) -> Vec<(String, Bytes)> {
        let markers = restart_markers(bytes);
        assert!(markers.len() >= 8, "{name} has {} restarts", markers.len());
        let (fifth, seventh, eighth) = (markers[4], markers[6], markers[7]);
        let with = |label: &str, edit: &dyn Fn(&mut Vec<u8>)| {
            let mut damaged = bytes.to_vec();
            edit(&mut damaged);
            (format!("{name} {label}"), Bytes::from(damaged))
        };
        vec![
            with("truncated mid-interval", &|b| {
                b.truncate((seventh + eighth) / 2)
            }),
            with("truncated at a restart marker", &|b| {
                b.truncate(seventh + 2)
            }),
            // RST4 numbered RST6: out of sequence.
            with("with a restart out of sequence", &|b| b[fifth + 1] = 0xD6),
            // FF D4 becomes 00 D4: the marker is gone, its bytes are data.
            with("with a restart marker lost", &|b| b[fifth] = 0x00),
            // RST4 becomes APP1: a header marker inside the entropy data.
            with("with a restart become APP1", &|b| b[fifth + 1] = 0xE1),
        ]
    }

    /// The marker segments of a JPEG up to its first scan, after the
    /// start-of-image marker: offset, marker and length counting its
    /// `FF xx` and length bytes.
    fn segments(bytes: &[u8]) -> Vec<(usize, u8, usize)> {
        let mut segments = Vec::new();
        let mut at = 2;
        while bytes[at] == 0xFF {
            let marker = bytes[at + 1];
            let len = 2 + u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
            segments.push((at, marker, len));
            if marker == 0xDA {
                break;
            }
            at += len;
        }
        segments
    }

    /// The segment with `marker`: offset and length.
    fn segment(bytes: &[u8], marker: u8) -> (usize, usize) {
        segments(bytes)
            .into_iter()
            .find(|&(_, m, _)| m == marker)
            .map(|(at, _, len)| (at, len))
            .unwrap_or_else(|| panic!("no segment FF {marker:02X}"))
    }

    /// The frame header (SOF0 or SOF2): offset and length.
    fn frame_header(bytes: &[u8]) -> (usize, usize) {
        segments(bytes)
            .into_iter()
            .find(|&(_, m, _)| m == 0xC0 || m == 0xC2)
            .map(|(at, _, len)| (at, len))
            .expect("no frame header")
    }

    /// An Adobe APP14 segment with colour `transform` (1 is YCbCr).
    fn adobe_app14(transform: u8) -> [u8; 16] {
        let mut segment = [0; 16];
        segment[..4].copy_from_slice(&[0xFF, 0xEE, 0x00, 0x0E]);
        segment[4..9].copy_from_slice(b"Adobe");
        segment[9..11].copy_from_slice(&100u16.to_be_bytes());
        segment[15] = transform;
        segment
    }

    /// `bytes` with `segment` inserted right after the frame header, where
    /// zune reads it after it has set the colour space from the component
    /// count.
    fn with_segment_after_frame(bytes: &[u8], segment: &[u8]) -> Vec<u8> {
        let (at, len) = frame_header(bytes);
        let mut edited = bytes[..at + len].to_vec();
        edited.extend_from_slice(segment);
        edited.extend_from_slice(&bytes[at + len..]);
        edited
    }

    /// JPEGs whose frame does not have the components zune's colour space
    /// says after it reads the headers: YCbCr with 4, 2 or 1 components.
    /// zune settles on another colour space only when it decodes them, and
    /// works out which components to decode from the output colour space,
    /// so RGBA and RGB output may take different paths.
    pub(crate) fn odd_component_jpegs() -> Vec<(String, Bytes)> {
        // The YCCK fixture's Adobe segment, before its four component frame,
        // made YCbCr (transform 1) and moved after the frame: zune reads the
        // frame as CMYK, then the segment makes it YCbCr.
        let ycck = fixture("ycck_q40");
        let (adobe, adobe_len) = segment(&ycck, 0xEE);
        let mut without_adobe = ycck[..adobe].to_vec();
        without_adobe.extend_from_slice(&ycck[adobe + adobe_len..]);
        let four = with_segment_after_frame(&without_adobe, &adobe_app14(1));

        // A 4:4:4 JPEG with its third component dropped from the frame and
        // the scan: two components, which zune leaves YCbCr in the headers.
        let colour = jpeg_of(&DynamicImage::ImageRgb8(textured(0)), 75);
        let (sof, sof_len) = frame_header(&colour);
        let (sos, sos_len) = segment(&colour, 0xDA);
        let mut two = colour[..sof + 2].to_vec();
        two.extend_from_slice(&((sof_len - 2 - 3) as u16).to_be_bytes());
        two.extend_from_slice(&colour[sof + 4..sof + 9]);
        two.push(2);
        two.extend_from_slice(&colour[sof + 10..sof + sof_len - 3]);
        two.extend_from_slice(&colour[sof + sof_len..sos + 2]);
        two.extend_from_slice(&((sos_len - 2 - 2) as u16).to_be_bytes());
        two.push(2);
        two.extend_from_slice(&colour[sos + 5..sos + 9]);
        two.extend_from_slice(&colour[sos + sos_len - 3..]);

        // A grayscale JPEG with an Adobe YCbCr segment after its frame.
        let gray = DynamicImage::ImageLuma8(DynamicImage::ImageRgb8(textured(0)).to_luma8());
        let one = with_segment_after_frame(&jpeg_of(&gray, 75), &adobe_app14(1));

        vec![
            ("four component ycbcr".into(), Bytes::from(four)),
            ("two component jpeg".into(), Bytes::from(two)),
            ("one component ycbcr".into(), Bytes::from(one)),
        ]
    }

    /// Every kind of chunk body [`super::decode_into`] must decode exactly as
    /// [`super::decode`] does, by name: JPEGs of every subsampling, of
    /// progressive and baseline coding, of several qualities, in grayscale
    /// and in the colour spaces zune does not convert to RGBA like `image`
    /// does; damaged JPEGs; other formats; wrong sizes; and bodies that are
    /// not images.
    pub(crate) fn corpus() -> Vec<(String, Bytes)> {
        let mut corpus: Vec<(String, Bytes)> = FIXTURES
            .iter()
            .map(|(name, bytes)| (name.to_string(), Bytes::from_static(bytes)))
            .collect();
        let colour = DynamicImage::ImageRgb8(textured(0));
        let gray = DynamicImage::ImageLuma8(colour.to_luma8());
        for quality in [10, 50, 75, 90, 100] {
            corpus.push((format!("image 4:4:4 q{quality}"), jpeg_of(&colour, quality)));
        }
        for quality in [50, 90] {
            corpus.push((
                format!("image grayscale q{quality}"),
                jpeg_of(&gray, quality),
            ));
        }
        corpus.push(("flat jpeg".into(), jpeg()));
        for name in [
            "ycc420_q75",
            "ycc420_q60_restart",
            "ycc420_q80_progressive",
            "gray_q85_progressive",
        ] {
            corpus.extend(damaged(name, &fixture(name)));
        }
        corpus.extend(damaged_restarts(
            "ycc420_q60_restart",
            &fixture("ycc420_q60_restart"),
        ));
        corpus.extend(odd_component_jpegs());
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(
            CHUNK_PIXELS,
            CHUNK_PIXELS,
            |x, y| image::Rgba([x as u8, y as u8, (x ^ y) as u8, (x + y) as u8]),
        ));
        corpus.extend([
            ("png".into(), encode_as(&colour, ImageFormat::Png)),
            ("png with alpha".into(), encode_as(&rgba, ImageFormat::Png)),
            ("grayscale png".into(), encode_as(&gray, ImageFormat::Png)),
            ("512 jpeg".into(), encoded(512, 512, ImageFormat::Jpeg)),
            ("255x256 jpeg".into(), encoded(255, 256, ImageFormat::Jpeg)),
            ("128 png".into(), encoded(128, 128, ImageFormat::Png)),
            ("error page".into(), error_page()),
            ("empty body".into(), Bytes::new()),
            (
                "jpeg signature only".into(),
                Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            ),
        ]);
        corpus
    }
}

#[cfg(test)]
mod tests {
    use super::test_chunks::{self, encoded, error_page, jpeg, COLOUR};
    use super::*;
    use bytes::Bytes;
    use image::ImageFormat;

    #[test]
    fn a_256_jpeg_passes_the_header_check() {
        assert_eq!(check_header(&jpeg()), Ok(()));
    }

    #[test]
    fn the_header_check_takes_any_format_image_reads_not_only_jpeg() {
        let png = encoded(CHUNK_PIXELS, CHUNK_PIXELS, ImageFormat::Png);
        assert_eq!(check_header(&png), Ok(()));
    }

    #[test]
    fn an_error_page_fails_the_header_check() {
        assert!(matches!(
            check_header(&error_page()),
            Err(ChunkImageError::NotAnImage(_))
        ));
    }

    #[test]
    fn an_empty_body_fails_the_header_check() {
        assert!(matches!(
            check_header(&[]),
            Err(ChunkImageError::NotAnImage(_))
        ));
    }

    #[test]
    fn a_jpeg_signature_with_no_header_fails_the_header_check() {
        assert!(matches!(
            check_header(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            Err(ChunkImageError::NotAnImage(_))
        ));
    }

    #[test]
    fn wrong_sized_images_fail_the_header_check() {
        for (width, height) in [(512, 512), (128, 128), (256, 255), (255, 256)] {
            let image = encoded(width, height, ImageFormat::Jpeg);
            assert_eq!(
                check_header(&image),
                Err(ChunkImageError::WrongSize { width, height }),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn a_usable_chunk_decodes_to_256_rgba() {
        let image = decode(&jpeg()).unwrap();

        assert_eq!(image.dimensions(), (CHUNK_PIXELS, CHUNK_PIXELS));
        // JPEG is lossy: close to the colour, fully opaque.
        let pixel = image.get_pixel(128, 128).0;
        for (got, want) in pixel.iter().zip(COLOUR.0) {
            assert!(got.abs_diff(want) <= 4, "{pixel:?}");
        }
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn decode_rejects_a_wrong_sized_image() {
        let image = encoded(512, 512, ImageFormat::Jpeg);
        assert_eq!(
            decode(&image),
            Err(ChunkImageError::WrongSize {
                width: 512,
                height: 512
            })
        );
    }

    #[test]
    fn decode_rejects_an_error_page() {
        assert!(matches!(
            decode(&error_page()),
            Err(ChunkImageError::NotAnImage(_))
        ));
    }

    // ------------------------------------------------------------------
    // decode_into: exactly what decode does, faster for JPEG (#390)
    // ------------------------------------------------------------------

    /// Fills `pixels` with bytes no decoder writes in this pattern, as the
    /// previous chunk of a tile would have left them.
    fn poison(pixels: &mut ChunkPixels) {
        for (i, byte) in pixels.rgba.iter_mut().enumerate() {
            *byte = (i * 7 + 3) as u8;
        }
    }

    /// Asserts that [`decode_into`] decodes `bytes` into a used buffer
    /// exactly as [`decode`] does: the same verdict and, for a usable chunk,
    /// the same RGBA bytes.
    fn assert_decodes_as_decode_does(name: &str, bytes: &[u8]) {
        let mut pixels = ChunkPixels::new();
        poison(&mut pixels);

        let got = decode_into(bytes, &mut pixels);

        match decode(bytes) {
            Ok(image) => {
                assert_eq!(got, Ok(()), "{name}");
                let differing = image
                    .as_raw()
                    .iter()
                    .zip(pixels.rgba.iter())
                    .filter(|(want, got)| want != got)
                    .count();
                assert_eq!(differing, 0, "{name}: bytes differing from decode");
            }
            Err(error) => assert_eq!(got, Err(error), "{name}"),
        }
    }

    #[test]
    fn decode_into_decodes_every_kind_of_chunk_exactly_as_decode_does() {
        for (name, bytes) in test_chunks::corpus() {
            assert_decodes_as_decode_does(&name, &bytes);
        }
    }

    #[test]
    fn decode_into_reuses_its_buffer_without_carrying_pixels_over() {
        // One buffer through the corpus in order, as one tile's chunks go
        // through it: a chunk zune leaves part unwritten follows one that
        // filled the whole buffer.
        let mut pixels = ChunkPixels::new();
        for (name, bytes) in test_chunks::corpus() {
            let got = decode_into(&bytes, &mut pixels);
            match decode(&bytes) {
                Ok(image) => {
                    assert_eq!(got, Ok(()), "{name}");
                    assert!(image.as_raw()[..] == pixels.rgba[..], "{name}");
                }
                Err(error) => assert_eq!(got, Err(error), "{name}"),
            }
        }
    }

    /// Whether the fast path decoded `bytes` itself, rather than leaving
    /// them to [`decode`].
    fn takes_the_fast_path(bytes: &[u8]) -> bool {
        decode_jpeg_into(bytes, &mut ChunkPixels::new().rgba)
    }

    #[test]
    fn ycbcr_and_grayscale_jpegs_take_the_fast_path() {
        // Without this the equivalence tests would pass with no fast path.
        let colour = DynamicImage::ImageRgb8(test_chunks::textured(0));
        let gray = DynamicImage::ImageLuma8(colour.to_luma8());
        let mut jpegs: Vec<(String, Bytes)> = [
            "ycc420_q75",
            "ycc420_q60_restart",
            "ycc422_q70",
            "ycc440_q70",
            "ycc411_q75",
            "ycc420_q80_progressive",
            "ycc444_q70_progressive",
            "gray_q85_progressive",
        ]
        .into_iter()
        .map(|name| (name.to_string(), test_chunks::fixture(name)))
        .collect();
        jpegs.push(("image 4:4:4".into(), test_chunks::jpeg_of(&colour, 85)));
        jpegs.push(("image grayscale".into(), test_chunks::jpeg_of(&gray, 85)));
        jpegs.push(("flat jpeg".into(), jpeg()));

        for (name, bytes) in jpegs {
            assert!(takes_the_fast_path(&bytes), "{name}");
        }
    }

    #[test]
    fn other_colour_spaces_and_formats_are_left_to_decode() {
        let png = encoded(CHUNK_PIXELS, CHUNK_PIXELS, ImageFormat::Png);
        let bodies = [
            // zune has no RGB to RGBA conversion.
            ("rgb jpeg", test_chunks::fixture("rgb_q40")),
            // zune converts YCCK to RGBA differently from YCCK to RGB.
            ("ycck jpeg", test_chunks::fixture("ycck_q40")),
            // zune does not decode arithmetic coding; decode rejects it.
            ("arithmetic jpeg", test_chunks::fixture("arithmetic_q75")),
            ("png", png),
            ("512 jpeg", encoded(512, 512, ImageFormat::Jpeg)),
            ("error page", error_page()),
            ("empty body", Bytes::new()),
        ];

        for (name, bytes) in bodies {
            assert!(!takes_the_fast_path(&bytes), "{name}");
        }
    }

    /// The damaged copy called `name`, made from the fixture its name
    /// starts with by [`test_chunks::damaged`] or, for the fixture with
    /// restart markers, [`test_chunks::damaged_restarts`].
    fn damaged_variant(name: &str) -> Bytes {
        let fixture_name = name.split(' ').next().expect(name);
        let fixture = test_chunks::fixture(fixture_name);
        let mut variants = test_chunks::damaged(fixture_name, &fixture);
        if fixture_name == "ycc420_q60_restart" {
            variants.extend(test_chunks::damaged_restarts(fixture_name, &fixture));
        }
        variants
            .into_iter()
            .find(|(variant, _)| variant == name)
            .map(|(_, bytes)| bytes)
            .unwrap_or_else(|| panic!("no damaged variant {name}"))
    }

    #[test]
    fn a_jpeg_zune_renders_only_in_part_is_left_to_decode() {
        // zune fills what a truncated JPEG lacks with byte 128, alpha too
        // in RGBA (`image` gets opaque grey), and stops at a stray marker
        // followed by EOI with the rows below unwritten (`image` has them
        // black, in a fresh buffer). Both return Ok.
        for name in [
            "ycc420_q75 truncated at 60%",
            "ycc420_q75 with APP1 and EOI at 30%",
            "ycc420_q75 with COM and EOI at 70%",
            "ycc420_q60_restart truncated mid-interval",
            "ycc420_q60_restart truncated at a restart marker",
        ] {
            let bytes = damaged_variant(name);
            assert!(decode(&bytes).is_ok(), "{name} decodes in part");
            assert!(!takes_the_fast_path(&bytes), "{name}");
        }
    }

    #[test]
    fn a_damaged_jpeg_zune_converts_whole_takes_the_fast_path() {
        // Damage that leaves every row converted is decoded by the fast
        // path, to decode's bytes (the equivalence tests check those): a
        // check that turned these away would lose the speed-up on them
        // without failing any other test.
        for name in [
            "ycc420_q75 without its end marker",
            "ycc420_q75 truncated at 95%",
            "ycc420_q75 overwritten at 50%",
            "ycc420_q60_restart overwritten at 35%",
            "ycc420_q60_restart with a restart out of sequence",
            "ycc420_q60_restart with a restart marker lost",
            // A progressive JPEG is rendered from whatever coefficients
            // arrived, so every row is converted however it ends.
            "ycc420_q80_progressive truncated at 20%",
            "ycc420_q80_progressive with APP1 and EOI at 30%",
            "ycc420_q80_progressive overwritten at 75%",
            "gray_q85_progressive truncated at 60%",
            "gray_q85_progressive overwritten at 35%",
        ] {
            assert!(takes_the_fast_path(&damaged_variant(name)), "{name}");
        }
    }

    #[test]
    fn a_jpeg_whose_components_are_not_its_colour_space_is_left_to_decode() {
        // zune names the colour space in the headers and settles it only
        // when it decodes (two YCbCr components become two bands, a CMYK
        // frame relabelled YCbCr keeps four), and picks the components to
        // decode by the output colour space, so RGBA and RGB output need
        // not take the same path. Only 1 component Luma and 3 component
        // YCbCr take the fast path.
        for (name, bytes) in test_chunks::odd_component_jpegs() {
            assert!(!takes_the_fast_path(&bytes), "{name}");
        }
    }

    #[test]
    fn decode_into_rejects_what_decode_rejects() {
        let mut pixels = ChunkPixels::new();
        assert_eq!(
            decode_into(&encoded(512, 512, ImageFormat::Jpeg), &mut pixels),
            Err(ChunkImageError::WrongSize {
                width: 512,
                height: 512
            })
        );
        assert!(matches!(
            decode_into(&error_page(), &mut pixels),
            Err(ChunkImageError::NotAnImage(_))
        ));
        assert!(matches!(
            decode_into(&test_chunks::fixture("arithmetic_q75"), &mut pixels),
            Err(ChunkImageError::NotAnImage(_))
        ));
    }

    #[test]
    fn chunk_pixels_are_256_rows_of_256_rgba_pixels() {
        let mut pixels = ChunkPixels::new();
        decode_into(&jpeg(), &mut pixels).unwrap();

        let rows: Vec<&[u8]> = pixels.rows().collect();
        assert_eq!(rows.len(), CHUNK_PIXELS as usize);
        assert!(rows.iter().all(|row| row.len() == CHUNK_ROW_BYTES));
        assert_eq!(CHUNK_ROW_BYTES, CHUNK_PIXELS as usize * 4);
        let pixel = &rows[128][128 * 4..129 * 4];
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn errors_say_what_was_wrong() {
        assert_eq!(
            ChunkImageError::WrongSize {
                width: 512,
                height: 512
            }
            .to_string(),
            "a 512x512 image, not 256x256"
        );
        assert_eq!(
            ChunkImageError::NotAnImage("format not recognised".into()).to_string(),
            "not an image (format not recognised)"
        );
    }
}
