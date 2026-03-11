//! GPU encoder channel types for mpsc-based GPU compression.
//!
//! Provides the message types and channel handle for submitting
//! single mip-level images to a dedicated GPU worker thread.

#[cfg(feature = "gpu-encode")]
mod inner {
    use image::RgbaImage;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};

    use crate::dds::compressor::BlockCompressor;
    use crate::dds::{DdsError, DdsFormat};

    /// Bounded channel capacity for GPU encode requests.
    pub const CHANNEL_CAPACITY: usize = 32;

    /// Message sent from callers to the GPU worker for compression.
    pub struct GpuEncodeRequest {
        /// A single mip-level RGBA image to compress.
        pub image: RgbaImage,
        /// The target block compression format.
        pub format: DdsFormat,
        /// One-shot channel for returning the compressed data (or error).
        pub response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
    }

    /// Sender-side handle for submitting GPU encode requests.
    ///
    /// Implements [`BlockCompressor`] so it can be used as a drop-in replacement
    /// for direct compressors. Requests are forwarded to the GPU worker via an
    /// mpsc channel; the caller blocks until the worker responds via oneshot.
    pub struct GpuEncoderChannel {
        sender: mpsc::Sender<GpuEncodeRequest>,
    }

    impl GpuEncoderChannel {
        /// Create a new channel handle wrapping the given sender.
        pub fn new(sender: mpsc::Sender<GpuEncodeRequest>) -> Self {
            Self { sender }
        }

        /// Returns `true` if the receiver end is still alive.
        pub fn is_connected(&self) -> bool {
            !self.sender.is_closed()
        }
    }

    impl BlockCompressor for GpuEncoderChannel {
        /// Note: The image is cloned to transfer ownership through the channel.
        /// For 4096×4096 RGBA images this is ~64MB per mip level. This is an
        /// inherent cost of the `BlockCompressor` trait contract (which takes
        /// `&RgbaImage`). Future `TextureEncoder`-level integration could avoid
        /// this by transferring ownership directly.
        fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
            let (resp_tx, resp_rx) = oneshot::channel();
            let request = GpuEncodeRequest {
                image: image.clone(),
                format,
                response: resp_tx,
            };

            self.sender.blocking_send(request).map_err(|_| {
                DdsError::CompressionFailed("GPU worker is not running".to_string())
            })?;

            resp_rx.blocking_recv().map_err(|_| {
                DdsError::CompressionFailed("GPU worker dropped response channel".to_string())
            })?
        }

        fn name(&self) -> &str {
            "gpu-channel"
        }
    }

    /// Spawn the GPU encoder worker task.
    ///
    /// The worker receives compression requests and processes them using
    /// the provided block compressor. Returns a `JoinHandle` that resolves
    /// when the channel is closed (all senders dropped).
    pub fn spawn_gpu_worker(
        compressor: Arc<dyn BlockCompressor>,
        mut rx: mpsc::Receiver<GpuEncodeRequest>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let comp = Arc::clone(&compressor);
                let result = tokio::task::spawn_blocking(move || {
                    comp.compress(&request.image, request.format)
                })
                .await;

                let response = match result {
                    Ok(r) => r,
                    Err(e) => Err(DdsError::CompressionFailed(format!(
                        "worker task panicked: {e}"
                    ))),
                };

                // Send response; ignore error if caller dropped their receiver
                let _ = request.response.send(response);
            }
            tracing::info!("GPU encoder worker shutting down (channel closed)");
        })
    }

    /// Create a [`GpuEncoderChannel`] and spawn the worker, returning
    /// the channel handle and worker `JoinHandle`.
    pub fn create_gpu_encoder_channel(
        compressor: Arc<dyn BlockCompressor>,
    ) -> (GpuEncoderChannel, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = spawn_gpu_worker(compressor, rx);
        (GpuEncoderChannel::new(tx), handle)
    }
}

#[cfg(feature = "gpu-encode")]
pub use inner::*;

#[cfg(test)]
#[cfg(feature = "gpu-encode")]
mod tests {
    use super::*;
    use crate::dds::compressor::BlockCompressor;
    use crate::dds::{DdsFormat, SoftwareCompressor};
    use image::RgbaImage;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn test_channel_capacity_constant() {
        assert_eq!(CHANNEL_CAPACITY, 32);
    }

