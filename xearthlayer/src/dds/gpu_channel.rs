//! GPU encoder channel types for mpsc-based GPU compression.
//!
//! Provides the message types and channel handle for submitting
//! single mip-level images to a dedicated GPU worker thread.

#[cfg(feature = "gpu-encode")]
mod inner {
    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use image::RgbaImage;
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

    // =========================================================================
    // Pipeline overlap internals
    // =========================================================================

    /// Tracks a GPU submission whose results have not yet been read back.
    struct InFlightRequest {
        readback_buffer: wgpu::Buffer,
        output_size: u64,
        submission_index: wgpu::SubmissionIndex,
        response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
        /// Receives the result of `map_async` — `Ok(())` on success, or
        /// `Err(BufferAsyncError)` if the device was lost / mapping failed.
        map_result_rx: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    }

    /// Convert a [`DdsFormat`] to the corresponding `block_compression` variant
    /// and compressed block size in bytes.
    fn format_params(format: DdsFormat) -> (CompressionVariant, u32) {
        match format {
            DdsFormat::BC1 => (CompressionVariant::BC1, 8),
            DdsFormat::BC3 => (CompressionVariant::BC3, 16),
        }
    }

    /// Upload an image to the GPU, run a compression compute pass, and submit.
    ///
    /// Returns an [`InFlightRequest`] whose readback buffer will contain the
    /// compressed data once `device.poll(Wait)` completes.
    fn upload_and_submit(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        compressor: &mut GpuBlockCompressor,
        request: GpuEncodeRequest,
    ) -> InFlightRequest {
        let width = request.image.width();
        let height = request.image.height();
        let (variant, block_size) = format_params(request.format);
        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);
        let output_size = (blocks_wide * blocks_high * block_size) as u64;

        // Create GPU texture and upload pixel data
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pipeline-input"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let bytes_per_row = width * 4;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            request.image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Create output + readback buffers
        let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pipeline-output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pipeline-readback"),
            size: output_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        // Run compression compute pass
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pipeline-compress"),
        });

        compressor.add_compression_task(
            variant,
            &texture_view,
            width,
            height,
            &output_buffer,
            None,
            None,
        );

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pipeline-compute"),
                timestamp_writes: None,
            });
            compressor.compress(&mut pass);
        }

        // Copy compressed output to readback buffer
        encoder.copy_buffer_to_buffer(&output_buffer, 0, &readback_buffer, 0, output_size);

        // Submit — non-blocking, GPU starts executing immediately
        let submission_index = queue.submit(std::iter::once(encoder.finish()));

        // Initiate async buffer mapping (will be ready after device.poll).
        // Use a channel to propagate mapping errors instead of silently
        // ignoring them — accessing an unmapped buffer causes SIGBUS.
        let buffer_slice = readback_buffer.slice(..);
        let (map_tx, map_rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = map_tx.send(result);
        });

        InFlightRequest {
            readback_buffer,
            output_size,
            submission_index,
            response: request.response,
            map_result_rx: map_rx,
        }
    }

    /// Wait for a specific GPU submission to complete and send the result.
    fn complete_readback(device: &wgpu::Device, in_flight: InFlightRequest) {
        let result = (|| -> Result<Vec<u8>, DdsError> {
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(in_flight.submission_index),
                    timeout: Some(std::time::Duration::from_secs(10)),
                })
                .map_err(|e| DdsError::CompressionFailed(format!("GPU poll failed: {e}")))?;

            // Check that buffer mapping succeeded (callback fired during poll).
            // wgpu guarantees map_async callback fires during poll(), so recv()
            // cannot deadlock here — the sender always sends before poll returns.
            in_flight
                .map_result_rx
                .recv()
                .map_err(|_| {
                    DdsError::CompressionFailed("GPU map_async callback never fired".to_string())
                })?
                .map_err(|e| {
                    DdsError::CompressionFailed(format!("GPU buffer mapping failed: {e}"))
                })?;

            let buffer_slice = in_flight.readback_buffer.slice(..in_flight.output_size);
            let data = buffer_slice.get_mapped_range();
            let result = data.to_vec();
            drop(data);
            in_flight.readback_buffer.unmap();

            Ok(result)
        })();

        // Send response; ignore error if caller dropped their receiver
        let _ = in_flight.response.send(result);
    }

    /// Spawn the GPU pipeline worker on a dedicated OS thread.
    ///
    /// The worker receives compression requests via the channel and processes
    /// them using pipeline overlap: while the GPU compresses tile A, the CPU
    /// uploads tile B's data. This keeps the GPU constantly busy.
    ///
    /// Returns a `JoinHandle` that resolves when the channel is closed.
    pub fn spawn_gpu_worker(
        device: wgpu::Device,
        queue: wgpu::Queue,
        mut compressor: GpuBlockCompressor,
        mut rx: mpsc::Receiver<GpuEncodeRequest>,
    ) -> tokio::task::JoinHandle<()> {
        // Use spawn_blocking for a long-lived dedicated thread.
        // GpuBlockCompressor is !Send, so it must stay on one thread.
        tokio::task::spawn_blocking(move || {
            let mut in_flight: Option<InFlightRequest> = None;

            while let Some(request) = rx.blocking_recv() {
                let has_more = CHANNEL_CAPACITY - rx.capacity() > 0;

                // Wrap GPU work in catch_unwind so panics don't silently kill
                // the worker thread (callers would hang forever on blocking_recv).
                // AssertUnwindSafe is sound because we break on panic and never
                // reuse potentially corrupted GPU state.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tracing::trace!(
                        format = ?request.format,
                        width = request.image.width(),
                        height = request.image.height(),
                        pipeline_depth = if in_flight.is_some() { 2 } else { 1 },
                        has_more,
                        "GPU pipeline: uploading + submitting"
                    );

                    // Upload and submit the new request (non-blocking GPU submit)
                    let new_in_flight =
                        upload_and_submit(&device, &queue, &mut compressor, request);

                    // Complete the previous in-flight request (GPU already has new work queued)
                    if let Some(prev) = in_flight.take() {
                        complete_readback(&device, prev);
                    }

                    // If no more requests are queued, complete this one immediately.
                    // Otherwise, defer readback to overlap with the next upload.
                    if !has_more {
                        complete_readback(&device, new_in_flight);
                        in_flight = None;
                    } else {
                        in_flight = Some(new_in_flight);
                    }
                }));

                if let Err(panic_info) = result {
                    let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = panic_info.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic".to_string()
                    };

                    tracing::error!(error = %msg, "GPU pipeline worker panicked, shutting down");

                    // The current request's response_tx was moved into the closure
                    // and dropped on panic — caller gets RecvError which maps to
                    // "GPU worker dropped response channel". Handle any in-flight
                    // request from the previous iteration:
                    if let Some(prev) = in_flight.take() {
                        let _ = prev.response.send(Err(DdsError::CompressionFailed(format!(
                            "GPU worker panicked: {msg}"
                        ))));
                    }

                    // Drain remaining queued requests so callers don't hang
                    while let Ok(req) = rx.try_recv() {
                        let _ = req.response.send(Err(DdsError::CompressionFailed(
                            "GPU worker terminated after panic".to_string(),
                        )));
                    }

                    break;
                }
            }

            // Drain any remaining in-flight request
            if let Some(prev) = in_flight {
                complete_readback(&device, prev);
            }

            tracing::info!("GPU pipeline worker shutting down (channel closed)");
        })
    }

    /// Create a [`GpuEncoderChannel`] with a pipeline overlap worker.
    ///
    /// Initializes GPU resources and spawns the worker thread. Returns the
    /// channel handle and worker `JoinHandle`.
    ///
    /// # Errors
    ///
    /// Returns `DdsError` if GPU initialization fails.
    pub fn create_gpu_encoder_channel(
        gpu_device: &str,
    ) -> Result<(GpuEncoderChannel, tokio::task::JoinHandle<()>), DdsError> {
        let (device, queue, compressor, adapter_name) =
            crate::dds::create_gpu_resources(gpu_device)?;

        tracing::info!(adapter = %adapter_name, "GPU pipeline worker starting");

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = spawn_gpu_worker(device, queue, compressor, rx);
        Ok((GpuEncoderChannel::new(tx), handle))
    }
}

