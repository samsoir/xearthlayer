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

use crate::fuse::{DdsHandler, DdsRequest, DdsResponse, EXPECTED_DDS_SIZE};
use crate::pipeline::coalesce::{CoalesceResult, RequestCoalescer};
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::processor::{process_tile, process_tile_cancellable};
use crate::pipeline::{
    BlockingExecutor, ChunkProvider, ConcurrencyLimiter, DiskCache, MemoryCache, PipelineConfig,
    TextureEncoderAsync,
};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{debug, error, info, instrument, warn};

/// Creates a `DdsHandler` that processes requests through the async pipeline.
///
/// This function creates the bridge between FUSE's synchronous callback model
/// and the async pipeline. Each DDS request spawns a task on the Tokio runtime
/// that processes the tile and sends the result back via oneshot channel.
///
/// **Request Coalescing**: When multiple requests for the same tile arrive
/// simultaneously, only one processing task runs. All waiters receive the
/// same result, preventing duplicate work.
///
/// # Type Parameters
///
/// * `P` - Chunk provider (downloads satellite imagery)
/// * `E` - Texture encoder (DDS compression)
/// * `M` - Memory cache (tile-level)
/// * `D` - Disk cache (chunk-level)
/// * `X` - Blocking executor
///
/// # Arguments
///
/// * `provider` - The chunk provider
/// * `encoder` - The texture encoder
/// * `memory_cache` - Memory cache for tiles
/// * `disk_cache` - Disk cache for chunks
/// * `executor` - Executor for blocking operations
/// * `config` - Pipeline configuration
/// * `runtime_handle` - Handle to the Tokio runtime
///
/// # Example
///
/// ```ignore
/// use xearthlayer::pipeline::{create_dds_handler, TokioExecutor, PipelineConfig};
/// use xearthlayer::pipeline::adapters::*;
///
/// let handler = create_dds_handler(
///     Arc::new(provider_adapter),
///     Arc::new(encoder_adapter),
///     Arc::new(memory_cache_adapter),
///     Arc::new(disk_cache_adapter),
///     Arc::new(TokioExecutor::new()),
///     PipelineConfig::default(),
///     runtime.handle().clone(),
/// );
///
/// // Use handler with AsyncPassthroughFS
/// let fs = AsyncPassthroughFS::new(source_dir, handler, ...);
/// ```
pub fn create_dds_handler<P, E, M, D, X>(
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    config: PipelineConfig,
    runtime_handle: Handle,
) -> DdsHandler
where
    P: ChunkProvider + 'static,
    E: TextureEncoderAsync + 'static,
    M: MemoryCache + 'static,
    D: DiskCache + 'static,
    X: BlockingExecutor + 'static,
{
    create_dds_handler_with_metrics(
        provider,
        encoder,
        memory_cache,
        disk_cache,
        executor,
        config,
        runtime_handle,
        None,
    )
}

/// Creates a `DdsHandler` with optional telemetry metrics collection.
///
/// This is the full version that accepts an optional `PipelineMetrics` for
/// telemetry collection. Use `create_dds_handler` for the simpler API without metrics.
#[allow(clippy::too_many_arguments)]
pub fn create_dds_handler_with_metrics<P, E, M, D, X>(
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
    // Create shared coalescer for all requests
    let coalescer = Arc::new(RequestCoalescer::new());

    // Create global HTTP concurrency limiter based on config
    let http_limiter = Arc::new(HttpConcurrencyLimiter::new(config.max_global_http_requests));

    // Create shared CPU limiter for both assemble and encode stages
    // Both are CPU-bound and compete for blocking thread pool, so sharing a limiter
    // with modest over-subscription (~1.25x cores) keeps cores busy during brief
    // I/O waits while preventing deadlock from thread pool exhaustion
    let cpu_limiter = Arc::new(ConcurrencyLimiter::with_cpu_oversubscribe("cpu_bound"));

    info!(
        metrics_enabled = metrics.is_some(),
        max_http_concurrent = config.max_global_http_requests,
        max_cpu_concurrent = cpu_limiter.max_concurrent(),
        "Created DDS handler with request coalescing and concurrency limiting"
    );

    Arc::new(move |request: DdsRequest| {
        let provider = Arc::clone(&provider);
        let encoder = Arc::clone(&encoder);
        let memory_cache = Arc::clone(&memory_cache);
        let disk_cache = Arc::clone(&disk_cache);
        let executor = Arc::clone(&executor);
        let coalescer = Arc::clone(&coalescer);
        let http_limiter = Arc::clone(&http_limiter);
        let cpu_limiter = Arc::clone(&cpu_limiter);
        let metrics = metrics.clone();
        let config = config.clone();
        let job_id = request.job_id;
        let tile = request.tile;

        debug!(
            job_id = %job_id,
            tile = ?tile,
            "Received DDS request"
        );

        // Spawn the processing task on the Tokio runtime
        let spawn_result = runtime_handle.spawn(async move {
            process_dds_request_coalesced(
                request,
                provider,
                encoder,
                memory_cache,
                disk_cache,
                executor,
                coalescer,
                http_limiter,
                Arc::clone(&cpu_limiter), // Shared for assemble
                cpu_limiter,              // Shared for encode
                metrics,
                config,
            )
            .await;
        });

        // Check if spawn succeeded (it should, but let's be sure)
        if spawn_result.is_finished() {
            error!(
                job_id = %job_id,
                "Task finished immediately after spawn - this is unexpected"
            );
        }
    })
}

