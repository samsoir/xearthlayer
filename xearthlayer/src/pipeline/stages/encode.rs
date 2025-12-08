//! Encode stage - compresses image to DDS format.
//!
//! This stage takes an assembled RGBA image and compresses it to DDS format
//! with BC1 or BC3 compression and mipmaps.

use crate::pipeline::{BlockingExecutor, JobError, JobId, TextureEncoderAsync};
use image::RgbaImage;
use std::sync::Arc;
use tracing::{debug, instrument};

/// Encodes an RGBA image to DDS format.
///
/// This stage:
/// 1. Compresses the image using BC1 or BC3 compression
/// 2. Generates mipmaps
/// 3. Produces a complete DDS file
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `image` - The assembled 4096x4096 RGBA image
/// * `encoder` - The texture encoder to use
/// * `executor` - Executor for running blocking work
///
/// # Returns
///
/// The encoded DDS data as bytes.
#[instrument(skip(image, encoder, executor), fields(job_id = %job_id))]
pub async fn encode_stage<E, X>(
    job_id: JobId,
    image: RgbaImage,
    encoder: Arc<E>,
    executor: &X,
) -> Result<Vec<u8>, JobError>
where
    E: TextureEncoderAsync,
    X: BlockingExecutor,
{
    let width = image.width();
    let height = image.height();

    // Move the CPU-intensive encoding to a blocking task via the executor
    let dds_data = executor
        .execute_blocking(move || encoder.encode(&image))
        .await
        .map_err(|e| JobError::Internal(format!("encode task failed: {}", e)))?
        .map_err(|e| JobError::EncodingFailed(e.message))?;

    debug!(
        job_id = %job_id,
        width,
        height,
        size_bytes = dds_data.len(),
        "Encode stage complete"
    );

    Ok(dds_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::TextureEncodeError;

    /// Mock encoder for testing
    struct MockEncoder {
        fail: bool,
    }

    impl MockEncoder {
        fn new() -> Self {
            Self { fail: false }
        }

        fn failing() -> Self {
            Self { fail: true }
        }
    }

    impl TextureEncoderAsync for MockEncoder {
        fn encode(&self, _image: &RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            if self.fail {
                return Err(TextureEncodeError::new("mock encoding failure"));
            }
            // Return a simple mock DDS header
            Ok(vec![0xDD, 0x53, 0x20, 0x00])
        }

        fn expected_size(&self, width: u32, height: u32) -> usize {
            (width * height) as usize
        }
    }

    #[tokio::test]
    async fn test_encode_stage_success_with_tokio() {
        use crate::pipeline::TokioExecutor;

        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::new());
        let executor = TokioExecutor::new();

        let result = encode_stage(JobId::new(), image, encoder, &executor).await;

        assert!(result.is_ok());
        let dds_data = result.unwrap();
        assert_eq!(dds_data[0], 0xDD);
        assert_eq!(dds_data[1], 0x53);
    }

    #[tokio::test]
    async fn test_encode_stage_failure_with_tokio() {
        use crate::pipeline::TokioExecutor;

        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::failing());
        let executor = TokioExecutor::new();

        let result = encode_stage(JobId::new(), image, encoder, &executor).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            JobError::EncodingFailed(msg) => {
                assert!(msg.contains("mock encoding failure"));
            }
            other => panic!("unexpected error type: {:?}", other),
        }
    }

    // This test demonstrates DIP - it doesn't require Tokio!
    #[test]
    fn test_encode_stage_with_sync_executor() {
        use crate::pipeline::executor::SyncExecutor;

        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::new());
        let executor = SyncExecutor;

        let future = encode_stage(JobId::new(), image, encoder, &executor);

        // Can run without Tokio runtime
        let result = futures::executor::block_on(future).unwrap();
        assert_eq!(result[0], 0xDD);
    }
}
