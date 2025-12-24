//! Download stage - fetches chunks from imagery provider.
//!
//! This stage downloads all 256 chunks (16x16 grid) that compose a single tile.
//! It uses Tokio's JoinSet for concurrent downloads and handles retries for
//! transient failures.
//!
//! # Concurrency Control
//!
//! The download stage supports an optional global HTTP concurrency limiter
//! (`HttpConcurrencyLimiter`) that constrains the total number of concurrent
//! HTTP requests across all tiles being processed. This prevents network
//! stack exhaustion under heavy load.
//!
//! Without the limiter, all 256 chunks per tile are downloaded concurrently.
//! With the limiter, downloads wait for permits before making HTTP requests.

use crate::coord::TileCoord;
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::{ChunkProvider, ChunkResults, DiskCache, JobId, PipelineConfig};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use std::time::Instant;
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
                // Abort remaining tasks in the JoinSet
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
                        // Don't log if this was due to cancellation
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
                        // Task was aborted (likely due to cancellation) or panicked
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

/// Downloads a single chunk with cancellation support.
#[allow(clippy::too_many_arguments)]
async fn download_chunk_cancellable<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: std::time::Duration,
    max_retries: u32,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
    cancellation_token: CancellationToken,
) -> Result<ChunkData, ChunkError>
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Check disk cache first (no HTTP permit needed, fast operation)
    if let Some(cached) = disk_cache
        .get(tile.row, tile.col, tile.zoom, chunk_row, chunk_col)
        .await
    {
        if let Some(ref m) = metrics {
            m.disk_cache_hit();
        }
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

    // Check cancellation before network request
    if cancellation_token.is_cancelled() {
        return Err(ChunkError {
            row: chunk_row,
            col: chunk_col,
            attempts: 0,
            error: "cancelled".to_string(),
        });
    }

    if let Some(ref m) = metrics {
        m.disk_cache_miss();
    }

    let global_row = tile.row * 16 + chunk_row as u32;
    let global_col = tile.col * 16 + chunk_col as u32;
    let chunk_zoom = tile.zoom + 4;

    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        // Check cancellation before each retry
        if cancellation_token.is_cancelled() {
            return Err(ChunkError {
                row: chunk_row,
                col: chunk_col,
                attempts: attempt - 1,
                error: "cancelled".to_string(),
            });
        }

        if let Some(ref m) = metrics {
            m.download_started();
        }
        let start = Instant::now();

        // Acquire HTTP permit (with cancellation check while waiting)
        let _http_permit = if let Some(ref limiter) = http_limiter {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    // IMPORTANT: Balance the download_started() call above
                    if let Some(ref m) = metrics {
                        m.download_cancelled();
                    }
                    return Err(ChunkError {
                        row: chunk_row,
                        col: chunk_col,
                        attempts: attempt - 1,
                        error: "cancelled while waiting for HTTP permit".to_string(),
                    });
                }
                permit = limiter.acquire() => Some(permit),
            }
        } else {
            None
        };

        // Perform the download with cancellation support
        let download_result = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                // IMPORTANT: Balance the download_started() call above
                if let Some(ref m) = metrics {
                    m.download_cancelled();
                }
                return Err(ChunkError {
                    row: chunk_row,
                    col: chunk_col,
                    attempts: attempt,
                    error: "cancelled during download".to_string(),
                });
            }
            result = tokio::time::timeout(
                timeout,
                provider.download_chunk(global_row, global_col, chunk_zoom),
            ) => result,
        };

        match download_result {
            Ok(Ok(data)) => {
                let duration_us = start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;
                if let Some(ref m) = metrics {
                    m.download_completed(bytes, duration_us);
                }

                drop(_http_permit);

                // Cache write (fire and forget)
                let dc = Arc::clone(&disk_cache);
                let chunk_data = data.clone();
                let cache_bytes = chunk_data.len() as u64;
                let cache_metrics = metrics.clone();
                tokio::spawn(async move {
                    if dc
                        .put(
                            tile.row, tile.col, tile.zoom, chunk_row, chunk_col, chunk_data,
                        )
                        .await
                        .is_ok()
                    {
                        if let Some(ref m) = cache_metrics {
                            m.add_disk_cache_bytes(cache_bytes);
                        }
                    }
                });

                return Ok(ChunkData {
                    row: chunk_row,
                    col: chunk_col,
                    data,
                });
            }
            Ok(Err(e)) => {
                if let Some(ref m) = metrics {
                    m.download_failed();
                }
                last_error = e.message.clone();
                drop(_http_permit);

                if !e.is_retryable {
                    break;
                }
                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
            Err(_) => {
                if let Some(ref m) = metrics {
                    m.download_failed();
                }
                last_error = "timeout".to_string();
                drop(_http_permit);

                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
        }

        // Backoff with cancellation check
        if attempt < max_retries {
            let backoff = std::time::Duration::from_millis(100 * (1 << attempt));
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    return Err(ChunkError {
                        row: chunk_row,
                        col: chunk_col,
                        attempts: attempt,
                        error: "cancelled during backoff".to_string(),
                    });
                }
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
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
///
/// If an `http_limiter` is provided, acquires a permit before making the HTTP
/// request. The permit is held for the duration of the HTTP request only,
/// not during cache checks or retries.
#[allow(clippy::too_many_arguments)]
async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: std::time::Duration,
    max_retries: u32,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
) -> Result<ChunkData, ChunkError>
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Check disk cache first (no HTTP permit needed)
    if let Some(cached) = disk_cache
        .get(tile.row, tile.col, tile.zoom, chunk_row, chunk_col)
        .await
    {
        // Record disk cache hit
        if let Some(ref m) = metrics {
            m.disk_cache_hit();
        }
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

    // Record disk cache miss
    if let Some(ref m) = metrics {
        m.disk_cache_miss();
    }

    // Calculate global coordinates for the provider
    let global_row = tile.row * 16 + chunk_row as u32;
    let global_col = tile.col * 16 + chunk_col as u32;
    let chunk_zoom = tile.zoom + 4;

    // Try downloading with retries
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        // Record download start
        if let Some(ref m) = metrics {
            m.download_started();
        }
        let start = Instant::now();

        // Acquire HTTP permit if limiter is configured.
        // The permit is held only for the duration of the HTTP request.
        // We acquire inside the retry loop so that:
        // 1. Cache hits don't consume permits
        // 2. Backoff delays don't hold permits
        // 3. Other downloads can proceed during our backoff
        let _http_permit = if let Some(ref limiter) = http_limiter {
            Some(limiter.acquire().await)
        } else {
            None
        };

        match tokio::time::timeout(
            timeout,
            provider.download_chunk(global_row, global_col, chunk_zoom),
        )
        .await
        {
            Ok(Ok(data)) => {
                // Record successful download
                let duration_us = start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;
                if let Some(ref m) = metrics {
                    m.download_completed(bytes, duration_us);
                }

                // Release permit before cache write (permit drops here)
                drop(_http_permit);

                // Success - cache the chunk (fire and forget, don't block on cache write)
                let dc = Arc::clone(&disk_cache);
                let chunk_data = data.clone();
                let cache_bytes = chunk_data.len() as u64;
                let cache_metrics = metrics.clone();
                tokio::spawn(async move {
                    if dc
                        .put(
                            tile.row, tile.col, tile.zoom, chunk_row, chunk_col, chunk_data,
                        )
                        .await
                        .is_ok()
                    {
                        // Track disk cache growth
                        if let Some(ref m) = cache_metrics {
                            m.add_disk_cache_bytes(cache_bytes);
                        }
                    }
                });

                return Ok(ChunkData {
                    row: chunk_row,
                    col: chunk_col,
                    data,
                });
            }
            Ok(Err(e)) => {
                // Record failed download
                if let Some(ref m) = metrics {
                    m.download_failed();
                }
                last_error = e.message.clone();

                // Release permit before backoff (permit drops here)
                drop(_http_permit);

                if !e.is_retryable {
                    // Permanent error, don't retry
                    break;
                }
                // Record retry
                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
            Err(_) => {
                // Record failed download (timeout)
                if let Some(ref m) = metrics {
                    m.download_failed();
                }
                last_error = "timeout".to_string();

                // Release permit before backoff (permit drops here)
                drop(_http_permit);

                // Record retry
                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
        }

        // Brief delay before retry (exponential backoff)
        // Permit is already released so other downloads can proceed
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

/// Downloads all chunks for a tile with permit-bounded task spawning.
///
/// This is the recommended download function that prevents task avalanche by only
/// spawning download tasks when HTTP permits are available. This ensures that
/// `downloads_active` accurately reflects actual HTTP connections, not queued tasks.
///
/// # Two-Phase Approach
///
/// 1. **Cache Check Phase (unbounded):** All 256 chunks are checked against disk
///    cache in parallel. This is fast I/O that doesn't require HTTP permits.
///
/// 2. **Download Phase (bounded):** Only cache misses proceed to download. Tasks
///    are spawned one-by-one as HTTP permits become available, preventing the
///    spawn of thousands of waiting tasks.
///
/// # Benefits
///
/// - `downloads_active` matches actual HTTP connections (not inflated by waiters)
/// - Memory usage bounded by HTTP limiter capacity
/// - No file descriptor exhaustion from spawned-but-waiting tasks
/// - Smooth ramp-up of network utilization
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `tile` - The tile coordinates to download
/// * `provider` - Chunk download provider
/// * `disk_cache` - Disk cache for chunk persistence
/// * `config` - Pipeline configuration
/// * `metrics` - Optional metrics collector
/// * `http_limiter` - HTTP concurrency limiter (required for bounded behavior)
/// * `cancellation_token` - Token to signal cancellation
///
/// # Returns
///
/// `ChunkResults` containing successful downloads and failures.
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
    use std::collections::VecDeque;

    let mut results = ChunkResults::new();

    // Early cancellation check
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, "Download stage cancelled before starting");
        return results;
    }

    // Phase 1: Check disk cache for all chunks in parallel (no HTTP permits needed)
    // This is fast disk I/O that doesn't consume network resources
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

    // Collect cache results, building list of chunks that need downloading
    let mut chunks_to_download: VecDeque<(u8, u8)> = VecDeque::new();

    while let Some(result) = cache_checks.join_next().await {
        if cancellation_token.is_cancelled() {
            debug!(job_id = %job_id, "Cache check cancelled");
            cache_checks.abort_all();
            return results;
        }

        match result {
            Ok((chunk_row, chunk_col, Some(data))) => {
                // Cache hit - add directly to results
                results.add_success(chunk_row, chunk_col, data);
            }
            Ok((chunk_row, chunk_col, None)) => {
                // Cache miss - queue for download
                chunks_to_download.push_back((chunk_row, chunk_col));
            }
            Err(join_err) => {
                warn!(job_id = %job_id, error = %join_err, "Cache check task panicked");
            }
        }
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
    // Only spawn tasks as HTTP permits become available
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
            // Try non-blocking permit acquisition
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
            }
        } else if !chunks_to_download.is_empty() {
            // No active downloads and no permits available - wait for a permit
            // Use blocking acquire since we have no other work to do
            // Note: We don't use select! here due to lifetime issues with HttpPermit
            // Instead, we wait with a short timeout and check cancellation
            let permit_timeout = std::time::Duration::from_millis(100);
            match tokio::time::timeout(permit_timeout, http_limiter.acquire()).await {
                Ok(permit) => {
                    // Check cancellation after acquiring permit
                    if cancellation_token.is_cancelled() {
                        debug!(job_id = %job_id, "Download cancelled after acquiring permit");
                        // Permit drops here, releasing it
                        break;
                    }

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
                }
                Err(_) => {
                    // Timeout waiting for permit - loop back and check cancellation
                    continue;
                }
            }
        }
    }

    debug!(
        job_id = %job_id,
        success = results.success_count(),
        failed = results.failure_count(),
        cancelled = cancellation_token.is_cancelled(),
        "Download stage complete (bounded)"
    );

    results
}

