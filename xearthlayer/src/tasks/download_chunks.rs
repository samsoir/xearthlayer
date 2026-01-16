//! Download chunks task implementation.
//!
//! [`DownloadChunksTask`] downloads all 256 chunks (16×16) for a tile from the
//! imagery provider, with disk cache integration and retry logic.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::Network` since it performs HTTP downloads.
//!
//! # Output
//!
//! Produces `TaskOutput` with key "chunks" containing `ChunkResults`.

use crate::coord::TileCoord;
use crate::executor::{
    ChunkProvider, ChunkResults, DiskCache, DownloadConfig, ResourceType, Task, TaskContext,
    TaskOutput, TaskResult,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, info, trace, warn};

/// Task that downloads all chunks for a tile.
///
/// This task performs parallel downloads of 256 chunks (16×16 grid), checking
/// the disk cache first and retrying failed downloads with exponential backoff.
///
/// # Outputs
///
/// - Key: "chunks"
/// - Type: `ChunkResults` (256 chunks with successes and failures)
pub struct DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    /// Tile coordinates to download
    tile: TileCoord,

    /// Chunk provider for downloading satellite imagery
    provider: Arc<P>,

    /// Disk cache for storing individual chunks
    disk_cache: Arc<D>,

    /// Download configuration (timeout, retries)
    config: DownloadConfig,
}

impl<P, D> DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    /// Creates a new download chunks task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates to download
    /// * `provider` - Chunk provider for downloads
    /// * `disk_cache` - Disk cache for chunks
    pub fn new(tile: TileCoord, provider: Arc<P>, disk_cache: Arc<D>) -> Self {
        Self {
            tile,
            provider,
            disk_cache,
            config: DownloadConfig::default(),
        }
    }

    /// Creates a new download chunks task with custom configuration.
    pub fn with_config(
        tile: TileCoord,
        provider: Arc<P>,
        disk_cache: Arc<D>,
        config: DownloadConfig,
    ) -> Self {
        Self {
            tile,
            provider,
            disk_cache,
            config,
        }
    }
}

impl<P, D> Task for DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    fn name(&self) -> &str {
        "DownloadChunks"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::Network
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            // Check for cancellation before starting
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }

            let job_id = ctx.job_id();
            debug!(
                job_id = %job_id,
                tile = ?self.tile,
                "Starting chunk downloads"
            );

            // Download all chunks
            let chunks = download_all_chunks(
                self.tile,
                Arc::clone(&self.provider),
                Arc::clone(&self.disk_cache),
                &self.config,
            )
            .await;

            // Check for cancellation after downloads
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }

            debug!(
                job_id = %job_id,
                success_count = chunks.success_count(),
                failure_count = chunks.failure_count(),
                "Chunk downloads complete"
            );

            // Store chunks in task output for next task
            let mut output = TaskOutput::new();
            output.set("chunks", chunks);

            TaskResult::SuccessWithOutput(output)
        })
    }
}

/// Output key for chunk results.
pub const OUTPUT_KEY_CHUNKS: &str = "chunks";

/// Helper function to extract chunks from task output.
pub fn get_chunks_from_output(output: &TaskOutput) -> Option<&ChunkResults> {
    output.get::<ChunkResults>(OUTPUT_KEY_CHUNKS)
}

// ============================================================================
// Internal implementation - lifted from pipeline/stages/download
// ============================================================================

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

/// Downloads all 256 chunks for a tile with controlled concurrency.
///
/// Uses a semaphore to limit concurrent HTTP requests, preventing
/// provider rate limiting while still achieving good throughput.
async fn download_all_chunks<P, D>(
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &DownloadConfig,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    let mut results = ChunkResults::new();
    let mut downloads = JoinSet::new();

    // Semaphore to limit concurrent HTTP requests within this tile
    // This prevents flooding the provider when multiple tiles download simultaneously
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_chunks));

    // Spawn download tasks for all 256 chunks
    for chunk in tile.chunks() {
        let provider = Arc::clone(&provider);
        let disk_cache = Arc::clone(&disk_cache);
        let timeout = config.request_timeout;
        let max_retries = config.max_retries;
        let sem = Arc::clone(&semaphore);

        downloads.spawn(async move {
            // Acquire semaphore permit before starting download
            let _permit = sem.acquire().await.expect("semaphore closed unexpectedly");

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
                warn!(error = %join_err, "Download task panicked");
            }
        }
    }

    results
}

/// Downloads a single chunk, checking disk cache first.
async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
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
        trace!(
            tile_row = tile.row,
            tile_col = tile.col,
            chunk_row = chunk_row,
            chunk_col = chunk_col,
            "Chunk cache hit"
        );
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

    trace!(
        tile = ?tile,
        chunk_row = chunk_row,
        chunk_col = chunk_col,
        global_row = global_row,
        global_col = global_col,
        chunk_zoom = chunk_zoom,
        provider = provider.name(),
        "Starting chunk download"
    );

    // Try downloading with retries
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        debug!(
            provider = provider.name(),
            global_row = global_row,
            global_col = global_col,
            zoom = chunk_zoom,
            attempt = attempt,
            "HTTP download attempt"
        );

        match tokio::time::timeout(
            timeout,
            provider.download_chunk(global_row, global_col, chunk_zoom),
        )
        .await
        {
            Ok(Ok(data)) => {
                info!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    bytes = data.len(),
                    attempt = attempt,
                    "Chunk download success"
                );

                // Cache the chunk (fire and forget)
                spawn_cache_write(
                    Arc::clone(&disk_cache),
                    tile,
                    chunk_row,
                    chunk_col,
                    data.clone(),
                );

                return Ok(ChunkData {
                    row: chunk_row,
                    col: chunk_col,
                    data,
                });
            }
            Ok(Err(e)) => {
                warn!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    attempt = attempt,
                    error = %e.message,
                    retryable = e.is_retryable,
                    "Chunk download error"
                );
                last_error = e.message.clone();
                if !e.is_retryable {
                    break;
                }
            }
            Err(_) => {
                warn!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    attempt = attempt,
                    timeout_secs = timeout.as_secs(),
                    "Chunk download timeout"
                );
                last_error = "timeout".to_string();
            }
        }

        // Exponential backoff before retry
        if attempt < max_retries {
            let backoff = Duration::from_millis(100 * (1 << attempt));
            trace!(backoff_ms = backoff.as_millis(), "Backoff before retry");
            tokio::time::sleep(backoff).await;
        }
    }

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
}

/// Spawns a fire-and-forget task to write chunk data to disk cache.
fn spawn_cache_write<D>(
    disk_cache: Arc<D>,
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    data: Vec<u8>,
) where
    D: DiskCache,
{
    tokio::spawn(async move {
        let _ = disk_cache
            .put(tile.row, tile.col, tile.zoom, chunk_row, chunk_col, data)
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_name() {
        assert_eq!("DownloadChunks", "DownloadChunks");
    }

    #[test]
    fn test_output_key() {
        assert_eq!(OUTPUT_KEY_CHUNKS, "chunks");
    }
}
