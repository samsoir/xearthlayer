//! Combined build and cache DDS task implementation.
//!
//! [`BuildAndCacheDdsTask`] combines the assembly, encoding, and caching stages
//! into a single sequential task for optimal performance.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::CPU` since both image assembly and DDS encoding
//! are CPU-bound operations.
//!
//! # Input
//!
//! Reads `TaskOutput` key "chunks" containing `ChunkResults` from DownloadChunksTask.
//!
//! # Output
//!
//! Returns the DDS data via `TaskOutput` key "dds_data" (`Vec<u8>`).
//! Cache write is fire-and-forget (spawned async task).

use super::download_chunks::OUTPUT_KEY_CHUNKS;
use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkResults, DdsDiskCache, MemoryCache, ResourceType, Task, TaskContext,
    TaskError, TaskOutput, TaskResult, TextureEncoderAsync,
};
use crate::metrics::{EncodeGuard, MetricsClient, OptionalMetrics};
use bytes::Bytes;
use image::{Rgba, RgbaImage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;
use tracing::{debug, warn};

/// Tile dimensions
const TILE_SIZE: u32 = 4096;
const CHUNK_SIZE: u32 = 256;
const CHUNKS_PER_SIDE: u32 = 16;

/// Magenta color for failed chunks (R=255, G=0, B=255, A=255)
const MAGENTA: Rgba<u8> = Rgba([255, 0, 255, 255]);

/// Task that builds a DDS texture from downloaded chunks and caches it.
///
/// This task combines three sequential operations:
/// 1. Assemble chunks into a 4096×4096 RGBA image
/// 2. Encode the image to DDS format with BC1 compression
/// 3. Spawn fire-and-forget cache write (non-blocking)
///
/// # Cancellation
///
/// A job cancelled while its encode runs (a FUSE timeout or its error
/// policy) still caches the encoded tile, then returns `Cancelled`: the work
/// is paid for, and the next request for the tile becomes a cache hit. A
/// cancel by executor shutdown caches nothing. An incomplete tile is never
/// cached, cancelled or not (#180, #385).
///
/// # Inputs
///
/// - Key: "chunks" (from DownloadChunksTask)
/// - Type: `ChunkResults`
///
/// # Outputs
///
/// - Key: "dds_data"
/// - Type: `Vec<u8>` - The encoded DDS data
///
/// The DDS data is returned directly to avoid cache read race conditions
/// with eventual consistency caches like moka.
pub struct BuildAndCacheDdsTask<E, M, DD, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    X: BlockingExecutor,
{
    /// Tile coordinates
    tile: TileCoord,

    /// Texture encoder for DDS compression
    encoder: Arc<E>,

    /// Memory cache for storing completed DDS tiles
    memory_cache: Arc<M>,

    /// DDS disk cache for persistent tile storage
    dds_disk_cache: Arc<DD>,

    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<E, M, DD, X> BuildAndCacheDdsTask<E, M, DD, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    X: BlockingExecutor,
{
    /// Creates a new build and cache DDS task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates
    /// * `encoder` - Texture encoder for DDS compression
    /// * `memory_cache` - Memory cache to write completed tiles
    /// * `dds_disk_cache` - DDS disk cache for persistent storage
    /// * `executor` - Executor for blocking operations (spawn_blocking)
    pub fn new(
        tile: TileCoord,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        executor: Arc<X>,
    ) -> Self {
        Self {
            tile,
            encoder,
            memory_cache,
            dds_disk_cache,
            executor,
        }
    }

    /// Spawns the fire-and-forget memory and DDS disk cache writes of an
    /// encoded tile. Each spawn clones a refcount of `dds_data`, not the
    /// tile (#237).
    fn spawn_cache_writes(&self, dds_data: &Bytes, metrics: &Option<MetricsClient>) {
        // Memory cache write. Spawned so the job's completion does not wait
        // on it, and to avoid race conditions with eventual consistency
        // caches.
        //
        // Instrumented with mem_cache_write_started/completed (mirroring
        // the DDS-disk spawn below) so this backlog is visible as
        // mem_cache_writes_active — previously this spawn emitted no
        // start/complete pair at all, only a size gauge that only moves
        // after `cache.put()` completes. See issue #209.
        let cache = Arc::clone(&self.memory_cache);
        let tile = self.tile;
        let dds_data_for_cache = dds_data.clone();
        let metrics_for_mem = metrics.clone();
        tokio::spawn(async move {
            metrics_for_mem.mem_cache_write_started();

            cache
                .put(tile.row, tile.col, tile.zoom, dds_data_for_cache)
                .await;

            metrics_for_mem.mem_cache_write_completed();

            debug!(
                tile = ?tile,
                "Memory cache write complete (async)"
            );
        });

        // DDS disk cache write. Persists the encoded DDS tile to disk so it
        // can be served without re-encoding after memory eviction.
        let dds_disk = Arc::clone(&self.dds_disk_cache);
        let tile_for_disk = self.tile;
        let dds_data_for_disk = dds_data.clone();
        let dds_bytes = dds_data_for_disk.len() as u64;
        let metrics_for_dds = metrics.clone();
        tokio::spawn(async move {
            metrics_for_dds.disk_write_started();

            dds_disk
                .put(
                    tile_for_disk.row,
                    tile_for_disk.col,
                    tile_for_disk.zoom,
                    dds_data_for_disk,
                )
                .await;

            metrics_for_dds.disk_write_completed(dds_bytes, DD::TIER);

            debug!(
                tile = ?tile_for_disk,
                "DDS disk cache write complete (async)"
            );
        });
    }
}

impl<E, M, DD, X> Task for BuildAndCacheDdsTask<E, M, DD, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    X: BlockingExecutor,
{
    fn name(&self) -> &str {
        "BuildAndCacheDds"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        let span = tracing::debug_span!(
            target: "profiling",
            "build_and_cache_dds",
            tile_row = self.tile.row,
            tile_col = self.tile.col,
            tile_zoom = self.tile.zoom,
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

                // Step 1: Get chunks from previous task (returns owned clone)
                let chunks: ChunkResults = match ctx.get_output("DownloadChunks", OUTPUT_KEY_CHUNKS)
                {
                    Some(c) => c,
                    None => {
                        return TaskResult::Failed(TaskError::missing_input("chunks"));
                    }
                };

                let success_count = chunks.success_count();
                let failure_count = chunks.failure_count();
                // Capture completeness before chunks are moved into assembly.
                // Cache writes (memory + DDS disk) are gated on this further down;
                // see issue #180.
                let tile_complete = chunks.is_complete();

                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    success_count = success_count,
                    failure_count = failure_count,
                    "Starting image assembly"
                );

                // Step 2: Assemble image (CPU-bound)
                let assembly_start = Instant::now();

                let image_result = self
                    .executor
                    .execute_blocking(move || assemble_chunks(chunks))
                    .await;

                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let image = match image_result {
                    Ok(Ok(img)) => {
                        let duration_us = assembly_start.elapsed().as_micros() as u64;
                        metrics.assembly_completed(duration_us);
                        img
                    }
                    Ok(Err(e)) => {
                        return TaskResult::Failed(TaskError::new(format!(
                            "Assembly failed: {}",
                            e
                        )));
                    }
                    Err(e) => {
                        return TaskResult::Failed(TaskError::new(format!(
                            "Assembly task failed: {}",
                            e
                        )));
                    }
                };

                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    width = image.width(),
                    height = image.height(),
                    "Image assembly complete, starting DDS encoding"
                );

                // Step 3: Encode to DDS (CPU-bound). The guard ends the encode
                // in `encodes_active` on every way out, and only a completed
                // encode counts as one (#385).
                let encode = EncodeGuard::start(&metrics);
                let encode_start = Instant::now();

                let encoder = Arc::clone(&self.encoder);
                let encode_result = self
                    .executor
                    .execute_blocking(move || encoder.encode(image))
                    .await;

                let dds_data = match encode_result {
                    Ok(Ok(data)) => {
                        let duration_us = encode_start.elapsed().as_micros() as u64;
                        encode.completed(data.len() as u64, duration_us);
                        // Bytes::from adopts the encoder's allocation (O(1)),
                        // so the two fire-and-forget cache spawns below clone a
                        // refcount rather than 10.66 MiB each (#237).
                        Bytes::from(data)
                    }
                    Ok(Err(_)) | Err(_) if ctx.is_cancelled() => {
                        return TaskResult::Cancelled;
                    }
                    Ok(Err(e)) => {
                        return TaskResult::Failed(TaskError::new(format!(
                            "Encoding failed: {}",
                            e.message
                        )));
                    }
                    Err(e) => {
                        return TaskResult::Failed(TaskError::new(format!(
                            "Encode task failed: {}",
                            e
                        )));
                    }
                };

                let dds_size = dds_data.len();

                // The job may have been cancelled while the encode ran: by a
                // FUSE timeout, its error policy, or the executor shutting
                // down. The tile is paid for, so it is cached before the
                // cancel is honoured, and the next request for it is a cache
                // hit instead of a second download and encode. Not when the
                // cancel is the executor's shutdown (#385). The job's cancel
                // is read before the shutdown (see
                // `TaskContext::is_shutting_down`).
                let cancelled = ctx.is_cancelled();

                // Cache writes are gated on tile completeness (captured above
                // before `chunks` was moved into assembly). A tile assembled
                // with any failed chunks contains magenta-filled regions that
                // would be structurally indistinguishable from a successful tile
                // once persisted, and would never be re-fetched. We still serve
                // the tile to X-Plane (so the sim doesn't stall), but never
                // remember it, cancelled or not. See issue #180.
                if tile_complete {
                    if cancelled && ctx.is_shutting_down() {
                        debug!(
                            job_id = %job_id,
                            tile = ?self.tile,
                            "Executor shutting down; encoded tile not cached"
                        );
                    } else {
                        debug!(
                            job_id = %job_id,
                            tile = ?self.tile,
                            size_bytes = dds_size,
                            cancelled = cancelled,
                            "DDS encoding complete, spawning cache write"
                        );
                        self.spawn_cache_writes(&dds_data, &metrics);
                    }
                } else if !cancelled {
                    warn!(
                        job_id = %job_id,
                        tile = ?self.tile,
                        success_count = success_count,
                        failure_count = failure_count,
                        size_bytes = dds_size,
                        "Tile incomplete; serving DDS to X-Plane but skipping cache writes"
                    );
                }

                if cancelled {
                    return TaskResult::Cancelled;
                }

                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    size_bytes = dds_size,
                    "DDS tile complete"
                );

                // Return DDS data directly to avoid cache read race conditions
                let mut output = TaskOutput::new();
                output.set("dds_data", dds_data);
                TaskResult::SuccessWithOutput(output)
            }
            .instrument(span),
        )
    }
}

