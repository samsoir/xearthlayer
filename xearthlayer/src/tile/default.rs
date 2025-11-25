//! Default tile generator implementation.
//!
//! Provides a `TileGenerator` implementation that downloads satellite imagery
//! using a `TileOrchestrator` and encodes it to texture format using a
//! `TextureEncoder`.

use crate::coord::to_tile_coords;
use crate::fuse::generate_default_placeholder;
use crate::orchestrator::TileOrchestrator;
use crate::texture::TextureEncoder;
use crate::tile::{TileGenerator, TileGeneratorError, TileRequest};
use std::sync::Arc;
use tracing::{error, info, warn};

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
}

impl DefaultTileGenerator {
    /// Create a new default tile generator.
    ///
    /// # Arguments
    ///
    /// * `orchestrator` - Tile orchestrator for downloading imagery
    /// * `encoder` - Texture encoder for encoding to target format
    pub fn new(orchestrator: TileOrchestrator, encoder: Arc<dyn TextureEncoder>) -> Self {
        Self {
            orchestrator: Arc::new(orchestrator),
            encoder,
        }
    }

    /// Get a reference to the encoder.
    pub fn encoder(&self) -> &dyn TextureEncoder {
        self.encoder.as_ref()
    }
}

impl TileGenerator for DefaultTileGenerator {
    fn generate(&self, request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
        info!(
            "Generating tile: lat={}, lon={}, zoom={}",
            request.lat(),
            request.lon(),
            request.zoom()
        );

        // Convert request coordinates to TileCoord
        let tile = match to_tile_coords(request.lat_f64(), request.lon_f64(), request.zoom()) {
            Ok(t) => {
                info!(
                    "Converted lat={}, lon={}, zoom={} to tile row={}, col={}",
                    request.lat(),
                    request.lon(),
                    request.zoom(),
                    t.row,
                    t.col
                );
                t
            }
            Err(e) => {
                error!("Failed to convert coordinates: {}", e);
                warn!("Returning magenta placeholder for invalid coordinates");
                return generate_default_placeholder().map_err(|e| {
                    TileGeneratorError::Internal(format!("Failed to generate placeholder: {}", e))
                });
            }
        };

        // Download tile imagery
        let image = match self.orchestrator.download_tile(&tile) {
            Ok(img) => {
                info!(
                    "Downloaded tile successfully: {}×{} pixels",
                    img.width(),
                    img.height()
                );
                img
            }
            Err(e) => {
                error!("Failed to download tile: {}", e);
                warn!("Returning magenta placeholder for failed tile");

                return generate_default_placeholder().map_err(|e| {
                    TileGeneratorError::Internal(format!("Failed to generate placeholder: {}", e))
                });
            }
        };

        // Encode to texture format
        match self.encoder.encode(&image) {
            Ok(texture_data) => {
                info!(
                    "Texture encoding completed ({} encoder): {} bytes",
                    self.encoder.name(),
                    texture_data.len()
                );
                Ok(texture_data)
            }
            Err(e) => {
                error!("Failed to encode texture: {}", e);
                warn!("Returning magenta placeholder for failed encoding");

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
    use crate::provider::{BingMapsProvider, MockHttpClient, Provider, ProviderError};
    use crate::texture::DdsTextureEncoder;
    use image::{Rgb, RgbImage};
    use std::io::Cursor;

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

        let generator = DefaultTileGenerator::new(orchestrator, encoder);
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

        let generator = DefaultTileGenerator::new(orchestrator, encoder.clone());

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

        let generator = DefaultTileGenerator::new(orchestrator, encoder);
        let request = TileRequest::new(37, -123, 10);

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

        let generator = DefaultTileGenerator::new(orchestrator, encoder);
        let request = TileRequest::new(37, -123, 10);

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

        let generator: Arc<dyn TileGenerator> =
            Arc::new(DefaultTileGenerator::new(orchestrator, encoder));

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

        let generator = DefaultTileGenerator::new(orchestrator, encoder);

        assert_eq!(generator.encoder().name(), "DDS BC1");
        assert_eq!(generator.encoder().extension(), "dds");
    }
}
