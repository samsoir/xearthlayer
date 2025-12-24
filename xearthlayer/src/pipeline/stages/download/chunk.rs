//! Chunk download functions.
//!
//! This module handles individual chunk downloads with cache integration,
//! retry logic, and optional HTTP concurrency limiting.

use crate::coord::TileCoord;
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::{ChunkProvider, DiskCache};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Result of a successful chunk download.
pub(super) struct ChunkData {
    pub row: u8,
    pub col: u8,
    pub data: Vec<u8>,
}

/// Result of a failed chunk download.
pub(super) struct ChunkError {
    pub row: u8,
    pub col: u8,
    pub attempts: u32,
    pub error: String,
}

/// Downloads a single chunk, checking cache first.
///
/// If an `http_limiter` is provided, acquires a permit before making the HTTP
/// request. The permit is held for the duration of the HTTP request only,
/// not during cache checks or retries.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
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
        if let Some(ref m) = metrics {
            m.disk_cache_hit();
        }
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

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
        if let Some(ref m) = metrics {
            m.download_started();
        }
        let start = Instant::now();

        // Acquire HTTP permit if limiter is configured.
        // The permit is held only for the duration of the HTTP request.
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
                let duration_us = start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;
                if let Some(ref m) = metrics {
                    m.download_completed(bytes, duration_us);
                }

                // Release permit before cache write
                drop(_http_permit);

                // Cache the chunk (fire and forget)
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

        // Exponential backoff before retry
        if attempt < max_retries {
            tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
        }
    }

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
}

/// Downloads a single chunk with cancellation support.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_chunk_cancellable<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
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
            let backoff = Duration::from_millis(100 * (1 << attempt));
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

/// Downloads a single chunk with an already-acquired HTTP permit.
///
/// This function is used by `download_stage_bounded` where the permit is acquired
/// before spawning the task, ensuring `downloads_active` accurately reflects
/// actual HTTP connections.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_chunk_with_permit<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
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
            let backoff = Duration::from_millis(100 * (1 << attempt));
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

/// Spawns a fire-and-forget task to write chunk data to disk cache.
fn spawn_cache_write<D>(
    disk_cache: Arc<D>,
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    data: Vec<u8>,
    metrics: Option<Arc<PipelineMetrics>>,
) where
    D: DiskCache,
{
    let cache_bytes = data.len() as u64;
    tokio::spawn(async move {
        if disk_cache
            .put(tile.row, tile.col, tile.zoom, chunk_row, chunk_col, data)
            .await
            .is_ok()
        {
            if let Some(ref m) = metrics {
                m.add_disk_cache_bytes(cache_bytes);
            }
        }
    });
}
