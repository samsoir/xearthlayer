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
//!   (#180), and is discarded from the chunk cache.
//!
//! The format is whatever `image` recognises from the bytes, not JPEG
//! specifically: the providers have not been audited for returning JPEG only
//! (see [`crate::cache::integrity::raw_chunk_validator`]).

use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::fmt;
use std::io::Cursor;

/// Width and height of every chunk, in pixels.
pub(crate) const CHUNK_PIXELS: u32 = 256;

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
}

#[cfg(test)]
mod tests {
    use super::test_chunks::{encoded, error_page, jpeg, COLOUR};
    use super::*;
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
