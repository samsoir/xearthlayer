//! Pipeline runner for processing DDS requests.
//!
//! This module provides the connection between the FUSE filesystem's
//! `DdsHandler` callback and the async pipeline processor.
//!
//! # Request Coalescing
//!
//! The runner includes automatic request coalescing to prevent duplicate work
//! when multiple requests for the same tile arrive simultaneously. This is
//! common during X-Plane's burst loading patterns where multiple file handles
//! request the same texture concurrently.
//!
//! # Control Plane Integration
//!
//! All DDS handlers are created with control plane integration, which provides:
//! - Job-level concurrency limiting
//! - Stage progression tracking
//! - Health monitoring and stall recovery

use crate::fuse::{DdsHandler, DdsRequest, DdsResponse, EXPECTED_DDS_SIZE};
use crate::pipeline::coalesce::CoalesceResult;
use crate::pipeline::control_plane::{PipelineControlPlane, StageObserver, SubmitResult};
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::processor::process_tile_with_observer;
use crate::pipeline::{
    BlockingExecutor, ChunkProvider, ConcurrencyLimiter, DiskCache, MemoryCache, PipelineConfig,
    TextureEncoderAsync,
};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{debug, error, info, instrument, warn};

/// Creates a `DdsHandler` integrated with the Pipeline Control Plane.
///
/// This version uses the control plane for:
/// - Job-level concurrency limiting (managed by control plane)
/// - Stage progression tracking (via StageObserver)
/// - Health monitoring and stall recovery
///
/// The handler still manages:
/// - Request coalescing (business logic)
/// - HTTP concurrency limiting (stage-level)
/// - CPU concurrency limiting (stage-level)
///
/// # Arguments
///
/// * `control_plane` - The pipeline control plane for job management
/// * `provider` - The chunk provider
/// * `encoder` - The texture encoder
/// * `memory_cache` - Memory cache for tiles
/// * `disk_cache` - Disk cache for chunks
/// * `executor` - Executor for blocking operations
/// * `config` - Pipeline configuration
/// * `runtime_handle` - Handle to the Tokio runtime
/// * `metrics` - Optional telemetry metrics
#[allow(clippy::too_many_arguments)]
pub fn create_dds_handler_with_control_plane<P, E, M, D, X>(
    control_plane: Arc<PipelineControlPlane>,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    config: PipelineConfig,
    runtime_handle: Handle,
    metrics: Option<Arc<PipelineMetrics>>,
) -> DdsHandler
where
    P: ChunkProvider + 'static,
    E: TextureEncoderAsync + 'static,
    M: MemoryCache + 'static,
    D: DiskCache + 'static,
    X: BlockingExecutor + 'static,
{
    // Create HTTP concurrency limiter (stage-level, not job-level)
    let http_limiter = Arc::new(HttpConcurrencyLimiter::new(config.max_global_http_requests));

    // Create shared CPU limiter for both assemble and encode stages
    let cpu_limiter = Arc::new(ConcurrencyLimiter::with_cpu_oversubscribe("cpu_bound"));

    info!(
        metrics_enabled = metrics.is_some(),
        max_http_concurrent = config.max_global_http_requests,
        max_cpu_concurrent = cpu_limiter.max_concurrent(),
        max_concurrent_jobs = control_plane.max_concurrent_jobs(),
        "Created DDS handler with control plane integration"
    );

    Arc::new(move |request: DdsRequest| {
        let control_plane = Arc::clone(&control_plane);
        let provider = Arc::clone(&provider);
        let encoder = Arc::clone(&encoder);
        let memory_cache = Arc::clone(&memory_cache);
        let disk_cache = Arc::clone(&disk_cache);
        let executor = Arc::clone(&executor);
        let http_limiter = Arc::clone(&http_limiter);
        let cpu_limiter = Arc::clone(&cpu_limiter);
        let metrics = metrics.clone();
        let config = config.clone();
        let job_id = request.job_id;
        let tile = request.tile;
        let is_prefetch = request.is_prefetch;

        debug!(
            job_id = %job_id,
            tile = ?tile,
            is_prefetch = is_prefetch,
            "Received DDS request (control plane)"
        );

        // Spawn the processing task on the Tokio runtime
        let spawn_result = runtime_handle.spawn(async move {
            process_dds_request_with_control_plane(
                request,
                control_plane,
                provider,
                encoder,
                memory_cache,
                disk_cache,
                executor,
                http_limiter,
                Arc::clone(&cpu_limiter),
                cpu_limiter,
                metrics,
                config,
            )
            .await;
        });

        if spawn_result.is_finished() {
            error!(
                job_id = %job_id,
                "Task finished immediately after spawn - this is unexpected"
            );
        }
    })
}

