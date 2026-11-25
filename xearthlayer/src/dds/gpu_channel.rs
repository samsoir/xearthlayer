//! GPU encoder channel for mpsc-based GPU compression.
//!
//! Provides the message types and channel handle for submitting
//! full tile requests to a dedicated GPU worker thread. The worker
//! owns mipmap iteration internally, reducing cross-thread round-trips.
//!
//! The worker is split in two (#419): the loop ([`spawn_tile_worker`]:
//! receive, shutdown, `catch_unwind`, drain, reply, the loop's own timing)
//! is generic over a [`TileProcessor`], which does one tile's work. The
//! production processor is [`WgpuTileProcessor`]; tests drive the real loop
//! with processors that panic, sleep, fail or record order, so the loop is
//! covered without a GPU.

mod inner {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use image::RgbaImage;
    use tokio::sync::mpsc::error::TrySendError;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::dds::compressor::MipmapCompressor;
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::layout;
    use crate::dds::{DdsError, DdsFormat, IspcCompressor};

    /// Bounded channel capacity for GPU encode requests.
    pub const CHANNEL_CAPACITY: usize = 32;

    /// What a tile does when the worker's queue has no room.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum WhenBusy {
        /// Wait for room (`texture.compressor = gpu`); a stopped worker is
        /// an error.
        Wait,
        /// Encode with ISPC on the calling thread instead
        /// (`texture.compressor = hybrid`), also when the worker stopped.
        EncodeWithIspc,
    }

    /// How long an idle worker sleeps between looks at its channel and its
    /// shutdown token.
    const IDLE_POLL: Duration = Duration::from_millis(10);

    /// Wall time of each stage of one tile on the GPU path.
    ///
    /// The worker loop fills [`queue_wait`](Self::queue_wait) and
    /// [`process`](Self::process); the [`TileProcessor`] fills the stages
    /// inside its call, each summed over the tile's mip levels. The GPU
    /// figure is wall time from recording to the end of the poll, not a GPU
    /// timestamp query, so it includes the driver.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct GpuTileTiming {
        /// From the caller starting its send (time blocked on a full channel
        /// included) to the worker taking the request. For a lone tile this
        /// is mostly the idle worker's poll: up to `IDLE_POLL` (10 ms).
        pub queue_wait: Duration,
        /// CPU mip generation ([`MipmapStream`](crate::dds::MipmapStream))
        /// and the padding of the sub-block tail. The stream works one level
        /// ahead: taking level `i` computes level `i + 1`.
        pub downsample: Duration,
        /// Creating the tile's output and readback buffers and each level's
        /// texture, and `queue.write_texture`, which copies the level into a
        /// wgpu staging buffer.
        pub upload: Duration,
        /// Command recording, submit and the blocking poll: GPU and driver.
        pub gpu: Duration,
        /// Waiting for the buffer map, and copying the mapped range out into
        /// the tile's result `Vec`, its growth (reallocation) included.
        pub readback: Duration,
        /// The processor's whole call, as the loop timed it.
        pub process: Duration,
    }

    /// Message sent from callers to the GPU worker for compression.
    pub struct GpuEncodeRequest {
        /// Source RGBA image (moved, not cloned).
        pub image: RgbaImage,
        /// The target block compression format.
        pub format: DdsFormat,
        /// Number of mipmap levels to generate (including original).
        pub mipmap_count: usize,
        /// When the caller started to send: the start of the queue wait.
        pub sent_at: Instant,
        /// One-shot channel for returning the compressed data (or error).
        pub response: oneshot::Sender<GpuEncodeReply>,
    }

    /// The worker's answer to one [`GpuEncodeRequest`].
    #[derive(Debug)]
    pub struct GpuEncodeReply {
        /// The compressed chain, every level back to back, or why not.
        pub result: Result<Vec<u8>, DdsError>,
        /// The stages as far as the tile got; all zero for a request the
        /// worker drained without processing.
        pub timing: GpuTileTiming,
    }

    impl GpuEncodeReply {
        fn failed(message: impl Into<String>, timing: GpuTileTiming) -> Self {
            Self {
                result: Err(DdsError::CompressionFailed(message.into())),
                timing,
            }
        }
    }

    /// One tile's work for the worker loop: the full mip chain of `image`,
    /// compressed level by level and concatenated, largest first.
    ///
    /// Implementations fill the stages of `timing` they perform; the loop
    /// owns `queue_wait` and `process`. A panic is caught by the loop (see
    /// [`spawn_tile_worker`]).
    pub trait TileProcessor: Send {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError>;
    }

    /// Sender-side handle for submitting GPU encode requests.
    ///
    /// Implements [`MipmapCompressor`] — receives the full source image and
    /// returns compressed data for all mipmap levels. The GPU worker owns
    /// the mipmap iteration internally, eliminating per-level channel round-trips.
    pub struct GpuEncoderChannel {
        sender: Option<mpsc::Sender<GpuEncodeRequest>>,
        when_busy: WhenBusy,
        stats: Arc<GpuEncodeStats>,
    }

    impl GpuEncoderChannel {
        /// Create a new channel handle wrapping the given sender. A full
        /// channel blocks the caller until the worker takes a request
        /// ([`WhenBusy::Wait`]).
        pub fn new(sender: mpsc::Sender<GpuEncodeRequest>) -> Self {
            Self {
                sender: Some(sender),
                when_busy: WhenBusy::Wait,
                stats: Arc::default(),
            }
        }

        /// A handle following `when_busy` and the receiver for its worker,
        /// on the queue that goes with the policy: [`CHANNEL_CAPACITY`] to
        /// wait in, or one request to overflow from.
        ///
        /// With [`WhenBusy::EncodeWithIspc`] a tile never waits for room in
        /// the queue: one that finds it full, or the worker stopped, is
        /// encoded with ISPC on the calling thread. One tile waits besides
        /// the one the worker encodes; a deeper queue would only add queue
        /// wait, as ISPC on one thread is about as fast as the worker. A
        /// tile the worker accepts is still waited for. The calling thread
        /// is a tile task's, which holds a CPU-pool permit, so the CPU pool
        /// stays the only limit on concurrent encodes. ISPC and the GPU
        /// kernel give different bytes for the same tile; both are valid
        /// BC1.
        pub fn with_policy(when_busy: WhenBusy) -> (Self, mpsc::Receiver<GpuEncodeRequest>) {
            let capacity = match when_busy {
                WhenBusy::Wait => CHANNEL_CAPACITY,
                WhenBusy::EncodeWithIspc => 1,
            };
            let (tx, rx) = mpsc::channel(capacity);
            let channel = Self {
                when_busy,
                ..Self::new(tx)
            };
            (channel, rx)
        }

        /// The sender, for tests that put a request in the queue themselves.
        #[cfg(test)]
        pub(crate) fn sender(&self) -> &mpsc::Sender<GpuEncodeRequest> {
            self.sender.as_ref().expect("not shut down")
        }

        /// The stage timings of the tiles answered through this channel,
        /// the requests that failed, and its queue depth. Someone must take
        /// its samples ([`GpuEncodeStats::take_sample`]); in the service the
        /// per-minute log does.
        pub fn stats(&self) -> &Arc<GpuEncodeStats> {
            &self.stats
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
        /// Once the requests already buffered are served, the worker's
        /// `try_recv()` reports the channel disconnected and the loop ends,
        /// releasing the GPU resources. Required because
        /// `spawn_blocking` threads are not cancelled by tokio runtime
        /// shutdown — the channel must be explicitly closed.
        pub fn shutdown(&mut self) {
            self.sender.take();
        }

        /// Encodes a tile the worker could not take with ISPC, on this
        /// thread: the chain `texture.compressor = ispc` writes. Counted as
        /// overflowed when encoded, as failed otherwise.
        fn encode_overflow(&self, request: GpuEncodeRequest) -> Result<Vec<u8>, DdsError> {
            let _span = tracing::debug_span!(target: "profiling", "gpu_overflow_ispc").entered();
            let result = crate::dds::encoder::compress_chain(
                &IspcCompressor,
                request.image,
                request.format,
                request.mipmap_count,
            );
            if result.is_ok() {
                self.stats.record_overflowed();
            } else {
                self.stats.record_failed();
            }
            result
        }
    }

    impl MipmapCompressor for GpuEncoderChannel {
        fn compress_mipmap_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
        ) -> Result<Vec<u8>, DdsError> {
            // Refused on the caller's side: the worker never sees it.
            format.ensure_supported()?;
            let Some(sender) = self.sender.as_ref() else {
                self.stats.record_failed();
                return Err(DdsError::CompressionFailed(
                    "GPU worker has been shut down".to_string(),
                ));
            };

            let (width, height) = image.dimensions();
            let _span = tracing::debug_span!(
                target: "profiling",
                "gpu_encode",
                width,
                height,
                levels = mipmap_count,
            )
            .entered();

            let sent_at = Instant::now();
            let (resp_tx, resp_rx) = oneshot::channel();
            let request = GpuEncodeRequest {
                image, // moved, zero clones
                format,
                mipmap_count,
                sent_at,
                response: resp_tx,
            };

            // In the queue depth until the reply or the failure is back:
            // from before a send that waits for room (it blocks on a full
            // channel), from acceptance for one that overflows, so a tile
            // handed to ISPC is never counted.
            let in_channel;
            let sent = match self.when_busy {
                WhenBusy::Wait => {
                    in_channel = self.stats.enter();
                    sender.blocking_send(request).map_err(|_| {
                        DdsError::CompressionFailed("GPU worker is not running".to_string())
                    })
                }
                WhenBusy::EncodeWithIspc => match sender.try_send(request) {
                    Ok(()) => {
                        in_channel = self.stats.enter();
                        Ok(())
                    }
                    Err(TrySendError::Full(request) | TrySendError::Closed(request)) => {
                        return self.encode_overflow(request);
                    }
                },
            };
            let reply = sent.and_then(|()| {
                resp_rx.blocking_recv().map_err(|_| {
                    DdsError::CompressionFailed("GPU worker dropped response channel".to_string())
                })
            });
            let total = sent_at.elapsed();
            drop(in_channel);
            let reply = match reply {
                Ok(reply) => reply,
                Err(e) => {
                    self.stats.record_failed();
                    return Err(e);
                }
            };

            let t = &reply.timing;
            let encoded = reply.result.is_ok();
            if encoded {
                self.stats.record_encoded(t, total);
            } else {
                self.stats.record_failed();
            }
            let us = |d: Duration| d.as_micros() as u64;
            tracing::debug!(
                width,
                height,
                levels = mipmap_count,
                encoded,
                queue_wait_us = us(t.queue_wait),
                downsample_us = us(t.downsample),
                upload_us = us(t.upload),
                gpu_us = us(t.gpu),
                readback_us = us(t.readback),
                process_us = us(t.process),
                total_us = us(total),
                "GPU encode: tile stages"
            );
            reply.result
        }

        fn name(&self) -> &str {
            "gpu-channel"
        }
    }

    // =========================================================================
    // Full-tile processing internals
    // =========================================================================

    /// The `block_compression` variant for a supported [`DdsFormat`];
    /// [`DdsError::UnsupportedFormat`] otherwise. `compress_mipmap_chain`
    /// refuses an unsupported format before sending, so the worker only
    /// reaches the error if called directly.
    fn compression_variant(format: DdsFormat) -> Result<CompressionVariant, DdsError> {
        format.ensure_supported()?;
        match format {
            DdsFormat::BC1 => Ok(CompressionVariant::BC1),
            other => Err(DdsError::UnsupportedFormat(other.to_string())),
        }
    }

    /// Compressed bytes of one `width` x `height` level, as a GPU buffer size.
    fn compressed_size(width: u32, height: u32, format: DdsFormat) -> u64 {
        layout::level_len(width, height, format) as u64
    }

    /// Runs `f`, adding its wall time to `stage` inside `span` (target
    /// `profiling`, so `run --profile` shows it).
    fn timed<T>(stage: &mut Duration, span: tracing::Span, f: impl FnOnce() -> T) -> T {
        let _entered = span.entered();
        let start = Instant::now();
        let out = f();
        *stage += start.elapsed();
        out
    }

    /// Process a full tile: generate mipmaps, compress each level on GPU,
    /// return concatenated compressed output.
    ///
    /// The worker owns the mipmap iteration via [`MipmapStream`]. GPU output
    /// and readback buffers are pre-allocated for level 0 (largest) and reused
    /// for smaller levels. Each stage's wall time is added to `timing`.
    ///
    /// [`MipmapStream`]: crate::dds::MipmapStream
    fn process_tile(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        compressor: &mut GpuBlockCompressor,
        image: RgbaImage,
        format: DdsFormat,
        mipmap_count: usize,
        timing: &mut GpuTileTiming,
    ) -> Result<Vec<u8>, DdsError> {
        let width = image.width();
        let height = image.height();
        let variant = compression_variant(format)?;

        // Pre-allocate output and readback buffers for level 0 (largest).
        // Smaller levels reuse the same buffers.
        let max_output_size = compressed_size(width, height, format);

        let (output_buffer, readback_buffer) = timed(
            &mut timing.upload,
            tracing::debug_span!(target: "profiling", "gpu_buffers"),
            || {
                let output = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tile-output"),
                    size: max_output_size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tile-readback"),
                    size: max_output_size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                (output, readback)
            },
        );

        let mut stream = crate::dds::mipmap::MipmapStream::new(image, mipmap_count);
        let mut result = Vec::new();

        // At most `mipmap_count` takes, so a full chain makes no extra call
        // that only finds the stream's end (one is made, and spanned, only
        // when the count asks for more levels than the image has).
        for level_idx in 0..mipmap_count {
            // The stream works one level ahead: taking level `i` computes
            // level `i + 1`, which this span names as `computes_level`. The
            // last take computes nothing, and its span is near zero.
            let downsample = tracing::debug_span!(
                target: "profiling",
                "gpu_downsample",
                computes_level = level_idx + 1
            )
            .entered();
            let started = Instant::now();
            let Some(level) = stream.next() else {
                break;
            };
            // The GPU kernel asserts both dimensions are multiples of 4, so the
            // 2×2 and 1×1 tail of a full mipmap chain must be padded up to one
            // whole block first. Without this the worker panics and shuts down
            // for the rest of the process lifetime.
            let level = crate::dds::compressor::pad_to_block_multiple(&level);
            timing.downsample += started.elapsed();
            drop(downsample);

            let lw = level.width();
            let lh = level.height();
            let level_output_size = compressed_size(lw, lh, format);

            let texture_view = timed(
                &mut timing.upload,
                tracing::debug_span!(target: "profiling", "gpu_upload", level = level_idx),
                || {
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

                    texture.create_view(&wgpu::TextureViewDescriptor::default())
                },
            );

            let buffer_slice = readback_buffer.slice(..level_output_size);
            let map_rx = timed(
                &mut timing.gpu,
                tracing::debug_span!(target: "profiling", "gpu_submit_poll", level = level_idx),
                || -> Result<_, DdsError> {
                    // Dispatch compute pass
                    let mut encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
                    let (map_tx, map_rx) = std::sync::mpsc::channel();
                    buffer_slice.map_async(wgpu::MapMode::Read, move |r| {
                        let _ = map_tx.send(r);
                    });

                    device
                        .poll(wgpu::PollType::Wait {
                            submission_index: Some(sub_idx),
                            timeout: Some(std::time::Duration::from_secs(10)),
                        })
                        .map_err(|e| {
                            DdsError::CompressionFailed(format!("GPU poll failed: {e}"))
                        })?;
                    Ok(map_rx)
                },
            )?;

            timed(
                &mut timing.readback,
                tracing::debug_span!(target: "profiling", "gpu_readback", level = level_idx),
                || -> Result<(), DdsError> {
                    map_rx
                        .recv()
                        .map_err(|_| {
                            DdsError::CompressionFailed("map_async callback never fired".into())
                        })?
                        .map_err(|e| {
                            DdsError::CompressionFailed(format!("buffer mapping failed: {e}"))
                        })?;

                    let data = buffer_slice.get_mapped_range().map_err(|e| {
                        DdsError::CompressionFailed(format!("mapped range unavailable: {e}"))
                    })?;
                    result.extend_from_slice(&data);
                    drop(data);
                    readback_buffer.unmap();
                    Ok(())
                },
            )?;

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

    /// The production [`TileProcessor`]: the GPU device, its queue and the
    /// `block_compression` kernels, owned by the worker thread.
    pub struct WgpuTileProcessor {
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: GpuBlockCompressor,
    }

    impl WgpuTileProcessor {
        pub fn new(
            device: wgpu::Device,
            queue: wgpu::Queue,
            compressor: GpuBlockCompressor,
        ) -> Self {
            Self {
                device,
                queue,
                compressor,
            }
        }
    }

    impl TileProcessor for WgpuTileProcessor {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            process_tile(
                &self.device,
                &self.queue,
                &mut self.compressor,
                image,
                format,
                levels,
                timing,
            )
        }
    }

    /// The worker loop, on the worker's thread; [`spawn_tile_worker`] says
    /// what ends it and how each request is answered.
    fn run_worker_loop<P: TileProcessor>(
        mut processor: P,
        mut rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
    ) {
        loop {
            if shutdown.is_cancelled() {
                tracing::info!("GPU pipeline worker: shutdown signal received");
                // Queued requests are dropped, so their callers see the
                // response channel closed; drained as after a panic.
                rx.close();
                while rx.blocking_recv().is_some() {}
                break;
            }

            let request = match rx.try_recv() {
                Ok(req) => req,
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            };

            let GpuEncodeRequest {
                image,
                format,
                mipmap_count,
                sent_at,
                response,
            } = request;

            let mut timing = GpuTileTiming {
                queue_wait: sent_at.elapsed(),
                ..GpuTileTiming::default()
            };

            tracing::debug!(
                format = ?format,
                width = image.width(),
                height = image.height(),
                mipmap_count,
                "GPU pipeline: processing full tile"
            );

            let tile_span = tracing::debug_span!(
                target: "profiling",
                "gpu_tile",
                width = image.width(),
                height = image.height(),
                levels = mipmap_count,
            )
            .entered();
            let started = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                processor.process(image, format, mipmap_count, &mut timing)
            }));
            timing.process = started.elapsed();
            drop(tile_span);

            match result {
                Ok(result) => {
                    let _ = response.send(GpuEncodeReply { result, timing });
                }
                Err(panic_info) => {
                    let msg = crate::panic::panic_message(panic_info.as_ref());

                    tracing::error!(
                        error = %msg,
                        "GPU pipeline worker panicked, shutting down"
                    );

                    let _ = response.send(GpuEncodeReply::failed(
                        format!("GPU worker panicked: {msg}"),
                        timing,
                    ));

                    // Closed first, so no request is sent while the queue
                    // drains: a caller with `WhenBusy::EncodeWithIspc`
                    // encodes with ISPC, one that waits fails at once.
                    // Drained with `blocking_recv`, which returns `None`
                    // only once no sender holds a slot: a `try_send` that
                    // took its slot before the close and sends after it is
                    // answered too, where `try_recv` would leave its caller
                    // waiting forever.
                    rx.close();
                    while let Some(req) = rx.blocking_recv() {
                        let _ = req.response.send(GpuEncodeReply::failed(
                            "GPU worker terminated after panic",
                            GpuTileTiming::default(),
                        ));
                    }

                    break;
                }
            }
        }

        tracing::info!("GPU pipeline worker shutting down");
    }

    /// Spawn the worker loop around `processor` on a dedicated blocking
    /// thread. The loop ends when the shutdown token is cancelled (between
    /// tiles: a tile in progress is finished and answered, queued requests
    /// are dropped and their callers see the response channel closed), when
    /// every sender is dropped, or when `processor` panics: the request in
    /// progress is failed with the panic's message, every queued one with
    /// "terminated after panic", and the loop ends (#420 is to change this).
    ///
    /// Returns a `JoinHandle` that resolves when the loop ends.
    pub fn spawn_tile_worker<P: TileProcessor + 'static>(
        processor: P,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        // spawn_blocking for a long-lived dedicated thread: the processor
        // stays on one thread for its whole life.
        tokio::task::spawn_blocking(move || run_worker_loop(processor, rx, shutdown))
    }

    /// Spawn the GPU pipeline worker: [`spawn_tile_worker`] with a
    /// [`WgpuTileProcessor`].
    ///
    /// The worker receives full tile requests via the channel, generates
    /// mipmaps internally, and compresses each level on the GPU. One message
    /// in, one result out — no per-level channel round-trips.
    pub fn spawn_gpu_worker(
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: GpuBlockCompressor,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        spawn_tile_worker(
            WgpuTileProcessor::new(device, queue, compressor),
            rx,
            shutdown,
        )
    }

    /// Create a [`GpuEncoderChannel`] with a full-tile streaming worker.
    ///
    /// The handle follows `when_busy` ([`GpuEncoderChannel::with_policy`]):
    /// callers wait for room (`texture.compressor = gpu`) or overflow to
    /// ISPC (`hybrid`).
    ///
    /// Initializes GPU resources and spawns the worker thread. Returns the
    /// channel handle, the worker `JoinHandle`, and the selection: the
    /// adapter chosen (for display; the worker owns the device) with the
    /// host's other adapters for the encoder policy.
    ///
    /// Nothing else is started: the per-minute `GPU encode sample` line of
    /// the channel's [`stats`](GpuEncoderChannel::stats) is started by the
    /// service wiring (`service::builder::create_encoder`), so a test that
    /// samples the stats itself loses nothing to it.
    ///
    /// # Errors
    ///
    /// Returns `DdsError` if GPU initialization fails.
    pub fn create_gpu_encoder_channel(
        gpu_device: &str,
        when_busy: WhenBusy,
        shutdown: CancellationToken,
    ) -> Result<
        (
            GpuEncoderChannel,
            tokio::task::JoinHandle<()>,
            crate::system::GpuSelection,
        ),
        DdsError,
    > {
        let (device, queue, compressor, selection) = crate::dds::create_gpu_resources(gpu_device)?;

        tracing::info!(adapter = %selection.adapter, "GPU pipeline worker starting");

        let (channel, rx) = GpuEncoderChannel::with_policy(when_busy);
        let handle = spawn_gpu_worker(device, queue, compressor, rx, shutdown);
        Ok((channel, handle, selection))
    }
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::compressor::tests::varied;
    use crate::dds::compressor::{ImageCompressor, MipmapCompressor};
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::{DdsError, DdsFormat, IspcCompressor, MipmapStream, SoftwareCompressor};
    use image::RgbaImage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    // =========================================================================
    // Test processors: the real worker loop around fake tile work
    // =========================================================================

    /// Compresses with the software kernel: real output, no GPU.
    struct SoftwareProcessor;

    impl TileProcessor for SoftwareProcessor {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            _timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            MipmapStream::new(image, levels).try_fold(Vec::new(), |mut acc, level| {
                SoftwareCompressor.compress(&level, format).map(|data| {
                    acc.extend_from_slice(&data);
                    acc
                })
            })
        }
    }

    /// Tile work given as a closure, for processors that panic, sleep, fail
    /// or record order.
    struct FnProcessor<F>(F);

    impl<F> TileProcessor for FnProcessor<F>
    where
        F: FnMut(RgbaImage, &mut GpuTileTiming) -> Result<Vec<u8>, DdsError> + Send,
    {
        fn process(
            &mut self,
            image: RgbaImage,
            _format: DdsFormat,
            _levels: usize,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            (self.0)(image, timing)
        }
    }

    /// A request whose image is `width` pixels wide, so a processor can
    /// tell requests apart, and the receiver of its reply.
    fn request(width: u32) -> (GpuEncodeRequest, oneshot::Receiver<GpuEncodeReply>) {
        let (response, reply) = oneshot::channel();
        let request = GpuEncodeRequest {
            image: RgbaImage::new(width, 4),
            format: DdsFormat::BC1,
            mipmap_count: 1,
            sent_at: Instant::now(),
            response,
        };
        (request, reply)
    }

    /// Queues requests of the given widths, before any worker runs.
    fn queue(
        tx: &mpsc::Sender<GpuEncodeRequest>,
        widths: &[u32],
    ) -> Vec<oneshot::Receiver<GpuEncodeReply>> {
        widths
            .iter()
            .map(|&w| {
                let (req, reply) = request(w);
                tx.try_send(req).expect("channel has room");
                reply
            })
            .collect()
    }

    async fn reply_of(reply: oneshot::Receiver<GpuEncodeReply>) -> GpuEncodeReply {
        tokio::time::timeout(Duration::from_secs(5), reply)
            .await
            .expect("a reply within 5 s")
            .expect("the worker answered")
    }

    fn error_text(reply: &GpuEncodeReply) -> String {
        reply
            .result
            .as_ref()
            .expect_err("the request failed")
            .to_string()
    }

    async fn loop_ends(handle: tokio::task::JoinHandle<()>, within: Duration) {
        tokio::time::timeout(within, handle)
            .await
            .expect("the worker loop ended in time")
            .expect("the worker loop itself never panics");
    }

    // =========================================================================
    // The worker loop (#419 item 4)
    // =========================================================================

    /// Today's behaviour, pinned so #420's change is visible: the request in
    /// progress fails with the panic's message, every queued one with
    /// "terminated after panic", and the loop ends.
    #[tokio::test]
    async fn panicking_processor_fails_in_flight_and_queued_requests_and_ends_the_loop() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8, 12]);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("kernel asserted on width {}", image.width())
        });

        let handle = spawn_tile_worker(processor, rx, CancellationToken::new());
        loop_ends(handle, Duration::from_secs(5)).await;

        let mut replies = replies.into_iter();
        let first = reply_of(replies.next().unwrap()).await;
        assert_eq!(
            error_text(&first),
            "Compression failed: GPU worker panicked: kernel asserted on width 4"
        );
        for reply in replies {
            let reply = reply_of(reply).await;
            assert_eq!(
                error_text(&reply),
                "Compression failed: GPU worker terminated after panic"
            );
            assert_eq!(reply.timing, GpuTileTiming::default());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one tile was started");
        assert!(tx.is_closed(), "the receiver is gone with the loop");
    }

    #[tokio::test]
    async fn shutdown_stops_an_idle_loop_at_once() {
        let (_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let handle = spawn_tile_worker(SoftwareProcessor, rx, shutdown.clone());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!handle.is_finished(), "an idle loop waits for work");

        shutdown.cancel();
        // One idle poll (10 ms) at most; the bound leaves room for a busy host.
        loop_ends(handle, Duration::from_millis(500)).await;
    }

    /// Shutdown is seen between tiles: the tile in progress is finished and
    /// answered, the queued one is never started and its caller sees the
    /// response channel closed.
    #[tokio::test]
    async fn shutdown_stops_a_busy_loop_after_its_tile() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut replies = queue(&tx, &[4, 8]).into_iter();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<u32>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            started_tx.send(image.width()).unwrap();
            release_rx.recv().unwrap();
            Ok(vec![0; 8])
        });
        let shutdown = CancellationToken::new();
        let handle = spawn_tile_worker(processor, rx, shutdown.clone());

        let started = tokio::task::spawn_blocking(move || {
            let first = started_rx.recv_timeout(Duration::from_secs(5));
            (first, started_rx)
        });
        let (first, started_rx) = started.await.unwrap();
        assert_eq!(first, Ok(4), "the first request is in progress");

        shutdown.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "the tile in progress is finished first"
        );

        release_tx.send(()).unwrap();
        loop_ends(handle, Duration::from_secs(5)).await;

        let first = reply_of(replies.next().unwrap()).await;
        assert_eq!(first.result.unwrap().len(), 8, "the busy tile is answered");
        let queued = replies.next().unwrap().await;
        assert!(queued.is_err(), "the queued request is dropped unanswered");
        assert!(
            started_rx.try_recv().is_err(),
            "the queued request was never started"
        );
    }

    /// Requests already buffered are still served after the last sender is
    /// dropped; then the loop ends.
    #[tokio::test]
    async fn closed_channel_ends_the_loop_after_the_buffered_requests() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8]);
        drop(tx);

        let handle = spawn_tile_worker(SoftwareProcessor, rx, CancellationToken::new());
        loop_ends(handle, Duration::from_secs(5)).await;

        // One BC1 block per 4 x 4: 8 bytes for the 4-wide, 16 for the 8-wide.
        let mut lengths = Vec::new();
        for reply in replies {
            lengths.push(reply_of(reply).await.result.unwrap().len());
        }
        assert_eq!(lengths, [8, 16]);
    }

    #[tokio::test]
    async fn closed_channel_ends_an_idle_loop() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let handle = spawn_tile_worker(SoftwareProcessor, rx, CancellationToken::new());
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(tx);
        loop_ends(handle, Duration::from_millis(500)).await;
    }

    /// The loop times the queue wait and the processor's call, and passes
    /// the processor's own stages through. Four requests queued together:
    /// the one at position `i` waited for the `i` tiles ahead of it.
    #[tokio::test]
    async fn timings_are_filled_and_queue_wait_grows_with_queue_position() {
        const TILE: Duration = Duration::from_millis(20);
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8, 12, 16]);
        let processor = FnProcessor(|_: RgbaImage, timing: &mut GpuTileTiming| {
            std::thread::sleep(TILE);
            timing.downsample = Duration::from_millis(1);
            timing.upload = Duration::from_millis(2);
            timing.gpu = Duration::from_millis(3);
            timing.readback = Duration::from_millis(4);
            Ok(vec![0; 8])
        });
        let _worker = spawn_tile_worker(processor, rx, CancellationToken::new());

        let mut waits = Vec::new();
        for reply in replies {
            let reply = reply_of(reply).await;
            assert!(reply.result.is_ok());
            let t = reply.timing;
            assert_eq!(
                (t.downsample, t.upload, t.gpu, t.readback),
                (
                    Duration::from_millis(1),
                    Duration::from_millis(2),
                    Duration::from_millis(3),
                    Duration::from_millis(4)
                ),
                "the processor's stages reach the caller"
            );
            assert!(t.process >= TILE, "process {:?} covers the call", t.process);
            waits.push(t.queue_wait);
        }
        for (i, wait) in waits.iter().enumerate() {
            assert!(
                *wait >= TILE * i as u32,
                "position {i} waited {wait:?}, behind {i} tiles of {TILE:?}"
            );
        }
        assert!(
            waits.windows(2).all(|w| w[0] < w[1]),
            "queue wait grows with position: {waits:?}"
        );
    }

    // =========================================================================
    // The channel handle
    // =========================================================================

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
    // The overflow handle (`texture.compressor = hybrid`)
    // =========================================================================

    /// What `texture.compressor = ispc` writes behind the header for
    /// `image`: the bytes an overflowed tile must have.
    fn ispc_payload(image: RgbaImage, levels: usize) -> Vec<u8> {
        let dds = crate::dds::DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(IspcCompressor))
            .with_mipmap_count(levels)
            .encode(image)
            .expect("ISPC encodes the reference");
        dds[crate::dds::layout::HEADER_LEN..].to_vec()
    }

    /// An overflow handle whose one queue slot is taken by a tile of width
    /// 4, with no worker: the next tile overflows. Returns the receiver too,
    /// to see what reached the queue.
    fn taken_overflow_channel() -> (GpuEncoderChannel, mpsc::Receiver<GpuEncodeRequest>) {
        let (channel, rx) = GpuEncoderChannel::with_policy(WhenBusy::EncodeWithIspc);
        let (waiting, _reply) = request(4);
        channel
            .sender()
            .try_send(waiting)
            .expect("room for one tile");
        (channel, rx)
    }

    #[test]
    fn each_policy_has_its_queue() {
        let (channel, _rx) = GpuEncoderChannel::with_policy(WhenBusy::Wait);
        assert_eq!(channel.sender().max_capacity(), CHANNEL_CAPACITY);
        let (channel, _rx) = GpuEncoderChannel::with_policy(WhenBusy::EncodeWithIspc);
        assert_eq!(channel.sender().max_capacity(), 1);
    }

    #[tokio::test]
    async fn overflow_sends_a_tile_to_a_worker_with_room() {
        let (channel, rx) = GpuEncoderChannel::with_policy(WhenBusy::EncodeWithIspc);
        let worker = FnProcessor(|_: RgbaImage, _: &mut GpuTileTiming| Ok(vec![0xAB; 8]));
        let _worker = spawn_tile_worker(worker, rx, CancellationToken::new());

        let (result, channel) = tokio::task::spawn_blocking(move || {
            let result = channel.compress_mipmap_chain(varied(64, 64), DdsFormat::BC1, 1);
            (result, channel)
        })
        .await
        .expect("the caller does not panic");

        assert_eq!(result.expect("the worker encoded it"), vec![0xAB; 8]);
        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.overflowed, s.failed), (1, 0, 0));
    }

    #[test]
    fn overflow_encodes_what_ispc_writes_when_the_queue_is_taken() {
        let (channel, mut rx) = taken_overflow_channel();
        let image = varied(64, 64);
        let expected = ispc_payload(image.clone(), 7);

        let result = channel.compress_mipmap_chain(image, DdsFormat::BC1, 7);

        assert_eq!(result.expect("ISPC encoded it"), expected);
        let queued: Vec<u32> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|r| r.image.width())
            .collect();
        assert_eq!(queued, [4], "only the waiting tile reached the queue");
        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.overflowed, s.failed), (0, 1, 0));
        assert_eq!(
            (s.depth_now, s.depth_max),
            (0, 0),
            "an overflowed tile is never in the queue depth"
        );
    }

    #[test]
    fn overflow_encodes_with_ispc_when_the_worker_stopped() {
        let (channel, rx) = GpuEncoderChannel::with_policy(WhenBusy::EncodeWithIspc);
        drop(rx);
        let image = varied(64, 64);
        let expected = ispc_payload(image.clone(), 7);

        let result = channel.compress_mipmap_chain(image, DdsFormat::BC1, 7);

        assert_eq!(result.expect("ISPC encoded it"), expected);
        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.overflowed, s.failed), (0, 1, 0));
    }

    #[test]
    fn overflow_that_fails_to_encode_counts_as_failed() {
        let (channel, _rx) = taken_overflow_channel();

        let result = channel.compress_mipmap_chain(RgbaImage::new(0, 0), DdsFormat::BC1, 1);

        assert!(
            matches!(result, Err(DdsError::InvalidDimensions(0, 0))),
            "{result:?}"
        );
        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.overflowed, s.failed), (0, 0, 1));
    }

    // =========================================================================
    // The channel against the real loop (software kernel, no GPU required)
    // =========================================================================

    /// A channel and the real worker loop around `processor`.
    fn channel_with<P: TileProcessor + 'static>(
        processor: P,
    ) -> (GpuEncoderChannel, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let worker = spawn_tile_worker(processor, rx, CancellationToken::new());
        (GpuEncoderChannel::new(tx), worker)
    }

    fn mock_channel() -> (GpuEncoderChannel, Option<mpsc::Receiver<GpuEncodeRequest>>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (GpuEncoderChannel::new(tx), Some(rx))
    }

    #[tokio::test]
    async fn test_worker_processes_single_request() {
        let (channel, _worker) = channel_with(SoftwareProcessor);

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
    async fn test_worker_processes_multiple_sequential_requests() {
        let (channel, _worker) = channel_with(SoftwareProcessor);
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
    async fn test_worker_handles_concurrent_submissions() {
        let (channel, _worker) = channel_with(SoftwareProcessor);
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

    /// Dropping the channel handle closes the channel and ends the loop.
    #[tokio::test]
    async fn test_worker_stops_when_channel_dropped() {
        let (channel, worker) = channel_with(SoftwareProcessor);
        drop(channel);
        loop_ends(worker, Duration::from_secs(2)).await;
    }

    /// `shutdown()` on the handle closes the channel the same way.
    #[tokio::test]
    async fn test_worker_stops_on_channel_shutdown() {
        let (mut channel, worker) = channel_with(SoftwareProcessor);
        channel.shutdown();
        assert!(!channel.is_connected());
        loop_ends(worker, Duration::from_secs(2)).await;
    }

    /// An unsupported format is refused by the caller's side of the channel:
    /// nothing is sent, so the worker never sees it and no GPU is needed.
    #[tokio::test]
    async fn channel_rejects_unsupported_format_without_sending() {
        let (channel, mut rx) = mock_channel();
        let mut rx = rx.take().expect("receiver");

        let result = tokio::task::spawn_blocking(move || {
            channel.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC3, 1)
        })
        .await
        .unwrap();

        assert!(matches!(
            result,
            Err(crate::dds::DdsError::UnsupportedFormat(_))
        ));
        assert!(rx.try_recv().is_err(), "no request may reach the worker");
    }

    /// A panic in the worker reaches the caller as an error instead of
    /// leaving it waiting forever.
    #[tokio::test]
    async fn test_gpu_worker_panic_does_not_hang_caller() {
        let (channel, _worker) = channel_with(FnProcessor(
            |_: RgbaImage, _: &mut GpuTileTiming| -> Result<Vec<u8>, DdsError> {
                panic!("simulated kernel panic")
            },
        ));

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .expect("spawn_blocking should not panic");

        let err = result.expect_err("should fail with worker panic error");
        assert!(
            err.to_string()
                .contains("GPU worker panicked: simulated kernel panic"),
            "error should carry the panic, got: {err}"
        );
    }

    /// A processor's error (a buffer mapping failure, say) reaches the caller.
    #[tokio::test]
    async fn test_gpu_worker_mapping_error_propagates() {
        let (channel, _worker) =
            channel_with(FnProcessor(|_: RgbaImage, _: &mut GpuTileTiming| {
                Err(DdsError::CompressionFailed(
                    "buffer mapping failed: simulated".to_string(),
                ))
            }));

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

    // =========================================================================
    // Stage timing through the channel (#419 item 2)
    // =========================================================================

    /// Encodes one 4 x 4 image through `channel` from a blocking thread.
    async fn encode_one(channel: &Arc<GpuEncoderChannel>) -> Result<Vec<u8>, DdsError> {
        let ch = Arc::clone(channel);
        tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC1, 1)
        })
        .await
        .expect("spawn_blocking should not panic")
    }

    /// Every answered tile lands in the channel's stats: an encoded one with
    /// the processor's stages and the caller's round trip, a failed one as a
    /// failure only.
    #[tokio::test]
    async fn the_channel_records_each_answered_tile_in_its_stats() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let (channel, _worker) = channel_with(FnProcessor(
            move |_: RgbaImage, timing: &mut GpuTileTiming| {
                if seen.fetch_add(1, Ordering::SeqCst) == 1 {
                    return Err(DdsError::CompressionFailed("simulated".into()));
                }
                std::thread::sleep(Duration::from_millis(5));
                timing.downsample = Duration::from_millis(1);
                timing.upload = Duration::from_millis(2);
                timing.gpu = Duration::from_millis(3);
                timing.readback = Duration::from_millis(4);
                Ok(vec![0; 8])
            },
        ));
        let channel = Arc::new(channel);

        encode_one(&channel).await.expect("first tile encodes");
        encode_one(&channel).await.expect_err("second tile fails");

        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.failed), (1, 1));
        let max = |s: Option<crate::metrics::LatencySummary>| s.unwrap().max_us;
        assert_eq!(max(s.downsample), 1_000);
        assert_eq!(max(s.upload), 2_000);
        assert_eq!(max(s.gpu), 3_000);
        assert_eq!(max(s.readback), 4_000);
        assert!(max(s.process) >= 5_000, "process covers the call");
        assert!(
            max(s.total) >= max(s.process) + max(s.queue_wait),
            "the round trip covers the queue and the worker: {s:?}"
        );
    }

    // =========================================================================
    // Queue depth (#419 item 3)
    // =========================================================================

    /// Waits until `stats` shows `depth` requests, or fails after 5 s.
    async fn depth_reaches(stats: &GpuEncodeStats, depth: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while stats.depth() != depth {
            assert!(
                Instant::now() < deadline,
                "depth stayed at {}, expected {depth}",
                stats.depth()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// The depth counts every caller from before its send to its reply:
    /// in progress, buffered, and blocked in `blocking_send` on a full
    /// channel, which the channel's own capacity does not see. It returns
    /// to zero after the burst, and the sample keeps the burst's peak.
    #[tokio::test]
    async fn queue_depth_rises_under_a_burst_and_returns_to_zero() {
        // A one-slot channel: of five callers, one is in progress, one is
        // buffered and three are blocked in their send.
        let (tx, rx) = mpsc::channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let _worker = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                release_rx.recv().unwrap();
                Ok(vec![0; 8])
            }),
            rx,
            CancellationToken::new(),
        );
        let raw = tx.clone();
        let channel = Arc::new(GpuEncoderChannel::new(tx));
        let stats = Arc::clone(channel.stats());

        let callers: Vec<_> = (0..5)
            .map(|_| {
                let ch = Arc::clone(&channel);
                tokio::task::spawn_blocking(move || {
                    ch.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC1, 1)
                })
            })
            .collect();

        depth_reaches(&stats, 5).await;
        // The counter is raised before the send: wait for the buffered
        // request to reach the channel's one slot too.
        let deadline = Instant::now() + Duration::from_secs(5);
        while raw.capacity() != 0 {
            assert!(Instant::now() < deadline, "the slot was never filled");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(
            raw.max_capacity() - raw.capacity(),
            1,
            "the channel itself sees only the buffered request"
        );

        for _ in 0..5 {
            release_tx.send(()).unwrap();
        }
        for caller in callers {
            caller.await.unwrap().expect("every caller is answered");
        }
        assert_eq!(stats.depth(), 0, "the burst has drained");

        let s = stats.take_sample().expect("a busy sample");
        assert_eq!((s.depth_now, s.depth_max, s.tiles()), (0, 5, 5));
    }

    /// A caller whose send fails is not left counted.
    #[tokio::test]
    async fn queue_depth_is_lowered_when_the_worker_is_gone() {
        let (channel, rx) = mock_channel();
        drop(rx);
        let channel = Arc::new(channel);

        encode_one(&channel).await.expect_err("no worker");

        assert_eq!(channel.stats().depth(), 0);
        let s = channel
            .stats()
            .take_sample()
            .expect("the request was counted");
        assert_eq!((s.depth_now, s.depth_max), (0, 1));
        assert_eq!((s.tiles(), s.failed), (0, 1), "the failed send is counted");
    }

    /// Every request that comes back without a tile is a failure in the
    /// sample: a worker gone before replying, and a handle shut down.
    #[tokio::test]
    async fn dropped_replies_and_a_shut_down_handle_count_as_failures() {
        // A worker that takes one request and drops it unanswered.
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let worker = tokio::task::spawn_blocking(move || drop(rx.blocking_recv()));
        let channel = Arc::new(GpuEncoderChannel::new(tx));
        let stats = Arc::clone(channel.stats());

        let err = encode_one(&channel).await.expect_err("reply dropped");
        assert!(err.to_string().contains("dropped response channel"));
        worker.await.unwrap();

        let mut handle = Arc::try_unwrap(channel).ok().expect("sole owner");
        handle.shutdown();
        let handle = Arc::new(handle);
        let err = encode_one(&handle).await.expect_err("shut down");
        assert!(err.to_string().contains("shut down"));

        let s = stats.take_sample().expect("two failures");
        assert_eq!((s.tiles(), s.failed, s.depth_now), (0, 2, 0));
    }

    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_create_gpu_encoder_channel_convenience() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channel, worker_handle, _adapter) =
            match create_gpu_encoder_channel("integrated", WhenBusy::Wait, shutdown) {
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

    /// A single BC1 block through the pipeline.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_single_bc1_block() {
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