// ============================================================================
// Chunk assembly
// ============================================================================

/// Synchronous chunk assembly (runs in spawn_blocking).
fn assemble_chunks(chunks: ChunkResults) -> Result<RgbaImage, String> {
    let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);

    // Process each chunk position
    for row in 0..CHUNKS_PER_SIDE as u8 {
        for col in 0..CHUNKS_PER_SIDE as u8 {
            let x_offset = col as u32 * CHUNK_SIZE;
            let y_offset = row as u32 * CHUNK_SIZE;

            if let Some(jpeg_data) = chunks.get(row, col) {
                // Decode and place the chunk
                match decode_chunk(jpeg_data) {
                    Ok(chunk_image) => {
                        place_chunk(&mut canvas, &chunk_image, x_offset, y_offset);
                    }
                    Err(e) => {
                        // Decode failed - use magenta placeholder
                        warn!(
                            chunk_row = row,
                            chunk_col = col,
                            error = %e,
                            "Failed to decode chunk, using magenta placeholder"
                        );
                        fill_magenta(&mut canvas, x_offset, y_offset);
                    }
                }
            } else {
                // Download failed - use magenta placeholder
                fill_magenta(&mut canvas, x_offset, y_offset);
            }
        }
    }

    Ok(canvas)
}

/// Decodes JPEG data into an RGBA image.
fn decode_chunk(jpeg_data: &[u8]) -> Result<RgbaImage, String> {
    let img =
        image::load_from_memory(jpeg_data).map_err(|e| format!("image decode error: {}", e))?;
    Ok(img.to_rgba8())
}

