//! GPU encoder channel for mpsc-based GPU compression.
//!
//! Provides the message types and channel handle for submitting
//! full tile requests to a dedicated GPU worker thread. The worker
//! owns mipmap iteration internally, reducing cross-thread round-trips.

mod inner {
    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use image::RgbaImage;
    use tokio::sync::{mpsc, oneshot};

    use crate::dds::compressor::MipmapCompressor;
    use crate::dds::{DdsError, DdsFormat};

    /// Bounded channel capacity for GPU encode requests.
    pub const CHANNEL_CAPACITY: usize = 32;

    /// Message sent from callers to the GPU worker for compression.
    pub struct GpuEncodeRequest {
        /// Source RGBA image (moved, not cloned).
        pub image: RgbaImage,
        /// The target block compression format.
        pub format: DdsFormat,
        /// Number of mipmap levels to generate (including original).
        pub mipmap_count: usize,
        /// One-shot channel for returning the compressed data (or error).
        pub response: oneshot::Sender<Result<Vec<u8>, DdsError>>,
    }

    /// Sender-side handle for submitting GPU encode requests.
    ///
    /// Implements [`MipmapCompressor`] — receives the full source image and
    /// returns compressed data for all mipmap levels. The GPU worker owns
    /// the mipmap iteration internally, eliminating per-level channel round-trips.
    pub struct GpuEncoderChannel {
        sender: Option<mpsc::Sender<GpuEncodeRequest>>,
    }

    impl GpuEncoderChannel {
        /// Create a new channel handle wrapping the given sender.
        pub fn new(sender: mpsc::Sender<GpuEncodeRequest>) -> Self {
            Self {
                sender: Some(sender),
            }
        }

        /// Returns `true` if the receiver end is still alive.
        pub fn is_connected(&self) -> bool {
            self.sender
                .as_ref()
                .map(|s| !s.is_closed())
                .unwrap_or(false)
        }

        /// Shut down the GPU pipeline by closing the channel.
        ///
        /// This causes the worker's `blocking_recv()` to return `None`,
        /// triggering clean GPU resource release. Required because
        /// `spawn_blocking` threads are not cancelled by tokio runtime
        /// shutdown — the channel must be explicitly closed.
        pub fn shutdown(&mut self) {
            self.sender.take();
        }
    }