    #[test]
    fn test_gpu_encoder_channel_connected() {
        let (tx, _rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        assert!(channel.is_connected());
    }

    #[test]
    fn test_gpu_encoder_channel_disconnected_when_rx_dropped() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        drop(rx);
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_gpu_encode_request_roundtrip() {
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let (resp_tx, resp_rx) = oneshot::channel();

        let image = RgbaImage::new(4, 4);
        let request = GpuEncodeRequest {
            image,
            format: DdsFormat::BC1,
            response: resp_tx,
        };

        tx.send(request).await.expect("send should succeed");

        let received = rx.recv().await.expect("should receive request");
        assert_eq!(received.format, DdsFormat::BC1);
        assert_eq!(received.image.width(), 4);
        assert_eq!(received.image.height(), 4);

        let mock_data = vec![0xDE, 0xAD];
        received
            .response
            .send(Ok(mock_data.clone()))
            .expect("response send should succeed");

        let result = resp_rx.await.expect("should receive response");
        assert_eq!(result.unwrap(), mock_data);
    }

    // =========================================================================
    // Task 2: BlockCompressor for GpuEncoderChannel
    // =========================================================================

    #[test]
    fn test_gpu_encoder_channel_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GpuEncoderChannel>();
    }

    #[test]
    fn test_block_compressor_name() {
        let (tx, _rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        assert_eq!(channel.name(), "gpu-channel");
    }

    #[tokio::test]
    async fn test_block_compressor_sends_request_and_receives_response() {
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);

        // Spawn a mock worker that receives and responds with SoftwareCompressor
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let compressor = SoftwareCompressor;
                let result = compressor.compress(&req.image, req.format);
                let _ = req.response.send(result);
            }
        });

        // Call compress from spawn_blocking (it uses blocking_send/blocking_recv)
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let data = result.expect("compress should succeed");
        // 1 block × 8 bytes for BC1
        assert_eq!(data.len(), 8);
    }

    #[tokio::test]
    async fn test_block_compressor_error_when_worker_dead() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);
        // Drop receiver immediately — worker is "dead"
        drop(rx);

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let err = result.expect_err("should fail when worker is dead");
        let msg = err.to_string();
        assert!(
            msg.contains("GPU worker"),
            "error should mention GPU worker, got: {msg}"
        );
    }

    // =========================================================================
    // Task 3: GPU Worker Task
    // =========================================================================

    #[tokio::test]
    async fn test_worker_processes_single_request() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, worker_handle) = create_gpu_encoder_channel(compressor);

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let data = result.expect("compress should succeed");
        assert_eq!(data.len(), 8); // 1 block × 8 bytes for BC1

        drop(worker_handle);
    }

    #[tokio::test]
    async fn test_worker_processes_multiple_sequential_requests() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, _worker_handle) = create_gpu_encoder_channel(compressor);
        let channel = Arc::new(channel);

        for i in 0..3 {
            let ch = Arc::clone(&channel);
            let result = tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress(&image, DdsFormat::BC1)
            })
            .await
            .expect("spawn_blocking should not panic");

            let data = result.unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
            assert_eq!(data.len(), 8);
        }
    }

    #[tokio::test]
    async fn test_worker_handles_concurrent_submissions() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, _worker_handle) = create_gpu_encoder_channel(compressor);
        let channel = Arc::new(channel);

        let mut handles = vec![];
        for _ in 0..6 {
            let ch = Arc::clone(&channel);
            handles.push(tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress(&image, DdsFormat::BC1)
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            let data = result.expect("concurrent compress should succeed");
            assert_eq!(data.len(), 8); // 1 BC1 block = 8 bytes
        }
    }

    #[tokio::test]
    async fn test_worker_stops_when_channel_closed() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, worker_handle) = create_gpu_encoder_channel(compressor);

        // Drop the sender side
        drop(channel);

        // Worker should complete
        tokio::time::timeout(std::time::Duration::from_secs(2), worker_handle)
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }

    #[tokio::test]
    async fn test_worker_handles_mixed_formats() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, _worker_handle) = create_gpu_encoder_channel(compressor);
        let channel = Arc::new(channel);

        // BC1: 8 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc1_result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress(&image, DdsFormat::BC1)
        })
        .await
        .unwrap();
        assert_eq!(bc1_result.unwrap().len(), 8);

        // BC3: 16 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc3_result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress(&image, DdsFormat::BC3)
        })
        .await
        .unwrap();
        assert_eq!(bc3_result.unwrap().len(), 16);
    }

    #[tokio::test]
    async fn test_create_gpu_encoder_channel_convenience() {
        let compressor: Arc<dyn BlockCompressor> = Arc::new(SoftwareCompressor);
        let (channel, worker_handle) = create_gpu_encoder_channel(compressor);

        // Verify channel is connected
        assert!(channel.is_connected());

        // Drop channel, verify worker stops
        drop(channel);
        tokio::time::timeout(std::time::Duration::from_secs(2), worker_handle)
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }
}
