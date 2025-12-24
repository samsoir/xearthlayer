//! Permit-bounded download stage.
//!
//! This module implements a two-phase download approach that prevents task
//! avalanche by only spawning download tasks when HTTP permits are available.
//!
//! # Two-Phase Approach
//!
//! 1. **Cache Check Phase (unbounded):** All 256 chunks are checked against disk
//!    cache in parallel. This is fast I/O that doesn't require HTTP permits.
//!
//! 2. **Download Phase (bounded):** Only cache misses proceed to download. Tasks
//!    are spawned one-by-one as HTTP permits become available, preventing the
//!    spawn of thousands of waiting tasks.
//!
//! # Benefits
//!
//! - `downloads_active` matches actual HTTP connections (not inflated by waiters)
//! - Memory usage bounded by HTTP limiter capacity
//! - No file descriptor exhaustion from spawned-but-waiting tasks
//! - Smooth ramp-up of network utilization

use super::chunk::{download_chunk_with_permit, ChunkData, ChunkError};
use crate::coord::TileCoord;
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::{ChunkProvider, ChunkResults, DiskCache, JobId, PipelineConfig};
use crate::telemetry::PipelineMetrics;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Downloads all chunks for a tile with permit-bounded task spawning.
///
/// This is the recommended download function that prevents task avalanche by only
/// spawning download tasks when HTTP permits are available. This ensures that
/// `downloads_active` accurately reflects actual HTTP connections, not queued tasks.
#[allow(clippy::too_many_arguments)]
pub async fn download_stage_bounded<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Arc<HttpConcurrencyLimiter>,
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

    // Phase 1: Check disk cache for all chunks in parallel (no HTTP permits needed)
    let chunks_to_download = cache_check_phase(
        job_id,
        tile,
        Arc::clone(&disk_cache),
        metrics.clone(),
        &cancellation_token,
        &mut results,
    )
    .await;

    if cancellation_token.is_cancelled() {
        return results;
    }

    let cache_hits = results.success_count();
    let to_download = chunks_to_download.len();

    debug!(
        job_id = %job_id,
        cache_hits = cache_hits,
        to_download = to_download,
        "Cache check complete, starting bounded downloads"
    );

    // Phase 2: Download cache misses with permit-bounded spawning
    download_phase(
        job_id,
        tile,
        provider,
        disk_cache,
        config,
        metrics,
        http_limiter,
        cancellation_token.clone(),
        chunks_to_download,
        &mut results,
    )
    .await;

    debug!(
        job_id = %job_id,
        success = results.success_count(),
        failed = results.failure_count(),
        cancelled = cancellation_token.is_cancelled(),
        "Download stage complete (bounded)"
    );

    results
}

/// Phase 1: Check disk cache for all chunks in parallel.
///
/// Returns a list of (chunk_row, chunk_col) pairs that need downloading.
async fn cache_check_phase<D>(
    job_id: JobId,
    tile: TileCoord,
    disk_cache: Arc<D>,
    metrics: Option<Arc<PipelineMetrics>>,
    cancellation_token: &CancellationToken,
    results: &mut ChunkResults,
) -> VecDeque<(u8, u8)>
where
    D: DiskCache,
{
    let mut cache_checks = JoinSet::new();

    for chunk in tile.chunks() {
        let disk_cache = Arc::clone(&disk_cache);
        let metrics = metrics.clone();

        cache_checks.spawn(async move {
            let cached = disk_cache
                .get(
                    tile.row,
                    tile.col,
                    tile.zoom,
                    chunk.chunk_row,
                    chunk.chunk_col,
                )
                .await;

            if cached.is_some() {
                if let Some(ref m) = metrics {
                    m.disk_cache_hit();
                }
            } else if let Some(ref m) = metrics {
                m.disk_cache_miss();
            }

            (chunk.chunk_row, chunk.chunk_col, cached)
        });
    }

    let mut chunks_to_download = VecDeque::new();

    while let Some(result) = cache_checks.join_next().await {
        if cancellation_token.is_cancelled() {
            debug!(job_id = %job_id, "Cache check cancelled");
            cache_checks.abort_all();
            return chunks_to_download;
        }

        match result {
            Ok((chunk_row, chunk_col, Some(data))) => {
                results.add_success(chunk_row, chunk_col, data);
            }
            Ok((chunk_row, chunk_col, None)) => {
                chunks_to_download.push_back((chunk_row, chunk_col));
            }
            Err(join_err) => {
                warn!(job_id = %job_id, error = %join_err, "Cache check task panicked");
            }
        }
    }

    chunks_to_download
}

