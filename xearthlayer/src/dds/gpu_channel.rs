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
//!
//! # When the GPU fails (#420)
//!
//! The worker publishes its health ([`SharedGpuHealth`]) and gives back every
//! image the GPU did not consume, as [`GpuEncodeReply::Returned`];
//! [`GpuEncoderChannel`] then answers [`ChainOutcome::Returned`] and
//! `DdsEncoder` encodes the image with its fallback compressor (ISPC). The
//! health turns Failed, for the rest of the session, on a panic the loop
//! catches, on a lost device (the callback `create_gpu_resources` registers)
//! and on [`MISSES_BEFORE_FAILED`] tiles in a row that missed: past their
//! [`TILE_DEADLINE`], or failed in time, as every tile does on a destroyed
//! device ([`HangRule`]). Then the worker hands its whole queue
//! back (close the channel, answer every request in it) and
//! ends, and the channel hands every later image back without sending it. A
//! tile past its deadline on a GPU not yet failed hands back only the
//! requests queued behind it, so no caller waits longer than the deadline
//! plus the tile in progress. The one tile already inside the processor when
//! it failed is lost and fails as before.

mod inner {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use image::RgbaImage;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::dds::compressor::{ChainOutcome, MipmapCompressor};
    use crate::dds::gpu_health::SharedGpuHealth;
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::layout;
    use crate::dds::{DdsError, DdsFormat};

    /// Bounded channel capacity for GPU encode requests.
    pub const CHANNEL_CAPACITY: usize = 32;

    /// The failure every request still queued at shutdown is answered with.
    const SHUT_DOWN: &str = "GPU worker shut down";

    /// How long an idle worker sleeps between looks at its channel, its
    /// shutdown token and its health.
    const IDLE_POLL: Duration = Duration::from_millis(10);

    /// The GPU's time for one tile, from the worker taking it: each level's
    /// poll waits for what remains of it (#420). A 4096 tile takes tens of
    /// milliseconds on a working GPU; the FUSE generation timeout is 10 s.
    pub const TILE_DEADLINE: Duration = Duration::from_secs(2);

    /// Tiles in a row past [`TILE_DEADLINE`], or failed, that fail the GPU
    /// (#420).
    pub const MISSES_BEFORE_FAILED: u32 = 3;