/// Process a DDS request using the control plane for job management.
///
/// This function:
/// 1. Checks coalescer for in-flight requests (prevents duplicate work)
/// 2. Submits work to control plane (job management, concurrency limiting)
/// 3. Uses process_tile_with_observer for stage tracking
/// 4. Broadcasts results to coalesced waiters
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(job_id = %request.job_id, tile = ?request.tile))]
async fn process_dds_request_with_control_plane<P, E, M, D, X>(
    request: DdsRequest,
    control_plane: Arc<PipelineControlPlane>,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    http_limiter: Arc<HttpConcurrencyLimiter>,
    assemble_limiter: Arc<ConcurrencyLimiter>,
    encode_limiter: Arc<ConcurrencyLimiter>,
    metrics: Option<Arc<PipelineMetrics>>,
    config: PipelineConfig,
) where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    let tile = request.tile;
    let job_id = request.job_id;
    let cancellation_token = request.cancellation_token.clone();
    let is_prefetch = request.is_prefetch;

    // Check for early cancellation before doing any work
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, tile = ?tile, "Request already cancelled before processing");
        return;
    }

    // Record job submission (request arrived, now waiting for processing)
    if let Some(ref m) = metrics {
        m.job_submitted();
    }

    // Get coalescer from control plane for request deduplication
    let coalescer = control_plane.coalescer();

    // Try to register this request with the coalescer (lock-free)
    let coalesce_result = coalescer.register(tile);

    match coalesce_result {
        CoalesceResult::Coalesced(mut rx) => {
            // Another request is in flight - wait for its result
            // This request moves from "waiting" to being serviced by an existing job
            if let Some(ref m) = metrics {
                m.job_coalesced();
                m.job_started(); // Moves from waiting to active (shared with existing)
            }

            debug!(
                job_id = %job_id,
                tile = ?tile,
                "Waiting for coalesced result (control plane path)"
            );

            // Wait for result OR cancellation
            let response = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    debug!(job_id = %job_id, tile = ?tile, "Coalesced wait cancelled");
                    // Record cancellation as timeout (not an error - FUSE timed out)
                    if let Some(ref m) = metrics {
                        m.job_timed_out();
                    }
                    // Don't send response - FUSE already timed out
                    return;
                }
                result = rx.recv() => {
                    match result {
                        Ok(result) => {
                            let data_len = result.data.len();
                            debug!(
                                job_id = %job_id,
                                tile = ?tile,
                                data_len,
                                "Received coalesced result"
                            );

                            // Validate coalesced data size
                            if data_len != EXPECTED_DDS_SIZE {
                                warn!(
                                    job_id = %job_id,
                                    tile = ?tile,
                                    actual_size = data_len,
                                    expected_size = EXPECTED_DDS_SIZE,
                                    "Coalesced result has unexpected size - FUSE layer will validate"
                                );
                            }

                            DdsResponse {
                                data: (*result.data).clone(),
                                cache_hit: result.cache_hit,
                                duration: result.duration,
                            }
                        }
                        Err(_) => {
                            // Channel closed without sending - leader was cancelled
                            debug!(
                                job_id = %job_id,
                                tile = ?tile,
                                "Coalesced request failed - leader cancelled"
                            );
                            DdsResponse {
                                data: crate::fuse::get_default_placeholder(),
                                cache_hit: false,
                                duration: std::time::Duration::ZERO,
                            }
                        }
                    }
                }
            };

            // Send response back to FUSE handler (may fail if already timed out)
            let _ = request.result_tx.send(response);

            // Record job completion (coalesced jobs still count as completed)
            if let Some(ref m) = metrics {
                m.job_completed();
            }
        }
        CoalesceResult::NewRequest { .. } => {
            // This is the first request for this tile - process it via control plane
            // Move from "waiting" to "active" state
            if let Some(ref m) = metrics {
                m.job_started();
            }

            debug!(
                job_id = %job_id,
                tile = ?tile,
                "Processing DDS request - submitting to control plane"
            );

            // Submit work to control plane
            let result = control_plane
                .submit(
                    job_id,
                    tile,
                    is_prefetch,
                    cancellation_token.clone(),
                    |observer: Arc<dyn StageObserver>| {
                        let provider = Arc::clone(&provider);
                        let encoder = Arc::clone(&encoder);
                        let memory_cache = Arc::clone(&memory_cache);
                        let disk_cache = Arc::clone(&disk_cache);
                        let executor_ref = executor.as_ref();
                        let config = config.clone();
                        let metrics = metrics.clone();
                        let http_limiter = Some(Arc::clone(&http_limiter));
                        let assemble_limiter = Some(Arc::clone(&assemble_limiter));
                        let encode_limiter = Some(Arc::clone(&encode_limiter));
                        let cancellation_token = cancellation_token.clone();

                        async move {
                            process_tile_with_observer(
                                job_id,
                                tile,
                                provider,
                                encoder,
                                memory_cache,
                                disk_cache,
                                executor_ref,
                                &config,
                                metrics,
                                http_limiter,
                                assemble_limiter,
                                encode_limiter,
                                cancellation_token,
                                Some(observer),
                            )
                            .await
                        }
                    },
                )
                .await;

            debug!(
                job_id = %job_id,
                success = matches!(result, SubmitResult::Completed(_)),
                cancelled = cancellation_token.is_cancelled(),
                "Control plane submission finished"
            );

            // Handle result and send response
            let response = match result {
                SubmitResult::Completed(job_result) => {
                    // Record successful completion
                    if let Some(ref m) = metrics {
                        m.job_completed();
                    }

                    // Validate generated data size before sending
                    if job_result.dds_data.len() != EXPECTED_DDS_SIZE {
                        warn!(
                            job_id = %job_id,
                            tile = ?tile,
                            actual_size = job_result.dds_data.len(),
                            expected_size = EXPECTED_DDS_SIZE,
                            cache_hit = job_result.cache_hit,
                            failed_chunks = job_result.failed_chunks,
                            "Generated DDS has unexpected size - FUSE layer will validate"
                        );
                    }

                    DdsResponse::from(job_result)
                }
                SubmitResult::NoCapacity => {
                    debug!(job_id = %job_id, tile = ?tile, "Prefetch rejected - no capacity");
                    // Clean up coalescer entry so waiters get notified
                    coalescer.cancel(tile);
                    if let Some(ref m) = metrics {
                        m.job_failed();
                    }
                    // Don't send response - prefetch was skipped
                    return;
                }
                SubmitResult::Cancelled | SubmitResult::Recovered => {
                    debug!(job_id = %job_id, tile = ?tile, "Job cancelled or recovered");
                    if let Some(ref m) = metrics {
                        // This is a timeout/cancellation, not an error
                        m.job_timed_out();
                    }
                    // Clean up coalescer entry so waiters get notified
                    coalescer.cancel(tile);
                    // Don't send response for cancellation
                    return;
                }
                SubmitResult::Failed(e) => {
                    error!(job_id = %job_id, error = %e, "Pipeline processing failed");
                    if let Some(ref m) = metrics {
                        m.job_failed();
                    }
                    DdsResponse {
                        data: crate::fuse::get_default_placeholder(),
                        cache_hit: false,
                        duration: std::time::Duration::ZERO,
                    }
                }
                SubmitResult::Shutdown => {
                    debug!(job_id = %job_id, tile = ?tile, "Control plane shutting down");
                    if let Some(ref m) = metrics {
                        m.job_failed();
                    }
                    // Clean up coalescer entry
                    coalescer.cancel(tile);
                    DdsResponse {
                        data: crate::fuse::get_default_placeholder(),
                        cache_hit: false,
                        duration: std::time::Duration::ZERO,
                    }
                }
            };

            // Complete the coalesced request - broadcasts to all waiters
            coalescer.complete(tile, response.clone());

            // Send response back to the original requester
            debug!(
                job_id = %job_id,
                data_len = response.data.len(),
                "Sending response (control plane)"
            );
            let _ = request.result_tx.send(response);
        }
    }
}

// Tests for the control plane integration are in the control_plane module.
// The runner simply wires up the control plane with the pipeline components.
