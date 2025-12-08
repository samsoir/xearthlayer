//! Assembly stage - combines chunks into a single image.
//!
//! This stage takes downloaded chunks and assembles them into a 4096x4096
//! RGBA image. Failed chunks are replaced with magenta placeholders.

use crate::pipeline::{BlockingExecutor, ChunkResults, JobError, JobId};
use image::{Rgba, RgbaImage};
use tracing::{debug, instrument, warn};

/// Tile dimensions
const TILE_SIZE: u32 = 4096;
const CHUNK_SIZE: u32 = 256;
const CHUNKS_PER_SIDE: u32 = 16;

/// Magenta color for failed chunks (R=255, G=0, B=255, A=255)
const MAGENTA: Rgba<u8> = Rgba([255, 0, 255, 255]);

/// Assembles chunks into a 4096x4096 RGBA image.
///
/// This stage:
/// 1. Creates a canvas of 4096x4096 pixels
/// 2. Decodes each successful chunk (JPEG) and places it on the canvas
/// 3. Fills failed chunk positions with magenta placeholders
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `chunks` - Results from the download stage
/// * `executor` - Executor for running blocking work
///
/// # Returns
///
/// The assembled RGBA image, or an error if assembly fails catastrophically.
#[instrument(skip(chunks, executor), fields(job_id = %job_id))]
pub async fn assembly_stage<E>(
    job_id: JobId,
    chunks: ChunkResults,
    executor: &E,
) -> Result<RgbaImage, JobError>
where
    E: BlockingExecutor,
{
    let success_count = chunks.success_count();
    let failure_count = chunks.failure_count();

    // Move the CPU-intensive work to a blocking task via the executor
    let image = executor
        .execute_blocking(move || assemble_chunks(chunks))
        .await
        .map_err(|e| JobError::Internal(format!("assembly task failed: {}", e)))?
        .map_err(JobError::AssemblyFailed)?;

    debug!(
        job_id = %job_id,
        success_count,
        failure_count,
        "Assembly stage complete"
    );

    Ok(image)
}

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
        // Create a simple 256x256 image and encode as JPEG
        let img = RgbaImage::from_fn(256, 256, |_, _| Rgba([r, g, b, 255]));
        let mut buffer = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buffer
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

    #[tokio::test]
    async fn test_assembly_stage_complete_with_tokio() {
        use crate::pipeline::TokioExecutor;

        let mut chunks = ChunkResults::new();

        // Add all 256 chunks
        for row in 0..16u8 {
            for col in 0..16u8 {
                chunks.add_success(row, col, create_test_jpeg(row * 16, col * 16, 128));
            }
        }

        let executor = TokioExecutor::new();
        let result = assembly_stage(JobId::new(), chunks, &executor)
            .await
            .unwrap();

        assert_eq!(result.width(), TILE_SIZE);
        assert_eq!(result.height(), TILE_SIZE);
    }

    // This test demonstrates DIP - it doesn't require Tokio!
    #[test]
    fn test_assembly_stage_with_sync_executor() {
        use crate::pipeline::executor::SyncExecutor;

        let mut chunks = ChunkResults::new();
        chunks.add_success(0, 0, create_test_jpeg(255, 0, 0));

        let executor = SyncExecutor;
        let future = assembly_stage(JobId::new(), chunks, &executor);

        // Can run without Tokio runtime
        let result = futures::executor::block_on(future).unwrap();

        assert_eq!(result.width(), TILE_SIZE);
        assert_eq!(result.height(), TILE_SIZE);
    }
}