/// Phase 2: Download cache misses with permit-bounded spawning.
#[allow(clippy::too_many_arguments)]
async fn download_phase<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Arc<HttpConcurrencyLimiter>,
    cancellation_token: CancellationToken,
    mut chunks_to_download: VecDeque<(u8, u8)>,
    results: &mut ChunkResults,
) where
    P: ChunkProvider,
    D: DiskCache,
{
    let mut downloads: JoinSet<Result<ChunkData, ChunkError>> = JoinSet::new();

    while !chunks_to_download.is_empty() || !downloads.is_empty() {
        // Check cancellation
        if cancellation_token.is_cancelled() {
            debug!(
                job_id = %job_id,
                completed = results.total_count(),
                pending = chunks_to_download.len(),
                active = downloads.len(),
                "Download stage cancelled - aborting remaining downloads"
            );
            downloads.abort_all();
            break;
        }

        // Try to spawn new tasks if we have pending work and can get a permit
        while !chunks_to_download.is_empty() {
            if let Some(permit) = http_limiter.try_acquire() {
                let (chunk_row, chunk_col) = chunks_to_download.pop_front().unwrap();
                let provider = Arc::clone(&provider);
                let disk_cache = Arc::clone(&disk_cache);
                let metrics = metrics.clone();
                let timeout = config.request_timeout;
                let max_retries = config.max_retries;
                let token = cancellation_token.clone();

                downloads.spawn(async move {
                    download_chunk_with_permit(
                        tile,
                        chunk_row,
                        chunk_col,
                        provider,
                        disk_cache,
                        timeout,
                        max_retries,
                        metrics,
                        permit,
                        token,
                    )
                    .await
                });
            } else {
                // No permit available, break to wait for downloads to complete
                break;
            }
        }

        // Wait for either a download to complete or cancellation
        if !downloads.is_empty() {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    debug!(job_id = %job_id, "Download cancelled while waiting for results");
                    downloads.abort_all();
                    break;
                }

                result = downloads.join_next() => {
                    handle_download_result(job_id, result, &cancellation_token, results);
                }
            }
        } else if !chunks_to_download.is_empty() {
            // No active downloads and no permits available - wait for a permit
            wait_for_permit_and_spawn(
                job_id,
                tile,
                &provider,
                &disk_cache,
                config,
                &metrics,
                &http_limiter,
                &cancellation_token,
                &mut chunks_to_download,
                &mut downloads,
            )
            .await;

            if cancellation_token.is_cancelled() {
                break;
            }
        }
    }
}

/// Waits for a permit to become available and spawns a download task.
#[allow(clippy::too_many_arguments)]
async fn wait_for_permit_and_spawn<P, D>(
    job_id: JobId,
    tile: TileCoord,
    provider: &Arc<P>,
    disk_cache: &Arc<D>,
    config: &PipelineConfig,
    metrics: &Option<Arc<PipelineMetrics>>,
    http_limiter: &Arc<HttpConcurrencyLimiter>,
    cancellation_token: &CancellationToken,
    chunks_to_download: &mut VecDeque<(u8, u8)>,
    downloads: &mut JoinSet<Result<ChunkData, ChunkError>>,
) where
    P: ChunkProvider,
    D: DiskCache,
{
    // Use blocking acquire with short timeout and check cancellation
    let permit_timeout = std::time::Duration::from_millis(100);
    match tokio::time::timeout(permit_timeout, http_limiter.acquire()).await {
        Ok(permit) => {
            if cancellation_token.is_cancelled() {
                debug!(job_id = %job_id, "Download cancelled after acquiring permit");
                return;
            }

            let (chunk_row, chunk_col) = chunks_to_download.pop_front().unwrap();
            let provider = Arc::clone(provider);
            let disk_cache = Arc::clone(disk_cache);
            let metrics = metrics.clone();
            let timeout = config.request_timeout;
            let max_retries = config.max_retries;
            let token = cancellation_token.clone();

            downloads.spawn(async move {
                download_chunk_with_permit(
                    tile,
                    chunk_row,
                    chunk_col,
                    provider,
                    disk_cache,
                    timeout,
                    max_retries,
                    metrics,
                    permit,
                    token,
                )
                .await
            });
        }
        Err(_) => {
            // Timeout waiting for permit - caller will loop back and check cancellation
        }
    }
}

/// Handles the result of a download task.
fn handle_download_result(
    job_id: JobId,
    result: Option<Result<Result<ChunkData, ChunkError>, tokio::task::JoinError>>,
    cancellation_token: &CancellationToken,
    results: &mut ChunkResults,
) {
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
            // JoinSet is empty
        }
    }
}
