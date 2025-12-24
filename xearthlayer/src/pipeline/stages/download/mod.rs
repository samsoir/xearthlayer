//! Download stage - fetches chunks from imagery provider.
//!
//! This stage downloads all 256 chunks (16x16 grid) that compose a single tile.
//! It uses Tokio's JoinSet for concurrent downloads and handles retries for
//! transient failures.
//!
//! # Module Structure
//!
//! - [`chunk`] - Individual chunk download functions with cache and retry logic
//! - [`bounded`] - Two-phase permit-bounded download stage (recommended)
//!
//! # Concurrency Control
//!
//! The download stage supports an optional global HTTP concurrency limiter
//! (`HttpConcurrencyLimiter`) that constrains the total number of concurrent
//! HTTP requests across all tiles being processed. This prevents network
//! stack exhaustion under heavy load.
//!
//! For production use, prefer [`download_stage_bounded`] which only spawns
//! download tasks when HTTP permits are available, preventing task avalanche.

mod bounded;
mod chunk;

#[cfg(test)]
mod tests;

pub use bounded::download_stage_bounded;

use chunk::{download_chunk_cancellable, download_chunk_with_cache};

use crate::coord::TileCoord;
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::{ChunkProvider, ChunkResults, DiskCache, JobId, PipelineConfig};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
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
/// * `metrics` - Optional metrics collector
///
/// # Returns
///
/// `ChunkResults` containing successful downloads and failures.
#[instrument(skip(provider, disk_cache, config, metrics), fields(job_id = %job_id))]
pub async fn download_stage<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    download_stage_with_limiter(job_id, tile, provider, disk_cache, config, metrics, None).await
}

/// Downloads all chunks for a tile with optional global HTTP concurrency limiting.
///
/// This is the primary download function that supports constrained HTTP concurrency.
/// When an `http_limiter` is provided, download tasks will wait for permits before
/// making HTTP requests, preventing network stack exhaustion under heavy load.
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `tile` - The tile coordinates to download
/// * `provider` - Chunk download provider
/// * `disk_cache` - Disk cache for chunk persistence
/// * `config` - Pipeline configuration
/// * `metrics` - Optional metrics collector
/// * `http_limiter` - Optional global HTTP concurrency limiter
///
/// # Returns
///
/// `ChunkResults` containing successful downloads and failures.
#[instrument(skip(provider, disk_cache, config, metrics, http_limiter), fields(job_id = %job_id))]
pub async fn download_stage_with_limiter<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
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
        let metrics = metrics.clone();
        let http_limiter = http_limiter.clone();
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
                metrics,
                http_limiter,
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

/// Downloads all chunks for a tile with cancellation support.
///
/// This version accepts a `CancellationToken` that can abort in-progress downloads
/// when the FUSE layer times out waiting for a response. This prevents orphaned
/// downloads from holding HTTP connections and semaphore permits indefinitely.
///
/// When cancelled:
/// - Completes already-started chunk downloads (to avoid leaving connections in bad state)
/// - Aborts waiting/pending chunk downloads immediately
/// - Returns partial results with whatever was completed
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `tile` - The tile coordinates to download
/// * `provider` - Chunk download provider
/// * `disk_cache` - Disk cache for chunk persistence
/// * `config` - Pipeline configuration
/// * `metrics` - Optional metrics collector
/// * `http_limiter` - Optional global HTTP concurrency limiter
/// * `cancellation_token` - Token to signal cancellation
///
/// # Returns
///
/// `ChunkResults` containing successful downloads and failures. If cancelled,
/// returns partial results.
#[allow(clippy::too_many_arguments)]
#[instrument(skip(provider, disk_cache, config, metrics, http_limiter, cancellation_token), fields(job_id = %job_id))]
pub async fn download_stage_cancellable<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
    cancellation_token: CancellationToken,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    let mut results = ChunkResults::new();

    // Early cancellation check
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, "Download stage cancelled before starting");
        return results;
    }

    let mut downloads = JoinSet::new();

    // Spawn download tasks for all 256 chunks
    for chunk in tile.chunks() {
        let provider = Arc::clone(&provider);
        let disk_cache = Arc::clone(&disk_cache);
        let metrics = metrics.clone();
        let http_limiter = http_limiter.clone();
        let timeout = config.request_timeout;
        let max_retries = config.max_retries;
        let token = cancellation_token.clone();

        downloads.spawn(async move {
            download_chunk_cancellable(
                tile,
                chunk.chunk_row,
                chunk.chunk_col,
                provider,
                disk_cache,
                timeout,
                max_retries,
                metrics,
                http_limiter,
                token,
            )
            .await
        });
    }

    // Collect results as they complete, checking for cancellation
    loop {
        tokio::select! {
            biased;

            // Check cancellation first
            _ = cancellation_token.cancelled() => {
                debug!(
                    job_id = %job_id,
                    completed = results.total_count(),
                    "Download stage cancelled - aborting remaining downloads"
                );
                downloads.abort_all();
                break;
            }

            // Wait for next download to complete
            result = downloads.join_next() => {
                match result {
                    Some(Ok(Ok(chunk_data))) => {
                        results.add_success(chunk_data.row, chunk_data.col, chunk_data.data);
                    }
                    Some(Ok(Err(chunk_err))) => {
                        if !cancellation_token.is_cancelled() {
                            warn!(
                                job_id = %job_id,
                                chunk_row = chunk_err.row,
                                chunk_col = chunk_err.col,
                                error = %chunk_err.error,
                                "Chunk download failed"
                            );
                        }
                        results.add_failure(
                            chunk_err.row,
                            chunk_err.col,
                            chunk_err.attempts,
                            chunk_err.error,
                        );
                    }
                    Some(Err(join_err)) => {
                        if !join_err.is_cancelled() {
                            warn!(job_id = %job_id, error = %join_err, "Download task panicked");
                        }
                    }
                    None => {
                        // All tasks completed
                        break;
                    }
                }
            }
        }
    }

    debug!(
        job_id = %job_id,
        success = results.success_count(),
        failed = results.failure_count(),
        cancelled = cancellation_token.is_cancelled(),
        "Download stage complete"
    );

    results
}
