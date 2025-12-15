//! Job processor - orchestrates pipeline stages.
//!
//! The processor takes a Job and runs it through all pipeline stages:
//! 1. Check memory cache (short-circuit if hit)
//! 2. Download chunks
//! 3. Assemble into image
//! 4. Encode to DDS
//! 5. Write to cache
//! 6. Return result

use crate::coord::TileCoord;
use crate::fuse::EXPECTED_DDS_SIZE;
use crate::pipeline::http_limiter::HttpConcurrencyLimiter;
use crate::pipeline::stages::{
    assembly_stage, cache_stage, check_memory_cache, download_stage_cancellable,
    download_stage_with_limiter, encode_stage,
};
use crate::pipeline::{
    BlockingExecutor, ChunkProvider, DiskCache, Job, JobError, JobId, JobResult, MemoryCache,
    PipelineConfig, PipelineContext, TextureEncoderAsync,
};
use crate::telemetry::PipelineMetrics;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, warn};

/// Processes a job through all pipeline stages.
///
/// This is the main entry point for job processing. It orchestrates all stages
/// and produces a `JobResult` that can be sent back to the FUSE handler.
///
/// # Flow
///
/// ```text
/// Job → Cache Check → [hit] → Return cached data
///                   → [miss] → Download → Assemble → Encode → Cache → Return
/// ```
///
/// # Error Handling
///
/// The processor follows an optimistic strategy:
/// - Individual chunk failures result in magenta placeholders, not job failure
/// - Only catastrophic failures (e.g., all chunks fail, encoding crashes) fail the job
/// - A job always produces _some_ result (possibly all magenta)
#[instrument(skip(ctx, metrics, http_limiter), fields(job_id = %job.id, tile = ?job.tile_coords))]
pub async fn process_job<P, E, M, D, X>(
    job: Job,
    ctx: &PipelineContext<P, E, M, D, X>,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
) -> Result<JobResult, JobError>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    let start = Instant::now();
    let job_id = job.id;
    let tile = job.tile_coords;

    // Stage 0: Check memory cache
    if let Some(cached_data) = check_memory_cache(tile, ctx.memory_cache.as_ref(), metrics.as_ref())
    {
        debug!(job_id = %job_id, "Memory cache hit");
        return Ok(JobResult::cache_hit(job_id, cached_data, start.elapsed()));
    }

    // Stage 1: Download chunks (with optional HTTP concurrency limiting)
    let chunks = download_stage_with_limiter(
        job_id,
        tile,
        Arc::clone(&ctx.provider),
        Arc::clone(&ctx.disk_cache),
        &ctx.config,
        metrics.clone(),
        http_limiter,
    )
    .await;

    let failed_chunks = chunks.failure_count() as u16;

    // Stage 2: Assemble image
    let image = assembly_stage(job_id, chunks, ctx.executor.as_ref()).await?;

    // Stage 3: Encode to DDS
    let dds_data = encode_stage(
        job_id,
        image,
        Arc::clone(&ctx.encoder),
        ctx.executor.as_ref(),
        metrics,
    )
    .await?;

    // Validate encoded DDS size - log warning if unexpected
    // This helps trace the source of corrupted DDS data
    if dds_data.len() != EXPECTED_DDS_SIZE {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            actual_size = dds_data.len(),
            expected_size = EXPECTED_DDS_SIZE,
            failed_chunks,
            "Encoded DDS has unexpected size - may cause validation to substitute placeholder"
        );
    }

    // Validate DDS magic bytes
    if dds_data.len() < 4 || &dds_data[0..4] != b"DDS " {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            size = dds_data.len(),
            "Encoded DDS has invalid magic bytes - will be replaced with placeholder"
        );
    }

    // Stage 4: Write to memory cache (only if no failed chunks)
    // We don't cache tiles with magenta placeholders - they would return
    // corrupted imagery on subsequent requests
    if failed_chunks == 0 {
        cache_stage(job_id, tile, &dds_data, Arc::clone(&ctx.memory_cache)).await;
    } else {
        debug!(
            job_id = %job_id,
            failed_chunks,
            "Skipping memory cache write due to failed chunks"
        );
    }

    let duration = start.elapsed();
    debug!(
        job_id = %job_id,
        duration_ms = duration.as_millis(),
        failed_chunks,
        size_bytes = dds_data.len(),
        "Job complete"
    );

    Ok(JobResult::partial(
        job_id,
        dds_data,
        duration,
        failed_chunks,
    ))
}

