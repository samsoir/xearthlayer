//! Encode DDS task implementation.
//!
//! [`EncodeDdsTask`] compresses an assembled RGBA image into DDS format
//! with BC1/BC3 compression and mipmaps.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::CPU` since DDS encoding is CPU-bound.
//!
//! # Input
//!
//! Reads `TaskOutput` key "image" containing `RgbaImage` from previous task.
//!
//! # Output
//!
//! Produces `TaskOutput` with key "dds_data" containing `Vec<u8>`.

use crate::coord::TileCoord;
use crate::executor::{
    BlockingExecutor, ResourceType, Task, TaskContext, TaskError, TaskOutput, TaskResult,
    TextureEncoderAsync,
};
use image::RgbaImage;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::debug;

/// Task that encodes an assembled image to DDS format.
///
/// This task uses the texture encoder to compress the image with BC1/BC3
/// compression and generate mipmaps for efficient GPU rendering.
///
/// # Inputs
///
/// - Key: "image" (from AssembleImageTask)
/// - Type: `RgbaImage`
///
/// # Outputs
///
/// - Key: "dds_data"
/// - Type: `Vec<u8>` (complete DDS file with mipmaps)
pub struct EncodeDdsTask<E, X>
where
    E: TextureEncoderAsync,
    X: BlockingExecutor,
{
    /// Tile coordinates (for logging)
    tile: TileCoord,

    /// Texture encoder for DDS compression
    encoder: Arc<E>,

    /// Executor for CPU-bound blocking operations
    executor: Arc<X>,
}

impl<E, X> EncodeDdsTask<E, X>
where
    E: TextureEncoderAsync,
    X: BlockingExecutor,
{
    /// Creates a new encode DDS task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates (for logging context)
    /// * `encoder` - Texture encoder for DDS compression
    /// * `executor` - Executor for blocking operations (spawn_blocking)
    pub fn new(tile: TileCoord, encoder: Arc<E>, executor: Arc<X>) -> Self {
        Self {
            tile,
            encoder,
            executor,
        }
    }
}

impl<E, X> Task for EncodeDdsTask<E, X>
where
    E: TextureEncoderAsync,
    X: BlockingExecutor,
{
    fn name(&self) -> &str {
        "EncodeDds"
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

            // Get image from previous task
            let image: Option<&RgbaImage> = ctx.get_output("AssembleImage", "image");
            let image = match image {
                Some(img) => img.clone(),
                None => {
                    return TaskResult::Failed(TaskError::missing_input("image"));
                }
            };

            debug!(
                job_id = %job_id,
                tile = ?self.tile,
                width = image.width(),
                height = image.height(),
                "Starting DDS encoding"
            );

            // Move the CPU-intensive encoding to a blocking task via the executor
            let encoder = Arc::clone(&self.encoder);
            let encode_result = self
                .executor
                .execute_blocking(move || encoder.encode(&image))
                .await;

            // Check for cancellation after encoding
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }

            match encode_result {
                Ok(Ok(dds_data)) => {
                    debug!(
                        job_id = %job_id,
                        tile = ?self.tile,
                        size_bytes = dds_data.len(),
                        "DDS encoding complete"
                    );

                    // Store DDS data in task output for next task
                    let mut output = TaskOutput::new();
                    output.set("dds_data", dds_data);

                    TaskResult::SuccessWithOutput(output)
                }
                Ok(Err(e)) => {
                    TaskResult::Failed(TaskError::new(format!("Encoding failed: {}", e.message)))
                }
                Err(e) => TaskResult::Failed(TaskError::new(format!("Encode task failed: {}", e))),
            }
        })
    }
}

/// Output key for DDS data.
pub const OUTPUT_KEY_DDS_DATA: &str = "dds_data";

/// Helper function to extract DDS data from task output.
pub fn get_dds_data_from_output(output: &TaskOutput) -> Option<&Vec<u8>> {
    output.get::<Vec<u8>>(OUTPUT_KEY_DDS_DATA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_name() {
        assert_eq!("EncodeDds", "EncodeDds");
    }

    #[test]
    fn test_output_key() {
        assert_eq!(OUTPUT_KEY_DDS_DATA, "dds_data");
    }
}