#[cfg(feature = "gpu-encode")]
pub use inner::*;

#[cfg(test)]
#[cfg(feature = "gpu-encode")]
mod tests {
    use super::*;
    use crate::dds::compressor::BlockCompressor;
    use crate::dds::{DdsError, DdsFormat, SoftwareCompressor};
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
    // Mock worker tests (SoftwareCompressor, no GPU required)
    // =========================================================================

    /// Spawn a mock worker using SoftwareCompressor for non-GPU tests.
    fn spawn_mock_worker(
        rx: &mut Option<mpsc::Receiver<GpuEncodeRequest>>,
    ) -> tokio::task::JoinHandle<()> {
        let rx = rx.take().expect("receiver already consumed");
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(req) = rx.recv().await {
                let compressor = SoftwareCompressor;
                let result = compressor.compress(&req.image, req.format);
                let _ = req.response.send(result);
            }
        })
    }

    fn mock_channel() -> (GpuEncoderChannel, Option<mpsc::Receiver<GpuEncodeRequest>>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (GpuEncoderChannel::new(tx), Some(rx))
    }

    #[tokio::test]
    async fn test_mock_worker_processes_single_request() {
        let (channel, mut rx) = mock_channel();
        let _worker = spawn_mock_worker(&mut rx);

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let data = result.expect("compress should succeed");
        assert_eq!(data.len(), 8); // 1 block × 8 bytes for BC1
    }

    #[tokio::test]
    async fn test_mock_worker_processes_multiple_sequential_requests() {
        let (channel, mut rx) = mock_channel();
        let _worker = spawn_mock_worker(&mut rx);
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
    async fn test_mock_worker_handles_concurrent_submissions() {
        let (channel, mut rx) = mock_channel();
        let _worker = spawn_mock_worker(&mut rx);
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
    async fn test_mock_worker_stops_when_channel_closed() {
        let (channel, mut rx) = mock_channel();
        let worker = spawn_mock_worker(&mut rx);

        // Drop the sender side
        drop(channel);

        // Worker should complete
        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }

    #[tokio::test]
    async fn test_mock_worker_handles_mixed_formats() {
        let (channel, mut rx) = mock_channel();
        let _worker = spawn_mock_worker(&mut rx);
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
    #[ignore] // Requires GPU hardware
    async fn test_create_gpu_encoder_channel_convenience() {
        let (channel, worker_handle) = match create_gpu_encoder_channel("integrated") {
            Ok(r) => r,
            Err(_) => return,
        };

        // Verify channel is connected
        assert!(channel.is_connected());

        // Drop channel, verify worker stops
        drop(channel);
        tokio::time::timeout(std::time::Duration::from_secs(2), worker_handle)
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }

    // =========================================================================
    // Pipeline overlap tests (require GPU hardware)
    // =========================================================================

    /// Helper to create GPU resources for pipeline tests.
    fn gpu_resources_or_skip() -> Option<(
        wgpu::Device,
        wgpu::Queue,
        block_compression::GpuBlockCompressor,
    )> {
        use crate::dds::create_gpu_resources;
        match create_gpu_resources("integrated") {
            Ok((device, queue, compressor, _name)) => Some((device, queue, compressor)),
            Err(_) => None, // Skip if no GPU
        }
    }

    /// Single request through the pipeline worker with overlap loop.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_single_request() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(device, queue, compressor, rx);

        let channel = GpuEncoderChannel::new(tx);
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .unwrap();

        let data = result.expect("pipeline compress should succeed");
        // 1 block × 8 bytes for BC1
        assert_eq!(data.len(), 8);
    }

    /// Multiple sequential requests exercise the pipeline overlap path.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_sequential_requests() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(device, queue, compressor, rx);

        let channel = Arc::new(GpuEncoderChannel::new(tx));

        for i in 0..3 {
            let ch = Arc::clone(&channel);
            let result = tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress(&image, DdsFormat::BC1)
            })
            .await
            .unwrap();

            let data = result.unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
            assert_eq!(data.len(), 8);
        }
    }

    /// Concurrent submissions from multiple threads exercise backpressure.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_concurrent_submissions() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(device, queue, compressor, rx);

        let channel = Arc::new(GpuEncoderChannel::new(tx));
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
            let data = result.expect("concurrent pipeline compress should succeed");
            assert_eq!(data.len(), 8);
        }
    }

    /// Mixed BC1 and BC3 formats through the pipeline.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_mixed_formats() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(device, queue, compressor, rx);

        let channel = Arc::new(GpuEncoderChannel::new(tx));

        // BC1: 8 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc1 = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress(&image, DdsFormat::BC1)
        })
        .await
        .unwrap();
        assert_eq!(bc1.unwrap().len(), 8);

        // BC3: 16 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc3 = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress(&image, DdsFormat::BC3)
        })
        .await
        .unwrap();
        assert_eq!(bc3.unwrap().len(), 16);
    }

    /// Worker shuts down cleanly when channel is closed.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_worker_shutdown() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let worker = spawn_gpu_worker(device, queue, compressor, rx);

        // Drop sender to close channel
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }

    /// Worker panic sends error to caller instead of hanging forever.
    #[tokio::test]
    async fn test_gpu_worker_panic_does_not_hang_caller() {
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);

        // Mock worker that panics on first request (simulating GPU internal panic)
        tokio::task::spawn_blocking(move || {
            if let Some(req) = rx.blocking_recv() {
                // Simulate catch_unwind behavior: send error, don't panic the whole thread
                let _ = req.response.send(Err(DdsError::CompressionFailed(
                    "GPU worker panicked: simulated panic".to_string(),
                )));
            }
        });

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let err = result.expect_err("should fail with worker panic error");
        assert!(
            err.to_string().contains("panic"),
            "error should mention panic, got: {err}"
        );
    }

    /// Mapping error is propagated through the channel (mock worker, no GPU needed).
    #[tokio::test]
    async fn test_gpu_worker_mapping_error_propagates() {
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = GpuEncoderChannel::new(tx);

        // Mock worker that always sends back an error (simulating map failure)
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.response.send(Err(DdsError::CompressionFailed(
                    "GPU buffer mapping failed: simulated".to_string(),
                )));
            }
        });

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let err = result.expect_err("should fail with mapping error");
        assert!(
            err.to_string().contains("mapping failed"),
            "error should mention mapping failure, got: {err}"
        );
    }

    /// Full pipeline with 4096×4096 tile (realistic workload).
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_full_tile_4096x4096() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(device, queue, compressor, rx);

        let channel = GpuEncoderChannel::new(tx);
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4096, 4096);
            channel.compress(&image, DdsFormat::BC1)
        })
        .await
        .unwrap();

        let data = result.expect("GPU compression should succeed");
        // BC1: 1024×1024 blocks × 8 bytes = 8388608 bytes
        assert_eq!(data.len(), 8_388_608);
    }
}