/// Simplified processor that only needs the essential components.
///
/// This is a convenience function for cases where you have the components
/// but not a full `PipelineContext`.
///
/// # Arguments
///
/// * `http_limiter` - Optional global HTTP concurrency limiter. When provided,
///   limits the total concurrent HTTP requests across all tiles being processed.
#[allow(clippy::too_many_arguments)]
pub async fn process_tile<P, E, M, D, X>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: &X,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
) -> Result<JobResult, JobError>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    let start = Instant::now();

    // Stage 0: Check memory cache
    if let Some(cached_data) = check_memory_cache(tile, memory_cache.as_ref(), metrics.as_ref()) {
        debug!(job_id = %job_id, "Memory cache hit");
        return Ok(JobResult::cache_hit(job_id, cached_data, start.elapsed()));
    }

    // Stage 1: Download chunks (with optional HTTP concurrency limiting)
    let chunks = download_stage_with_limiter(
        job_id,
        tile,
        Arc::clone(&provider),
        Arc::clone(&disk_cache),
        config,
        metrics.clone(),
        http_limiter,
    )
    .await;

    let failed_chunks = chunks.failure_count() as u16;

    // Stage 2: Assemble image
    let image = assembly_stage(job_id, chunks, executor).await?;

    // Stage 3: Encode to DDS
    let dds_data = encode_stage(job_id, image, encoder, executor, metrics).await?;

    // Validate encoded DDS size - log warning if unexpected
    if dds_data.len() != EXPECTED_DDS_SIZE {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            actual_size = dds_data.len(),
            expected_size = EXPECTED_DDS_SIZE,
            failed_chunks,
            "Encoded DDS has unexpected size - may cause validation to substitute placeholder"
        );
    }

    // Validate DDS magic bytes
    if dds_data.len() < 4 || &dds_data[0..4] != b"DDS " {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            size = dds_data.len(),
            "Encoded DDS has invalid magic bytes - will be replaced with placeholder"
        );
    }

    // Stage 4: Write to memory cache (only if no failed chunks)
    // We don't cache tiles with magenta placeholders - they would return
    // corrupted imagery on subsequent requests
    if failed_chunks == 0 {
        cache_stage(job_id, tile, &dds_data, memory_cache).await;
    } else {
        debug!(
            job_id = %job_id,
            failed_chunks,
            "Skipping memory cache write due to failed chunks"
        );
    }

    let duration = start.elapsed();

    Ok(JobResult::partial(
        job_id,
        dds_data,
        duration,
        failed_chunks,
    ))
}