    /// When the worker gives up on a GPU that stops making progress or
    /// fails every tile: a tile that takes [`deadline`](Self::deadline) or
    /// longer is a miss, and so is one that fails in time (a destroyed
    /// device refuses every submission at once, with no panic and no hang);
    /// [`misses`](Self::misses) in a row fail the GPU. A tile that succeeds
    /// in time resets the count. The default is [`TILE_DEADLINE`] and
    /// [`MISSES_BEFORE_FAILED`]; tests shorten the deadline.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HangRule {
        /// The time one tile may take, from the worker taking it.
        pub deadline: Duration,
        /// Missed tiles in a row that fail the GPU.
        pub misses: u32,
    }

    impl Default for HangRule {
        fn default() -> Self {
            Self {
                deadline: TILE_DEADLINE,
                misses: MISSES_BEFORE_FAILED,
            }
        }
    }

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
    pub enum GpuEncodeReply {
        /// The processor ran on the tile.
        Processed {
            /// The compressed chain, every level back to back, or why not.
            result: Result<Vec<u8>, DdsError>,
            /// The stages as far as the tile got.
            timing: GpuTileTiming,
        },
        /// The GPU did not take the tile: the image comes back unchanged for
        /// the caller to encode on the CPU (#420).
        Returned {
            /// The request's image, as it was sent.
            image: RgbaImage,
            /// Why: the GPU's failure, or a tile ahead past its deadline.
            reason: String,
        },
    }

    impl GpuEncodeReply {
        fn failed(message: impl Into<String>, timing: GpuTileTiming) -> Self {
            Self::Processed {
                result: Err(DdsError::CompressionFailed(message.into())),
                timing,
            }
        }
    }

    impl std::fmt::Debug for GpuEncodeReply {
        /// A returned image by its size: its pixels would fill the screen.
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Processed { result, timing } => f
                    .debug_struct("Processed")
                    .field("result", &result.as_ref().map(Vec::len))
                    .field("timing", timing)
                    .finish(),
                Self::Returned { image, reason } => f
                    .debug_struct("Returned")
                    .field(
                        "image",
                        &format_args!("{}x{}", image.width(), image.height()),
                    )
                    .field("reason", reason)
                    .finish(),
            }
        }
    }

    /// One tile's work for the worker loop: the full mip chain of `image`,
    /// compressed level by level and concatenated, largest first.
    ///
    /// Implementations fill the stages of `timing` they perform; the loop
    /// owns `queue_wait` and `process`. A processor should give up at
    /// `deadline` (the loop's [`HangRule`]), as [`WgpuTileProcessor`] does by
    /// giving each level's poll what remains of it; the loop counts a call
    /// that ends at or past it as a miss either way, and an error in time as
    /// one too. A panic is caught by the loop (see [`spawn_tile_worker`]).
    ///
    /// Must be `Send`: the loop moves it onto the worker's blocking thread,
    /// where it stays.
    pub trait TileProcessor: Send {
        /// Compresses `levels` mip levels of `image` in `format`, giving up
        /// at `deadline`, and fills the stages of `timing` it performs.
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError>;
    }

    /// Sender-side handle for submitting GPU encode requests.
    ///
    /// Implements [`MipmapCompressor`] — receives the full source image and
    /// returns compressed data for all mipmap levels. The GPU worker owns
    /// the mipmap iteration internally, eliminating per-level channel round-trips.
    ///
    /// An image the GPU does not take comes back as
    /// [`ChainOutcome::Returned`]: while the worker's `health` is Failed
    /// (never sent), when the channel is closed (the request comes back with
    /// the send error), and when the worker hands it back (#420).
    pub struct GpuEncoderChannel {
        sender: Option<mpsc::Sender<GpuEncodeRequest>>,
        stats: Arc<GpuEncodeStats>,
        health: SharedGpuHealth,
    }

    /// What became of a request after the send.
    enum Sent {
        /// The channel was closed: the request came back with the error.
        Refused(RgbaImage),
        /// The worker dropped the reply unanswered.
        Dropped,
        Replied(GpuEncodeReply),
    }

    impl GpuEncoderChannel {
        /// Create a new channel handle wrapping the given sender, with the
        /// health its worker publishes.
        pub fn new(sender: mpsc::Sender<GpuEncodeRequest>, health: SharedGpuHealth) -> Self {
            Self {
                sender: Some(sender),
                stats: Arc::default(),
                health,
            }
        }

        /// The GPU's health, as its worker and device publish it: the
        /// dashboard reads it every tick (#420).
        pub fn health(&self) -> &SharedGpuHealth {
            &self.health
        }

        /// The stage timings of the tiles answered through this channel,
        /// the requests that failed or were handed back, and its queue depth. Someone must take
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
    }

    impl MipmapCompressor for GpuEncoderChannel {
        fn compress_mipmap_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
        ) -> Result<ChainOutcome, DdsError> {
            // Refused on the caller's side: the worker never sees it.
            format.ensure_supported()?;
            // A failed GPU takes nothing more: not sent at all.
            if let Some(reason) = self.health.failure() {
                self.stats.record_fallback();
                return Ok(ChainOutcome::Returned {
                    image,
                    reason: reason.to_string(),
                });
            }
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

            // Counted from before the send (which blocks on a full channel)
            // until the reply or the failure is back.
            let in_channel = self.stats.enter();
            let sent_at = Instant::now();
            let (resp_tx, resp_rx) = oneshot::channel();
            let request = GpuEncodeRequest {
                image, // moved, zero clones
                format,
                mipmap_count,
                sent_at,
                response: resp_tx,
            };

            let sent = match sender.blocking_send(request) {
                // The send error hands the request back, image and all.
                Err(mpsc::error::SendError(request)) => Sent::Refused(request.image),
                Ok(()) => match resp_rx.blocking_recv() {
                    Ok(reply) => Sent::Replied(reply),
                    Err(_) => Sent::Dropped,
                },
            };
            let total = sent_at.elapsed();
            drop(in_channel);
            let (result, timing) = match sent {
                Sent::Refused(image) => {
                    self.stats.record_fallback();
                    return Ok(ChainOutcome::Returned {
                        image,
                        reason: "GPU worker is not running".to_string(),
                    });
                }
                Sent::Dropped => {
                    self.stats.record_failed();
                    return Err(DdsError::CompressionFailed(
                        "GPU worker dropped response channel".to_string(),
                    ));
                }
                Sent::Replied(GpuEncodeReply::Returned { image, reason }) => {
                    self.stats.record_fallback();
                    tracing::debug!(width, height, reason = %reason, "GPU encode: tile handed back");
                    return Ok(ChainOutcome::Returned { image, reason });
                }
                Sent::Replied(GpuEncodeReply::Processed { result, timing }) => (result, timing),
            };

            let t = &timing;
            let encoded = result.is_ok();
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
            result.map(ChainOutcome::Compressed)
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

        /// Process a full tile: generate mipmaps, compress each level on GPU,
        /// return concatenated compressed output.
        ///
        /// The worker owns the mipmap iteration via [`MipmapStream`]. GPU output
        /// and readback buffers are pre-allocated for level 0 (largest) and reused
        /// for smaller levels. Each stage's wall time is added to `timing`.
        ///
        /// Each level's poll waits for what remains until `deadline` (#420): a
        /// GPU that stops making progress fails the tile at the deadline rather
        /// than after a timeout per level.
        ///
        /// [`MipmapStream`]: crate::dds::MipmapStream
        fn process_tile(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            let Self {
                device,
                queue,
                compressor,
            } = self;
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
                            usage: wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::COPY_DST,
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
                            let mut pass =
                                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
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
                                timeout: Some(deadline.saturating_duration_since(Instant::now())),
                            })
                            .map_err(|e| match e {
                                wgpu::PollError::Timeout => DdsError::CompressionFailed(format!(
                                    "GPU poll timed out at the tile's deadline, level {level_idx}"
                                )),
                                e => DdsError::CompressionFailed(format!("GPU poll failed: {e}")),
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
    }

    impl TileProcessor for WgpuTileProcessor {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            self.process_tile(image, format, levels, deadline, timing)
        }
    }

    /// What the worker does with its processor once the loop ends.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Ending {
        /// Drop it, releasing the GPU: after a shutdown, a closed channel, a
        /// device lost (wgpu's wait for the queue returns at once on a lost
        /// device) or tiles that failed in time.
        Release,
        /// Leave it undropped for the rest of the process: the GPU failed by
        /// a hang (a late tile among the misses) or a panic (see
        /// [`run_worker_loop`]).
        Abandon,
    }

    /// The worker loop, on the worker's thread; [`spawn_tile_worker`] says
    /// what ends it and how each request is answered.
    ///
    /// When the GPU failed by a hang or a panic, the processor is leaked
    /// rather than dropped (#420). Dropping [`WgpuTileProcessor`] drops the
    /// last handles to its device and queue, and wgpu-core's `Queue` drop
    /// waits for the GPU to go idle with no timeout: on a GPU that stopped
    /// making progress that pins this thread for good, and the runtime's
    /// drop at exit waits for it, so XEarthLayer could not quit. The same
    /// drop panics on any error but a lost device, outside the loop's
    /// `catch_unwind`, and after a panic nothing is known of the device.
    /// The GPU is never used again in the session, and the process's exit
    /// reclaims what is leaked.
    pub(super) fn run_worker_loop<P: TileProcessor>(
        mut processor: P,
        mut rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
    ) {
        let ending = serve(&mut processor, &mut rx, &shutdown, &health, rule);
        tracing::info!("GPU pipeline worker shutting down");
        match ending {
            Ending::Release => drop(processor),
            Ending::Abandon => {
                tracing::debug!("GPU worker leaves the failed GPU's device undropped");
                std::mem::forget(processor);
            }
        }
    }

    /// The loop itself: serves requests until something ends it, and says
    /// what is to become of the processor.
    fn serve<P: TileProcessor>(
        processor: &mut P,
        rx: &mut mpsc::Receiver<GpuEncodeRequest>,
        shutdown: &CancellationToken,
        health: &SharedGpuHealth,
        rule: HangRule,
    ) -> Ending {
        let mut misses = Misses::default();
        loop {
            if shutdown.is_cancelled() {
                tracing::info!("GPU pipeline worker: shutdown signal received");
                let failed = drain(rx, |request| {
                    let _ = request
                        .response
                        .send(GpuEncodeReply::failed(SHUT_DOWN, GpuTileTiming::default()));
                });
                if failed > 0 {
                    tracing::debug!(failed, "GPU worker failed its queue at shutdown");
                }
                return Ending::Release;
            }
            // Failed from outside the loop: the device was lost.
            if let Some(reason) = health.failure() {
                hand_back_all(rx, reason);
                return Ending::Release;
            }

            let request = match rx.try_recv() {
                Ok(req) => req,
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => return Ending::Release,
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
            let deadline = started + rule.deadline;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                processor.process(image, format, mipmap_count, deadline, &mut timing)
            }));
            timing.process = started.elapsed();
            drop(tile_span);

            match result {
                Ok(result) => {
                    let late = timing.process >= rule.deadline;
                    let error = match (&result, late) {
                        (Err(e), false) => Some(e.to_string()),
                        _ => None,
                    };
                    let reply = GpuEncodeReply::Processed { result, timing };
                    if !late && error.is_none() {
                        misses.clear();
                        let _ = response.send(reply);
                        continue;
                    }
                    misses.record(error);
                    tracing::debug!(
                        misses = misses.count,
                        late,
                        process_ms = timing.process.as_millis() as u64,
                        "GPU tile missed its deadline or failed"
                    );
                    // The queue is handed back before the tile is answered,
                    // so it is the queue that waited behind that tile, never
                    // a request its caller sent after the reply.
                    let failed = misses.count >= rule.misses;
                    if failed {
                        health.fail(misses.reason(rule.deadline));
                        hand_back_all(rx, health.failure().unwrap_or_default());
                    } else if late {
                        // The queue behind a slow tile goes to the CPU now
                        // rather than wait for the next one.
                        hand_back_queued(rx, "a GPU tile ahead of it missed its deadline");
                    }
                    let _ = response.send(reply);
                    if failed {
                        return if misses.hung() {
                            Ending::Abandon
                        } else {
                            Ending::Release
                        };
                    }
                }
                Err(panic_info) => {
                    let msg = crate::panic::panic_message(panic_info.as_ref());
                    let message = format!("GPU worker panicked: {msg}");
                    health.fail(message.clone());
                    // The image was inside the processor: this tile is lost.
                    let _ = response.send(GpuEncodeReply::failed(message, timing));
                    hand_back_all(rx, health.failure().unwrap_or_default());
                    return Ending::Abandon;
                }
            }
        }
    }

    /// The worker's tiles in a row that missed: ended at or past the
    /// deadline, or failed in time. A tile that succeeds in time clears it.
    #[derive(Default)]
    struct Misses {
        count: u32,
        /// How many of them failed in time, rather than ran late.
        failed: u32,
        /// The last in-time failure's error.
        last_error: Option<String>,
    }

    impl Misses {
        fn clear(&mut self) {
            *self = Self::default();
        }

        /// One more miss: `error` is the failure of a tile in time, `None`
        /// for a late tile (whose error, if any, is the deadline's).
        fn record(&mut self, error: Option<String>) {
            self.count += 1;
            if let Some(error) = error {
                self.failed += 1;
                self.last_error = Some(error);
            }
        }

        /// Whether any of them ran late: the GPU may have stopped making
        /// progress, not just refused the work.
        fn hung(&self) -> bool {
            self.count > self.failed
        }

        /// The health's reason when these misses fail the GPU.
        fn reason(&self, deadline: Duration) -> String {
            let n = self.count;
            let ms = deadline.as_millis();
            match &self.last_error {
                None => format!("{n} tiles in a row missed the GPU's {ms} ms deadline"),
                Some(e) if self.failed == n => {
                    format!("{n} tiles in a row failed on the GPU, the last with: {e}")
                }
                Some(e) => format!(
                    "{n} tiles in a row failed on the GPU or missed its {ms} ms deadline, \
                     the last failure: {e}"
                ),
            }
        }
    }

    /// Answers `request` with its own image, untouched.
    fn hand_back(request: GpuEncodeRequest, reason: &str) {
        let _ = request.response.send(GpuEncodeReply::Returned {
            image: request.image,
            reason: reason.to_string(),
        });
    }

    /// Hands back the requests buffered in the channel now, leaving it open.
    fn hand_back_queued(rx: &mut mpsc::Receiver<GpuEncodeRequest>, reason: &str) {
        let mut handed = 0;
        while let Ok(request) = rx.try_recv() {
            hand_back(request, reason);
            handed += 1;
        }
        if handed > 0 {
            tracing::debug!(handed, reason, "GPU worker handed queued tiles back");
        }
    }

    /// Closes the channel and hands back every request in it ([`drain`]),
    /// so no image is left behind with the receiver.
    ///
    /// Must be called on a blocking thread, as the worker is.
    fn hand_back_all(rx: &mut mpsc::Receiver<GpuEncodeRequest>, reason: &str) {
        let handed = drain(rx, |request| hand_back(request, reason));
        tracing::debug!(handed, reason, "GPU worker handed its queue back");
    }

    /// Closes the channel and gives every request in it to `answer`, until
    /// no sender holds a slot (#464): the buffered requests, and those whose
    /// senders reserved a slot before the close and send after it.
    /// `blocking_recv` returns `None` only then, where `try_recv` reports
    /// the queue empty while a reserved slot is still to be filled, and a
    /// request sent into it after the loop ended would never be answered
    /// (`GpuEncoderChannel` keeps its sender for the session, so the reply
    /// channel is never dropped either). A send that comes after the close
    /// fails and returns the request to its caller with the error. Returns
    /// how many requests were answered.
    ///
    /// Must be called on a blocking thread, as the worker is.
    fn drain(
        rx: &mut mpsc::Receiver<GpuEncodeRequest>,
        mut answer: impl FnMut(GpuEncodeRequest),
    ) -> usize {
        rx.close();
        let mut answered = 0;
        while let Some(request) = rx.blocking_recv() {
            answer(request);
            answered += 1;
        }
        answered
    }

    /// Spawn the worker loop around `processor` on a dedicated blocking
    /// thread. Each request is answered with the processor's result, or
    /// with its own image ([`GpuEncodeReply::Returned`]) when the GPU does not
    /// take it. The loop ends:
    ///
    /// - when the shutdown token is cancelled, between tiles: a tile in
    ///   progress is finished and answered; every queued request, and every
    ///   one whose sender already holds a slot, fails with "GPU worker shut
    ///   down" (#464);
    /// - when every sender is dropped, after the buffered requests;
    /// - when the GPU fails (#420): the processor panics (the request in
    ///   progress fails with the panic's message), `health` is failed from
    ///   outside (the device was lost), or `rule.misses` tiles in a row end
    ///   at or past `rule.deadline` or fail before it. `health` turns Failed
    ///   with the reason, and every queued request is handed back.
    ///
    /// A tile past its deadline that does not fail the GPU hands back the
    /// requests queued behind it.
    ///
    /// The processor is dropped when the loop ends, except after a GPU
    /// failed by a hang or a panic, when it is leaked so that its drop
    /// cannot pin the thread (see `run_worker_loop`).
    ///
    /// Returns a `JoinHandle` that resolves when the loop ends.
    pub fn spawn_tile_worker<P: TileProcessor + 'static>(
        processor: P,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
    ) -> tokio::task::JoinHandle<()> {
        // spawn_blocking for a long-lived dedicated thread: the processor
        // stays on one thread for its whole life.
        tokio::task::spawn_blocking(move || run_worker_loop(processor, rx, shutdown, health, rule))
    }

    /// Spawn the GPU pipeline worker: [`spawn_tile_worker`] with a
    /// [`WgpuTileProcessor`].
    ///
    /// The worker receives full tile requests via the channel, generates
    /// mipmaps internally, and compresses each level on the GPU. One message
    /// in, one result out — no per-level channel round-trips. It gives up on
    /// the GPU by the default [`HangRule`].
    pub fn spawn_gpu_worker(
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: GpuBlockCompressor,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
    ) -> tokio::task::JoinHandle<()> {
        spawn_tile_worker(
            WgpuTileProcessor::new(device, queue, compressor),
            rx,
            shutdown,
            health,
            HangRule::default(),
        )
    }

    /// Create a [`GpuEncoderChannel`] with a full-tile streaming worker.
    ///
    /// Initializes GPU resources and spawns the worker thread. Returns the
    /// channel handle, the worker `JoinHandle`, and the selection: the
    /// adapter chosen (for display; the worker owns the device) with the
    /// host's other adapters for the encoder policy. The channel's
    /// [`health`](GpuEncoderChannel::health) is the one the worker and the
    /// device's lost callback publish.
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
        shutdown: CancellationToken,
    ) -> Result<
        (
            GpuEncoderChannel,
            tokio::task::JoinHandle<()>,
            crate::system::GpuSelection,
        ),
        DdsError,
    > {
        let health = SharedGpuHealth::new();
        let (device, queue, compressor, selection) =
            crate::dds::create_gpu_resources(gpu_device, &health)?;

        tracing::info!(adapter = %selection.adapter, "GPU pipeline worker starting");

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = spawn_gpu_worker(device, queue, compressor, rx, shutdown, health.clone());
        Ok((GpuEncoderChannel::new(tx, health), handle, selection))
    }
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::compressor::tests::varied;
    use crate::dds::compressor::{ChainOutcome, ImageCompressor, MipmapCompressor};
    use crate::dds::gpu_health::{device_lost, GpuHealth, SharedGpuHealth};
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::{
        DdsEncoder, DdsError, DdsFormat, IspcCompressor, MipmapStream, SoftwareCompressor,
    };
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
            _deadline: Instant,
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
            _deadline: Instant,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            (self.0)(image, timing)
        }
    }

    /// The worker loop around `processor` with a fresh health and the
    /// default hang rule.
    fn spawn<P: TileProcessor + 'static>(
        processor: P,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        spawn_tile_worker(
            processor,
            rx,
            shutdown,
            SharedGpuHealth::new(),
            HangRule::default(),
        )
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

    /// The processor's result and timing; panics on a handed-back reply.
    fn processed(reply: GpuEncodeReply) -> (Result<Vec<u8>, DdsError>, GpuTileTiming) {
        match reply {
            GpuEncodeReply::Processed { result, timing } => (result, timing),
            GpuEncodeReply::Returned { reason, .. } => {
                panic!("expected a processed tile, it was handed back: {reason}")
            }
        }
    }

    /// The image and reason of a handed-back reply; panics otherwise.
    fn returned(reply: GpuEncodeReply) -> (RgbaImage, String) {
        match reply {
            GpuEncodeReply::Returned { image, reason } => (image, reason),
            other => panic!("expected the image back, got {other:?}"),
        }
    }

    fn error_text(result: &Result<Vec<u8>, DdsError>) -> String {
        result.as_ref().expect_err("the request failed").to_string()
    }

    async fn loop_ends(handle: tokio::task::JoinHandle<()>, within: Duration) {
        tokio::time::timeout(within, handle)
            .await
            .expect("the worker loop ended in time")
            .expect("the worker loop itself never panics");
    }

    /// The compressed chain of an outcome; panics on a handed-back image.
    fn compressed(outcome: Result<ChainOutcome, DdsError>) -> Result<Vec<u8>, DdsError> {
        outcome.map(|outcome| match outcome {
            ChainOutcome::Compressed(data) => data,
            ChainOutcome::Returned { reason, .. } => {
                panic!("expected the GPU's chain, the image was handed back: {reason}")
            }
        })
    }

    // =========================================================================
    // The worker loop (#419 item 4, failure behaviour changed by #420)
    // =========================================================================

    /// #419 pinned the old behaviour here (every queued request failed with
    /// "terminated after panic"). Since #420 the request in progress still
    /// fails with the panic's message, but the queued ones come back with
    /// their images, the health turns Failed with the panic's message, and
    /// the loop ends with the channel closed.
    #[tokio::test]
    async fn a_panic_fails_the_tile_in_progress_and_hands_the_queue_back() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8, 12]);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("kernel asserted on width {}", image.width())
        });
        let health = SharedGpuHealth::new();

        let handle = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
        );
        loop_ends(handle, Duration::from_secs(5)).await;

        let mut replies = replies.into_iter();
        let (first, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            error_text(&first),
            "Compression failed: GPU worker panicked: kernel asserted on width 4"
        );
        for (reply, width) in replies.zip([8, 12]) {
            let (image, reason) = returned(reply_of(reply).await);
            assert_eq!(image.width(), width, "each caller gets its own image");
            assert_eq!(reason, "GPU worker panicked: kernel asserted on width 4");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one tile was started");
        assert_eq!(
            health.health(),
            GpuHealth::Failed {
                reason: "GPU worker panicked: kernel asserted on width 4".to_string()
            }
        );
        assert!(tx.is_closed(), "the receiver is gone with the loop");
    }

    /// The loop on this thread, as the worker's blocking thread runs it.
    fn run_worker_loop_here<P: TileProcessor>(
        processor: P,
        rx: mpsc::Receiver<GpuEncodeRequest>,
        health: SharedGpuHealth,
        rule: HangRule,
    ) {
        super::inner::run_worker_loop(processor, rx, CancellationToken::new(), health, rule)
    }

    /// A device lost while the worker is idle (the callback fails the
    /// health from another thread) ends the loop and hands back what is
    /// queued; nothing more reaches the processor.
    #[tokio::test]
    async fn a_device_lost_while_idle_ends_the_loop() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let handle = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0; 8])
            }),
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
        );
        tokio::time::sleep(Duration::from_millis(30)).await;

        device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
        loop_ends(handle, Duration::from_millis(500)).await;

        assert!(tx.is_closed());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let channel = Arc::new(GpuEncoderChannel::new(tx, health));
        let (image, reason) = returned_by(&channel, 4).await;
        assert_eq!(image.width(), 4);
        assert_eq!(reason, "GPU device lost: driver reset");
    }

    #[tokio::test]
    async fn shutdown_stops_an_idle_loop_at_once() {
        let (_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let handle = spawn(SoftwareProcessor, rx, shutdown.clone());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!handle.is_finished(), "an idle loop waits for work");

        shutdown.cancel();
        // One idle poll (10 ms) at most; the bound leaves room for a busy host.
        loop_ends(handle, Duration::from_millis(500)).await;
    }

    /// Shutdown is seen between tiles: the tile in progress is finished and
    /// answered, the queued one is never started and fails (#464).
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
        let handle = spawn(processor, rx, shutdown.clone());

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

        let (first, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(first.unwrap().len(), 8, "the busy tile is answered");
        let (queued, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            error_text(&queued),
            "Compression failed: GPU worker shut down"
        );
        assert!(
            started_rx.try_recv().is_err(),
            "the queued request was never started"
        );
    }

    /// A request sent while the shutdown drains the queue is answered
    /// (#464, reported in #459). Its sender reserved its slot before the
    /// drain began and sends after the queue looked empty: a drain that
    /// stopped there, dropping the receiver, left the request in a channel
    /// nobody reads, and `GpuEncoderChannel` keeps its sender for the
    /// session, so the caller waited forever.
    #[tokio::test]
    async fn shutdown_answers_a_request_sent_while_the_queue_drains() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let permit = tx.clone().reserve_owned().await.expect("a free slot");
        let shutdown = CancellationToken::new();
        let handle = spawn(SoftwareProcessor, rx, shutdown.clone());

        shutdown.cancel();
        // Several idle polls: the worker has seen the shutdown and found
        // the queue empty.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (late, reply) = request(4);
        permit.send(late);

        let (result, _) = processed(reply_of(reply).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU worker shut down"
        );
        loop_ends(handle, Duration::from_secs(1)).await;
        drop(tx);
    }

    /// The same after a panic (#464): a request sent while the failed
    /// worker drains its queue comes back with its image (#420).
    #[tokio::test]
    async fn a_panic_answers_a_request_sent_while_the_queue_drains() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let permit = tx.clone().reserve_owned().await.expect("a free slot");
        let in_progress = queue(&tx, &[4]).pop().unwrap();
        let health = SharedGpuHealth::new();
        let handle = spawn_tile_worker(
            FnProcessor(
                |_: RgbaImage, _: &mut GpuTileTiming| -> Result<Vec<u8>, DdsError> {
                    panic!("device lost")
                },
            ),
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
        );

        // The health turns Failed just before the drain begins.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !health.is_failed() {
            assert!(Instant::now() < deadline, "the worker never failed");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (late, reply) = request(8);
        permit.send(late);

        let (image, reason) = returned(reply_of(reply).await);
        assert_eq!(
            (image.width(), reason.as_str()),
            (8, "GPU worker panicked: device lost")
        );
        assert!(processed(reply_of(in_progress).await).0.is_err());
        loop_ends(handle, Duration::from_secs(1)).await;
        drop(tx);
    }

    /// Requests already buffered are still served after the last sender is
    /// dropped; then the loop ends.
    #[tokio::test]
    async fn closed_channel_ends_the_loop_after_the_buffered_requests() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8]);
        drop(tx);

        let handle = spawn(SoftwareProcessor, rx, CancellationToken::new());
        loop_ends(handle, Duration::from_secs(5)).await;

        // One BC1 block per 4 x 4: 8 bytes for the 4-wide, 16 for the 8-wide.
        let mut lengths = Vec::new();
        for reply in replies {
            lengths.push(processed(reply_of(reply).await).0.unwrap().len());
        }
        assert_eq!(lengths, [8, 16]);
    }

    #[tokio::test]
    async fn closed_channel_ends_an_idle_loop() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let handle = spawn(SoftwareProcessor, rx, CancellationToken::new());
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
        let _worker = spawn(processor, rx, CancellationToken::new());

        let mut waits = Vec::new();
        for reply in replies {
            let (result, t) = processed(reply_of(reply).await);
            assert!(result.is_ok());
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
    // The hung-GPU rule (#420)
    // =========================================================================

    #[test]
    fn the_default_rule_is_a_two_second_deadline_and_three_misses() {
        assert_eq!(TILE_DEADLINE, Duration::from_secs(2));
        assert_eq!(MISSES_BEFORE_FAILED, 3);
        assert_eq!(
            HangRule::default(),
            HangRule {
                deadline: TILE_DEADLINE,
                misses: MISSES_BEFORE_FAILED
            }
        );
    }

    /// The processor is given the deadline: the rule's deadline from when
    /// the worker took the tile, whatever the tile waited in the queue.
    #[tokio::test]
    async fn the_processor_is_given_the_deadline_from_when_its_tile_starts() {
        struct Deadlines(std::sync::mpsc::Sender<Duration>);
        impl TileProcessor for Deadlines {
            fn process(
                &mut self,
                _image: RgbaImage,
                _format: DdsFormat,
                _levels: usize,
                deadline: Instant,
                _timing: &mut GpuTileTiming,
            ) -> Result<Vec<u8>, DdsError> {
                self.0
                    .send(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                std::thread::sleep(Duration::from_millis(20));
                Ok(vec![0; 8])
            }
        }
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8]);
        let (left_tx, left_rx) = std::sync::mpsc::channel();
        let _worker = spawn(Deadlines(left_tx), rx, CancellationToken::new());
        for reply in replies {
            processed(reply_of(reply).await).0.unwrap();
        }
        for left in left_rx.try_iter() {
            assert!(
                left > TILE_DEADLINE - Duration::from_millis(15) && left <= TILE_DEADLINE,
                "{left:?} of {TILE_DEADLINE:?} left at the start of the call"
            );
        }
    }

    /// A rule short enough for tests, and a processor that overruns it.
    const SHORT: HangRule = HangRule {
        deadline: Duration::from_millis(40),
        misses: 3,
    };
    const OVERRUN: Duration = Duration::from_millis(60);

    fn overrunning() -> impl TileProcessor {
        FnProcessor(|_: RgbaImage, _: &mut GpuTileTiming| {
            std::thread::sleep(OVERRUN);
            Ok(vec![0; 8])
        })
    }

    /// A tile past its deadline is answered, and the requests queued behind
    /// it come back at once with their images: none waits for more than the
    /// deadline plus the tile in progress. One miss does not fail the GPU.
    #[tokio::test]
    async fn a_missed_deadline_hands_the_queue_back_without_failing_the_gpu() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let started = Instant::now();
        let replies = queue(&tx, &[4, 8, 12, 16, 20]);
        let health = SharedGpuHealth::new();
        let _worker = spawn_tile_worker(
            overrunning(),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );

        let mut replies = replies.into_iter();
        let (first, timing) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(first.unwrap().len(), 8, "the slow tile is still answered");
        assert!(timing.process >= SHORT.deadline);
        for (reply, width) in replies.zip([8, 12, 16, 20]) {
            let (image, reason) = returned(reply_of(reply).await);
            assert_eq!(image.width(), width);
            assert_eq!(reason, "a GPU tile ahead of it missed its deadline");
        }
        let waited = started.elapsed();
        assert!(
            waited < SHORT.deadline + OVERRUN + Duration::from_millis(500),
            "every caller was answered within the deadline plus one tile: {waited:?}"
        );
        assert_eq!(
            health.health(),
            GpuHealth::Healthy,
            "one miss is not a hang"
        );
        assert!(!tx.is_closed(), "the worker keeps running");
    }

    /// `misses` tiles in a row past the deadline fail the GPU: the worker
    /// hands back its queue, closes the channel and ends, and the channel
    /// hands every later image back without sending it.
    #[tokio::test]
    async fn misses_in_a_row_fail_the_gpu_and_hand_the_queue_back() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let handle = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                seen.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(OVERRUN);
                Ok(vec![0; 8])
            }),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );

        // Two misses, one tile at a time: each is answered.
        for width in [4, 8] {
            let reply = queue(&tx, &[width]).pop().unwrap();
            processed(reply_of(reply).await).0.unwrap();
            assert!(!health.is_failed());
        }
        // The third, with two more queued behind it.
        let mut replies = queue(&tx, &[12, 16, 20]).into_iter();
        let started = Instant::now();
        processed(reply_of(replies.next().unwrap()).await)
            .0
            .unwrap();
        let reason = "3 tiles in a row missed the GPU's 40 ms deadline";
        for (reply, width) in replies.zip([16, 20]) {
            let (image, why) = returned(reply_of(reply).await);
            assert_eq!((image.width(), why.as_str()), (width, reason));
        }
        assert!(
            started.elapsed() < SHORT.deadline + OVERRUN + Duration::from_millis(500),
            "the queue came back within the deadline plus one tile"
        );
        loop_ends(handle, Duration::from_secs(1)).await;
        assert_eq!(
            health.health(),
            GpuHealth::Failed {
                reason: reason.to_string()
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let channel = Arc::new(GpuEncoderChannel::new(tx, health));
        let (image, why) = returned_by(&channel, 24).await;
        assert_eq!((image.width(), why.as_str()), (24, reason));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "never sent");
    }

    /// A processor that fails every tile at once, as on a device whose
    /// submissions are refused.
    fn refusing(calls: &Arc<AtomicUsize>) -> impl TileProcessor {
        let seen = Arc::clone(calls);
        FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            Err(DdsError::CompressionFailed(format!(
                "GPU poll failed: submission {n} refused"
            )))
        })
    }

    /// Tiles that fail in a row are misses too: a destroyed device fails
    /// each tile at once, with no panic and no hang (seen on hardware with
    /// `Device::destroy`). `misses` of them fail the GPU with the last
    /// error, the queue behind them comes back, and the channel hands every
    /// later image back without sending it.
    #[tokio::test]
    async fn failed_tiles_in_a_row_fail_the_gpu_and_hand_the_queue_back() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = spawn_tile_worker(
            refusing(&calls),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );

        // Two failures, one tile at a time: each fails, the GPU does not.
        for width in [4, 8] {
            let reply = queue(&tx, &[width]).pop().unwrap();
            let (result, _) = processed(reply_of(reply).await);
            assert!(result.is_err());
            assert!(!health.is_failed(), "two failures are not a lost GPU");
        }
        // The third, with two more queued behind it.
        let mut replies = queue(&tx, &[12, 16, 20]).into_iter();
        let (result, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU poll failed: submission 2 refused"
        );
        let reason = "3 tiles in a row failed on the GPU, the last with: \
                      Compression failed: GPU poll failed: submission 2 refused";
        for (reply, width) in replies.zip([16, 20]) {
            let (image, why) = returned(reply_of(reply).await);
            assert_eq!((image.width(), why.as_str()), (width, reason));
        }
        loop_ends(handle, Duration::from_secs(1)).await;
        assert_eq!(health.failure(), Some(reason));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let channel = Arc::new(GpuEncoderChannel::new(tx, health));
        let (image, why) = returned_by(&channel, 24).await;
        assert_eq!((image.width(), why.as_str()), (24, reason));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "never sent");
    }

    /// Failed and late tiles count together, and the reason names both.
    #[tokio::test]
    async fn failed_and_late_tiles_in_a_row_fail_the_gpu_together() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let handle = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                if seen.fetch_add(1, Ordering::SeqCst) == 1 {
                    std::thread::sleep(OVERRUN);
                    return Ok(vec![0; 8]);
                }
                Err(DdsError::CompressionFailed("mapping refused".to_string()))
            }),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );
        for width in [4, 8, 12] {
            let reply = queue(&tx, &[width]).pop().unwrap();
            let _ = processed(reply_of(reply).await);
        }
        loop_ends(handle, Duration::from_secs(1)).await;
        assert_eq!(
            health.failure(),
            Some(
                "3 tiles in a row failed on the GPU or missed its 40 ms deadline, \
                 the last failure: Compression failed: mapping refused"
            )
        );
    }

    /// One failed tile is not a lost GPU, and a tile that succeeds resets
    /// the count: failures not in a row never fail the GPU.
    #[tokio::test]
    async fn a_tile_that_succeeds_resets_the_failures() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let _worker = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                if seen.fetch_add(1, Ordering::SeqCst) % 3 == 2 {
                    Ok(vec![0; 8])
                } else {
                    Err(DdsError::CompressionFailed("transient".to_string()))
                }
            }),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );
        for width in [4, 8, 12, 16, 20, 24, 28, 32, 36] {
            let reply = queue(&tx, &[width]).pop().unwrap();
            let _ = processed(reply_of(reply).await);
        }
        assert_eq!(health.health(), GpuHealth::Healthy);
    }

    /// A tile in time resets the count: misses that are not in a row never
    /// fail the GPU.
    #[tokio::test]
    async fn a_tile_in_time_resets_the_misses() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let _worker = spawn_tile_worker(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                if seen.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                    std::thread::sleep(OVERRUN);
                }
                Ok(vec![0; 8])
            }),
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );
        for width in [4, 8, 12, 16, 20, 24, 28] {
            let reply = queue(&tx, &[width]).pop().unwrap();
            processed(reply_of(reply).await).0.unwrap();
        }
        assert_eq!(health.health(), GpuHealth::Healthy);
    }

    // =========================================================================
    // What the loop does with its processor at the end (#420)
    // =========================================================================

    /// A processor whose drop takes `hold` (as dropping a hung GPU's queue
    /// would, forever) and then reports it.
    struct DropProbe<P> {
        inner: P,
        hold: Duration,
        dropped: std::sync::mpsc::Sender<()>,
    }

    impl<P: TileProcessor> TileProcessor for DropProbe<P> {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
        ) -> Result<Vec<u8>, DdsError> {
            self.inner.process(image, format, levels, deadline, timing)
        }
    }

    impl<P> Drop for DropProbe<P> {
        fn drop(&mut self) {
            std::thread::sleep(self.hold);
            let _ = self.dropped.send(());
        }
    }

    /// Long enough that a loop which dropped the processor misses
    /// [`loop_ends`]'s bound.
    const HUNG_DROP: Duration = Duration::from_secs(2);

    fn probe<P: TileProcessor>(
        inner: P,
        hold: Duration,
    ) -> (DropProbe<P>, std::sync::mpsc::Receiver<()>) {
        let (dropped, rx) = std::sync::mpsc::channel();
        let probe = DropProbe {
            inner,
            hold,
            dropped,
        };
        (probe, rx)
    }

    /// A GPU failed by late tiles is a GPU that may never finish its work,
    /// and wgpu's `Queue` drop waits for it with no timeout: the worker
    /// leaves the processor undropped, and its thread ends at once.
    #[tokio::test]
    async fn a_gpu_failed_by_late_tiles_is_left_undropped() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, dropped) = probe(overrunning(), HUNG_DROP);
        let health = SharedGpuHealth::new();
        let handle = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );
        // One at a time: a late tile hands back the queue behind it.
        for width in [4, 8, 12] {
            reply_of(queue(&tx, &[width]).pop().unwrap()).await;
        }
        assert!(health.is_failed());
        loop_ends(handle, Duration::from_millis(500)).await;
        assert!(dropped.try_recv().is_err(), "the processor was not dropped");
    }

    /// The same after a panic: nothing is known of the device it left.
    #[tokio::test]
    async fn a_processor_that_panicked_is_left_undropped() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let reply = queue(&tx, &[4]).pop().unwrap();
        let (processor, dropped) = probe(
            FnProcessor(
                |_: RgbaImage, _: &mut GpuTileTiming| -> Result<Vec<u8>, DdsError> {
                    panic!("device lost")
                },
            ),
            HUNG_DROP,
        );
        let handle = spawn(processor, rx, CancellationToken::new());
        reply_of(reply).await;
        loop_ends(handle, Duration::from_millis(500)).await;
        assert!(dropped.try_recv().is_err(), "the processor was not dropped");
    }

    /// Every other end releases the GPU: a shutdown, a closed channel, a
    /// device lost (its queue's drop returns at once), and tiles that fail
    /// in time (a destroyed device).
    #[tokio::test]
    async fn every_other_end_drops_the_processor() {
        let ok = || FnProcessor(|_: RgbaImage, _: &mut GpuTileTiming| Ok(vec![0; 8]));
        let within = Duration::from_secs(1);

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, dropped) = probe(ok(), Duration::ZERO);
        let shutdown = CancellationToken::new();
        let handle = spawn(processor, rx, shutdown.clone());
        shutdown.cancel();
        loop_ends(handle, within).await;
        assert_eq!(dropped.try_recv(), Ok(()), "shutdown");
        drop(tx);

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, dropped) = probe(ok(), Duration::ZERO);
        let handle = spawn(processor, rx, CancellationToken::new());
        drop(tx);
        loop_ends(handle, within).await;
        assert_eq!(dropped.try_recv(), Ok(()), "closed channel");

        let (_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, dropped) = probe(ok(), Duration::ZERO);
        let health = SharedGpuHealth::new();
        let handle = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
        );
        device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
        loop_ends(handle, within).await;
        assert_eq!(dropped.try_recv(), Ok(()), "device lost");

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &[4, 8, 12]);
        let calls = Arc::new(AtomicUsize::new(0));
        let (processor, dropped) = probe(refusing(&calls), Duration::ZERO);
        let health = SharedGpuHealth::new();
        let handle = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            SHORT,
        );
        for reply in replies {
            reply_of(reply).await;
        }
        loop_ends(handle, within).await;
        assert!(health.is_failed());
        assert_eq!(dropped.try_recv(), Ok(()), "tiles failed in time");
    }

    // =========================================================================
    // The channel handle
    // =========================================================================

    #[test]
    fn test_channel_capacity_constant() {
        assert_eq!(CHANNEL_CAPACITY, 32);
    }

    fn handle(tx: mpsc::Sender<GpuEncodeRequest>) -> GpuEncoderChannel {
        GpuEncoderChannel::new(tx, SharedGpuHealth::new())
    }

    #[test]
    fn test_gpu_encoder_channel_connected() {
        let (tx, _rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = handle(tx);
        assert!(channel.is_connected());
    }

    #[test]
    fn test_gpu_encoder_channel_disconnected_when_rx_dropped() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = handle(tx);
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
        let channel = handle(tx);
        assert_eq!(channel.name(), "gpu-channel");
    }

    /// Encodes one `width` x 4 image through `channel` from a blocking
    /// thread, expecting it handed back.
    async fn returned_by(channel: &Arc<GpuEncoderChannel>, width: u32) -> (RgbaImage, String) {
        let ch = Arc::clone(channel);
        let outcome = tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(RgbaImage::new(width, 4), DdsFormat::BC1, 1)
        })
        .await
        .expect("spawn_blocking should not panic");
        match outcome {
            Ok(ChainOutcome::Returned { image, reason }) => (image, reason),
            other => panic!("expected the image back, got {other:?}"),
        }
    }

    /// The worker is gone (its receiver dropped): the send error returns the
    /// request, and the image comes back to the caller instead of failing
    /// it (#420). The GPU is not failed by it: a closed channel without a
    /// failure is a worker that was shut down.
    #[tokio::test]
    async fn a_send_on_a_closed_channel_hands_the_image_back() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = Arc::new(handle(tx));
        drop(rx);

        let (image, reason) = returned_by(&channel, 8).await;
        assert_eq!(image.width(), 8);
        assert_eq!(reason, "GPU worker is not running");
        assert!(!channel.health().is_failed());
    }

    // =========================================================================
    // The channel against the real loop (software kernel, no GPU required)
    // =========================================================================

    /// A channel and the real worker loop around `processor`, sharing one
    /// health.
    fn channel_with<P: TileProcessor + 'static>(
        processor: P,
    ) -> (GpuEncoderChannel, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let worker = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
        );
        (GpuEncoderChannel::new(tx, health), worker)
    }

    fn mock_channel() -> (GpuEncoderChannel, Option<mpsc::Receiver<GpuEncodeRequest>>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (handle(tx), Some(rx))
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

        let data = compressed(result).expect("compress should succeed");
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

            let data =
                compressed(result).unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
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
            let data = compressed(result).expect("concurrent compress should succeed");
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
    // The fallback end to end (#420): the encoder, the channel, the real loop
    // =========================================================================

    /// The GPU path as the service builds it: `DdsEncoder` on the channel,
    /// with the ISPC fallback.
    fn gpu_encoder(channel: &Arc<GpuEncoderChannel>) -> DdsEncoder {
        DdsEncoder::new(DdsFormat::BC1).with_mipmap_compressor(
            Arc::clone(channel) as Arc<dyn MipmapCompressor>,
            Arc::new(IspcCompressor),
        )
    }

    /// What ISPC alone makes of `image`.
    fn ispc_encode(image: RgbaImage) -> Vec<u8> {
        DdsEncoder::new(DdsFormat::BC1)
            .with_compressor(Arc::new(IspcCompressor))
            .encode(image)
            .unwrap()
    }

    /// Waits until `n` requests are buffered in the channel `raw` sends on.
    fn buffered(raw: &mpsc::Sender<GpuEncodeRequest>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while raw.max_capacity() - raw.capacity() != n {
            assert!(Instant::now() < deadline, "{n} requests were never queued");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A panic fails the tile in progress; the queued tiles and every later
    /// one are encoded by the fallback, byte for byte what ISPC makes; the
    /// health is Failed with the panic's message. (Its one WARN is counted
    /// in `tests/gpu_fallback_warning.rs`, a process of its own.)
    #[test]
    fn after_a_panic_the_queue_and_every_later_tile_are_the_ispc_encode() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let raw = tx.clone();
        let health = SharedGpuHealth::new();
        let channel = Arc::new(GpuEncoderChannel::new(tx, health.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let processor = FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("Device::poll: device lost")
        });
        // Distinct images, so each caller's file can be checked.
        let sizes = [16, 32, 48];

        let results = std::thread::scope(|scope| {
            let callers: Vec<_> = sizes
                .iter()
                .map(|&size| {
                    let channel = Arc::clone(&channel);
                    scope.spawn(move || (size, gpu_encoder(&channel).encode(varied(size, size))))
                })
                .collect();
            // Every request is queued before the worker starts.
            buffered(&raw, sizes.len());
            let worker = scope
                .spawn(|| run_worker_loop_here(processor, rx, health.clone(), HangRule::default()));
            let results: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
            worker.join().unwrap();
            results
        });

        let mut failed = 0;
        for (size, result) in results {
            match result {
                Err(e) => {
                    assert!(e.to_string().contains("GPU worker panicked"), "{e}");
                    failed += 1;
                }
                Ok(dds) => assert!(dds == ispc_encode(varied(size, size)), "{size}: bytes"),
            }
        }
        assert_eq!(failed, 1, "only the tile in progress fails");

        let later = gpu_encoder(&channel).encode(varied(64, 64)).unwrap();
        assert!(later == ispc_encode(varied(64, 64)), "a later tile: bytes");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "nothing more reached the GPU"
        );

        assert_eq!(
            health.failure(),
            Some("GPU worker panicked: Device::poll: device lost")
        );

        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!(
            (s.tiles(), s.failed, s.fallback),
            (0, 1, 3),
            "the fallback encodes are not GPU tiles"
        );
    }

    /// A send on a closed channel is encoded by the fallback, not failed.
    #[test]
    fn a_tile_sent_to_a_closed_channel_is_the_ispc_encode() {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(rx);
        let channel = Arc::new(handle(tx));
        let dds = gpu_encoder(&channel).encode(varied(64, 64)).unwrap();
        assert!(dds == ispc_encode(varied(64, 64)), "the bytes differ");
        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.failed, s.fallback), (0, 0, 1));
    }

    // =========================================================================
    // Stage timing through the channel (#419 item 2)
    // =========================================================================

    /// Encodes one 4 x 4 image through `channel` from a blocking thread.
    async fn encode_one(channel: &Arc<GpuEncoderChannel>) -> Result<Vec<u8>, DdsError> {
        let ch = Arc::clone(channel);
        let outcome = tokio::task::spawn_blocking(move || {
            ch.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC1, 1)
        })
        .await
        .expect("spawn_blocking should not panic");
        compressed(outcome)
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
        assert_eq!((s.tiles(), s.failed, s.fallback), (1, 1, 0));
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
        let _worker = spawn(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                release_rx.recv().unwrap();
                Ok(vec![0; 8])
            }),
            rx,
            CancellationToken::new(),
        );
        let raw = tx.clone();
        let channel = Arc::new(handle(tx));
        let stats = Arc::clone(channel.stats());

        let callers: Vec<_> = (0..5)
            .map(|_| {
                let ch = Arc::clone(&channel);
                tokio::task::spawn_blocking(move || {
                    compressed(ch.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC1, 1))
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

    /// A caller whose send fails is not left counted, and its image is
    /// counted as handed back to the fallback, not as a failure (#420).
    #[tokio::test]
    async fn queue_depth_is_lowered_when_the_worker_is_gone() {
        let (channel, rx) = mock_channel();
        drop(rx);
        let channel = Arc::new(channel);

        returned_by(&channel, 4).await;

        assert_eq!(channel.stats().depth(), 0);
        let s = channel
            .stats()
            .take_sample()
            .expect("the request was counted");
        assert_eq!((s.depth_now, s.depth_max), (0, 1));
        assert_eq!(
            (s.tiles(), s.failed, s.fallback),
            (0, 0, 1),
            "the refused send is a fallback"
        );
    }

    /// Every request that comes back without a tile or an image is a failure
    /// in the sample: a worker gone before replying, and a handle shut down.
    #[tokio::test]
    async fn dropped_replies_and_a_shut_down_handle_count_as_failures() {
        // A worker that takes one request and drops it unanswered.
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let worker = tokio::task::spawn_blocking(move || drop(rx.blocking_recv()));
        let channel = Arc::new(handle(tx));
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
            match create_gpu_encoder_channel("integrated", shutdown) {
                Ok(r) => r,
                Err(_) => return,
            };

        // Verify channel is connected
        assert!(channel.is_connected());
        assert!(!channel.health().is_failed());

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
        match create_gpu_resources("integrated", &SharedGpuHealth::new()) {
            Ok((device, queue, compressor, _name)) => Some((device, queue, compressor)),
            Err(_) => None, // Skip if no GPU
        }
    }

    /// A channel on a real GPU worker.
    fn gpu_channel(
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: block_compression::GpuBlockCompressor,
    ) -> (GpuEncoderChannel, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            tokio_util::sync::CancellationToken::new(),
            health.clone(),
        );
        (GpuEncoderChannel::new(tx, health), worker)
    }

    /// Single request through the pipeline worker with overlap loop.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_single_request() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };
        let (channel, _worker) = gpu_channel(device, queue, compressor);

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();

        let data = compressed(result).expect("pipeline compress should succeed");
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
        let (channel, _worker) = gpu_channel(device, queue, compressor);
        let channel = Arc::new(channel);

        for i in 0..3 {
            let ch = Arc::clone(&channel);
            let result = tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
            })
            .await
            .unwrap();

            let data =
                compressed(result).unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
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
        let (channel, _worker) = gpu_channel(device, queue, compressor);
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
            let data = compressed(result).expect("concurrent pipeline compress should succeed");
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
        let (channel, _worker) = gpu_channel(device, queue, compressor);
        let channel = Arc::new(channel);

        // BC1: 8 bytes per 4×4 block
        let ch = Arc::clone(&channel);
        let bc1 = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            ch.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();
        assert_eq!(compressed(bc1).unwrap().len(), 8);
    }

    /// Worker shuts down cleanly when channel is closed.
    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_pipeline_worker_shutdown() {
        let (device, queue, compressor) = match gpu_resources_or_skip() {
            Some(r) => r,
            None => return,
        };
        let (channel, worker) = gpu_channel(device, queue, compressor);

        // Drop sender to close channel
        drop(channel);

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
        let (channel, _worker) = gpu_channel(device, queue, compressor);
        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4096, 4096);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1)
        })
        .await
        .unwrap();

        let data = compressed(result).expect("GPU compression should succeed");
        // BC1: 1024×1024 blocks × 8 bytes = 8388608 bytes
        assert_eq!(data.len(), 8_388_608);
    }
}
