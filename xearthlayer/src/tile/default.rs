//! Default tile generator implementation.
//!
//! Provides a `TileGenerator` implementation that downloads satellite imagery
//! using a `TileOrchestrator` and encodes it to texture format using a
//! `TextureEncoder`.

use crate::coord::TileCoord;
use crate::fuse::generate_default_placeholder;
use crate::log::Logger;
use crate::orchestrator::TileOrchestrator;
use crate::texture::TextureEncoder;
use crate::tile::{TileGenerator, TileGeneratorError, TileRequest};
use crate::{log_error, log_info, log_warn};
use std::sync::Arc;

/// Default tile generator that downloads imagery and encodes to texture format.
///
/// This implementation combines a `TileOrchestrator` for downloading satellite
/// imagery chunks and a `TextureEncoder` for encoding the assembled image to
/// the target texture format.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::tile::{DefaultTileGenerator, TileGenerator, TileRequest};
/// use xearthlayer::texture::{DdsTextureEncoder, TextureEncoder};
/// use xearthlayer::orchestrator::TileOrchestrator;
/// use xearthlayer::provider::{BingMapsProvider, Provider, ReqwestClient};
/// use xearthlayer::dds::DdsFormat;
/// use std::sync::Arc;
///
/// // Create components (requires network access)
/// let http_client = ReqwestClient::new()?;
/// let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(http_client));
/// let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
/// let encoder: Arc<dyn TextureEncoder> = Arc::new(
///     DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5)
/// );
///
/// // Create generator
/// let generator: Arc<dyn TileGenerator> = Arc::new(
///     DefaultTileGenerator::new(orchestrator, encoder)
/// );
///
/// // Generate tile
/// let request = TileRequest::new(37, -123, 16);
/// let data = generator.generate(&request)?;
/// ```
pub struct DefaultTileGenerator {
    /// Tile orchestrator for downloading imagery
    orchestrator: Arc<TileOrchestrator>,
    /// Texture encoder
    encoder: Arc<dyn TextureEncoder>,
    /// Logger for diagnostic output
    logger: Arc<dyn Logger>,
}

impl DefaultTileGenerator {
    /// Create a new default tile generator.
    ///
    /// # Arguments
    ///
    /// * `orchestrator` - Tile orchestrator for downloading imagery
    /// * `encoder` - Texture encoder for encoding to target format
    /// * `logger` - Logger for diagnostic output
    pub fn new(
        orchestrator: TileOrchestrator,
        encoder: Arc<dyn TextureEncoder>,
        logger: Arc<dyn Logger>,
    ) -> Self {
        Self {
            orchestrator: Arc::new(orchestrator),
            encoder,
            logger,
        }
    }

    /// Get a reference to the encoder.
    pub fn encoder(&self) -> &dyn TextureEncoder {
        self.encoder.as_ref()
    }
}

impl TileGenerator for DefaultTileGenerator {
    fn generate(&self, request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
        // TileRequest contains chunk-level coordinates from FUSE filenames
        // e.g., "100000_125184_BI18.dds" -> row=100000, col=125184, zoom=18
        // These are at chunk resolution (256x256 pixels each)
        //
        // We need to convert to tile coordinates for the orchestrator:
        // - Tile row = chunk row / 16 (each tile = 16x16 chunks)
        // - Tile col = chunk col / 16
        // - Tile zoom = chunk zoom - 4 (16 = 2^4)
        let tile_zoom = request.zoom().saturating_sub(4);
        let tile = TileCoord {
            row: request.row() / 16,
            col: request.col() / 16,
            zoom: tile_zoom,
        };

        log_info!(
            self.logger,
            "Generating tile: chunk coords ({}, {}, zoom {}) -> tile coords ({}, {}, zoom {})",
            request.row(),
            request.col(),
            request.zoom(),
            tile.row,
            tile.col,
            tile_zoom
        );

        // Download tile imagery
        let image = match self.orchestrator.download_tile(&tile) {
            Ok(img) => {
                log_info!(
                    self.logger,
                    "Downloaded tile successfully: {}×{} pixels",
                    img.width(),
                    img.height()
                );
                img
            }
            Err(e) => {
                log_error!(self.logger, "Failed to download tile: {}", e);
                log_warn!(self.logger, "Returning magenta placeholder for failed tile");

                return generate_default_placeholder().map_err(|e| {
                    TileGeneratorError::Internal(format!("Failed to generate placeholder: {}", e))
                });
            }
        };

        // Encode to texture format
        match self.encoder.encode(&image) {
            Ok(texture_data) => {
                log_info!(
                    self.logger,
                    "Texture encoding completed ({} encoder): {} bytes",
                    self.encoder.name(),
                    texture_data.len()
                );
                Ok(texture_data)
            }
            Err(e) => {
                log_error!(self.logger, "Failed to encode texture: {}", e);
                log_warn!(
                    self.logger,
                    "Returning magenta placeholder for failed encoding"
                );

                generate_default_placeholder().map_err(|e| {
                    TileGeneratorError::Internal(format!("Failed to generate placeholder: {}", e))
                })
            }
        }
    }

