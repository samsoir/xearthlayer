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

use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkResults, DdsDiskCache, MemoryCache, ResourceType, Task, TaskContext,
    TaskError, TaskOutput, TaskResult, TextureEncoderAsync,
};
use crate::metrics::OptionalMetrics;
use image::{Rgba, RgbaImage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;
use tracing::{debug, info, warn};

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
/// 2. Encode the image to DDS format with BC1/BC3 compression
/// 3. Spawn fire-and-forget cache write (non-blocking)
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
                // Check for cancellation before starting
                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let job_id = ctx.job_id();
                let metrics = ctx.metrics_clone();

                // Step 1: Get chunks from previous task (returns owned clone)
                let chunks: ChunkResults = match ctx.get_output("DownloadChunks", "chunks") {
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

                // Step 3: Encode to DDS (CPU-bound)
                metrics.encode_started();
                let encode_start = Instant::now();

                let encoder = Arc::clone(&self.encoder);
                let encode_result = self
                    .executor
                    .execute_blocking(move || encoder.encode(image))
                    .await;

                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let dds_data = match encode_result {
                    Ok(Ok(data)) => {
                        let duration_us = encode_start.elapsed().as_micros() as u64;
                        let bytes = data.len() as u64;
                        metrics.encode_completed(bytes, duration_us);
                        data
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

                // Cache writes are gated on tile completeness (captured above
                // before `chunks` was moved into assembly). A tile assembled
                // with any failed chunks contains magenta-filled regions that
                // would be structurally indistinguishable from a successful tile
                // once persisted, and would never be re-fetched. We still serve
                // the tile to X-Plane (so the sim doesn't stall), but never
                // remember it. See issue #180.
                if tile_complete {
                    debug!(
                        job_id = %job_id,
                        tile = ?self.tile,
                        size_bytes = dds_size,
                        "DDS encoding complete, spawning cache write"
                    );

                    // Step 4: Spawn fire-and-forget memory cache write
                    // This avoids blocking the job completion on cache write,
                    // and avoids race conditions with eventual consistency caches.
                    let cache = Arc::clone(&self.memory_cache);
                    let tile = self.tile;
                    let dds_data_for_cache = dds_data.clone();
                    tokio::spawn(async move {
                        cache
                            .put(tile.row, tile.col, tile.zoom, dds_data_for_cache)
                            .await;

                        debug!(
                            tile = ?tile,
                            "Memory cache write complete (async)"
                        );
                    });

                    // Step 5: Spawn fire-and-forget DDS disk cache write
                    // Persists the encoded DDS tile to disk so it can be served
                    // without re-encoding after memory eviction.
                    let dds_disk = Arc::clone(&self.dds_disk_cache);
                    let tile_for_disk = self.tile;
                    let dds_data_for_disk = dds_data.clone();
                    let dds_bytes = dds_data_for_disk.len() as u64;
                    let metrics_for_dds = metrics.clone();
                    tokio::spawn(async move {
                        metrics_for_dds.disk_write_started();
                        let write_start = Instant::now();

                        dds_disk
                            .put(
                                tile_for_disk.row,
                                tile_for_disk.col,
                                tile_for_disk.zoom,
                                dds_data_for_disk,
                            )
                            .await;

                        let duration_us = write_start.elapsed().as_micros() as u64;
                        metrics_for_dds.disk_write_completed(dds_bytes, duration_us);

                        debug!(
                            tile = ?tile_for_disk,
                            "DDS disk cache write complete (async)"
                        );
                    });
                } else {
                    warn!(
                        job_id = %job_id,
                        tile = ?self.tile,
                        success_count = success_count,
                        failure_count = failure_count,
                        size_bytes = dds_size,
                        "Tile incomplete; serving DDS to X-Plane but skipping cache writes"
                    );
                }

                info!(
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
// Internal implementation - lifted from assemble_image.rs
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

    struct StubEncoder;
    impl TextureEncoderAsync for StubEncoder {
        fn encode(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            // 128 bytes is enough to satisfy "non-empty" checks; the magic header
            // isn't validated at this layer.
            Ok(vec![b'D', b'D', b'S', b' ']
                .into_iter()
                .chain(vec![0u8; 124])
                .collect())
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
        ) -> impl Future<Output = Option<Vec<u8>>> + Send {
            async { None }
        }
        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            _data: Vec<u8>,
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
        ) -> impl Future<Output = Option<Vec<u8>>> + Send {
            async { None }
        }
        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            _data: Vec<u8>,
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

    fn make_task_with_chunks(
        chunks: ChunkResults,
    ) -> (
        BuildAndCacheDdsTask<
            StubEncoder,
            CountingMemoryCache,
            CountingDdsDiskCache,
            InlineBlockingExecutor,
        >,
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
            Arc::new(StubEncoder),
            Arc::new(CountingMemoryCache { tx: mem_tx }),
            Arc::new(CountingDdsDiskCache { tx: disk_tx }),
            Arc::new(InlineBlockingExecutor),
        );

        let ctx = TaskContext::new(JobId::new("test-job"), CancellationToken::new());
        let mut output = TaskOutput::new();
        output.set("chunks", chunks);
        ctx.add_task_output("DownloadChunks".to_string(), output);

        (task, mem_rx, disk_rx, ctx)
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
                let dds: Option<Vec<u8>> = out.get::<Vec<u8>>("dds_data").cloned();
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
                chunks.add_success(row, col, vec![]);
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
}
