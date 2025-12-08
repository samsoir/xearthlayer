//! Download stage - fetches chunks from imagery provider.
//!
//! This stage downloads all 256 chunks (16x16 grid) that compose a single tile.
//! It uses Tokio's JoinSet for concurrent downloads and handles retries for
//! transient failures.

use crate::coord::TileCoord;
use crate::pipeline::{ChunkProvider, ChunkResults, DiskCache, JobId, PipelineConfig};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{debug, instrument, warn};

/// Downloads all chunks for a tile.
///
/// This stage:
/// 1. Checks disk cache for each chunk
/// 2. Downloads missing chunks from the provider
/// 3. Retries failed downloads up to max_retries times
/// 4. Returns partial results (with failures tracked) rather than failing entirely
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `tile` - The tile coordinates to download
/// * `provider` - Chunk download provider
/// * `disk_cache` - Disk cache for chunk persistence
/// * `config` - Pipeline configuration
///
/// # Returns
///
/// `ChunkResults` containing successful downloads and failures.
#[instrument(skip(provider, disk_cache, config), fields(job_id = %job_id))]
pub async fn download_stage<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    let mut results = ChunkResults::new();
    let mut downloads = JoinSet::new();

    // Spawn download tasks for all 256 chunks
    for chunk in tile.chunks() {
        let provider = Arc::clone(&provider);
        let disk_cache = Arc::clone(&disk_cache);
        let timeout = config.request_timeout;
        let max_retries = config.max_retries;

        downloads.spawn(async move {
            download_chunk_with_cache(
                tile,
                chunk.chunk_row,
                chunk.chunk_col,
                provider,
                disk_cache,
                timeout,
                max_retries,
            )
            .await
        });
    }

    // Collect results as they complete
    while let Some(result) = downloads.join_next().await {
        match result {
            Ok(Ok(chunk_data)) => {
                results.add_success(chunk_data.row, chunk_data.col, chunk_data.data);
            }
            Ok(Err(chunk_err)) => {
                warn!(
                    job_id = %job_id,
                    chunk_row = chunk_err.row,
                    chunk_col = chunk_err.col,
                    error = %chunk_err.error,
                    "Chunk download failed"
                );
                results.add_failure(
                    chunk_err.row,
                    chunk_err.col,
                    chunk_err.attempts,
                    chunk_err.error,
                );
            }
            Err(join_err) => {
                // Task panicked - this shouldn't happen but handle gracefully
                warn!(job_id = %job_id, error = %join_err, "Download task panicked");
            }
        }
    }

    debug!(
        job_id = %job_id,
        success = results.success_count(),
        failed = results.failure_count(),
        "Download stage complete"
    );

    results
}

/// Result of a successful chunk download.
struct ChunkData {
    row: u8,
    col: u8,
    data: Vec<u8>,
}

/// Result of a failed chunk download.
struct ChunkError {
    row: u8,
    col: u8,
    attempts: u32,
    error: String,
}

