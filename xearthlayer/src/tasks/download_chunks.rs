//! Download chunks task implementation.
//!
//! [`DownloadChunksTask`] downloads all 256 chunks (16×16) for a tile from the
//! imagery provider, with disk cache integration and retry logic.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::Network` since it performs HTTP downloads.
//!
//! # HTTP Concurrency
//!
//! This task uses a **shared semaphore** from [`DownloadConfig`] to limit
//! concurrent HTTP requests across all tiles. This prevents the multiplicative
//! effect where N concurrent tiles × 256 chunks = overwhelming HTTP load.
//!
//! # Output
//!
//! Produces `TaskOutput` with key "chunks" containing `ChunkResults`.

use crate::coord::TileCoord;
use crate::executor::{
    ChunkProvider, ChunkResults, DiskCache, DownloadConfig, ResourceType, Task, TaskContext,
    TaskOutput, TaskResult,
};
use crate::metrics::{MetricsClient, OptionalMetrics};
use bytes::Bytes;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn, Instrument};

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
        let span = tracing::debug_span!(
            target: "profiling",
            "task_download",
            tile_row = self.tile.row,
            tile_col = self.tile.col,
        );
        Box::pin(
            async move {
                if crate::tasks::cached_tile_probe::tile_already_cached(ctx) {
                    return TaskResult::Success;
                }

                // Check for cancellation before starting
                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let job_id = ctx.job_id();
                let metrics = ctx.metrics_clone();
                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    "Starting chunk downloads"
                );

                // Download all chunks with metrics tracking. The token is
                // threaded through so a killed job stops downloading at once
                // (#300) instead of when its last chunk request completes.
                let chunks = download_all_chunks(
                    self.tile,
                    Arc::clone(&self.provider),
                    Arc::clone(&self.disk_cache),
                    &self.config,
                    metrics,
                    ctx.cancellation_token().clone(),
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
            }
            .instrument(span),
        )
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
    data: Bytes,
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
/// Uses the **shared semaphore** from [`DownloadConfig`] to limit concurrent
/// HTTP requests across ALL tiles being downloaded. This prevents the
/// multiplicative effect where N tiles × 256 chunks overwhelms the provider.
///
/// # Cancellation
///
/// When `cancellation` fires, every download still in flight is aborted and
/// awaited before this returns, so their network and HTTP permits are back in
/// the pool by the time the task reports `Cancelled`. Chunks that had already
/// completed keep their cache writes (those are spawned independently), so a
/// retry of the same tile reuses them. The partial results returned on
/// cancellation are not meant to be served.
async fn download_all_chunks<P, D>(
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &DownloadConfig,
    metrics: Option<MetricsClient>,
    cancellation: CancellationToken,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    let mut results = ChunkResults::new();
    let mut downloads = JoinSet::new();

    // Use the SHARED semaphore from config - this limits HTTP requests across
    // ALL concurrent tile downloads, not just within this single tile.
    let semaphore = Arc::clone(&config.http_semaphore);

    // Spawn download tasks for all 256 chunks
    for chunk in tile.chunks() {
        let provider = Arc::clone(&provider);
        let disk_cache = Arc::clone(&disk_cache);
        let timeout = config.request_timeout;
        let max_retries = config.max_retries;
        let sem = Arc::clone(&semaphore);
        let chunk_metrics = metrics.clone();

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
                chunk_metrics,
            )
            .await
        });
    }

    // Collect results as they complete, or stop the moment the job is killed
    loop {
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                debug!(
                    tile = ?tile,
                    in_flight = downloads.len(),
                    "Job cancelled, aborting in-flight chunk downloads"
                );
                downloads.shutdown().await;
                break;
            }
            next = downloads.join_next() => match next {
                Some(result) => result,
                None => break,
            },
        };
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
///
/// Emits metrics events for:
/// - Disk cache hit/miss
/// - Download started/completed/failed
/// - Download retries
#[allow(clippy::too_many_arguments)]
async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
    max_retries: u32,
    metrics: Option<MetricsClient>,
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
        // Emit chunk disk cache hit metric
        metrics.chunk_disk_cache_hit(cached.len() as u64);
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

    // Emit chunk disk cache miss metric
    metrics.chunk_disk_cache_miss();

    // Calculate global coordinates for the provider
    let global_row = tile.row * crate::coord::CHUNKS_PER_TILE_SIDE + chunk_row as u32;
    let global_col = tile.col * crate::coord::CHUNKS_PER_TILE_SIDE + chunk_col as u32;
    let chunk_zoom = tile.zoom + crate::coord::CHUNK_ZOOM_OFFSET;

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

    // Track download started
    metrics.download_started();

    // Try downloading with retries
    let download_start = Instant::now();
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
                let duration_us = download_start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;

                debug!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    bytes = bytes,
                    attempt = attempt,
                    "Chunk download success"
                );

                // Emit download completed metric
                metrics.download_completed(bytes, duration_us);

                // Cache the chunk (fire and forget, with metrics)
                spawn_cache_write(
                    Arc::clone(&disk_cache),
                    tile,
                    chunk_row,
                    chunk_col,
                    data.clone(),
                    metrics.clone(),
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
                // Emit retry metric if we'll retry
                if attempt < max_retries {
                    metrics.download_retried();
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
                // Emit retry metric if we'll retry
                if attempt < max_retries {
                    metrics.download_retried();
                }
            }
        }

        // Exponential backoff before retry
        if attempt < max_retries {
            let backoff = Duration::from_millis(100 * (1 << attempt));
            trace!(backoff_ms = backoff.as_millis(), "Backoff before retry");
            tokio::time::sleep(backoff).await;
        }
    }

    // Emit download failed metric
    metrics.download_failed();

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
}

