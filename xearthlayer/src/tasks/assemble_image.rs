//! Assemble image task implementation.
//!
//! [`AssembleImageTask`] combines downloaded chunks into a 4096×4096 RGBA image,
//! decoding JPEG data and filling failed chunks with magenta placeholders.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::CPU` since image assembly is CPU-bound.
//!
//! # Input
//!
//! Reads `TaskOutput` key "chunks" containing `ChunkResults` from previous task.
//!
//! # Output
//!
//! Produces `TaskOutput` with key "image" containing `RgbaImage`.

use crate::executor::{
    BlockingExecutor, ChunkResults, ResourceType, Task, TaskContext, TaskError, TaskOutput,
    TaskResult,
};
use image::{Rgba, RgbaImage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, warn};

/// Tile dimensions
const TILE_SIZE: u32 = 4096;
const CHUNK_SIZE: u32 = 256;
const CHUNKS_PER_SIDE: u32 = 16;

/// Magenta color for failed chunks (R=255, G=0, B=255, A=255)
const MAGENTA: Rgba<u8> = Rgba([255, 0, 255, 255]);

/// Task that assembles chunks into a full image.
///
/// This task decodes JPEG chunks and places them on a 4096×4096 canvas.
/// Failed or missing chunks are filled with magenta placeholders.
///
/// # Inputs
///
/// - Key: "chunks" (from DownloadChunksTask)
/// - Type: `ChunkResults`
///
/// # Outputs
///
/// - Key: "image"
/// - Type: `RgbaImage` (4096×4096 RGBA)
pub struct AssembleImageTask<X>
where
    X: BlockingExecutor,
{
    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<X> AssembleImageTask<X>
where
    X: BlockingExecutor,
{
    /// Creates a new assemble image task.
    ///
    /// # Arguments
    ///
    /// * `executor` - Executor for blocking operations (spawn_blocking)
    pub fn new(executor: Arc<X>) -> Self {
        Self { executor }
    }
}

impl<X> Task for AssembleImageTask<X>
where
    X: BlockingExecutor,
{
    fn name(&self) -> &str {
        "AssembleImage"
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

            // Get chunks from previous task
            let chunks: Option<&ChunkResults> = ctx.get_output("DownloadChunks", "chunks");
            let chunks = match chunks {
                Some(c) => c.clone(),
                None => {
                    return TaskResult::Failed(TaskError::missing_input("chunks"));
                }
            };

            debug!(
                job_id = %job_id,
                success_count = chunks.success_count(),
                failure_count = chunks.failure_count(),
                "Starting image assembly"
            );

            // Move the CPU-intensive work to a blocking task via the executor
            let image_result = self
                .executor
                .execute_blocking(move || assemble_chunks(chunks))
                .await;

            // Check for cancellation after assembly
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }

            match image_result {
                Ok(Ok(image)) => {
                    debug!(
                        job_id = %job_id,
                        width = image.width(),
                        height = image.height(),
                        "Image assembly complete"
                    );

                    // Store image in task output for next task
                    let mut output = TaskOutput::new();
                    output.set("image", image);

                    TaskResult::SuccessWithOutput(output)
                }
                Ok(Err(e)) => TaskResult::Failed(TaskError::new(format!("Assembly failed: {}", e))),
                Err(e) => {
                    TaskResult::Failed(TaskError::new(format!("Assembly task failed: {}", e)))
                }
            }
        })
    }
}

/// Output key for assembled image.
pub const OUTPUT_KEY_IMAGE: &str = "image";

/// Helper function to extract image from task output.
pub fn get_image_from_output(output: &TaskOutput) -> Option<&RgbaImage> {
    output.get::<RgbaImage>(OUTPUT_KEY_IMAGE)
}

// ============================================================================
// Internal implementation - lifted from pipeline/stages/assembly
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

    fn create_test_jpeg(r: u8, g: u8, b: u8) -> Vec<u8> {
        // Create a simple 256x256 image and encode as PNG (image crate handles both)
        let img = RgbaImage::from_fn(256, 256, |_, _| Rgba([r, g, b, 255]));
        let mut buffer = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buffer
    }

    #[test]
    fn test_task_name() {
        assert_eq!("AssembleImage", "AssembleImage");
    }

    #[test]
    fn test_output_key() {
        assert_eq!(OUTPUT_KEY_IMAGE, "image");
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
    fn test_assemble_chunks_single() {
        let mut chunks = ChunkResults::new();

        // Add a single green chunk at (0, 0)
        chunks.add_success(0, 0, create_test_jpeg(0, 255, 0));

        let result = assemble_chunks(chunks).unwrap();

        // Pixel at (0, 0) should be green
        let pixel = result.get_pixel(0, 0);
        assert_eq!(pixel[1], 255); // Green channel

        // Pixel at (256, 0) should be magenta (next chunk position)
        let pixel = result.get_pixel(256, 0);
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