/// Downloads a single chunk, checking cache first.
async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: std::time::Duration,
    max_retries: u32,
) -> Result<ChunkData, ChunkError>
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Check disk cache first
    if let Some(cached) = disk_cache
        .get(tile.row, tile.col, tile.zoom, chunk_row, chunk_col)
        .await
    {
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

    // Calculate global coordinates for the provider
    let global_row = tile.row * 16 + chunk_row as u32;
    let global_col = tile.col * 16 + chunk_col as u32;
    let chunk_zoom = tile.zoom + 4;

    // Try downloading with retries
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        match tokio::time::timeout(
            timeout,
            provider.download_chunk(global_row, global_col, chunk_zoom),
        )
        .await
        {
            Ok(Ok(data)) => {
                // Success - cache the chunk (fire and forget, don't block on cache write)
                let dc = Arc::clone(&disk_cache);
                let chunk_data = data.clone();
                tokio::spawn(async move {
                    let _ = dc
                        .put(
                            tile.row, tile.col, tile.zoom, chunk_row, chunk_col, chunk_data,
                        )
                        .await;
                });

                return Ok(ChunkData {
                    row: chunk_row,
                    col: chunk_col,
                    data,
                });
            }
            Ok(Err(e)) => {
                last_error = e.message.clone();
                if !e.is_retryable {
                    // Permanent error, don't retry
                    break;
                }
            }
            Err(_) => {
                last_error = "timeout".to_string();
            }
        }

        // Brief delay before retry (exponential backoff)
        if attempt < max_retries {
            tokio::time::sleep(std::time::Duration::from_millis(100 * (1 << attempt))).await;
        }
    }

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::ChunkDownloadError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock provider that returns predictable data.
    struct MockProvider {
        /// Chunks that should fail
        failures: Mutex<HashMap<(u32, u32, u8), ChunkDownloadError>>,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                failures: Mutex::new(HashMap::new()),
            }
        }

        fn with_failure(self, row: u32, col: u32, zoom: u8, err: ChunkDownloadError) -> Self {
            self.failures.lock().unwrap().insert((row, col, zoom), err);
            self
        }
    }

    impl ChunkProvider for MockProvider {
        async fn download_chunk(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            if let Some(err) = self.failures.lock().unwrap().get(&(row, col, zoom)) {
                return Err(err.clone());
            }
            // Return predictable test data
            Ok(vec![row as u8, col as u8, zoom])
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    /// Mock disk cache that stores nothing.
    struct NullDiskCache;

    impl DiskCache for NullDiskCache {
        async fn get(
            &self,
            _tile_row: u32,
            _tile_col: u32,
            _zoom: u8,
            _chunk_row: u8,
            _chunk_col: u8,
        ) -> Option<Vec<u8>> {
            None
        }

        async fn put(
            &self,
            _tile_row: u32,
            _tile_col: u32,
            _zoom: u8,
            _chunk_row: u8,
            _chunk_col: u8,
            _data: Vec<u8>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    /// Mock disk cache that returns cached data.
    struct MockDiskCache {
        cached: Mutex<HashMap<(u32, u32, u8, u8, u8), Vec<u8>>>,
    }

    impl MockDiskCache {
        fn new() -> Self {
            Self {
                cached: Mutex::new(HashMap::new()),
            }
        }

        fn with_cached(
            self,
            tile_row: u32,
            tile_col: u32,
            zoom: u8,
            chunk_row: u8,
            chunk_col: u8,
            data: Vec<u8>,
        ) -> Self {
            self.cached
                .lock()
                .unwrap()
                .insert((tile_row, tile_col, zoom, chunk_row, chunk_col), data);
            self
        }
    }

    impl DiskCache for MockDiskCache {
        async fn get(
            &self,
            tile_row: u32,
            tile_col: u32,
            zoom: u8,
            chunk_row: u8,
            chunk_col: u8,
        ) -> Option<Vec<u8>> {
            self.cached
                .lock()
                .unwrap()
                .get(&(tile_row, tile_col, zoom, chunk_row, chunk_col))
                .cloned()
        }

        async fn put(
            &self,
            tile_row: u32,
            tile_col: u32,
            zoom: u8,
            chunk_row: u8,
            chunk_col: u8,
            data: Vec<u8>,
        ) -> Result<(), std::io::Error> {
            self.cached
                .lock()
                .unwrap()
                .insert((tile_row, tile_col, zoom, chunk_row, chunk_col), data);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_download_stage_all_success() {
        let provider = Arc::new(MockProvider::new());
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let results = download_stage(JobId::new(), tile, provider, cache, &config).await;

        assert_eq!(results.success_count(), 256);
        assert_eq!(results.failure_count(), 0);
        assert!(results.is_complete());
    }

    #[tokio::test]
    async fn test_download_stage_with_failures() {
        // Make one chunk fail permanently
        let provider = Arc::new(MockProvider::new().with_failure(
            100 * 16 + 5,  // chunk (5, 0)
            200 * 16 + 10, // at tile (100, 200)
            20,            // zoom 16 + 4
            ChunkDownloadError::permanent("not found"),
        ));
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig {
            max_retries: 1, // Quick test
            ..Default::default()
        };
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let results = download_stage(JobId::new(), tile, provider, cache, &config).await;

        assert_eq!(results.success_count(), 255);
        assert_eq!(results.failure_count(), 1);
        assert!(!results.is_complete());
    }

    #[tokio::test]
    async fn test_download_stage_uses_cache() {
        let provider = Arc::new(MockProvider::new());
        let cache =
            Arc::new(MockDiskCache::new().with_cached(100, 200, 16, 0, 0, vec![0xCA, 0xCE, 0xD]));
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let results = download_stage(JobId::new(), tile, provider, cache, &config).await;

        assert_eq!(results.success_count(), 256);

        // Check that chunk (0, 0) has cached data
        let cached_chunk = results.get(0, 0).unwrap();
        assert_eq!(cached_chunk, &[0xCA, 0xCE, 0xD]);
    }
}