    impl MipmapCompressor for GpuEncoderChannel {
        fn compress_mipmap_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
        ) -> Result<Vec<u8>, DdsError> {
            let sender = self.sender.as_ref().ok_or_else(|| {
                DdsError::CompressionFailed("GPU worker has been shut down".to_string())
            })?;

            let (resp_tx, resp_rx) = oneshot::channel();
            let request = GpuEncodeRequest {
                image, // moved, zero clones
                format,
                mipmap_count,
                response: resp_tx,
            };

            sender.blocking_send(request).map_err(|_| {
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
    // Full-tile processing internals
    // =========================================================================

    /// Convert a [`DdsFormat`] to the corresponding `block_compression` variant
    /// and compressed block size in bytes.
    fn format_params(format: DdsFormat) -> (CompressionVariant, u32) {
        match format {
            DdsFormat::BC1 => (CompressionVariant::BC1, 8),
            DdsFormat::BC3 => (CompressionVariant::BC3, 16),
        }
    }

    /// Compressed output size in bytes for a given image at the specified format.
    fn compressed_size(width: u32, height: u32, block_size: u32) -> u64 {
        let blocks_wide = width.div_ceil(4);
        let blocks_high = height.div_ceil(4);
        (blocks_wide * blocks_high * block_size) as u64
    }

    /// Process a full tile: generate mipmaps, compress each level on GPU,
    /// return concatenated compressed output.
    ///
    /// The worker owns the mipmap iteration via [`MipmapStream`]. GPU output
    /// and readback buffers are pre-allocated for level 0 (largest) and reused
    /// for smaller levels.
    fn process_tile(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        compressor: &mut GpuBlockCompressor,
        image: RgbaImage,
        format: DdsFormat,
        mipmap_count: usize,
    ) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();
        let (variant, block_size) = format_params(format);

        // Pre-allocate output and readback buffers for level 0 (largest).
        // Smaller levels reuse the same buffers.
        let max_output_size = compressed_size(width, height, block_size);

        let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tile-output"),
            size: max_output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tile-readback"),
            size: max_output_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let stream = crate::dds::mipmap::MipmapStream::new(image, mipmap_count);
        let mut result = Vec::new();

        for (level_idx, level) in stream.enumerate() {
            let lw = level.width();
            let lh = level.height();
            let level_output_size = compressed_size(lw, lh, block_size);

            // Create texture for this level (textures are immutable in size)
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tile-input"),
                size: wgpu::Extent3d {
                    width: lw,
                    height: lh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Upload pixel data
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                level.as_raw(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(lw * 4),
                    rows_per_image: Some(lh),
                },
                wgpu::Extent3d {
                    width: lw,
                    height: lh,
                    depth_or_array_layers: 1,
                },
            );

            let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            // Dispatch compute pass
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tile-compress"),
            });

            compressor.add_compression_task(
                variant,
                &texture_view,
                lw,
                lh,
                &output_buffer,
                None,
                None,
            );

            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("tile-compute"),
                    timestamp_writes: None,
                });
                compressor.compress(&mut pass);
            }

            encoder.copy_buffer_to_buffer(
                &output_buffer,
                0,
                &readback_buffer,
                0,
                level_output_size,
            );

            let sub_idx = queue.submit(std::iter::once(encoder.finish()));

            // Map readback buffer and wait for completion
            let buffer_slice = readback_buffer.slice(..level_output_size);
            let (map_tx, map_rx) = std::sync::mpsc::channel();
            buffer_slice.map_async(wgpu::MapMode::Read, move |r| {
                let _ = map_tx.send(r);
            });

            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(sub_idx),
                    timeout: Some(std::time::Duration::from_secs(10)),
                })
                .map_err(|e| DdsError::CompressionFailed(format!("GPU poll failed: {e}")))?;

            map_rx
                .recv()
                .map_err(|_| DdsError::CompressionFailed("map_async callback never fired".into()))?
                .map_err(|e| DdsError::CompressionFailed(format!("buffer mapping failed: {e}")))?;

            let data = buffer_slice.get_mapped_range();
            result.extend_from_slice(&data);
            drop(data);
            readback_buffer.unmap();

            tracing::debug!(
                level = level_idx,
                width = lw,
                height = lh,
                compressed_bytes = level_output_size,
                "GPU compressed mipmap level"
            );

            // `level` dropped here — CPU memory reclaimed before next downsample
        }

        Ok(result)
    }

    /// Spawn the GPU pipeline worker on a dedicated OS thread.
    ///
    /// The worker receives full tile requests via the channel, generates
    /// mipmaps internally, and compresses each level on the GPU. One message
    /// in, one result out — no per-level channel round-trips.
    ///
    /// Returns a `JoinHandle` that resolves when the channel is closed or
    /// the shutdown token is cancelled.
    pub fn spawn_gpu_worker(
        device: wgpu::Device,
        queue: wgpu::Queue,
        mut compressor: GpuBlockCompressor,
        mut rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        // Use spawn_blocking for a long-lived dedicated thread.
        // GpuBlockCompressor is !Send, so it must stay on one thread.
        tokio::task::spawn_blocking(move || {
            loop {
                if shutdown.is_cancelled() {
                    tracing::info!("GPU pipeline worker: shutdown signal received");
                    break;
                }

                let request = match rx.try_recv() {
                    Ok(req) => req,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                };

                let response = request.response;

                tracing::debug!(
                    format = ?request.format,
                    width = request.image.width(),
                    height = request.image.height(),
                    mipmap_count = request.mipmap_count,
                    "GPU pipeline: processing full tile"
                );

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    process_tile(
                        &device,
                        &queue,
                        &mut compressor,
                        request.image,
                        request.format,
                        request.mipmap_count,
                    )
                }));

                match result {
                    Ok(tile_result) => {
                        let _ = response.send(tile_result);
                    }
                    Err(panic_info) => {
                        let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = panic_info.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };

                        tracing::error!(
                            error = %msg,
                            "GPU pipeline worker panicked, shutting down"
                        );

                        let _ = response.send(Err(DdsError::CompressionFailed(format!(
                            "GPU worker panicked: {msg}"
                        ))));

                        // Drain remaining queued requests
                        while let Ok(req) = rx.try_recv() {
                            let _ = req.response.send(Err(DdsError::CompressionFailed(
                                "GPU worker terminated after panic".to_string(),
                            )));
                        }

                        break;
                    }
                }
            }

            tracing::info!("GPU pipeline worker shutting down");
        })
    }

    /// Create a [`GpuEncoderChannel`] with a full-tile streaming worker.
    ///
    /// Initializes GPU resources and spawns the worker thread. Returns the
    /// channel handle and worker `JoinHandle`.
    ///
    /// # Errors
    ///
    /// Returns `DdsError` if GPU initialization fails.
    pub fn create_gpu_encoder_channel(
        gpu_device: &str,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(GpuEncoderChannel, tokio::task::JoinHandle<()>), DdsError> {
        let (device, queue, compressor, adapter_name) =
            crate::dds::create_gpu_resources(gpu_device)?;

        tracing::info!(adapter = %adapter_name, "GPU pipeline worker starting");

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = spawn_gpu_worker(device, queue, compressor, rx, shutdown);
        Ok((GpuEncoderChannel::new(tx), handle))
    }
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::compressor::{ImageCompressor, MipmapCompressor};
    use crate::dds::{DdsError, DdsFormat, MipmapStream, SoftwareCompressor};
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
            mipmap_count: 1,
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
    // Task 2: MipmapCompressor for GpuEncoderChannel
    // =========================================================================

    #[test]
    fn test_gpu_encoder_channel_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GpuEncoderChannel>();
    }

    #[test]
    fn test_mipmap_compressor_name() {
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
                let result: Result<Vec<u8>, crate::dds::DdsError> = MipmapStream::new(
                    req.image,
                    req.mipmap_count,
                )
                .try_fold(Vec::new(), |mut acc, level| {
                    compressor.compress(&level, req.format).map(|data| {
                        acc.extend_from_slice(&data);
                        acc
                    })
                });
                let _ = req.response.send(result);
            }
        });

        // Call compress_mipmap_chain from spawn_blocking (it uses blocking_send/blocking_recv)
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
                let result: Result<Vec<u8>, crate::dds::DdsError> = MipmapStream::new(
                    req.image,
                    req.mipmap_count,
                )
                .try_fold(Vec::new(), |mut acc, level| {
                    compressor.compress(&level, req.format).map(|data| {
                        acc.extend_from_slice(&data);
                        acc
                    })
                });
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
            ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();
        assert_eq!(bc1_result.unwrap().len(), 8);

        // BC3: 16 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc3_result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress_mipmap_chain(image, DdsFormat::BC3, 1)
        })
        .await
        .unwrap();
        assert_eq!(bc3_result.unwrap().len(), 16);
    }

    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_create_gpu_encoder_channel_convenience() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channel, worker_handle) = match create_gpu_encoder_channel("integrated", shutdown) {
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
    // Full-tile GPU pipeline tests (require GPU hardware)
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
        let _worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

        let channel = GpuEncoderChannel::new(tx);
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();

        let data = result.expect("pipeline compress should succeed");
        // 1 block × 8 bytes for BC1
        assert_eq!(data.len(), 8);
    }

    /// Multiple sequential requests exercise the full-tile streaming path.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_sequential_requests() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };

        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let _worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

        let channel = Arc::new(GpuEncoderChannel::new(tx));

        for i in 0..3 {
            let ch = Arc::clone(&channel);
            let result = tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
        let _worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

        let channel = Arc::new(GpuEncoderChannel::new(tx));
        let mut handles = vec![];

        for _ in 0..6 {
            let ch = Arc::clone(&channel);
            handles.push(tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
        let _worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

        let channel = Arc::new(GpuEncoderChannel::new(tx));

        // BC1: 8 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc1 = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();
        assert_eq!(bc1.unwrap().len(), 8);

        // BC3: 16 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc3 = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress_mipmap_chain(image, DdsFormat::BC3, 1)
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
        let worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
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
        let _worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );

        let channel = GpuEncoderChannel::new(tx);
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4096, 4096);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();

        let data = result.expect("GPU compression should succeed");
        // BC1: 1024×1024 blocks × 8 bytes = 8388608 bytes
        assert_eq!(data.len(), 8_388_608);
    }
}
