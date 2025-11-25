//! Tile download orchestration implementation

use super::types::{ChunkResult, OrchestratorError};
use crate::coord::TileCoord;
use crate::provider::Provider;
use image::RgbaImage;
use std::sync::Arc;
use std::time::Instant;

/// Orchestrates parallel downloading and assembly of satellite imagery tiles.
///
/// Downloads all 256 chunks (16×16) of a tile in parallel using multiple threads,
/// with per-chunk retry logic and overall timeout.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::orchestrator::TileOrchestrator;
/// use xearthlayer::provider::{BingMapsProvider, ReqwestClient};
/// use std::sync::Arc;
///
/// let http_client = ReqwestClient::new()?;
/// let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(http_client));
/// let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
/// ```
pub struct TileOrchestrator {
    provider: Arc<dyn Provider>,
    timeout_secs: u64,
    max_retries_per_chunk: u32,
    max_parallel_downloads: usize,
}

impl TileOrchestrator {
    /// Creates a new TileOrchestrator with the given provider and settings.
    ///
    /// # Arguments
    ///
    /// * `provider` - The imagery provider to download chunks from (as trait object)
    /// * `timeout_secs` - Maximum time to spend downloading a tile
    /// * `max_retries_per_chunk` - Number of retry attempts per failed chunk
    /// * `max_parallel_downloads` - Maximum number of concurrent downloads
    pub fn new(
        provider: Arc<dyn Provider>,
        timeout_secs: u64,
        max_retries_per_chunk: u32,
        max_parallel_downloads: usize,
    ) -> Self {
        Self {
            provider,
            timeout_secs,
            max_retries_per_chunk,
            max_parallel_downloads,
        }
    }

    /// Downloads and assembles a complete tile image.
    ///
    /// Downloads all 256 chunks in parallel, retrying failed chunks up to
    /// `max_retries_per_chunk` times. Returns an error if the timeout is
    /// exceeded or too many chunks fail.
    ///
    /// # Arguments
    ///
    /// * `tile` - The tile coordinates to download
    ///
    /// # Returns
    ///
    /// A 4096×4096 RGBA image, or an error if download failed.
    pub fn download_tile(&self, tile: &TileCoord) -> Result<RgbaImage, OrchestratorError> {
        let start_time = Instant::now();

        // Download all 256 chunks in parallel
        let chunk_results = self.download_chunks_parallel(tile, start_time)?;

        // Assemble chunks into final 4096×4096 image
        self.assemble_image(&chunk_results, tile)
    }

    /// Downloads all chunks for a tile in parallel.
    fn download_chunks_parallel(
        &self,
        tile: &TileCoord,
        start_time: Instant,
    ) -> Result<Vec<ChunkResult>, OrchestratorError> {
        use std::sync::mpsc;
        use std::thread;

        let (tx, rx) = mpsc::channel();
        let chunks: Vec<_> = tile.chunks().collect();
        let chunk_count = chunks.len();

        // Batch chunks to limit concurrent threads
        let batch_size = self.max_parallel_downloads;
        let mut handles = vec![];

        for batch in chunks.chunks(batch_size) {
            for chunk in batch {
                let provider = Arc::clone(&self.provider);
                let tx = tx.clone();
                let chunk_row = chunk.chunk_row;
                let chunk_col = chunk.chunk_col;
                let (global_row, global_col, zoom) = chunk.to_global_coords();
                let max_retries = self.max_retries_per_chunk;
                let timeout = self.timeout_secs;
                let start = start_time;

                let handle = thread::spawn(move || {
                    // Download chunk with retries
                    let mut data = None;
                    for attempt in 0..=max_retries {
                        // Check timeout
                        if start.elapsed().as_secs() >= timeout {
                            break;
                        }

                        match provider.download_chunk(global_row, global_col, zoom) {
                            Ok(chunk_data) => {
                                data = Some(chunk_data);
                                break;
                            }
                            Err(_) if attempt < max_retries => {
                                // Retry
                                continue;
                            }
                            Err(_) => {
                                // Final attempt failed
                                break;
                            }
                        }
                    }

                    // Send result
                    if let Some(chunk_data) = data {
                        let _ = tx.send(Ok(ChunkResult {
                            chunk_row,
                            chunk_col,
                            data: chunk_data,
                        }));
                    } else {
                        let _ = tx.send(Err(chunk_row));
                    }
                });

                handles.push(handle);
            }

            // Wait for this batch to complete before starting next batch
            for handle in handles.drain(..) {
                let _ = handle.join();
            }
        }

        // Drop sender so receiver knows we're done
        drop(tx);

        // Collect results
        let mut results = Vec::new();
        let mut failed = 0;

        for result in rx {
            match result {
                Ok(chunk) => results.push(chunk),
                Err(_) => failed += 1,
            }
        }

        // Check if we have enough successful downloads
        let successful = results.len();
        let min_required = (chunk_count * 80) / 100; // Require 80% success

        if successful < min_required {
            return Err(OrchestratorError::TooManyFailures {
                tile: *tile,
                successful,
                failed,
                min_required,
            });
        }

        Ok(results)
    }