/// Downloads a single chunk with an already-acquired HTTP permit.
///
/// This function is used by `download_stage_bounded` where the permit is acquired
/// before spawning the task, ensuring `downloads_active` accurately reflects
/// actual HTTP connections.
#[allow(clippy::too_many_arguments)]
async fn download_chunk_with_permit<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: std::time::Duration,
    max_retries: u32,
    metrics: Option<Arc<PipelineMetrics>>,
    _permit: crate::pipeline::http_limiter::HttpPermit,
    cancellation_token: CancellationToken,
) -> Result<ChunkData, ChunkError>
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Note: We already have the HTTP permit, so we're counted in downloads_active
    // via the HttpConcurrencyLimiter's in_flight counter.

    // Record download start (for metrics - matches HTTP permit acquisition)
    if let Some(ref m) = metrics {
        m.download_started();
    }

    let global_row = tile.row * 16 + chunk_row as u32;
    let global_col = tile.col * 16 + chunk_col as u32;
    let chunk_zoom = tile.zoom + 4;

    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        // Check cancellation before each attempt
        if cancellation_token.is_cancelled() {
            if let Some(ref m) = metrics {
                m.download_cancelled();
            }
            return Err(ChunkError {
                row: chunk_row,
                col: chunk_col,
                attempts: attempt - 1,
                error: "cancelled".to_string(),
            });
        }

        let start = Instant::now();

        // Perform the download with cancellation support
        let download_result = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                if let Some(ref m) = metrics {
                    m.download_cancelled();
                }
                return Err(ChunkError {
                    row: chunk_row,
                    col: chunk_col,
                    attempts: attempt,
                    error: "cancelled during download".to_string(),
                });
            }
            result = tokio::time::timeout(
                timeout,
                provider.download_chunk(global_row, global_col, chunk_zoom),
            ) => result,
        };

        match download_result {
            Ok(Ok(data)) => {
                let duration_us = start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;
                if let Some(ref m) = metrics {
                    m.download_completed(bytes, duration_us);
                }

                // Cache write (fire and forget)
                let dc = Arc::clone(&disk_cache);
                let chunk_data = data.clone();
                let cache_bytes = chunk_data.len() as u64;
                let cache_metrics = metrics.clone();
                tokio::spawn(async move {
                    if dc
                        .put(
                            tile.row, tile.col, tile.zoom, chunk_row, chunk_col, chunk_data,
                        )
                        .await
                        .is_ok()
                    {
                        if let Some(ref m) = cache_metrics {
                            m.add_disk_cache_bytes(cache_bytes);
                        }
                    }
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
                    break;
                }
                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
            Err(_) => {
                last_error = "timeout".to_string();
                if attempt < max_retries {
                    if let Some(ref m) = metrics {
                        m.download_retried();
                    }
                }
            }
        }

        // Backoff with cancellation check
        if attempt < max_retries {
            let backoff = std::time::Duration::from_millis(100 * (1 << attempt));
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    if let Some(ref m) = metrics {
                        m.download_cancelled();
                    }
                    return Err(ChunkError {
                        row: chunk_row,
                        col: chunk_col,
                        attempts: attempt,
                        error: "cancelled during backoff".to_string(),
                    });
                }
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }

    // All retries exhausted
    if let Some(ref m) = metrics {
        m.download_failed();
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

        let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

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

        let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

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

        let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

        assert_eq!(results.success_count(), 256);

        // Check that chunk (0, 0) has cached data
        let cached_chunk = results.get(0, 0).unwrap();
        assert_eq!(cached_chunk, &[0xCA, 0xCE, 0xD]);
    }

    #[tokio::test]
    async fn test_download_stage_with_limiter() {
        let provider = Arc::new(MockProvider::new());
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        // Create a limiter with only 10 concurrent requests
        let limiter = Arc::new(HttpConcurrencyLimiter::new(10));

        let results = download_stage_with_limiter(
            JobId::new(),
            tile,
            provider,
            cache,
            &config,
            None,
            Some(Arc::clone(&limiter)),
        )
        .await;

        // All 256 chunks should still succeed
        assert_eq!(results.success_count(), 256);
        assert_eq!(results.failure_count(), 0);
        assert!(results.is_complete());

        // Limiter should have tracked peak usage (should be <= 10)
        assert!(limiter.peak_in_flight() <= 10);
        // After completion, no requests should be in flight
        assert_eq!(limiter.in_flight(), 0);
    }

    #[tokio::test]
    async fn test_download_stage_limiter_constrains_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Track maximum concurrent downloads observed
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current_concurrent = Arc::new(AtomicUsize::new(0));

        /// Mock provider that tracks concurrency
        struct ConcurrencyTrackingProvider {
            current: Arc<AtomicUsize>,
            max: Arc<AtomicUsize>,
        }

        impl ChunkProvider for ConcurrencyTrackingProvider {
            async fn download_chunk(
                &self,
                row: u32,
                col: u32,
                zoom: u8,
            ) -> Result<Vec<u8>, ChunkDownloadError> {
                // Increment current count
                let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;

                // Update max if this is a new peak
                let mut max = self.max.load(Ordering::SeqCst);
                while current > max {
                    match self.max.compare_exchange_weak(
                        max,
                        current,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(m) => max = m,
                    }
                }

                // Simulate some async work to allow other tasks to run
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;

                // Decrement current count
                self.current.fetch_sub(1, Ordering::SeqCst);

                Ok(vec![row as u8, col as u8, zoom])
            }

            fn name(&self) -> &str {
                "concurrency-tracker"
            }
        }

        let provider = Arc::new(ConcurrencyTrackingProvider {
            current: Arc::clone(&current_concurrent),
            max: Arc::clone(&max_concurrent),
        });
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        // Limit to 16 concurrent HTTP requests
        let limiter = Arc::new(HttpConcurrencyLimiter::new(16));

        let results = download_stage_with_limiter(
            JobId::new(),
            tile,
            provider,
            cache,
            &config,
            None,
            Some(limiter),
        )
        .await;

        assert_eq!(results.success_count(), 256);

        // Maximum observed concurrency should not exceed limiter's cap
        let observed_max = max_concurrent.load(Ordering::SeqCst);
        assert!(
            observed_max <= 16,
            "Expected max concurrent <= 16, got {}",
            observed_max
        );
    }

    #[tokio::test]
    async fn test_download_stage_bounded_basic() {
        let provider = Arc::new(MockProvider::new());
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let limiter = Arc::new(HttpConcurrencyLimiter::new(32));
        let token = CancellationToken::new();

        let results = download_stage_bounded(
            JobId::new(),
            tile,
            provider,
            cache,
            &config,
            None,
            Arc::clone(&limiter),
            token,
        )
        .await;

        assert_eq!(results.success_count(), 256);
        assert_eq!(results.failure_count(), 0);
        // After completion, no requests should be in flight
        assert_eq!(limiter.in_flight(), 0);
        // Peak should not exceed limit
        assert!(limiter.peak_in_flight() <= 32);
    }

    #[tokio::test]
    async fn test_download_stage_bounded_in_flight_matches_actual_connections() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Track how many times in_flight matched observed concurrent downloads
        let samples = Arc::new(AtomicUsize::new(0));
        let mismatches = Arc::new(AtomicUsize::new(0));

        /// Mock provider that verifies in_flight count during download
        struct VerifyingProvider {
            limiter: Arc<HttpConcurrencyLimiter>,
            samples: Arc<AtomicUsize>,
            mismatches: Arc<AtomicUsize>,
        }

        impl ChunkProvider for VerifyingProvider {
            async fn download_chunk(
                &self,
                row: u32,
                col: u32,
                zoom: u8,
            ) -> Result<Vec<u8>, ChunkDownloadError> {
                self.samples.fetch_add(1, Ordering::SeqCst);

                // The in_flight count should be > 0 since we're actively downloading
                let in_flight = self.limiter.in_flight();
                if in_flight == 0 {
                    self.mismatches.fetch_add(1, Ordering::SeqCst);
                }

                // Simulate some work
                tokio::time::sleep(std::time::Duration::from_micros(50)).await;

                Ok(vec![row as u8, col as u8, zoom])
            }

            fn name(&self) -> &str {
                "verifying"
            }
        }

        let limiter = Arc::new(HttpConcurrencyLimiter::new(16));
        let provider = Arc::new(VerifyingProvider {
            limiter: Arc::clone(&limiter),
            samples: Arc::clone(&samples),
            mismatches: Arc::clone(&mismatches),
        });
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let token = CancellationToken::new();

        let results = download_stage_bounded(
            JobId::new(),
            tile,
            provider,
            cache,
            &config,
            None,
            Arc::clone(&limiter),
            token,
        )
        .await;

        assert_eq!(results.success_count(), 256);
        assert_eq!(limiter.in_flight(), 0);

        // All downloads should have seen in_flight > 0
        assert_eq!(
            mismatches.load(Ordering::SeqCst),
            0,
            "Some downloads saw in_flight=0 when it should have been > 0"
        );
    }

    #[tokio::test]
    async fn test_download_stage_bounded_cache_hits_bypass_http_permits() {
        // Pre-populate cache with half the chunks (128 of 256)
        let mut cache = MockDiskCache::new();
        for row in 0..8u8 {
            for col in 0..16u8 {
                cache = cache.with_cached(100, 200, 16, row, col, vec![0xCA, 0xCE]);
            }
        }
        // 128 chunks cached (rows 0-7), 128 need download (rows 8-15)
        let cache = Arc::new(cache);

        let provider = Arc::new(MockProvider::new());
        let config = PipelineConfig::default();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        // Use a small HTTP limit - if cache hits counted against it, we'd be slow
        let limiter = Arc::new(HttpConcurrencyLimiter::new(16));
        let token = CancellationToken::new();

        let results = download_stage_bounded(
            JobId::new(),
            tile,
            provider,
            cache,
            &config,
            None,
            Arc::clone(&limiter),
            token,
        )
        .await;

        assert_eq!(results.success_count(), 256);
        assert_eq!(results.failure_count(), 0);
        // Peak should be ≤16 since only 128 chunks needed HTTP permits
        assert!(
            limiter.peak_in_flight() <= 16,
            "Peak {} exceeded limit 16",
            limiter.peak_in_flight()
        );
    }

    #[tokio::test]
    async fn test_download_stage_bounded_multiple_tiles_share_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current_concurrent = Arc::new(AtomicUsize::new(0));

        /// Mock provider that tracks concurrency across tiles
        struct MultiTileProvider {
            current: Arc<AtomicUsize>,
            max: Arc<AtomicUsize>,
        }

        impl ChunkProvider for MultiTileProvider {
            async fn download_chunk(
                &self,
                row: u32,
                col: u32,
                zoom: u8,
            ) -> Result<Vec<u8>, ChunkDownloadError> {
                let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;

                let mut max = self.max.load(Ordering::SeqCst);
                while current > max {
                    match self.max.compare_exchange_weak(
                        max,
                        current,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(m) => max = m,
                    }
                }

                tokio::time::sleep(std::time::Duration::from_micros(100)).await;

                self.current.fetch_sub(1, Ordering::SeqCst);

                Ok(vec![row as u8, col as u8, zoom])
            }

            fn name(&self) -> &str {
                "multi-tile"
            }
        }

        let provider = Arc::new(MultiTileProvider {
            current: Arc::clone(&current_concurrent),
            max: Arc::clone(&max_concurrent),
        });
        let cache = Arc::new(NullDiskCache);
        let config = PipelineConfig::default();
        let limiter = Arc::new(HttpConcurrencyLimiter::new(32));

        // Spawn 4 tiles concurrently, each with 256 chunks = 1024 total downloads
        // But should never exceed 32 concurrent
        let mut handles = Vec::new();
        for i in 0..4 {
            let provider = Arc::clone(&provider);
            let cache = Arc::clone(&cache);
            let config = config.clone();
            let limiter = Arc::clone(&limiter);

            handles.push(tokio::spawn(async move {
                let tile = TileCoord {
                    row: 100 + i,
                    col: 200,
                    zoom: 16,
                };
                let token = CancellationToken::new();

                download_stage_bounded(
                    JobId::new(),
                    tile,
                    provider,
                    cache,
                    &config,
                    None,
                    limiter,
                    token,
                )
                .await
            }));
        }

        // Wait for all to complete
        let mut total_success = 0;
        for handle in handles {
            let results = handle.await.unwrap();
            total_success += results.success_count();
        }

        assert_eq!(total_success, 256 * 4);

        // Maximum observed concurrency should not exceed limiter's cap
        let observed_max = max_concurrent.load(Ordering::SeqCst);
        assert!(
            observed_max <= 32,
            "Expected max concurrent <= 32, got {} (4 tiles × 256 chunks = 1024 potential)",
            observed_max
        );

        // After completion, no requests should be in flight
        assert_eq!(limiter.in_flight(), 0);
    }
}