    fn expected_size(&self) -> usize {
        // Standard tile size: 4096×4096
        self.encoder.expected_size(4096, 4096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsFormat;
    use crate::log::NoOpLogger;
    use crate::provider::{BingMapsProvider, MockHttpClient, Provider, ProviderError};
    use crate::texture::DdsTextureEncoder;
    use image::{Rgb, RgbImage};
    use std::io::Cursor;

    fn test_logger() -> Arc<dyn Logger> {
        Arc::new(NoOpLogger)
    }

    fn create_mock_jpeg_chunk() -> Vec<u8> {
        // Create a 256×256 test image
        let img = RgbImage::from_fn(256, 256, |x, y| {
            let r = ((x as f32 / 256.0) * 255.0) as u8;
            let g = ((y as f32 / 256.0) * 255.0) as u8;
            let b = 128;
            Rgb([r, g, b])
        });

        // Encode to JPEG
        let mut buffer = Cursor::new(Vec::new());
        img.write_to(&mut buffer, image::ImageFormat::Jpeg)
            .expect("Failed to encode JPEG");
        buffer.into_inner()
    }

    #[test]
    fn test_generator_creation() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        let generator = DefaultTileGenerator::new(orchestrator, encoder, test_logger());
        assert!(generator.expected_size() > 0);
    }

    #[test]
    fn test_expected_size() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        let generator = DefaultTileGenerator::new(orchestrator, encoder.clone(), test_logger());

        // Should match encoder's expected size for 4096×4096
        assert_eq!(generator.expected_size(), encoder.expected_size(4096, 4096));
    }

    #[test]
    fn test_generate_success() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1));

        let generator = DefaultTileGenerator::new(orchestrator, encoder, test_logger());
        // Use chunk-aligned coordinates (divisible by 16) at zoom 14
        // This will be converted to tile coords: row=18, col=31, zoom=10
        let request = TileRequest::new(288, 496, 14);

        let result = generator.generate(&request);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should be valid DDS data
        assert!(data.len() > 128); // At least header size
        assert_eq!(&data[0..4], b"DDS "); // DDS magic
    }

    #[test]
    fn test_generate_download_failure_returns_placeholder() {
        let mock = MockHttpClient {
            response: Err(ProviderError::HttpError("404".to_string())),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(1));

        let generator = DefaultTileGenerator::new(orchestrator, encoder, test_logger());
        // Use chunk-aligned coordinates at zoom 14
        let request = TileRequest::new(288, 496, 14);

        // Should return placeholder instead of error
        let result = generator.generate(&request);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should be valid DDS data (placeholder)
        assert!(data.len() > 128);
        assert_eq!(&data[0..4], b"DDS ");
    }

    #[test]
    fn test_as_trait_object() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        let generator: Arc<dyn TileGenerator> = Arc::new(DefaultTileGenerator::new(
            orchestrator,
            encoder,
            test_logger(),
        ));

        // Should work through trait object
        assert!(generator.expected_size() > 0);
    }

    #[test]
    fn test_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DefaultTileGenerator>();
    }

    #[test]
    fn test_encoder_access() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
        let encoder: Arc<dyn TextureEncoder> =
            Arc::new(DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5));

        let generator = DefaultTileGenerator::new(orchestrator, encoder, test_logger());

        assert_eq!(generator.encoder().name(), "DDS BC1");
        assert_eq!(generator.encoder().extension(), "dds");
    }
}