    /// Assembles downloaded chunks into a complete 4096×4096 image.
    fn assemble_image(
        &self,
        chunks: &[ChunkResult],
        _tile: &TileCoord,
    ) -> Result<RgbaImage, OrchestratorError> {
        use image::ImageReader;
        use std::io::Cursor;

        // Create 4096×4096 canvas (black background for missing chunks)
        let mut canvas = RgbaImage::new(4096, 4096);

        // Place each chunk at its correct position
        for chunk_result in chunks {
            // Decode JPEG chunk
            let img = ImageReader::new(Cursor::new(&chunk_result.data))
                .with_guessed_format()
                .map_err(|e| OrchestratorError::ImageError(format!("Format error: {}", e)))?
                .decode()?
                .to_rgba8();

            // Calculate position in final image
            let x = chunk_result.chunk_col as u32 * 256;
            let y = chunk_result.chunk_row as u32 * 256;

            // Copy chunk into canvas
            image::imageops::replace(&mut canvas, &img, x.into(), y.into());
        }

        Ok(canvas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BingMapsProvider;
    use crate::provider::{MockHttpClient, ProviderError};

    fn create_mock_jpeg_chunk() -> Vec<u8> {
        use image::{Rgb, RgbImage};
        use std::io::Cursor;

        // Create a 256×256 test image
        let img = RgbImage::from_fn(256, 256, |x, y| {
            // Simple gradient pattern
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
    fn test_orchestrator_creation() {
        let mock = MockHttpClient {
            response: Ok(vec![]),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);

        assert_eq!(orchestrator.timeout_secs, 30);
        assert_eq!(orchestrator.max_retries_per_chunk, 3);
        assert_eq!(orchestrator.max_parallel_downloads, 32);
    }

    #[test]
    fn test_download_tile_basic() {
        let mock = MockHttpClient {
            response: Ok(create_mock_jpeg_chunk()),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 10,
        };

        // This should download all 256 chunks and assemble into 4096×4096 image
        let result = orchestrator.download_tile(&tile);
        assert!(result.is_ok());

        let image = result.unwrap();
        assert_eq!(image.width(), 4096);
        assert_eq!(image.height(), 4096);
    }

    #[test]
    fn test_download_tile_with_failures() {
        // Mock that always fails
        let mock = MockHttpClient {
            response: Err(ProviderError::HttpError("404".to_string())),
        };
        let provider: Arc<dyn Provider> = Arc::new(BingMapsProvider::new(mock));
        let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 10,
        };

        let result = orchestrator.download_tile(&tile);
        assert!(result.is_err());
        // Should error due to too many failures
    }

    #[test]
    fn test_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TileOrchestrator>();
    }
}