/// Processor with cancellation support for FUSE timeout handling.
///
/// This version accepts a `CancellationToken` that can be used to abort
/// processing when the FUSE layer times out waiting for a response.
/// This prevents orphaned tasks from consuming resources indefinitely.
///
/// # Arguments
///
/// * `http_limiter` - Optional global HTTP concurrency limiter
/// * `cancellation_token` - Token to signal cancellation (e.g., from FUSE timeout)
#[allow(clippy::too_many_arguments)]
pub async fn process_tile_cancellable<P, E, M, D, X>(
    job_id: JobId,
    tile: TileCoord,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: &X,
    config: &PipelineConfig,
    metrics: Option<Arc<PipelineMetrics>>,
    http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
    cancellation_token: CancellationToken,
) -> Result<JobResult, JobError>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    let start = Instant::now();

    // Check for early cancellation
    if cancellation_token.is_cancelled() {
        return Err(JobError::Cancelled);
    }

    // Stage 0: Check memory cache
    if let Some(cached_data) = check_memory_cache(tile, memory_cache.as_ref(), metrics.as_ref()) {
        debug!(job_id = %job_id, "Memory cache hit");
        return Ok(JobResult::cache_hit(job_id, cached_data, start.elapsed()));
    }

    // Stage 1: Download chunks (with cancellation support)
    let chunks = download_stage_cancellable(
        job_id,
        tile,
        Arc::clone(&provider),
        Arc::clone(&disk_cache),
        config,
        metrics.clone(),
        http_limiter,
        cancellation_token.clone(),
    )
    .await;

    // Check cancellation after download stage (most time-consuming)
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, "Cancelled after download stage");
        return Err(JobError::Cancelled);
    }

    let failed_chunks = chunks.failure_count() as u16;

    // Stage 2: Assemble image
    let image = assembly_stage(job_id, chunks, executor).await?;

    // Check cancellation after assembly
    if cancellation_token.is_cancelled() {
        debug!(job_id = %job_id, "Cancelled after assembly stage");
        return Err(JobError::Cancelled);
    }

    // Stage 3: Encode to DDS
    let dds_data = encode_stage(job_id, image, encoder, executor, metrics).await?;

    // Validate encoded DDS size - log warning if unexpected
    if dds_data.len() != EXPECTED_DDS_SIZE {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            actual_size = dds_data.len(),
            expected_size = EXPECTED_DDS_SIZE,
            failed_chunks,
            "Encoded DDS has unexpected size - may cause validation to substitute placeholder"
        );
    }

    // Validate DDS magic bytes
    if dds_data.len() < 4 || &dds_data[0..4] != b"DDS " {
        warn!(
            job_id = %job_id,
            tile = ?tile,
            size = dds_data.len(),
            "Encoded DDS has invalid magic bytes - will be replaced with placeholder"
        );
    }

    // Stage 4: Write to memory cache (only if no failed chunks)
    if failed_chunks == 0 {
        cache_stage(job_id, tile, &dds_data, memory_cache).await;
    } else {
        debug!(
            job_id = %job_id,
            failed_chunks,
            "Skipping memory cache write due to failed chunks"
        );
    }

    let duration = start.elapsed();

    Ok(JobResult::partial(
        job_id,
        dds_data,
        duration,
        failed_chunks,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{ChunkDownloadError, TextureEncodeError, TokioExecutor};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Mock implementations for testing

    struct MockProvider;

    impl ChunkProvider for MockProvider {
        async fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            // Return a simple 1x1 PNG (smallest valid image)
            // In reality this would be a 256x256 JPEG
            Ok(create_test_png())
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    struct FailingProvider;

    impl ChunkProvider for FailingProvider {
        async fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            Err(ChunkDownloadError::permanent("always fails"))
        }

        fn name(&self) -> &str {
            "failing"
        }
    }

    struct MockEncoder;

    impl TextureEncoderAsync for MockEncoder {
        fn encode(&self, _image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            // Return mock DDS data
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

        fn with_cached(self, row: u32, col: u32, zoom: u8, data: Vec<u8>) -> Self {
            self.data.lock().unwrap().insert((row, col, zoom), data);
            self
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
        // Create a minimal valid PNG (1x1 red pixel)
        use image::{Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(256, 256, Rgba([255, 0, 0, 255]));
        let mut buffer = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buffer
    }

    #[tokio::test]
    async fn test_process_tile_success() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = TokioExecutor::new();
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let result = process_tile(
            JobId::new(),
            tile,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            &executor,
            &config,
            None,
            None, // No HTTP limiter for test
        )
        .await;

        assert!(result.is_ok());
        let job_result = result.unwrap();
        assert!(!job_result.cache_hit);
        assert_eq!(job_result.failed_chunks, 0);
    }

    #[tokio::test]
    async fn test_process_tile_cache_hit() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let cached_data = vec![0xCA, 0xCE, 0x0D, 0xDA, 0xAA];
        let memory_cache =
            Arc::new(MockMemoryCache::new().with_cached(100, 200, 16, cached_data.clone()));
        let disk_cache = Arc::new(NullDiskCache);
        let executor = TokioExecutor::new();
        let config = PipelineConfig::default();

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let result = process_tile(
            JobId::new(),
            tile,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            &executor,
            &config,
            None,
            None, // No HTTP limiter for test
        )
        .await;

        assert!(result.is_ok());
        let job_result = result.unwrap();
        assert!(job_result.cache_hit);
        assert_eq!(job_result.dds_data, cached_data);
    }

    #[tokio::test]
    async fn test_process_tile_with_failures() {
        let provider = Arc::new(FailingProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = TokioExecutor::new();
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let result = process_tile(
            JobId::new(),
            tile,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            &executor,
            &config,
            None,
            None, // No HTTP limiter for test
        )
        .await;

        // Should still succeed (with magenta placeholders)
        assert!(result.is_ok());
        let job_result = result.unwrap();
        assert_eq!(job_result.failed_chunks, 256); // All chunks failed
    }

    #[tokio::test]
    async fn test_process_tile_writes_to_cache() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = TokioExecutor::new();
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        // Process the tile
        let _ = process_tile(
            JobId::new(),
            tile,
            Arc::clone(&provider),
            Arc::clone(&encoder),
            Arc::clone(&memory_cache),
            Arc::clone(&disk_cache),
            &executor,
            &config,
            None,
            None, // No HTTP limiter for test
        )
        .await;

        // Verify it's now in cache
        let cached = memory_cache.get(100, 200, 16);
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn test_process_tile_with_failures_does_not_cache() {
        // Use FailingProvider which causes all chunks to fail
        let provider = Arc::new(FailingProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = TokioExecutor::new();
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        // Process the tile (will have failed chunks)
        let result = process_tile(
            JobId::new(),
            tile,
            Arc::clone(&provider),
            Arc::clone(&encoder),
            Arc::clone(&memory_cache),
            Arc::clone(&disk_cache),
            &executor,
            &config,
            None,
            None, // No HTTP limiter for test
        )
        .await;

        // Job should succeed (with magenta placeholders)
        assert!(result.is_ok());
        let job_result = result.unwrap();
        assert_eq!(job_result.failed_chunks, 256);

        // But tile should NOT be in cache (we don't cache tiles with failures)
        let cached = memory_cache.get(100, 200, 16);
        assert!(
            cached.is_none(),
            "Tiles with failed chunks should not be cached"
        );
    }
}
