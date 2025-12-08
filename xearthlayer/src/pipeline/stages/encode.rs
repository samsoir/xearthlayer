//! Encode stage - compresses image to DDS format.
//!
//! This stage takes an assembled RGBA image and compresses it to DDS format
//! with BC1 or BC3 compression and mipmaps.

use crate::pipeline::{JobError, JobId, TextureEncoderAsync};
use image::RgbaImage;
use std::sync::Arc;
use tokio::task::spawn_blocking;
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
///
/// # Returns
///
/// The encoded DDS data as bytes.
#[instrument(skip(image, encoder), fields(job_id = %job_id))]
pub async fn encode_stage<E>(
    job_id: JobId,
    image: RgbaImage,
    encoder: Arc<E>,
) -> Result<Vec<u8>, JobError>
where
    E: TextureEncoderAsync,
{
    let width = image.width();
    let height = image.height();

    // Move the CPU-intensive encoding to a blocking task
    let dds_data = spawn_blocking(move || encoder.encode(&image))
        .await
        .map_err(|e| JobError::Internal(format!("encode task panicked: {}", e)))?
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
        fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            if self.fail {
                return Err(TextureEncodeError::new("mock encoding failure"));
            }
            // Return a simple mock DDS header + some data based on image size
            let size = image.width() * image.height();
            Ok(vec![0xDD, 0x53, (size & 0xFF) as u8, ((size >> 8) & 0xFF) as u8])
        }

        fn expected_size(&self, width: u32, height: u32) -> usize {
            (width * height) as usize
        }
    }

    #[tokio::test]
    async fn test_encode_stage_success() {
        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::new());

        let result = encode_stage(JobId::new(), image, encoder).await;

        assert!(result.is_ok());
        let dds_data = result.unwrap();
        assert_eq!(dds_data[0], 0xDD);
        assert_eq!(dds_data[1], 0x53);
    }

    #[tokio::test]
    async fn test_encode_stage_failure() {
        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::failing());

        let result = encode_stage(JobId::new(), image, encoder).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            JobError::EncodingFailed(msg) => {
                assert!(msg.contains("mock encoding failure"));
            }
            other => panic!("unexpected error type: {:?}", other),
        }
    }
}