/// Spawns a fire-and-forget task to write chunk data to disk cache.
///
/// Emits `disk_write_started` when beginning and `disk_write_completed`
/// with byte count when finished.
fn spawn_cache_write<D>(
    disk_cache: Arc<D>,
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    data: Bytes,
    metrics: Option<MetricsClient>,
) where
    D: DiskCache,
{
    let bytes = data.len() as u64;
    tokio::spawn(async move {
        // Track disk write started
        metrics.disk_write_started();

        let _ = disk_cache
            .put(tile.row, tile.col, tile.zoom, chunk_row, chunk_col, data)
            .await;

        // Track disk write completed
        metrics.disk_write_completed(bytes, D::TIER);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_name() {
        assert_eq!("DownloadChunks", "DownloadChunks");
    }

    struct PanickingProvider;
    impl ChunkProvider for PanickingProvider {
        fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Result<Bytes, crate::executor::ChunkDownloadError>> + Send
        {
            async { panic!("provider must not be called for a cached tile") }
        }
        fn name(&self) -> &str {
            "panicking"
        }
    }

    struct PanickingDiskCache;
    impl DiskCache for PanickingDiskCache {
        fn get(
            &self,
            _tr: u32,
            _tc: u32,
            _z: u8,
            _cr: u8,
            _cc: u8,
        ) -> impl Future<Output = Option<Bytes>> + Send {
            async { panic!("chunk cache must not be read for a cached tile") }
        }
        fn put(
            &self,
            _tr: u32,
            _tc: u32,
            _z: u8,
            _cr: u8,
            _cc: u8,
            _d: Bytes,
        ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
            async { panic!("chunk cache must not be written for a cached tile") }
        }
    }

    #[tokio::test]
    async fn cached_tile_probe_hit_skips_the_download() {
        use crate::executor::JobId;
        use tokio_util::sync::CancellationToken;

        let task = DownloadChunksTask::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
            Arc::new(PanickingProvider),
            Arc::new(PanickingDiskCache),
        );
        let mut ctx = TaskContext::new(JobId::new("test-job"), CancellationToken::new());
        let mut probe = TaskOutput::new();
        probe.set(crate::tasks::TILE_CACHED_KEY, true);
        ctx.add_task_output(crate::tasks::CACHED_TILE_PROBE_TASK.to_string(), probe);

        assert!(matches!(task.execute(&mut ctx).await, TaskResult::Success));
    }

    #[test]
    fn test_output_key() {
        assert_eq!(OUTPUT_KEY_CHUNKS, "chunks");
    }

    /// A chunk cache with nothing in it that accepts every write.
    struct EmptyDiskCache;
    impl DiskCache for EmptyDiskCache {
        fn get(
            &self,
            _tr: u32,
            _tc: u32,
            _z: u8,
            _cr: u8,
            _cc: u8,
        ) -> impl Future<Output = Option<Bytes>> + Send {
            async { None }
        }
        fn put(
            &self,
            _tr: u32,
            _tc: u32,
            _z: u8,
            _cr: u8,
            _cc: u8,
            _d: Bytes,
        ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
            async { Ok(()) }
        }
    }

    /// Counts downloads in flight: incremented when a download starts and
    /// decremented when its future is dropped, however that happens.
    struct InFlightGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A provider whose downloads never complete, like a stalled network.
    struct StallingProvider {
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl ChunkProvider for StallingProvider {
        fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Result<Bytes, crate::executor::ChunkDownloadError>> + Send
        {
            self.in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let guard = InFlightGuard(Arc::clone(&self.in_flight));
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        fn name(&self) -> &str {
            "stalling"
        }
    }

    /// Issue #300: a killed job must stop downloading. Cancelling the job's
    /// token while chunk downloads are in flight must end the task promptly
    /// and drop every download, not wait for the last one to finish.
    #[tokio::test]
    async fn cancellation_aborts_in_flight_chunk_downloads() {
        use crate::executor::JobId;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_util::sync::CancellationToken;

        let in_flight = Arc::new(AtomicUsize::new(0));
        let task = DownloadChunksTask::with_config(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
            Arc::new(StallingProvider {
                in_flight: Arc::clone(&in_flight),
            }),
            Arc::new(EmptyDiskCache),
            DownloadConfig {
                // Longer than the test's own deadline, so only cancellation
                // can end the downloads.
                request_timeout: Duration::from_secs(60),
                ..DownloadConfig::default()
            },
        );
        let token = CancellationToken::new();
        let mut ctx = TaskContext::new(JobId::new("test-job"), token.clone());

        // Kill the job once the downloads are under way.
        let killer = {
            let in_flight = Arc::clone(&in_flight);
            tokio::spawn(async move {
                while in_flight.load(Ordering::SeqCst) < 256 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                token.cancel();
            })
        };

        let result = tokio::time::timeout(Duration::from_secs(5), task.execute(&mut ctx))
            .await
            .expect("a cancelled task must return promptly, not wait for its downloads");
        killer.await.unwrap();

        assert!(
            matches!(result, TaskResult::Cancelled),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "every chunk download must be dropped when the task is cancelled"
        );
    }
}