/// Process a single DDS request through the pipeline.
///
/// This is the non-coalescing version, useful for testing or when coalescing
/// is not desired.
#[allow(dead_code)]
#[instrument(skip_all, fields(job_id = %request.job_id, tile = ?request.tile))]
async fn process_dds_request<P, E, M, D, X>(
    request: DdsRequest,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    config: PipelineConfig,
) where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    debug!(
        job_id = %request.job_id,
        tile = ?request.tile,
        "Processing DDS request - starting pipeline"
    );

    let result = process_tile(
        request.job_id,
        request.tile,
        provider,
        encoder,
        memory_cache,
        disk_cache,
        executor.as_ref(),
        &config,
        None, // No metrics for legacy non-coalescing path
        None, // No HTTP limiter for legacy non-coalescing path
        None, // No assemble limiter for legacy non-coalescing path
        None, // No encode limiter for legacy non-coalescing path
    )
    .await;

    debug!(
        job_id = %request.job_id,
        success = result.is_ok(),
        "Pipeline processing finished"
    );

    let response = match result {
        Ok(job_result) => DdsResponse::from(job_result),
        Err(e) => {
            error!(error = %e, "Pipeline processing failed");
            // Return placeholder on error
            DdsResponse {
                data: crate::fuse::get_default_placeholder(),
                cache_hit: false,
                duration: std::time::Duration::ZERO,
            }
        }
    };

    // Send response back to FUSE handler
    // Ignore error if receiver dropped (FUSE handler timed out)
    debug!(
        job_id = %request.job_id,
        data_len = response.data.len(),
        "Sending response"
    );
    let _ = request.result_tx.send(response);
}