/// Places a chunk image onto the canvas at the specified offset.
fn place_chunk(canvas: &mut RgbaImage, chunk: &RgbaImage, x_offset: u32, y_offset: u32) {
    // Handle chunks that might not be exactly 256x256
    let chunk_width = chunk.width().min(CHUNK_SIZE);
    let chunk_height = chunk.height().min(CHUNK_SIZE);

    for y in 0..chunk_height {
        for x in 0..chunk_width {
            let pixel = chunk.get_pixel(x, y);
            canvas.put_pixel(x_offset + x, y_offset + y, *pixel);
        }
    }
}

/// Fills a chunk region with magenta.
fn fill_magenta(canvas: &mut RgbaImage, x_offset: u32, y_offset: u32) {
    for y in 0..CHUNK_SIZE {
        for x in 0..CHUNK_SIZE {
            canvas.put_pixel(x_offset + x, y_offset + y, MAGENTA);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutorError, JobId, TextureEncodeError};
    use std::time::Duration;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_task_name() {
        // Verify the task name constant
        assert_eq!("BuildAndCacheDds", "BuildAndCacheDds");
    }

    #[test]
    fn test_assemble_chunks_empty() {
        let chunks = ChunkResults::new();
        let result = assemble_chunks(chunks).unwrap();

        assert_eq!(result.width(), TILE_SIZE);
        assert_eq!(result.height(), TILE_SIZE);

        // All pixels should be magenta
        let pixel = result.get_pixel(0, 0);
        assert_eq!(*pixel, MAGENTA);
    }

    #[test]
    fn test_fill_magenta() {
        let mut canvas = RgbaImage::new(512, 512);

        fill_magenta(&mut canvas, 0, 0);

        // Check corners of the filled region
        assert_eq!(*canvas.get_pixel(0, 0), MAGENTA);
        assert_eq!(*canvas.get_pixel(255, 0), MAGENTA);
        assert_eq!(*canvas.get_pixel(0, 255), MAGENTA);
        assert_eq!(*canvas.get_pixel(256, 0), Rgba([0, 0, 0, 0]));
    }

    // ------------------------------------------------------------------
    // Cache-poisoning regression tests (#180)
    // ------------------------------------------------------------------

    /// 128 bytes is enough to satisfy "non-empty" checks; the magic header
    /// isn't validated at this layer.
    fn stub_dds() -> Vec<u8> {
        vec![b'D', b'D', b'S', b' ']
            .into_iter()
            .chain(vec![0u8; 124])
            .collect()
    }

    struct StubEncoder;
    impl TextureEncoderAsync for StubEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            Ok(stub_dds())
        }
        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            128
        }
    }

    /// Encodes successfully, cancelling the tokens it holds while it runs:
    /// the job's alone for a FUSE timeout, the executor's first and then the
    /// job's for a shutdown (the order `JobExecutor::shutdown` cancels them).
    struct CancellingEncoder {
        executor_shutdown: Option<CancellationToken>,
        job: CancellationToken,
    }
    impl TextureEncoderAsync for CancellingEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            if let Some(shutdown) = &self.executor_shutdown {
                shutdown.cancel();
            }
            self.job.cancel();
            Ok(stub_dds())
        }
        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            128
        }
    }

    struct FailingEncoder;
    impl TextureEncoderAsync for FailingEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            Err(TextureEncodeError::new("encoder refused the image"))
        }
        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            128
        }
    }

    /// Panics inside the blocking closure; `TokioExecutor` turns that into
    /// `ExecutorError::TaskPanicked`, as in production.
    struct PanickingEncoder;
    impl TextureEncoderAsync for PanickingEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            panic!("encoder panicked (test)");
        }
        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            128
        }
    }

    struct InlineBlockingExecutor;
    impl BlockingExecutor for InlineBlockingExecutor {
        fn execute_blocking<F, R>(
            &self,
            f: F,
        ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static,
        {
            Box::pin(async move { Ok(f()) })
        }
    }

    struct CountingMemoryCache {
        tx: UnboundedSender<(u32, u32, u8)>,
    }
    impl MemoryCache for CountingMemoryCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Option<Bytes>> + Send {
            async { None }
        }
        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            _data: Bytes,
        ) -> impl Future<Output = ()> + Send {
            let tx = self.tx.clone();
            async move {
                let _ = tx.send((row, col, zoom));
            }
        }
        fn size_bytes(&self) -> usize {
            0
        }
        fn entry_count(&self) -> usize {
            0
        }
    }

    struct CountingDdsDiskCache {
        tx: UnboundedSender<(u32, u32, u8)>,
    }
    impl DdsDiskCache for CountingDdsDiskCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Option<Bytes>> + Send {
            async { None }
        }
        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            _data: Bytes,
        ) -> impl Future<Output = ()> + Send {
            let tx = self.tx.clone();
            async move {
                let _ = tx.send((row, col, zoom));
            }
        }
        fn contains(&self, _row: u32, _col: u32, _zoom: u8) -> impl Future<Output = bool> + Send {
            async { false }
        }
    }

    type TestTask<E, X> = BuildAndCacheDdsTask<E, CountingMemoryCache, CountingDdsDiskCache, X>;

    fn make_task_with_chunks(
        chunks: ChunkResults,
    ) -> (
        TestTask<StubEncoder, InlineBlockingExecutor>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u32, u32, u8)>,
        TaskContext,
    ) {
        make_task(
            chunks,
            StubEncoder,
            InlineBlockingExecutor,
            CancellationToken::new(),
        )
    }

    /// Builds the task with `encoder` and `executor`, and a context on the
    /// job token `job` holding `chunks` as DownloadChunks' output.
    fn make_task<E: TextureEncoderAsync, X: BlockingExecutor>(
        chunks: ChunkResults,
        encoder: E,
        executor: X,
        job: CancellationToken,
    ) -> (
        TestTask<E, X>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u32, u32, u8)>,
        TaskContext,
    ) {
        let (mem_tx, mem_rx) = unbounded_channel();
        let (disk_tx, disk_rx) = unbounded_channel();

        let task = BuildAndCacheDdsTask::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
            Arc::new(encoder),
            Arc::new(CountingMemoryCache { tx: mem_tx }),
            Arc::new(CountingDdsDiskCache { tx: disk_tx }),
            Arc::new(executor),
        );

        let ctx = TaskContext::new(JobId::new("test-job"), job);
        let mut output = TaskOutput::new();
        output.set("chunks", chunks);
        ctx.add_task_output("DownloadChunks".to_string(), output);

        (task, mem_rx, disk_rx, ctx)
    }

    fn complete_chunks() -> ChunkResults {
        // Data content is irrelevant to the gate decision, which keys off
        // ChunkResults::is_complete().
        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                chunks.add_success(row, col, vec![].into());
            }
        }
        assert!(chunks.is_complete());
        chunks
    }

    fn incomplete_chunks() -> ChunkResults {
        let mut chunks = ChunkResults::new();
        chunks.add_failure(0, 0, 3, "network".to_string());
        assert!(!chunks.is_complete());
        chunks
    }

    /// Drains the channel up to the timeout. Returns Vec of any items received.
    /// Used to assert "no put fires within a reasonable wait window" without
    /// requiring perfect synchronization with the spawned writes.
    async fn drain_within(
        rx: &mut UnboundedReceiver<(u32, u32, u8)>,
        wait: Duration,
    ) -> Vec<(u32, u32, u8)> {
        let mut received = Vec::new();
        loop {
            match tokio::time::timeout(wait, rx.recv()).await {
                Ok(Some(item)) => received.push(item),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        received
    }

    #[tokio::test]
    async fn cached_tile_probe_hit_skips_the_build() {
        // No DownloadChunks output is seeded: reaching the build would fail
        // with missing_input, so Success proves the short-circuit.
        let (task, _mem_rx, _disk_rx, mut ctx) = make_task_with_chunks(ChunkResults::new());
        let mut probe = TaskOutput::new();
        probe.set(crate::tasks::TILE_CACHED_KEY, true);
        ctx.add_task_output(crate::tasks::CACHED_TILE_PROBE_TASK.to_string(), probe);

        assert!(matches!(task.execute(&mut ctx).await, TaskResult::Success));
    }

    #[tokio::test]
    async fn incomplete_tile_does_not_write_to_caches() {
        // Construct chunks with one failure - tile is incomplete.
        let mut chunks = ChunkResults::new();
        chunks.add_failure(0, 0, 3, "network".to_string());
        assert!(!chunks.is_complete());

        let (task, mut mem_rx, mut disk_rx, mut ctx) = make_task_with_chunks(chunks);

        let result = task.execute(&mut ctx).await;

        // X-Plane still gets a tile.
        match result {
            TaskResult::SuccessWithOutput(out) => {
                let dds: Option<Bytes> = out.get::<Bytes>("dds_data").cloned();
                assert!(
                    dds.is_some(),
                    "task must return DDS data even when incomplete"
                );
                assert!(!dds.unwrap().is_empty(), "DDS data must not be empty");
            }
            other => panic!("expected SuccessWithOutput, got {other:?}"),
        }

        // Neither cache must receive a put. 100ms is well over what the
        // spawn would need to fire if it were going to.
        let mem_puts = drain_within(&mut mem_rx, Duration::from_millis(100)).await;
        let disk_puts = drain_within(&mut disk_rx, Duration::from_millis(50)).await;
        assert!(
            mem_puts.is_empty(),
            "memory cache must not receive a put for incomplete tile, got {mem_puts:?}"
        );
        assert!(
            disk_puts.is_empty(),
            "DDS disk cache must not receive a put for incomplete tile, got {disk_puts:?}"
        );
    }

    #[tokio::test]
    async fn complete_tile_writes_to_both_caches() {
        // 256 successful chunks - tile is complete. Data content is irrelevant
        // to the gate decision, which keys off ChunkResults::is_complete().
        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                chunks.add_success(row, col, vec![].into());
            }
        }
        assert!(chunks.is_complete());

        let (task, mut mem_rx, mut disk_rx, mut ctx) = make_task_with_chunks(chunks);

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::SuccessWithOutput(_)));

        let mem_put = tokio::time::timeout(Duration::from_secs(1), mem_rx.recv())
            .await
            .expect("memory cache put must fire within 1s for complete tile")
            .expect("memory cache channel closed unexpectedly");
        assert_eq!(mem_put, (100, 200, 15));

        let disk_put = tokio::time::timeout(Duration::from_secs(1), disk_rx.recv())
            .await
            .expect("DDS disk cache put must fire within 1s for complete tile")
            .expect("DDS disk cache channel closed unexpectedly");
        assert_eq!(disk_put, (100, 200, 15));
    }

    // ------------------------------------------------------------------
    // mem_cache_writes_active instrumentation (#209 fix wave)
    //
    // Regression guard for the diagnosis hole: before this fix, the
    // memory-cache spawn emitted no start/complete metrics pair at all, so a
    // backlog concentrated there was invisible to `mem_cache_writes_active`
    // and would misread as allocator retention. This asserts the production
    // spawn actually emits the pair, not just that the daemon can count it.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn complete_tile_reports_mem_cache_write_started_and_completed() {
        use crate::metrics::MetricEvent;
        use crate::metrics::MetricsClient;

        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                chunks.add_success(row, col, vec![].into());
            }
        }
        assert!(chunks.is_complete());

        let (task, _mem_rx, _disk_rx, ctx) = make_task_with_chunks(chunks);

        let (metrics_tx, mut metrics_rx) = unbounded_channel();
        let mut ctx = ctx.with_metrics(MetricsClient::new(metrics_tx));

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::SuccessWithOutput(_)));

        // The memory-cache spawn and the DDS-disk spawn run concurrently and
        // share this channel, so interleave with other event types (encode,
        // disk write, ...) is expected. Filter to just the mem-cache pair;
        // sends from a single spawned task preserve their own call order.
        let mut mem_write_events = Vec::new();
        let deadline = Duration::from_secs(2);
        while mem_write_events.len() < 2 {
            match tokio::time::timeout(deadline, metrics_rx.recv()).await {
                Ok(Some(MetricEvent::MemCacheWriteStarted)) => {
                    mem_write_events.push("started");
                }
                Ok(Some(MetricEvent::MemCacheWriteCompleted)) => {
                    mem_write_events.push("completed");
                }
                Ok(Some(_)) => {} // other events (encode/assembly/disk) are irrelevant here
                Ok(None) | Err(_) => break,
            }
        }

        assert_eq!(
            mem_write_events,
            vec!["started", "completed"],
            "memory cache spawn must emit MemCacheWriteStarted then MemCacheWriteCompleted"
        );
    }

    // ------------------------------------------------------------------
    // Encode gauge and encodes finished under cancellation (#385)
    // ------------------------------------------------------------------

    /// Runs `task` with a metrics client and returns its result and the
    /// metric events sent by the time it returned. The encode events are
    /// sent from `execute` itself, so none can arrive later.
    async fn execute_with_metrics<E: TextureEncoderAsync, X: BlockingExecutor>(
        task: &TestTask<E, X>,
        ctx: TaskContext,
    ) -> (TaskResult, Vec<crate::metrics::MetricEvent>) {
        let (metrics_tx, mut metrics_rx) = unbounded_channel();
        let mut ctx = ctx.with_metrics(crate::metrics::MetricsClient::new(metrics_tx));
        let result = task.execute(&mut ctx).await;
        let mut events = Vec::new();
        while let Ok(event) = metrics_rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    /// `encodes_active` as the metrics daemon moves it for these events.
    fn encodes_active(events: &[crate::metrics::MetricEvent]) -> i64 {
        use crate::metrics::MetricEvent;
        events
            .iter()
            .map(|event| match event {
                MetricEvent::EncodeStarted => 1,
                MetricEvent::EncodeCompleted { .. } | MetricEvent::EncodeFailed => -1,
                _ => 0,
            })
            .sum()
    }

    /// The `EncodeCompleted` events, as `bytes`.
    fn encodes_completed(events: &[crate::metrics::MetricEvent]) -> Vec<u64> {
        use crate::metrics::MetricEvent;
        events
            .iter()
            .filter_map(|event| match event {
                MetricEvent::EncodeCompleted { bytes, .. } => Some(*bytes),
                _ => None,
            })
            .collect()
    }

    fn cancelled_mid_encode(
        chunks: ChunkResults,
    ) -> (
        TestTask<CancellingEncoder, InlineBlockingExecutor>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u32, u32, u8)>,
        TaskContext,
    ) {
        let job = CancellationToken::new();
        let encoder = CancellingEncoder {
            executor_shutdown: None,
            job: job.clone(),
        };
        make_task(chunks, encoder, InlineBlockingExecutor, job)
    }

    #[tokio::test]
    async fn successful_encode_counts_once_as_completed() {
        let (task, _mem_rx, _disk_rx, ctx) = make_task_with_chunks(complete_chunks());

        let (result, events) = execute_with_metrics(&task, ctx).await;

        assert!(matches!(result, TaskResult::SuccessWithOutput(_)));
        assert_eq!(encodes_active(&events), 0);
        assert_eq!(encodes_completed(&events), vec![stub_dds().len() as u64]);
    }

    #[tokio::test]
    async fn encode_cancelled_while_running_leaves_the_gauge_where_it_started() {
        let (task, _mem_rx, _disk_rx, ctx) = cancelled_mid_encode(complete_chunks());

        let (result, events) = execute_with_metrics(&task, ctx).await;

        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");
        assert_eq!(encodes_active(&events), 0, "events: {events:?}");
        // The encode itself finished: it is a completed encode.
        assert_eq!(encodes_completed(&events), vec![stub_dds().len() as u64]);
    }

    #[tokio::test]
    async fn failed_encode_leaves_the_gauge_where_it_started_and_is_not_completed() {
        let (task, _mem_rx, _disk_rx, ctx) = make_task(
            complete_chunks(),
            FailingEncoder,
            InlineBlockingExecutor,
            CancellationToken::new(),
        );

        let (result, events) = execute_with_metrics(&task, ctx).await;

        assert!(matches!(result, TaskResult::Failed(_)), "got {result:?}");
        assert_eq!(encodes_active(&events), 0, "events: {events:?}");
        assert!(encodes_completed(&events).is_empty(), "events: {events:?}");
    }

    #[tokio::test]
    async fn panicking_encode_leaves_the_gauge_where_it_started_and_is_not_completed() {
        let (task, _mem_rx, _disk_rx, ctx) = make_task(
            complete_chunks(),
            PanickingEncoder,
            crate::executor::TokioExecutor::new(),
            CancellationToken::new(),
        );

        let (result, events) = execute_with_metrics(&task, ctx).await;

        assert!(matches!(result, TaskResult::Failed(_)), "got {result:?}");
        assert_eq!(encodes_active(&events), 0, "events: {events:?}");
        assert!(encodes_completed(&events).is_empty(), "events: {events:?}");
    }

    #[tokio::test]
    async fn complete_tile_cancelled_mid_encode_reaches_both_caches() {
        let (task, mut mem_rx, mut disk_rx, mut ctx) = cancelled_mid_encode(complete_chunks());

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");

        let mem_put = tokio::time::timeout(Duration::from_secs(1), mem_rx.recv())
            .await
            .expect("memory cache put must fire for a complete tile encoded under cancellation")
            .expect("memory cache channel closed unexpectedly");
        assert_eq!(mem_put, (100, 200, 15));

        let disk_put = tokio::time::timeout(Duration::from_secs(1), disk_rx.recv())
            .await
            .expect("DDS disk cache put must fire for a complete tile encoded under cancellation")
            .expect("DDS disk cache channel closed unexpectedly");
        assert_eq!(disk_put, (100, 200, 15));
    }

    /// Like [`cancelled_mid_encode`], but the cancel comes from the
    /// executor shutting down.
    fn cancelled_by_shutdown_mid_encode(
        chunks: ChunkResults,
    ) -> (
        TestTask<CancellingEncoder, InlineBlockingExecutor>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u32, u32, u8)>,
        TaskContext,
    ) {
        let job = CancellationToken::new();
        let shutdown = CancellationToken::new();
        let encoder = CancellingEncoder {
            executor_shutdown: Some(shutdown.clone()),
            job: job.clone(),
        };
        let (task, mem_rx, disk_rx, ctx) = make_task(chunks, encoder, InlineBlockingExecutor, job);
        (task, mem_rx, disk_rx, ctx.with_executor_shutdown(shutdown))
    }

    #[tokio::test]
    async fn complete_tile_cancelled_by_shutdown_mid_encode_reaches_neither_cache() {
        let (task, mut mem_rx, mut disk_rx, mut ctx) =
            cancelled_by_shutdown_mid_encode(complete_chunks());

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");

        let mem_puts = drain_within(&mut mem_rx, Duration::from_millis(100)).await;
        let disk_puts = drain_within(&mut disk_rx, Duration::from_millis(50)).await;
        assert!(mem_puts.is_empty(), "memory cache got {mem_puts:?}");
        assert!(disk_puts.is_empty(), "DDS disk cache got {disk_puts:?}");
    }

    /// A redundancy check, not a test of the shutdown rule: an incomplete
    /// tile is never cached (#180), so this passes with or without it. The
    /// rule itself is pinned by
    /// `complete_tile_cancelled_by_shutdown_mid_encode_reaches_neither_cache`.
    #[tokio::test]
    async fn incomplete_tile_cancelled_by_shutdown_mid_encode_reaches_neither_cache() {
        let (task, mut mem_rx, mut disk_rx, mut ctx) =
            cancelled_by_shutdown_mid_encode(incomplete_chunks());

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");

        let mem_puts = drain_within(&mut mem_rx, Duration::from_millis(100)).await;
        let disk_puts = drain_within(&mut disk_rx, Duration::from_millis(50)).await;
        assert!(mem_puts.is_empty(), "memory cache got {mem_puts:?}");
        assert!(disk_puts.is_empty(), "DDS disk cache got {disk_puts:?}");
    }

    #[tokio::test]
    async fn encode_cancelled_by_shutdown_leaves_the_gauge_where_it_started() {
        let (task, _mem_rx, _disk_rx, ctx) = cancelled_by_shutdown_mid_encode(complete_chunks());

        let (result, events) = execute_with_metrics(&task, ctx).await;

        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");
        assert_eq!(encodes_active(&events), 0, "events: {events:?}");
    }

    #[tokio::test]
    async fn incomplete_tile_cancelled_mid_encode_reaches_neither_cache() {
        let (task, mut mem_rx, mut disk_rx, mut ctx) = cancelled_mid_encode(incomplete_chunks());

        let result = task.execute(&mut ctx).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");

        let mem_puts = drain_within(&mut mem_rx, Duration::from_millis(100)).await;
        let disk_puts = drain_within(&mut disk_rx, Duration::from_millis(50)).await;
        assert!(mem_puts.is_empty(), "memory cache got {mem_puts:?}");
        assert!(disk_puts.is_empty(), "DDS disk cache got {disk_puts:?}");
    }
}
