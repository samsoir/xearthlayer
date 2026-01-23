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
    BlockingExecutor, ChunkResults, MemoryCache, ResourceType, Task, TaskContext, TaskError,
    TaskOutput, TaskResult, TextureEncoderAsync,
};
use crate::metrics::OptionalMetrics;
use image::{Rgba, RgbaImage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
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
pub struct BuildAndCacheDdsTask<E, M, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    X: BlockingExecutor,
{
    /// Tile coordinates
    tile: TileCoord,

    /// Texture encoder for DDS compression
    encoder: Arc<E>,

    /// Memory cache for storing completed DDS tiles
    memory_cache: Arc<M>,

    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<E, M, X> BuildAndCacheDdsTask<E, M, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
    X: BlockingExecutor,
{
    /// Creates a new build and cache DDS task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates
    /// * `encoder` - Texture encoder for DDS compression
    /// * `memory_cache` - Memory cache to write completed tiles
    /// * `executor` - Executor for blocking operations (spawn_blocking)
    pub fn new(tile: TileCoord, encoder: Arc<E>, memory_cache: Arc<M>, executor: Arc<X>) -> Self {
        Self {
            tile,
            encoder,
            memory_cache,
            executor,
        }
    }
}

impl<E, M, X> Task for BuildAndCacheDdsTask<E, M, X>
where
    E: TextureEncoderAsync,
    M: MemoryCache,
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
        Box::pin(async move {
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

            debug!(
                job_id = %job_id,
                tile = ?self.tile,
                success_count = chunks.success_count(),
                failure_count = chunks.failure_count(),
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
                    return TaskResult::Failed(TaskError::new(format!("Assembly failed: {}", e)));
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
                .execute_blocking(move || encoder.encode(&image))
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

            debug!(
                job_id = %job_id,
                tile = ?self.tile,
                size_bytes = dds_size,
                "DDS encoding complete, spawning cache write"
            );

            // Step 4: Spawn fire-and-forget cache write
            // This avoids blocking the job completion on cache write,
            // and avoids race conditions with eventual consistency caches.
            let cache = Arc::clone(&self.memory_cache);
            let tile = self.tile;
            let dds_data_for_cache = dds_data.clone();
            let metrics_for_cache = metrics.clone();
            tokio::spawn(async move {
                cache
                    .put(tile.row, tile.col, tile.zoom, dds_data_for_cache)
                    .await;

                // Emit updated cache size to metrics
                let total_cache_size = cache.size_bytes();
                metrics_for_cache.memory_cache_size(total_cache_size as u64);

                debug!(
                    tile = ?tile,
                    cache_size_bytes = total_cache_size,
                    "Cache write complete (async)"
                );
            });

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
        })
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
        assert_eq!(*canvas.get_pixel(255, 255), MAGENTA);

        // Check just outside the filled region is not magenta
        assert_eq!(*canvas.get_pixel(256, 0), Rgba([0, 0, 0, 0]));
    }
}
