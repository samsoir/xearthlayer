//! Encode stage - compresses image to DDS format.
//!
//! This stage takes an assembled RGBA image and compresses it to DDS format
//! with BC1 or BC3 compression and mipmaps.

use crate::pipeline::{
    BlockingExecutor, JobError, JobId, PriorityConcurrencyLimiter, RequestPriority,
    TextureEncoderAsync,
};
use crate::telemetry::PipelineMetrics;
use image::RgbaImage;
use std::sync::Arc;
use std::time::Instant;
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
/// * `metrics` - Optional telemetry metrics
/// * `encode_limiter` - Optional priority-aware concurrency limiter
/// * `is_prefetch` - Whether this is a prefetch request (lower priority)
///
/// # Returns
///
/// The encoded DDS data as bytes.
#[instrument(skip(image, encoder, executor, metrics, encode_limiter), fields(job_id = %job_id))]
pub async fn encode_stage<E, X>(
    job_id: JobId,
    image: RgbaImage,
    encoder: Arc<E>,
    executor: &X,
    metrics: Option<Arc<PipelineMetrics>>,
    encode_limiter: Option<Arc<PriorityConcurrencyLimiter>>,
    is_prefetch: bool,
) -> Result<Vec<u8>, JobError>
where
    E: TextureEncoderAsync,
    X: BlockingExecutor,
{
    let width = image.width();
    let height = image.height();

    // Record encode start
    if let Some(ref m) = metrics {
        m.encode_started();
    }
    let start = Instant::now();

    // Acquire encode permit if limiter is provided.
    // Priority-aware: on-demand (high priority) always gets a permit,
    // prefetch (low priority) backs off if no permits available.
    let priority = if is_prefetch {
        RequestPriority::Low
    } else {
        RequestPriority::High
    };

    let _encode_permit = if let Some(ref limiter) = encode_limiter {
        match limiter.acquire(priority).await {
            Some(permit) => Some(permit),
            None => {
                // Prefetch couldn't get a permit - CPU is busy, skip encode
                debug!(
                    job_id = %job_id,
                    "Prefetch encode skipped - CPU busy"
                );
                return Err(JobError::Internal("Prefetch skipped: CPU busy".to_string()));
            }
        }
    } else {
        None
    };

    // Move the CPU-intensive encoding to a blocking task via the executor
    let dds_data = executor
        .execute_blocking(move || encoder.encode(&image))
        .await
        .map_err(|e| JobError::Internal(format!("encode task failed: {}", e)))?
        .map_err(|e| JobError::EncodingFailed(e.message))?;

    // Record encode completion
    let duration_us = start.elapsed().as_micros() as u64;
    let bytes = dds_data.len() as u64;
    if let Some(ref m) = metrics {
        m.encode_completed(bytes, duration_us);
    }

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

        let result = encode_stage(JobId::new(), image, encoder, &executor, None, None, false).await;

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

        let result = encode_stage(JobId::new(), image, encoder, &executor, None, None, false).await;

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

        let future = encode_stage(JobId::new(), image, encoder, &executor, None, None, false);

        // Can run without Tokio runtime
        let result = futures::executor::block_on(future).unwrap();
        assert_eq!(result[0], 0xDD);
    }

    #[tokio::test]
    async fn test_encode_stage_with_limiter() {
        use crate::pipeline::TokioExecutor;

        let image = RgbaImage::new(256, 256);
        let encoder = Arc::new(MockEncoder::new());
        let executor = TokioExecutor::new();
        let limiter = Arc::new(PriorityConcurrencyLimiter::new(2, 50, "test_encode"));

        let result = encode_stage(
            JobId::new(),
            image,
            encoder,
            &executor,
            None,
            Some(limiter),
            false,
        )
        .await;

        assert!(result.is_ok());
    }
}