/// Process a DDS request with coalescing support and cancellation.
///
/// This function checks if another request for the same tile is already in flight.
/// If so, it waits for that request to complete and shares the result.
/// Otherwise, it processes the tile and broadcasts the result to any waiting requests.
///
/// When the cancellation token is triggered (e.g., FUSE timeout), the pipeline
/// processing is aborted early to release resources.
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(job_id = %request.job_id, tile = ?request.tile))]
async fn process_dds_request_coalesced<P, E, M, D, X>(
    request: DdsRequest,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    coalescer: Arc<RequestCoalescer>,
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

    // Check for early cancellation before doing any work
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, tile = ?tile, "Request already cancelled before processing");
        return;
    }

    // Record job submission (request arrived, now waiting for processing)
    if let Some(ref m) = metrics {
        m.job_submitted();
    }

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
                "Waiting for coalesced result"
            );

            // Wait for result OR cancellation
            let response = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    debug!(job_id = %job_id, tile = ?tile, "Coalesced wait cancelled");
                    // Record cancellation as failure
                    if let Some(ref m) = metrics {
                        m.job_failed();
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
            // This is the first request for this tile - process it
            // Move from "waiting" to "active" state
            if let Some(ref m) = metrics {
                m.job_started();
            }

            debug!(
                job_id = %job_id,
                tile = ?tile,
                "Processing DDS request - starting pipeline"
            );

            let result = process_tile_cancellable(
                job_id,
                tile,
                provider,
                encoder,
                memory_cache,
                disk_cache,
                executor.as_ref(),
                &config,
                metrics.clone(),
                Some(http_limiter),
                Some(assemble_limiter),
                Some(encode_limiter),
                cancellation_token.clone(),
            )
            .await;

            debug!(
                job_id = %job_id,
                success = result.is_ok(),
                cancelled = cancellation_token.is_cancelled(),
                "Pipeline processing finished"
            );

            let response = match result {
                Ok(job_result) => {
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
                Err(e) => {
                    // Check if this was a cancellation
                    if cancellation_token.is_cancelled() {
                        debug!(job_id = %job_id, tile = ?tile, "Pipeline cancelled");
                        if let Some(ref m) = metrics {
                            m.job_failed();
                        }
                        // Clean up coalescer entry so waiters get notified
                        coalescer.cancel(tile);
                        return;
                    }

                    error!(error = %e, "Pipeline processing failed");
                    // Record job failure
                    if let Some(ref m) = metrics {
                        m.job_failed();
                    }
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
                "Sending response"
            );
            let _ = request.result_tx.send(response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::pipeline::{ChunkDownloadError, JobId, TextureEncodeError, TokioExecutor};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    // Mock implementations for testing

    struct MockProvider;

    impl ChunkProvider for MockProvider {
        async fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            // Return a minimal valid PNG
            Ok(create_test_png())
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    struct MockEncoder;

    impl TextureEncoderAsync for MockEncoder {
        fn encode(&self, _image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            Ok(vec![0xDD, 0x53, 0x20, 0x00])
        }

        fn expected_size(&self, _width: u32, _height: u32) -> usize {
            4
        }
    }

    struct MockMemoryCache {
        data: Mutex<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockMemoryCache {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    impl MemoryCache for MockMemoryCache {
        fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(&(row, col, zoom)).cloned()
        }

        fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
            self.data.lock().unwrap().insert((row, col, zoom), data);
        }

        fn size_bytes(&self) -> usize {
            0
        }

        fn entry_count(&self) -> usize {
            self.data.lock().unwrap().len()
        }
    }

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

    fn create_test_png() -> Vec<u8> {
        use image::{Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(256, 256, Rgba([255, 0, 0, 255]));
        let mut buffer = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buffer
    }

    #[tokio::test]
    async fn test_create_dds_handler() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let handler = create_dds_handler(
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
            Handle::current(),
        );

        // Create a request
        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
            cancellation_token: CancellationToken::new(),
        };

        // Call the handler
        handler(request);

        // Wait for response with timeout
        let response = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .expect("timeout")
            .expect("channel closed");

        assert!(!response.data.is_empty());
        assert!(!response.cache_hit);
    }

    #[tokio::test]
    async fn test_process_dds_request_success() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
            cancellation_token: CancellationToken::new(),
        };

        process_dds_request(
            request,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
        )
        .await;

        let response = rx.await.expect("channel closed");
        assert!(!response.data.is_empty());
    }

    #[tokio::test]
    async fn test_memory_cache_populated_after_request() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        // Initially empty
        assert_eq!(memory_cache.entry_count(), 0);

        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
            cancellation_token: CancellationToken::new(),
        };

        process_dds_request(
            request,
            provider,
            encoder,
            Arc::clone(&memory_cache),
            disk_cache,
            executor,
            config,
        )
        .await;

        let _ = rx.await;

        // Cache should now have an entry
        assert_eq!(memory_cache.entry_count(), 1);
        assert!(memory_cache.get(100, 200, 16).is_some());
    }

    #[tokio::test]
    async fn test_coalescing_multiple_requests_same_tile() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Provider that tracks how many times it's called
        struct CountingProvider {
            call_count: AtomicUsize,
        }

        impl CountingProvider {
            fn new() -> Self {
                Self {
                    call_count: AtomicUsize::new(0),
                }
            }
        }

        impl ChunkProvider for CountingProvider {
            async fn download_chunk(
                &self,
                _row: u32,
                _col: u32,
                _zoom: u8,
            ) -> Result<Vec<u8>, ChunkDownloadError> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                // Add a small delay to allow coalescing to happen
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(create_test_png())
            }

            fn name(&self) -> &str {
                "counting"
            }
        }

        let provider = Arc::new(CountingProvider::new());
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let handler = create_dds_handler(
            Arc::clone(&provider),
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
            Handle::current(),
        );

        // Send multiple requests for the same tile concurrently
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let mut handles = vec![];
        for _ in 0..5 {
            let (tx, rx) = oneshot::channel();
            let request = DdsRequest {
                job_id: JobId::new(),
                tile,
                result_tx: tx,
                cancellation_token: CancellationToken::new(),
            };

            handler(request);
            handles.push(rx);
        }

        // Wait for all responses
        for handle in handles {
            let response = tokio::time::timeout(Duration::from_secs(60), handle)
                .await
                .expect("timeout")
                .expect("channel closed");

            // All should succeed
            assert!(!response.data.is_empty());
        }

        // With coalescing, the provider should be called significantly fewer times
        // than 5 * 256 = 1280 times (one per chunk per request).
        // In the ideal case with perfect coalescing, it would be 256 calls
        // (one per chunk for the single processing request).
        let total_calls = provider.call_count.load(Ordering::SeqCst);

        // Allow some variance due to timing, but expect significant reduction
        // from 1280 (without coalescing) to ~256 (with perfect coalescing)
        assert!(
            total_calls < 1000,
            "Expected coalescing to reduce calls, got {} calls (expected <1000)",
            total_calls
        );
    }
}
