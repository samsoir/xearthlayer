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

use super::chunk_image::{self, ChunkPixels, CHUNK_PIXELS, CHUNK_ROW_BYTES};
use super::download_chunks::OUTPUT_KEY_CHUNKS;
use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ChunkResults, DdsDiskCache, DiskCache, MemoryCache, ResourceType, Task,
    TaskContext, TaskError, TaskOutput, TaskResult, TextureEncoderAsync,
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
const CHUNK_SIZE: u32 = CHUNK_PIXELS;
const CHUNKS_PER_SIDE: u32 = 16;
/// RGBA, one byte a channel.
const BYTES_PER_PIXEL: usize = 4;

/// Magenta color for failed chunks (R=255, G=0, B=255, A=255)
const MAGENTA: Rgba<u8> = Rgba([255, 0, 255, 255]);

/// Task that builds a DDS texture from downloaded chunks and caches it.
///
/// This task combines three sequential operations:
/// 1. Assemble chunks into a 4096×4096 RGBA image
/// 2. Encode the image to DDS format with BC1 compression
/// 3. Spawn fire-and-forget cache write (non-blocking)
///
/// # Complete tiles
///
/// Only a complete tile is cached: every one of its 256 chunks downloaded
/// (or came from the chunk cache) and decoded to a 256x256 image. Any other
/// tile has magenta squares where those chunks belong; it is served to
/// X-Plane so the sim does not stall, but never remembered (#180, #388).
/// A chunk that arrived but is not a usable image is also discarded from the
/// chunk cache, so the next request downloads it (#388).
///
/// # Cancellation
///
/// A job cancelled while its encode runs (a FUSE timeout or its error
/// policy) still caches the encoded tile, then returns `Cancelled`: the work
/// is paid for, and the next request for the tile becomes a cache hit. A
/// cancel by executor shutdown caches nothing. An incomplete tile is never
/// cached, cancelled or not (#180, #385).
///
/// With the GPU encoder the encode is awaited, not waited for on a blocking
/// thread (#422), and the job's token goes with the tile: a job cancelled
/// before the GPU starts its tile returns `Cancelled` at once, with its CPU
/// permit, and the tile is not encoded, as it is not paid for yet. A tile
/// the GPU has started is waited for and cached as above.
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
pub struct BuildAndCacheDdsTask<E, M, DD, D, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    D: DiskCache,
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

    /// Chunk disk cache, from which unusable chunks are discarded
    chunk_cache: Arc<D>,

    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<E, M, DD, D, X> BuildAndCacheDdsTask<E, M, DD, D, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    D: DiskCache,
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
    /// * `chunk_cache` - Chunk disk cache, from which chunks that are not
    ///   usable images are discarded (#388)
    /// * `executor` - Executor for blocking operations (spawn_blocking)
    pub fn new(
        tile: TileCoord,
        encoder: Arc<E>,
        memory_cache: Arc<M>,
        dds_disk_cache: Arc<DD>,
        chunk_cache: Arc<D>,
        executor: Arc<X>,
    ) -> Self {
        Self {
            tile,
            encoder,
            memory_cache,
            dds_disk_cache,
            chunk_cache,
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

    /// Spawns the fire-and-forget discard of `rejected` chunks from the chunk
    /// cache, so the next request for the tile downloads them (#388).
    ///
    /// Every rejected chunk is discarded, whether it was read from the chunk
    /// cache or just downloaded, so where each chunk came from need not be
    /// tracked: a downloaded chunk that passed the header check but fails to
    /// decode was written to the chunk cache too, and discarding a chunk
    /// that is not cached does nothing. A downloaded chunk's own write is
    /// spawned as well and may land after its discard; the entry is then
    /// rejected and discarded on its next read instead.
    fn spawn_chunk_discards(&self, rejected: Vec<(u8, u8)>) {
        if rejected.is_empty() {
            return;
        }
        let chunk_cache = Arc::clone(&self.chunk_cache);
        let tile = self.tile;
        tokio::spawn(async move {
            for (chunk_row, chunk_col) in rejected {
                chunk_cache
                    .discard(tile.row, tile.col, tile.zoom, chunk_row, chunk_col)
                    .await;
            }
        });
    }
}

impl<E, M, DD, D, X> Task for BuildAndCacheDdsTask<E, M, DD, D, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    DD: DdsDiskCache,
    D: DiskCache,
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
                // Captured before `chunks` is moved into assembly: every
                // chunk downloaded or came from the chunk cache (#180). The
                // tile is complete only if every one of them also decodes
                // (#388), which assembly tells.
                let all_downloaded = chunks.is_complete();

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

                let assembled = match image_result {
                    Ok(assembled) => assembled,
                    Err(_) if ctx.is_cancelled() => {
                        return TaskResult::Cancelled;
                    }
                    Err(e) => {
                        return TaskResult::Failed(TaskError::new(format!(
                            "Assembly task failed: {}",
                            e
                        )));
                    }
                };

                // The tile is complete, and may be cached, only if every
                // chunk downloaded and decoded to a 256x256 image (#180,
                // #388).
                let rejected_count = assembled.rejected.len();
                let tile_complete = all_downloaded && rejected_count == 0;

                // Unusable chunks leave the chunk cache whatever becomes of
                // this job: kept, they would rebuild the same magenta
                // squares on every request (#388).
                self.spawn_chunk_discards(assembled.rejected);

                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let duration_us = assembly_start.elapsed().as_micros() as u64;
                metrics.assembly_completed(duration_us);
                let image = assembled.image;

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

                // Awaited (#422): CPU work runs on the blocking executor
                // under this task's CPU permit, as before; a GPU encode is
                // awaited, parking no thread, and is skipped if the job is
                // cancelled before the GPU starts it.
                let encode_result = self
                    .encoder
                    .encode(image, ctx.cancellation_token(), self.executor.as_ref())
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

                // Cache writes are gated on tile completeness (decided above,
                // after assembly). A tile assembled with any chunk that failed
                // to download or to decode contains magenta-filled regions
                // that would be structurally indistinguishable from a
                // successful tile once persisted, and would never be
                // re-fetched. We still serve the tile to X-Plane (so the sim
                // doesn't stall), but never remember it, cancelled or not.
                // See issues #180 and #388.
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
                        rejected_count = rejected_count,
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

/// A tile image assembled from its chunks.
struct AssembledTile {
    image: RgbaImage,
    /// Chunks `(row, col)` that were downloaded or read from the chunk cache
    /// but are not usable images: they failed [`chunk_image::decode_into`]
    /// (did not decode, or were not 256x256). Each is painted magenta like a
    /// failed download. A tile with any is not complete (#388).
    rejected: Vec<(u8, u8)>,
}

/// Synchronous chunk assembly (runs in spawn_blocking).
///
/// Every chunk position gets either its chunk's pixels or a magenta square:
/// magenta for a chunk that failed to download (absent from `chunks`) and
/// for one that is not a usable image, which is also listed in
/// [`AssembledTile::rejected`]. Chunks are decoded one after another into a
/// single [`ChunkPixels`] and copied onto the canvas a row at a time (#390).
fn assemble_chunks(chunks: ChunkResults) -> AssembledTile {
    let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
    let mut pixels = ChunkPixels::new();
    let mut rejected = Vec::new();

    // Process each chunk position
    for row in 0..CHUNKS_PER_SIDE as u8 {
        for col in 0..CHUNKS_PER_SIDE as u8 {
            let x_offset = col as u32 * CHUNK_SIZE;
            let y_offset = row as u32 * CHUNK_SIZE;

            if let Some(data) = chunks.get(row, col) {
                // Decode and place the chunk
                match chunk_image::decode_into(data, &mut pixels) {
                    Ok(()) => {
                        place_chunk(&mut canvas, &pixels, x_offset, y_offset);
                    }
                    Err(e) => {
                        warn!(
                            chunk_row = row,
                            chunk_col = col,
                            error = %e,
                            "Chunk is not a usable image, using magenta placeholder"
                        );
                        fill_magenta(&mut canvas, x_offset, y_offset);
                        rejected.push((row, col));
                    }
                }
            } else {
                // Download failed - use magenta placeholder
                fill_magenta(&mut canvas, x_offset, y_offset);
            }
        }
    }

    AssembledTile {
        image: canvas,
        rejected,
    }
}

/// Copies a decoded chunk onto the canvas at the specified offset, a row at
/// a time.
fn place_chunk(canvas: &mut RgbaImage, chunk: &ChunkPixels, x_offset: u32, y_offset: u32) {
    for (canvas_row, chunk_row) in chunk_rows_mut(canvas, x_offset, y_offset).zip(chunk.rows()) {
        canvas_row.copy_from_slice(chunk_row);
    }
}

/// Fills a chunk region with magenta, a row at a time.
fn fill_magenta(canvas: &mut RgbaImage, x_offset: u32, y_offset: u32) {
    for canvas_row in chunk_rows_mut(canvas, x_offset, y_offset) {
        canvas_row.copy_from_slice(&MAGENTA_ROW);
    }
}

/// One chunk row of [`MAGENTA`].
const MAGENTA_ROW: [u8; CHUNK_ROW_BYTES] = {
    let mut row = [0; CHUNK_ROW_BYTES];
    let mut i = 0;
    while i < CHUNK_ROW_BYTES {
        row[i] = MAGENTA.0[i % BYTES_PER_PIXEL];
        i += 1;
    }
    row
};

/// The [`CHUNK_SIZE`] rows of `canvas` that the chunk whose top left pixel
/// is at `(x_offset, y_offset)` covers, top to bottom, each
/// [`CHUNK_ROW_BYTES`] of RGBA.
fn chunk_rows_mut(
    canvas: &mut RgbaImage,
    x_offset: u32,
    y_offset: u32,
) -> impl Iterator<Item = &mut [u8]> {
    let stride = canvas.width() as usize * BYTES_PER_PIXEL;
    let left = x_offset as usize * BYTES_PER_PIXEL;
    let top = y_offset as usize;
    let canvas_bytes: &mut [u8] = canvas;
    canvas_bytes[top * stride..(top + CHUNK_SIZE as usize) * stride]
        .chunks_exact_mut(stride)
        .map(move |row| &mut row[left..left + CHUNK_ROW_BYTES])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutorError, JobId, TextureEncodeError};
    use crate::tasks::chunk_image::test_chunks::{self, encoded, error_page, jpeg};
    use image::ImageFormat;
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
        let assembled = assemble_chunks(chunks);
        let result = &assembled.image;

        assert_eq!(result.width(), TILE_SIZE);
        assert_eq!(result.height(), TILE_SIZE);

        // All pixels should be magenta
        let pixel = result.get_pixel(0, 0);
        assert_eq!(*pixel, MAGENTA);
        // Missing chunks are failed downloads, counted by `ChunkResults`.
        assert!(assembled.rejected.is_empty());
    }

    // ------------------------------------------------------------------
    // Unusable chunks: painted magenta, counted (#388)
    // ------------------------------------------------------------------

    /// Whether the chunk at `(row, col)` of `image` is magenta at its four
    /// corners and centre.
    fn chunk_is_magenta(image: &RgbaImage, row: u32, col: u32) -> bool {
        let (x, y) = (col * CHUNK_SIZE, row * CHUNK_SIZE);
        let last = CHUNK_SIZE - 1;
        [(0, 0), (last, 0), (0, last), (last, last), (128, 128)]
            .iter()
            .all(|&(dx, dy)| *image.get_pixel(x + dx, y + dy) == MAGENTA)
    }

    /// Whether the chunk at `(row, col)` of `image` holds the test chunks'
    /// colour at its four corners and centre (JPEG is lossy).
    fn chunk_is_imagery(image: &RgbaImage, row: u32, col: u32) -> bool {
        let (x, y) = (col * CHUNK_SIZE, row * CHUNK_SIZE);
        let last = CHUNK_SIZE - 1;
        [(0, 0), (last, 0), (0, last), (last, last), (128, 128)]
            .iter()
            .all(|&(dx, dy)| {
                let pixel = image.get_pixel(x + dx, y + dy).0;
                pixel[3] == 255
                    && pixel
                        .iter()
                        .zip(test_chunks::COLOUR.0)
                        .all(|(got, want)| got.abs_diff(want) <= 4)
            })
    }

    #[test]
    fn unusable_chunks_are_painted_magenta_and_counted() {
        let chunks = chunks_with([
            ((2, 3), error_page()),
            ((7, 9), encoded(128, 128, ImageFormat::Jpeg)),
            ((15, 15), encoded(512, 512, ImageFormat::Jpeg)),
        ]);

        let assembled = assemble_chunks(chunks);

        let mut rejected = assembled.rejected.clone();
        rejected.sort_unstable();
        assert_eq!(rejected, vec![(2, 3), (7, 9), (15, 15)]);
        // A smaller chunk is not placed in a corner of its square, nor a
        // larger one cropped: each square is magenta, like a failed download.
        for (row, col) in [(2, 3), (7, 9), (15, 15)] {
            assert!(
                chunk_is_magenta(&assembled.image, row, col),
                "chunk ({row}, {col})"
            );
        }
        assert!(chunk_is_imagery(&assembled.image, 0, 0));
        assert!(chunk_is_imagery(&assembled.image, 7, 8));
    }

    #[test]
    fn a_tile_of_usable_chunks_rejects_none() {
        let assembled = assemble_chunks(complete_chunks());

        assert!(assembled.rejected.is_empty());
        assert!(chunk_is_imagery(&assembled.image, 0, 0));
        assert!(chunk_is_imagery(&assembled.image, 15, 15));
    }

    // ------------------------------------------------------------------
    // Byte-identical to the assembly it replaced (#390)
    // ------------------------------------------------------------------

    /// The assembly before #390, kept as the reference the fast one must
    /// match byte for byte: every chunk through [`chunk_image::decode`] into
    /// its own image, then onto the canvas pixel by pixel, magenta likewise.
    fn reference_assembly(chunks: &ChunkResults) -> AssembledTile {
        let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
        let mut rejected = Vec::new();
        for row in 0..CHUNKS_PER_SIDE as u8 {
            for col in 0..CHUNKS_PER_SIDE as u8 {
                let (x_offset, y_offset) = (col as u32 * CHUNK_SIZE, row as u32 * CHUNK_SIZE);
                let chunk = chunks.get(row, col).and_then(|data| {
                    chunk_image::decode(data)
                        .map_err(|_| rejected.push((row, col)))
                        .ok()
                });
                for y in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let pixel = chunk.as_ref().map_or(MAGENTA, |c| *c.get_pixel(x, y));
                        canvas.put_pixel(x_offset + x, y_offset + y, pixel);
                    }
                }
            }
        }
        AssembledTile {
            image: canvas,
            rejected,
        }
    }

    /// Asserts that `assemble_chunks` builds exactly the reference's tile.
    fn assert_assembles_as_the_reference_does(chunks: ChunkResults) {
        let reference = reference_assembly(&chunks);

        let assembled = assemble_chunks(chunks);

        assert_eq!(assembled.rejected, reference.rejected);
        let differing = (assembled.image.as_raw().iter())
            .zip(reference.image.as_raw())
            .filter(|(got, want)| got != want)
            .count();
        assert_eq!(differing, 0, "canvas bytes differing from the reference");
    }

    #[test]
    fn assembly_is_byte_identical_to_the_reference() {
        // Every kind of chunk body at its own position (JPEGs of every mode,
        // damaged JPEGs, PNGs, wrong sizes, non-images), three failed
        // downloads, and usable flat JPEGs everywhere else.
        let corpus = test_chunks::corpus();
        assert!(corpus.len() + 3 <= 256, "{} bodies", corpus.len());
        let mut chunks = ChunkResults::new();
        for (index, (_, body)) in corpus.into_iter().enumerate() {
            chunks.add_success(index as u8 / 16, index as u8 % 16, body);
        }
        for index in 0..256usize {
            let (row, col) = (index as u8 / 16, index as u8 % 16);
            if chunks.get(row, col).is_some() {
                continue;
            }
            if [200, 230, 255].contains(&index) {
                chunks.add_failure(row, col, 3, "network".to_string());
            } else {
                chunks.add_success(row, col, jpeg());
            }
        }

        assert_assembles_as_the_reference_does(chunks);
    }

    /// Timing of the assembly before and after #390, on 256 distinct
    /// textured chunks (4:4:4, from the `image` encoder) and on 256 copies of
    /// a libjpeg-turbo 4:2:0 chunk. Run with
    /// `cargo test --release -p xearthlayer assembly_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn assembly_timing() {
        use std::hint::black_box;

        fn time(label: &str, mut assemble: impl FnMut() -> AssembledTile) -> f64 {
            let mut samples: Vec<f64> = (0..15)
                .map(|_| {
                    let start = Instant::now();
                    black_box(assemble());
                    start.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            samples.sort_by(f64::total_cmp);
            println!(
                "{label:<44} min {:>7.2} ms   median {:>7.2} ms",
                samples[0], samples[7]
            );
            samples[7]
        }

        let textured: Vec<Bytes> = (0..256)
            .map(|seed| {
                let image = image::DynamicImage::ImageRgb8(test_chunks::textured(seed));
                test_chunks::jpeg_of(&image, 85)
            })
            .collect();
        let ycc420 = vec![test_chunks::fixture("ycc420_q75"); 256];

        for (label, bodies) in [("textured 4:4:4 q85", textured), ("ycc420_q75", ycc420)] {
            let bytes: usize = bodies.iter().map(Bytes::len).sum();
            let mut chunks = ChunkResults::new();
            for (index, body) in bodies.into_iter().enumerate() {
                chunks.add_success(index as u8 / 16, index as u8 % 16, body);
            }
            println!("{label}: 256 chunks, {bytes} bytes of JPEG");
            let before = time("  before (decode, put_pixel)", || {
                reference_assembly(&chunks)
            });
            let after = time("  after (zune RGBA, row copies)", || {
                assemble_chunks(chunks.clone())
            });
            println!(
                "  median saved {:.2} ms ({:.0}%)",
                before - after,
                100.0 * (before - after) / before
            );
        }
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
        fn encode_blocking(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
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
        fn encode_blocking(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
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
        fn encode_blocking(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
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
        fn encode_blocking(&self, _image: image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
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

    /// A chunk cache that reports every discard as its `(row, col)`.
    struct RecordingChunkCache {
        discards: UnboundedSender<(u8, u8)>,
    }
    impl DiskCache for RecordingChunkCache {
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
        fn discard(
            &self,
            tile_row: u32,
            tile_col: u32,
            zoom: u8,
            chunk_row: u8,
            chunk_col: u8,
        ) -> impl Future<Output = ()> + Send {
            assert_eq!(
                (tile_row, tile_col, zoom),
                (100, 200, 15),
                "the task's tile"
            );
            let _ = self.discards.send((chunk_row, chunk_col));
            async {}
        }
    }

    type TestTask<E, X> =
        BuildAndCacheDdsTask<E, CountingMemoryCache, CountingDdsDiskCache, RecordingChunkCache, X>;

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
        let (task, mem_rx, disk_rx, _discards, ctx) =
            make_task_with_discards(chunks, encoder, executor, job);
        (task, mem_rx, disk_rx, ctx)
    }

    /// [`make_task`], also returning the chunk cache's discards.
    fn make_task_with_discards<E: TextureEncoderAsync, X: BlockingExecutor>(
        chunks: ChunkResults,
        encoder: E,
        executor: X,
        job: CancellationToken,
    ) -> (
        TestTask<E, X>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u32, u32, u8)>,
        UnboundedReceiver<(u8, u8)>,
        TaskContext,
    ) {
        let (mem_tx, mem_rx) = unbounded_channel();
        let (disk_tx, disk_rx) = unbounded_channel();
        let (discard_tx, discard_rx) = unbounded_channel();

        let task = BuildAndCacheDdsTask::new(
            TileCoord {
                row: 100,
                col: 200,
                zoom: 15,
            },
            Arc::new(encoder),
            Arc::new(CountingMemoryCache { tx: mem_tx }),
            Arc::new(CountingDdsDiskCache { tx: disk_tx }),
            Arc::new(RecordingChunkCache {
                discards: discard_tx,
            }),
            Arc::new(executor),
        );

        let ctx = TaskContext::new(JobId::new("test-job"), job);
        let mut output = TaskOutput::new();
        output.set("chunks", chunks);
        ctx.add_task_output("DownloadChunks".to_string(), output);

        (task, mem_rx, disk_rx, discard_rx, ctx)
    }

    /// 256 usable chunks: real 256x256 JPEGs.
    fn complete_chunks() -> ChunkResults {
        let chunks = chunks_with([]);
        assert!(chunks.is_complete());
        chunks
    }

    /// 256 downloaded chunks: usable JPEGs except the bodies given by
    /// position. Every chunk downloaded, so `is_complete()` holds whatever
    /// the bodies are.
    fn chunks_with<const N: usize>(bodies: [((u8, u8), Bytes); N]) -> ChunkResults {
        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                let body = bodies
                    .iter()
                    .find(|(at, _)| *at == (row, col))
                    .map_or_else(jpeg, |(_, body)| body.clone());
                chunks.add_success(row, col, body);
            }
        }
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
        // 256 downloaded chunks that are all 256x256 images: the tile is
        // complete. Before #388 this test fed 256 empty bodies, every one of
        // which failed to decode, and asserted that the resulting all-magenta
        // tile was cached.
        let (task, mut mem_rx, mut disk_rx, mut ctx) = make_task_with_chunks(complete_chunks());

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

    /// Runs a task over `chunks` and asserts X-Plane gets a tile but
    /// neither cache does.
    async fn assert_served_but_not_cached(chunks: ChunkResults) {
        let (task, mut mem_rx, mut disk_rx, mut ctx) = make_task_with_chunks(chunks);

        match task.execute(&mut ctx).await {
            TaskResult::SuccessWithOutput(out) => {
                let dds = out.get::<Bytes>("dds_data").cloned();
                assert!(
                    dds.is_some_and(|dds| !dds.is_empty()),
                    "X-Plane gets a tile"
                );
            }
            other => panic!("expected SuccessWithOutput, got {other:?}"),
        }

        let mem_puts = drain_within(&mut mem_rx, Duration::from_millis(100)).await;
        let disk_puts = drain_within(&mut disk_rx, Duration::from_millis(50)).await;
        assert!(mem_puts.is_empty(), "memory cache got {mem_puts:?}");
        assert!(disk_puts.is_empty(), "DDS disk cache got {disk_puts:?}");
    }

    /// #388: every chunk downloaded, so `ChunkResults::is_complete()` holds,
    /// but one is not an image. The tile has a magenta square and must not
    /// be remembered.
    #[tokio::test]
    async fn undecodable_chunk_leaves_both_caches_unwritten() {
        let chunks = chunks_with([((4, 11), error_page())]);
        assert!(chunks.is_complete());

        assert_served_but_not_cached(chunks).await;
    }

    #[tokio::test]
    async fn wrong_sized_chunk_leaves_both_caches_unwritten() {
        let chunks = chunks_with([((0, 0), encoded(512, 512, ImageFormat::Jpeg))]);
        assert!(chunks.is_complete());

        assert_served_but_not_cached(chunks).await;
    }

    /// The discards that reach the chunk cache within a short wait, sorted.
    async fn discards_within(rx: &mut UnboundedReceiver<(u8, u8)>) -> Vec<(u8, u8)> {
        let mut discards = Vec::new();
        while let Ok(Some(discard)) =
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
        {
            discards.push(discard);
        }
        discards.sort_unstable();
        discards
    }

    /// #388: a chunk that does not decode, or is not 256x256, is removed
    /// from the chunk cache so the next request downloads it, instead of
    /// rebuilding the same magenta square from it forever.
    #[tokio::test]
    async fn unusable_chunks_are_discarded_from_the_chunk_cache() {
        let chunks = chunks_with([
            ((2, 3), error_page()),
            ((7, 9), encoded(128, 128, ImageFormat::Jpeg)),
        ]);
        let (task, _mem_rx, _disk_rx, mut discard_rx, mut ctx) = make_task_with_discards(
            chunks,
            StubEncoder,
            InlineBlockingExecutor,
            CancellationToken::new(),
        );

        task.execute(&mut ctx).await;

        assert_eq!(
            discards_within(&mut discard_rx).await,
            vec![(2, 3), (7, 9)],
            "exactly the unusable chunks"
        );
    }

    #[tokio::test]
    async fn usable_chunks_and_failed_downloads_are_not_discarded() {
        let mut chunks = ChunkResults::new();
        for row in 0..16u8 {
            for col in 0..16u8 {
                if (row, col) == (5, 5) {
                    chunks.add_failure(row, col, 3, "timeout".to_string());
                } else {
                    chunks.add_success(row, col, jpeg());
                }
            }
        }
        let (task, _mem_rx, _disk_rx, mut discard_rx, mut ctx) = make_task_with_discards(
            chunks,
            StubEncoder,
            InlineBlockingExecutor,
            CancellationToken::new(),
        );

        task.execute(&mut ctx).await;

        assert!(discards_within(&mut discard_rx).await.is_empty());
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

        let (task, _mem_rx, _disk_rx, ctx) = make_task_with_chunks(complete_chunks());

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

    // ------------------------------------------------------------------
    // The GPU encoder, awaited, and a job cancelled before its tile starts
    // (#422)
    // ------------------------------------------------------------------

    use crate::dds::gpu_channel::{
        spawn_tile_worker_with_clock, Clock, GpuEncodeReply, GpuEncodeRequest, GpuEncoderChannel,
        GpuTileTiming, HangRule, TileProcessor, CHANNEL_CAPACITY,
    };
    use crate::dds::gpu_health::SharedGpuHealth;
    use crate::dds::{DdsError, DdsFormat};
    use crate::executor::TextureEncoderAdapter;
    use crate::texture::DdsTextureEncoder;

    type GpuEncoder = TextureEncoderAdapter<Arc<DdsTextureEncoder>>;

    /// A fake GPU: reports each tile's width as it starts, then waits for
    /// one release per tile.
    struct Gated {
        starts: UnboundedSender<u32>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl TileProcessor for Gated {
        fn process(
            &mut self,
            image: RgbaImage,
            _format: DdsFormat,
            _levels: usize,
            _deadline: Instant,
            _timing: &mut GpuTileTiming,
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            let _ = self.starts.send(image.width());
            self.release.recv().unwrap();
            // A complete chain, as far as the encoder can tell.
            payload.fill(0xA5);
            Ok(())
        }
    }

    /// A clock that never moves: no request waits and no tile takes any
    /// time, however slow the machine, so the hang rule never fires.
    struct Frozen(Instant);

    impl Clock for Frozen {
        fn now(&self) -> Instant {
            self.0
        }
    }

    /// The real worker loop around [`Gated`], the encoder the service
    /// builds on its channel, and the channel's raw sender, to queue tiles
    /// the task did not send.
    struct GpuRig {
        encoder: GpuEncoder,
        raw: tokio::sync::mpsc::Sender<GpuEncodeRequest>,
        starts: UnboundedReceiver<u32>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn gpu_rig() -> GpuRig {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (starts_tx, starts) = unbounded_channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let health = SharedGpuHealth::new();
        let clock: Arc<dyn Clock> = Arc::new(Frozen(Instant::now()));
        spawn_tile_worker_with_clock(
            Gated {
                starts: starts_tx,
                release: release_rx,
            },
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
            Arc::clone(&clock),
        );
        let channel =
            GpuEncoderChannel::new(tx.clone(), health, CancellationToken::new()).with_clock(clock);
        let encoder = TextureEncoderAdapter::new(Arc::new(
            DdsTextureEncoder::new(DdsFormat::BC1)
                .with_compressor(Arc::new(crate::dds::IspcCompressor))
                .with_mipmap_compressor(Arc::new(channel)),
        ));
        GpuRig {
            encoder,
            raw: tx,
            starts,
            release,
        }
    }

    /// Queues a 4-wide tile the task did not send, and its reply.
    fn queue_raw(
        raw: &tokio::sync::mpsc::Sender<GpuEncodeRequest>,
        width: u32,
    ) -> tokio::sync::oneshot::Receiver<GpuEncodeReply> {
        let (request, reply) = GpuEncodeRequest::new(
            RgbaImage::new(width, 4),
            DdsFormat::BC1,
            1,
            vec![0; crate::dds::layout::file_len(width, 4, 1, DdsFormat::BC1)],
            Instant::now(),
            CancellationToken::new(),
        );
        raw.try_send(request).expect("room in the channel");
        reply
    }

    async fn next_start(starts: &mut UnboundedReceiver<u32>) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), starts.recv())
            .await
            .expect("a tile starts")
            .expect("the worker is alive")
    }

    /// Yields until `raw`'s channel holds `n` requests: the task runs on
    /// this thread, and sends on its first poll after assembly.
    async fn queued(raw: &tokio::sync::mpsc::Sender<GpuEncodeRequest>, n: usize) {
        for _ in 0..10_000 {
            if raw.max_capacity() - raw.capacity() == n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{n} requests were never queued");
    }

    /// Whether the cache behind `rx` was written, once every sender is
    /// gone: the task's, and those of the writes it spawned.
    async fn written(rx: &mut UnboundedReceiver<(u32, u32, u8)>) -> bool {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the writes end")
            .is_some()
    }

    /// The task executed on its own tokio task, as the executor runs it.
    fn run(
        task: TestTask<GpuEncoder, InlineBlockingExecutor>,
        mut ctx: TaskContext,
    ) -> tokio::task::JoinHandle<TaskResult> {
        tokio::spawn(async move { task.execute(&mut ctx).await })
    }

    async fn result_of(handle: tokio::task::JoinHandle<TaskResult>) -> TaskResult {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the task returns")
            .unwrap()
    }

    /// A job cancelled while its tile waits behind a busy GPU returns
    /// `Cancelled` at once, without waiting for the worker; the worker
    /// never starts that tile; nothing is cached.
    #[tokio::test]
    async fn gpu_job_cancelled_while_queued_returns_at_once_and_is_never_encoded() {
        let mut rig = gpu_rig();
        let busy = queue_raw(&rig.raw, 4);
        assert_eq!(next_start(&mut rig.starts).await, 4);
        let job = CancellationToken::new();
        let (task, mut mem_rx, mut disk_rx, ctx) = make_task(
            complete_chunks(),
            rig.encoder,
            InlineBlockingExecutor,
            job.clone(),
        );

        let handle = run(task, ctx);
        queued(&rig.raw, 1).await;
        job.cancel();
        let result = result_of(handle).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");
        assert!(!written(&mut mem_rx).await && !written(&mut disk_rx).await);

        // The worker, released, goes past the cancelled tile.
        rig.release.send(()).unwrap();
        busy.await.unwrap();
        let after = queue_raw(&rig.raw, 12);
        assert_eq!(next_start(&mut rig.starts).await, 12, "not the task's 4096");
        rig.release.send(()).unwrap();
        after.await.unwrap();
    }

    /// A job cancelled while the GPU encodes its tile still caches the
    /// complete tile (#385): it is paid for.
    #[tokio::test]
    async fn gpu_job_cancelled_while_its_tile_is_encoded_caches_it() {
        let mut rig = gpu_rig();
        let job = CancellationToken::new();
        let (task, mut mem_rx, mut disk_rx, ctx) = make_task(
            complete_chunks(),
            rig.encoder,
            InlineBlockingExecutor,
            job.clone(),
        );

        let handle = run(task, ctx);
        assert_eq!(next_start(&mut rig.starts).await, 4096);
        job.cancel();
        rig.release.send(()).unwrap();
        let result = result_of(handle).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");
        assert!(written(&mut mem_rx).await, "memory cache");
        assert!(written(&mut disk_rx).await, "DDS disk cache");
    }

    /// A shutdown while the GPU encodes the tile caches nothing (#385).
    #[tokio::test]
    async fn gpu_job_cancelled_by_shutdown_while_encoded_caches_nothing() {
        let mut rig = gpu_rig();
        let job = CancellationToken::new();
        let shutdown = CancellationToken::new();
        let (task, mut mem_rx, mut disk_rx, ctx) = make_task(
            complete_chunks(),
            rig.encoder,
            InlineBlockingExecutor,
            job.clone(),
        );

        let handle = run(task, ctx.with_executor_shutdown(shutdown.clone()));
        assert_eq!(next_start(&mut rig.starts).await, 4096);
        shutdown.cancel();
        job.cancel();
        rig.release.send(()).unwrap();
        let result = result_of(handle).await;
        assert!(matches!(result, TaskResult::Cancelled), "got {result:?}");
        assert!(!written(&mut mem_rx).await && !written(&mut disk_rx).await);
    }
}
