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
//! with processors that panic, fail, wait on a gate or record order, so the
//! loop is covered without a GPU. The loop reads the time on a [`Clock`]:
//! tests give it one that moves only when a fake processor advances it, and
//! the rules it applies ([`HangRule`], the miss count, what becomes of the
//! processor) are tested with plain values.
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
//! device ([`HangRule`]). Then the worker hands its whole queue back, both
//! lanes' (close both channels, answer every request in them) and ends, and
//! the channels hand every later image back without sending it. A tile past
//! its deadline on a GPU not yet failed hands back only the demand requests
//! queued behind it. A demand request the worker takes after it waited
//! longer than the deadline in the queue, from its caller's send, is handed
//! back too ([`HangRule::waited_out`]); that is not a miss and never fails
//! the GPU. So no demand caller waits much longer than the deadline plus one
//! tile: the tile that was in progress when its wait passed the deadline, or
//! its own. Prefetch callers have no deadline (see below). The one tile already inside the processor when the GPU failed is
//! lost and fails as before.
//!
//! # Two lanes, one GPU (#421)
//!
//! The demand lane (FUSE) and the prefetch lane each send on a channel of
//! their own ([`GpuLane`], [`gpu_lanes`]), built apart so nothing on the
//! demand side can name the prefetch channel and no priority travels with a
//! request. The worker reads both ([`WorkerQueues`]): whenever it is free it
//! takes a demand request, and a prefetch request only while the demand
//! channel is empty. The tile in progress is never preempted, so the most a
//! prefetch tile can delay a demand tile is the one prefetch tile the
//! worker may already be encoding when the demand tile arrives: that one
//! tile is the residual of the no-harm contract in GPU mode. Prefetch may
//! starve under sustained demand; that is the intended trade.
//!
//! The deadline machinery that exists for the FUSE timeout applies to the
//! demand channel only: a demand request that waited past the deadline is
//! handed back ([`HangRule::waited_out`]), and a late tile hands back the
//! demand requests queued behind it. Prefetch has no deadline: a prefetch
//! request waits for the GPU however long demand keeps it busy, bounded by
//! its channel's capacity (the prefetch lane's CPU pool). When the GPU fails
//! or the worker shuts down, both channels are closed and every request in
//! either is answered.

mod inner {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use futures::future::BoxFuture;
    use tracing::Instrument;

    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use image::RgbaImage;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::dds::compressor::{ChainOutcome, MipmapCompressor};
    use crate::dds::gpu_health::SharedGpuHealth;
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::layout;
    use crate::dds::{DdsError, DdsFormat};

    /// Bounded channel capacity for the demand lane's GPU encode requests.
    /// The prefetch lane's channel is bounded by its CPU pool instead
    /// ([`gpu_lanes`]).
    pub const CHANNEL_CAPACITY: usize = 32;

    /// Which executor lane a GPU channel serves (#421). The worker takes a
    /// demand request whenever it is free, and a prefetch request only while
    /// the demand channel is empty.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum GpuLane {
        /// FUSE: X-Plane is waiting for the tile.
        Demand,
        /// Prefetch: nobody is waiting yet.
        Prefetch,
    }

    impl GpuLane {
        /// The lane's name in the log.
        pub fn name(self) -> &'static str {
            match self {
                Self::Demand => "demand",
                Self::Prefetch => "prefetch",
            }
        }

        /// Whether the lane's requests have a deadline: only demand, for
        /// the FUSE timeout (#421). The queue-age valve
        /// ([`HangRule::waited_out`]) and the hand-back of the queue behind
        /// a late tile apply to such a lane only; a prefetch request waits
        /// for the GPU.
        pub fn has_deadline(self) -> bool {
            matches!(self, Self::Demand)
        }
    }

    /// What one of the worker's channels holds when it looks (#421).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum QueueState {
        /// At least one request is buffered.
        Ready,
        /// Nothing buffered, and a sender may still send.
        Empty,
        /// Nothing buffered, and every sender is gone.
        Closed,
    }

    /// What the worker does next, by [`next_take`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum NextTake {
        /// Take the next request of this lane.
        Take(GpuLane),
        /// Wait for a request.
        Wait,
        /// Both lanes are closed and empty: the loop ends.
        End,
    }

    /// The lane rule (#421): a demand request whenever one is buffered, a
    /// prefetch request only while the demand channel holds none, and the
    /// end once both channels are closed and empty.
    pub fn next_take(demand: QueueState, prefetch: QueueState) -> NextTake {
        match (demand, prefetch) {
            (QueueState::Ready, _) => NextTake::Take(GpuLane::Demand),
            (_, QueueState::Ready) => NextTake::Take(GpuLane::Prefetch),
            (QueueState::Closed, QueueState::Closed) => NextTake::End,
            _ => NextTake::Wait,
        }
    }

    /// What the worker does with a request it took, by [`admit`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Admission {
        /// Its job was cancelled before the tile started: answer
        /// [`GpuEncodeReply::Cancelled`], encode nothing (#422).
        Skip,
        /// It waited past the deadline on a lane that has one: hand its
        /// image back for the CPU (#420, #421).
        HandBack,
        /// Its caller left on a cancel: nobody is waiting, drop it (#422).
        CallerLeft,
        /// Encode it: the worker has claimed it ([`RequestClaim::start`]).
        Start,
    }

    /// The worker's decision on a request taken from `lane` after waiting
    /// `queue_wait`, whose job is `job_cancelled`, under `rule`. A request
    /// still queued in `claim` is claimed for the worker when the answer is
    /// [`Admission::Start`]; none of the others is a miss.
    pub fn admit(
        lane: GpuLane,
        job_cancelled: bool,
        queue_wait: Duration,
        rule: &HangRule,
        claim: &RequestClaim,
    ) -> Admission {
        if job_cancelled {
            Admission::Skip
        } else if lane.has_deadline() && rule.waited_out(queue_wait) {
            Admission::HandBack
        } else if claim.start() {
            Admission::Start
        } else {
            Admission::CallerLeft
        }
    }

    /// The worker's two inputs (#421): it takes from `demand` whenever it is
    /// free, and from `prefetch` only while `demand` is empty.
    pub struct WorkerQueues {
        /// The demand lane's requests.
        pub demand: mpsc::Receiver<GpuEncodeRequest>,
        /// The prefetch lane's requests.
        pub prefetch: mpsc::Receiver<GpuEncodeRequest>,
    }

    /// What [`WorkerQueues::take`] found.
    enum Taken {
        Request(GpuLane, GpuEncodeRequest),
        /// Nothing queued, and at least one channel still open.
        Empty,
        /// Both channels closed and empty: every sender is gone.
        Closed,
    }

    impl WorkerQueues {
        /// The demand and prefetch receivers.
        pub fn new(
            demand: mpsc::Receiver<GpuEncodeRequest>,
            prefetch: mpsc::Receiver<GpuEncodeRequest>,
        ) -> Self {
            Self { demand, prefetch }
        }

        /// The next request by the lane rule ([`next_take`]).
        fn take(&mut self) -> Taken {
            match next_take(state(&self.demand), state(&self.prefetch)) {
                NextTake::Take(lane) => match self.lane(lane).try_recv() {
                    Ok(request) => Taken::Request(lane, request),
                    // The one reader: a buffered request is still there.
                    Err(_) => Taken::Empty,
                },
                NextTake::Wait => Taken::Empty,
                NextTake::End => Taken::Closed,
            }
        }

        fn lane(&mut self, lane: GpuLane) -> &mut mpsc::Receiver<GpuEncodeRequest> {
            match lane {
                GpuLane::Demand => &mut self.demand,
                GpuLane::Prefetch => &mut self.prefetch,
            }
        }

        /// Closes both channels and gives every request in either to
        /// `answer`, until no sender of either holds a slot (#464): both are
        /// closed first, so no later send on either succeeds, then each is
        /// read to its end. Returns how many requests were answered.
        ///
        /// Must be called off any runtime, on the worker's own thread.
        fn drain(&mut self, mut answer: impl FnMut(GpuEncodeRequest)) -> usize {
            self.demand.close();
            self.prefetch.close();
            drain(&mut self.demand, &mut answer) + drain(&mut self.prefetch, &mut answer)
        }
    }

    /// What `rx` holds now, without taking anything from it.
    fn state(rx: &mpsc::Receiver<GpuEncodeRequest>) -> QueueState {
        if !rx.is_empty() {
            QueueState::Ready
        } else if rx.is_closed() {
            QueueState::Closed
        } else {
            QueueState::Empty
        }
    }

    impl From<mpsc::Receiver<GpuEncodeRequest>> for WorkerQueues {
        /// One demand channel and no prefetch: the prefetch queue is closed
        /// from the start. For tests of the loop that need one lane only.
        fn from(demand: mpsc::Receiver<GpuEncodeRequest>) -> Self {
            let (_, prefetch) = mpsc::channel(1);
            Self { demand, prefetch }
        }
    }

    /// The two lanes' channel handles (#421), each with its own sender and
    /// stats, sharing the worker's health and shutdown token.
    pub struct GpuEncoderChannels {
        /// What the demand lane's encoder sends on.
        pub demand: GpuEncoderChannel,
        /// What the prefetch lane's encoder sends on.
        pub prefetch: GpuEncoderChannel,
    }

    /// Two channels for one worker (#421): the demand channel holds
    /// [`CHANNEL_CAPACITY`] requests and the prefetch channel
    /// `prefetch_capacity` (at least one), the prefetch lane's CPU pool in
    /// the service, as no more of its tasks can be encoding at once. Returns
    /// the handles and the worker's side.
    pub fn gpu_lanes(
        prefetch_capacity: usize,
        health: SharedGpuHealth,
        shutdown: CancellationToken,
    ) -> (GpuEncoderChannels, WorkerQueues) {
        let (demand_tx, demand_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (prefetch_tx, prefetch_rx) = mpsc::channel(prefetch_capacity.max(1));
        let channels = GpuEncoderChannels {
            demand: GpuEncoderChannel::new(demand_tx, health.clone(), shutdown.clone()),
            prefetch: GpuEncoderChannel::new(prefetch_tx, health, shutdown),
        };
        (channels, WorkerQueues::new(demand_rx, prefetch_rx))
    }

    /// The failure every request still queued at shutdown is answered with.
    const SHUT_DOWN: &str = "GPU worker shut down";

    /// The GPU's failure when its worker refuses a send while it is not
    /// shut down.
    const WORKER_STOPPED: &str = "GPU worker stopped unexpectedly";

    /// The worker thread's name, in `top` and profilers (#423).
    pub const WORKER_THREAD: &str = "xel-gpu-encoder";

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
    /// [`MISSES_BEFORE_FAILED`]. Both times are read on the worker's
    /// [`Clock`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HangRule {
        /// The time one tile may take, from the worker taking it.
        pub deadline: Duration,
        /// Missed tiles in a row that fail the GPU.
        pub misses: u32,
    }

    impl HangRule {
        /// Whether a request that waited `queue_wait` in the queue, from its
        /// caller starting the send, is handed back unprocessed when the
        /// worker takes it: past the [`deadline`](Self::deadline). Such a
        /// request is not a miss and does not fail the GPU; it bounds every
        /// demand caller's wait to about the deadline plus one tile, however
        /// deep the queue. The worker applies it to the demand channel only:
        /// it exists for the FUSE timeout, and a prefetch request has no
        /// deadline (#421). The one place this threshold is decided.
        pub fn waited_out(&self, queue_wait: Duration) -> bool {
            queue_wait > self.deadline
        }

        /// Whether a tile whose processor call took `took`, from the worker
        /// taking it, missed the [`deadline`](Self::deadline): at it or past
        /// it. The one place this threshold is decided.
        pub fn is_late(&self, took: Duration) -> bool {
            took >= self.deadline
        }
    }

    impl Default for HangRule {
        fn default() -> Self {
            Self {
                deadline: TILE_DEADLINE,
                misses: MISSES_BEFORE_FAILED,
            }
        }
    }

    /// Where the worker loop and [`GpuEncoderChannel`] read the time that
    /// [`HangRule`] judges: a request's `sent_at`, when the worker takes it,
    /// and when the processor returns. The channel and its worker must share
    /// one clock.
    ///
    /// Production uses [`SystemClock`], the default of
    /// [`spawn_tile_worker`] and [`GpuEncoderChannel::new`]. Tests give both
    /// a clock they advance themselves ([`spawn_tile_worker_with_clock`],
    /// [`GpuEncoderChannel::with_clock`]), so a tile is late, or a request
    /// waited out, because the test said so and never because the machine
    /// was slow.
    pub trait Clock: Send + Sync {
        /// The current instant on this clock.
        fn now(&self) -> Instant;
    }

    /// The clock the worker and the channel share.
    pub type SharedClock = Arc<dyn Clock>;

    /// The real clock: [`Instant::now`].
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SystemClock;

    impl Clock for SystemClock {
        fn now(&self) -> Instant {
            Instant::now()
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
        /// is the idle worker's wake-up alone: it blocks on its channels
        /// rather than poll them (#423).
        pub queue_wait: Duration,
        /// CPU mip generation ([`MipmapStream`](crate::dds::MipmapStream))
        /// and the padding of the sub-block tail. The stream works one level
        /// ahead: taking level `i` computes level `i + 1`.
        pub downsample: Duration,
        /// Creating the output and readback buffers and each level's
        /// texture, on the first tile of a size only (they are kept for the
        /// next, #423), and `queue.write_texture`, which copies the level
        /// into a wgpu staging buffer.
        pub upload: Duration,
        /// Command recording, submit and the blocking poll: GPU and driver.
        pub gpu: Duration,
        /// Waiting for the buffer map, and copying the mapped range into the
        /// level's slot of the DDS file: the payload's one copy (#423).
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
        /// When the caller started to send: the start of the queue wait, on
        /// the worker's [`Clock`].
        pub sent_at: Instant,
        /// The DDS file the tile is written into (#423): allocated once by
        /// the encoder at its exact size, the header written; the worker
        /// writes the chain after [`layout::HEADER_LEN`] in place and sends
        /// the same buffer back.
        pub output: Vec<u8>,
        /// The job's cancellation token (#422): the worker skips a request
        /// whose job is cancelled before it starts the tile, answering
        /// [`GpuEncodeReply::Cancelled`].
        pub cancel: CancellationToken,
        /// Whether the worker started the tile or its caller left (#422).
        pub claim: RequestClaim,
        /// One-shot channel for returning the compressed data (or error).
        pub response: oneshot::Sender<GpuEncodeReply>,
    }

    impl GpuEncodeRequest {
        /// A request for `image`, written into the DDS file `output`, sent at
        /// `sent_at` on the worker's [`Clock`], for a job whose token is
        /// `cancel`, and the receiver of its reply.
        pub fn new(
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
            output: Vec<u8>,
            sent_at: Instant,
            cancel: CancellationToken,
        ) -> (Self, oneshot::Receiver<GpuEncodeReply>) {
            let (response, reply) = oneshot::channel();
            let request = Self {
                image,
                format,
                mipmap_count,
                sent_at,
                output,
                cancel,
                claim: RequestClaim::default(),
                response,
            };
            (request, reply)
        }
    }

    /// Who has a queued request (#422): the worker, once it starts the tile,
    /// or its caller, once it leaves on a cancel. Whichever claims it first
    /// has it, so a tile is never encoded after its caller left, and a
    /// caller never leaves a tile the GPU is already encoding: that tile is
    /// paid for, and its caller waits for it and caches it (#385).
    #[derive(Clone, Debug, Default)]
    pub struct RequestClaim(Arc<AtomicU8>);

    impl RequestClaim {
        const QUEUED: u8 = 0;
        const STARTED: u8 = 1;
        const ABANDONED: u8 = 2;

        /// The worker's claim: `true` if it may start the tile, `false` if
        /// the caller left.
        pub fn start(&self) -> bool {
            self.claim(Self::STARTED)
        }

        /// The caller's claim: `true` if it may leave, `false` if the worker
        /// already started the tile.
        pub fn abandon(&self) -> bool {
            self.claim(Self::ABANDONED)
        }

        /// Whether the worker started the tile.
        pub fn is_started(&self) -> bool {
            self.0.load(Ordering::Acquire) == Self::STARTED
        }

        fn claim(&self, by: u8) -> bool {
            self.0
                .compare_exchange(Self::QUEUED, by, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        }
    }

    /// The worker's answer to one [`GpuEncodeRequest`].
    pub enum GpuEncodeReply {
        /// The processor ran on the tile.
        Processed {
            /// The request's DDS file, every level written back to back
            /// after the header, or why not.
            result: Result<Vec<u8>, DdsError>,
            /// The stages as far as the tile got.
            timing: GpuTileTiming,
        },
        /// The GPU did not take the tile: the image comes back unchanged for
        /// the caller to encode on the CPU (#420).
        Returned {
            /// The request's image, as it was sent.
            image: RgbaImage,
            /// Why: the GPU's failure, a tile ahead past its deadline, or
            /// its own wait in the queue past the deadline.
            reason: String,
        },
        /// The request's job was cancelled before the worker took it, so the
        /// tile was not encoded (#422).
        Cancelled,
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
                Self::Cancelled => f.write_str("Cancelled"),
            }
        }
    }

    /// One tile's work for the worker loop: the full mip chain of `image`,
    /// compressed level by level into `payload`, largest first, each level
    /// in its slot ([`layout::level_len`] bytes after the ones before it).
    /// The levels must fill `payload` exactly; anything else is an error
    /// (#423).
    ///
    /// Implementations fill the stages of `timing` they perform; the loop
    /// owns `queue_wait` and `process`. A processor should give up at
    /// `deadline` (the loop's [`HangRule`]), as [`WgpuTileProcessor`] does by
    /// giving each level's poll what remains of it; the loop counts a call
    /// that ends at or past it as a miss either way, and an error in time as
    /// one too. A panic is caught by the loop (see [`spawn_tile_worker`]).
    ///
    /// Must be `Send`: the loop moves it onto the worker's thread, where it
    /// stays.
    pub trait TileProcessor: Send {
        /// Compresses `levels` mip levels of `image` in `format` into
        /// `payload`, giving up at `deadline`, and fills the stages of
        /// `timing` it performs.
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
            payload: &mut [u8],
        ) -> Result<(), DdsError>;
    }

    /// Sender-side handle for submitting GPU encode requests.
    ///
    /// Implements [`MipmapCompressor`] — receives the full source image and
    /// returns compressed data for all mipmap levels. The GPU worker owns
    /// the mipmap iteration internally, eliminating per-level channel round-trips.
    ///
    /// The caller awaits the worker (#422,
    /// [`submit_mipmap_chain`](MipmapCompressor::submit_mipmap_chain)): no
    /// thread is parked while its tile waits. A caller whose job is
    /// cancelled leaves at once, unless the worker already started its tile
    /// ([`RequestClaim`]), which it then waits for; the worker skips a
    /// request whose job was cancelled before it took it.
    ///
    /// An image the GPU does not take comes back as
    /// [`ChainOutcome::Returned`]: while the worker's `health` is Failed
    /// (never sent), when the channel is closed (the request comes back with
    /// the send error), and when the worker hands it back (#420). A closed
    /// channel while the worker's shutdown token is not cancelled is a
    /// worker that stopped unexpectedly, and fails the health.
    pub struct GpuEncoderChannel {
        sender: mpsc::Sender<GpuEncodeRequest>,
        stats: Arc<GpuEncodeStats>,
        health: SharedGpuHealth,
        /// The worker's shutdown token: a send the worker refuses while it
        /// is not cancelled means the worker stopped unexpectedly.
        shutdown: CancellationToken,
        /// Stamps each request's `sent_at` and times its round trip: the
        /// worker's clock.
        clock: SharedClock,
    }

    /// What became of a request after the send.
    enum Sent {
        /// The channel was closed: the request came back with the error.
        Refused(RgbaImage),
        /// The worker dropped the reply unanswered.
        Dropped,
        /// The job was cancelled before the worker started the tile, and
        /// the caller left.
        Left,
        Replied(GpuEncodeReply),
    }

    impl GpuEncoderChannel {
        /// Create a new channel handle wrapping the given sender, with the
        /// health its worker publishes and the token that shuts the worker
        /// down.
        pub fn new(
            sender: mpsc::Sender<GpuEncodeRequest>,
            health: SharedGpuHealth,
            shutdown: CancellationToken,
        ) -> Self {
            Self {
                sender,
                stats: Arc::default(),
                health,
                shutdown,
                clock: Arc::new(SystemClock),
            }
        }

        /// The same handle on `clock`, which must be its worker's
        /// ([`spawn_tile_worker_with_clock`]): requests are stamped on it, and
        /// the worker measures their wait on it. Production keeps the
        /// default [`SystemClock`]; tests pass a clock they advance.
        pub fn with_clock(mut self, clock: SharedClock) -> Self {
            self.clock = clock;
            self
        }

        /// The GPU's health, as its worker and device publish it: the
        /// dashboard reads it every tick (#420).
        pub fn health(&self) -> &SharedGpuHealth {
            &self.health
        }

        /// The stage timings of the tiles answered through this channel,
        /// the requests that failed, were handed back or were cancelled,
        /// and its queue depth. Someone must take its samples
        /// ([`GpuEncodeStats::take_sample`]); in the service the per-minute
        /// log does.
        pub fn stats(&self) -> &Arc<GpuEncodeStats> {
            &self.stats
        }

        /// Returns `true` if the receiver end is still alive.
        ///
        /// Dropping every handle of both lanes ends the worker once it has
        /// served the requests already buffered. To stop it without serving
        /// its queue, stop it through its [`GpuWorkerHandle`] (the service
        /// does): between tiles it closes both channels, fails every request
        /// in them and every one whose sender already holds a slot (#464),
        /// then ends.
        pub fn is_connected(&self) -> bool {
            !self.sender.is_closed()
        }

        /// Sends one tile to the worker and awaits its answer (#422): the
        /// one implementation behind both forms of [`MipmapCompressor`].
        async fn encode_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
            output: Vec<u8>,
            cancel: CancellationToken,
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
            let sender = &self.sender;

            let (width, height) = image.dimensions();

            // Counted from before the send (which waits on a full channel)
            // until the reply, the failure or the caller's leaving.
            let in_channel = self.stats.enter();
            let sent_at = self.clock.now();
            let (request, mut resp_rx) =
                GpuEncodeRequest::new(image, format, mipmap_count, output, sent_at, cancel.clone());
            let claim = request.claim.clone();

            // A cancel while the channel is full leaves without sending: the
            // request, image and all, is dropped with the send.
            let sent = tokio::select! {
                biased;
                _ = cancel.cancelled() => Sent::Left,
                sent = sender.send(request) => match sent {
                    // The send error hands the request back, image and all.
                    Err(mpsc::error::SendError(request)) => Sent::Refused(request.image),
                    Ok(()) => tokio::select! {
                        biased;
                        reply = &mut resp_rx => reply.map_or(Sent::Dropped, Sent::Replied),
                        // Left only if the worker has not started the tile:
                        // a tile already being encoded is paid for (#385).
                        _ = cancel.cancelled() => if claim.abandon() {
                            Sent::Left
                        } else {
                            (&mut resp_rx).await.map_or(Sent::Dropped, Sent::Replied)
                        },
                    },
                },
            };
            let total = self.clock.now().saturating_duration_since(sent_at);
            drop(in_channel);
            let (result, timing) = match sent {
                Sent::Refused(image) => {
                    self.stats.record_fallback();
                    // The worker closes its channel when it is shut down or
                    // after failing the health. Neither: it stopped
                    // unexpectedly (a panic outside the loop's
                    // `catch_unwind`), and the GPU is failed, so the Status
                    // pane stops showing it and no later tile is sent.
                    if !self.shutdown.is_cancelled() {
                        self.health.fail(WORKER_STOPPED);
                    }
                    let reason = self.health.failure().unwrap_or("GPU worker is not running");
                    return Ok(ChainOutcome::Returned {
                        image,
                        reason: reason.to_string(),
                    });
                }
                Sent::Dropped => {
                    self.stats.record_failed();
                    return Err(DdsError::CompressionFailed(
                        "GPU worker dropped response channel".to_string(),
                    ));
                }
                Sent::Left | Sent::Replied(GpuEncodeReply::Cancelled) => {
                    self.stats.record_cancelled();
                    tracing::debug!(
                        width,
                        height,
                        "GPU encode: job cancelled before its tile started"
                    );
                    return Ok(ChainOutcome::Cancelled);
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
    }

    impl MipmapCompressor for GpuEncoderChannel {
        /// Blocks the calling thread on [`submit_mipmap_chain`] with a token
        /// never cancelled. Call it only where nothing is async (a test or a
        /// blocking thread): on a runtime worker thread it stalls the
        /// runtime. Production awaits [`submit_mipmap_chain`] (#422).
        ///
        /// [`submit_mipmap_chain`]: MipmapCompressor::submit_mipmap_chain
        fn compress_mipmap_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
            output: Vec<u8>,
        ) -> Result<ChainOutcome, DdsError> {
            pollster::block_on(self.submit_mipmap_chain(
                image,
                format,
                mipmap_count,
                output,
                CancellationToken::new(),
            ))
        }

        fn submit_mipmap_chain(
            &self,
            image: RgbaImage,
            format: DdsFormat,
            mipmap_count: usize,
            output: Vec<u8>,
            cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<ChainOutcome, DdsError>> {
            let span = tracing::debug_span!(
                target: "profiling",
                "gpu_encode",
                width = image.width(),
                height = image.height(),
                levels = mipmap_count,
            );
            Box::pin(
                self.encode_chain(image, format, mipmap_count, output, cancel)
                    .instrument(span),
            )
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

    /// One level's input texture, kept across tiles (#423).
    struct LevelTexture {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        width: u32,
        height: u32,
    }

    /// The GPU resources of one tile size, kept across tiles (#423): the
    /// output and readback buffers, sized for level 0 and reused by every
    /// level, and each level's texture and view, made the first time a tile
    /// of this size reaches that level. Held for the session: every tile
    /// of a session is the same size, so it is made once.
    struct TileResources {
        width: u32,
        height: u32,
        format: DdsFormat,
        output: wgpu::Buffer,
        readback: wgpu::Buffer,
        levels: Vec<LevelTexture>,
    }

    impl TileResources {
        fn new(device: &wgpu::Device, width: u32, height: u32, format: DdsFormat) -> Self {
            // Level 0 is the largest: smaller levels use a prefix.
            let size = compressed_size(width, height, format);
            let output = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tile-output"),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tile-readback"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            Self {
                width,
                height,
                format,
                output,
                readback,
                levels: Vec::new(),
            }
        }

        fn fits(&self, width: u32, height: u32, format: DdsFormat) -> bool {
            (self.width, self.height, self.format) == (width, height, format)
        }

        /// The texture of level `index`, `width` x `height` (padded to whole
        /// blocks), made now if no tile of this size has reached it yet.
        fn level(
            &mut self,
            device: &wgpu::Device,
            index: usize,
            width: u32,
            height: u32,
        ) -> &LevelTexture {
            let fits = |l: &LevelTexture| (l.width, l.height) == (width, height);
            if self.levels.get(index).is_some_and(|l| !fits(l)) {
                // Not reached: a size's levels are fixed. Made anew if so.
                self.levels.truncate(index);
            }
            if self.levels.len() <= index {
                // Levels are taken in order, so `index` is the next one.
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tile-input"),
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
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.levels.push(LevelTexture {
                    texture,
                    view,
                    width,
                    height,
                });
            }
            &self.levels[index]
        }
    }

    /// The production [`TileProcessor`]: the GPU device, its queue and the
    /// `block_compression` kernels, owned by the worker thread, and the
    /// resources of the tile size it last encoded (#423).
    pub struct WgpuTileProcessor {
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: GpuBlockCompressor,
        resources: Option<TileResources>,
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
                resources: None,
            }
        }

        /// Process a full tile: generate mipmaps, compress each level on
        /// GPU, and copy each level from the mapped readback buffer straight
        /// into its slot of `payload` (#423).
        ///
        /// The worker owns the mipmap iteration via [`MipmapStream`]. The
        /// output and readback buffers and the level textures are kept
        /// across tiles of one size ([`TileResources`]). Each stage's wall
        /// time is added to `timing`.
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
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            let width = image.width();
            let height = image.height();
            let variant = compression_variant(format)?;

            let Self {
                device,
                queue,
                compressor,
                resources,
            } = self;
            if !resources
                .as_ref()
                .is_some_and(|r| r.fits(width, height, format))
            {
                timed(
                    &mut timing.upload,
                    tracing::debug_span!(target: "profiling", "gpu_buffers"),
                    || *resources = Some(TileResources::new(device, width, height, format)),
                );
            }
            let resources = resources.as_mut().expect("made above");

            let mut slots = crate::dds::encoder::LevelSlots::new(payload, format);
            let mut stream = crate::dds::mipmap::MipmapStream::new(image, mipmap_count);

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

                timed(
                    &mut timing.upload,
                    tracing::debug_span!(target: "profiling", "gpu_upload", level = level_idx),
                    || {
                        let texture = &resources.level(device, level_idx, lw, lh).texture;
                        queue.write_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture,
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
                    },
                );
                let texture_view = &resources.levels[level_idx].view;
                let output_buffer = &resources.output;
                let readback_buffer = &resources.readback;

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
                            texture_view,
                            lw,
                            lh,
                            output_buffer,
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
                            output_buffer,
                            0,
                            readback_buffer,
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
                        // The payload's one copy: straight into the level's
                        // slot of the DDS file (#423).
                        let copied = slots.next(lw, lh).map(|slot| slot.copy_from_slice(&data));
                        drop(data);
                        readback_buffer.unmap();
                        copied
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

            slots.finish()
        }
    }

    /// What [`WgpuTileProcessor`] does with its kept [`TileResources`] once
    /// a tile is over ([`AfterTile::of`]).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum AfterTile {
        /// Keep them for the next tile of the size (#423).
        Keep,
        /// Drop them: the next tile makes fresh ones.
        Discard,
    }

    impl AfterTile {
        /// Keep after a tile that succeeded, discard after any error.
        ///
        /// A failed tile can leave the readback buffer mapped or with a map
        /// still pending: a poll that timed out returns before the map
        /// completes, and a failed map or mapped range returns before
        /// `unmap`. Every later submit that copies into that buffer then
        /// fails (`BufferStillMapped`), so one late tile became three misses
        /// in a row and turned the GPU off for the session, where fresh
        /// buffers per tile let it recover (#420). Which errors leave it so
        /// is the driver's business: any error discards, and the next tile
        /// pays one allocation, as every tile did before #423.
        pub(super) fn of(outcome: &Result<(), DdsError>) -> Self {
            match outcome {
                Ok(()) => Self::Keep,
                Err(_) => Self::Discard,
            }
        }
    }

    impl TileProcessor for WgpuTileProcessor {
        /// [`process_tile`](Self::process_tile), then the kept resources are
        /// dropped if the tile failed ([`AfterTile::of`]).
        ///
        /// Dropping them while the late tile's work may still be on the GPU
        /// is safe: wgpu-core holds every buffer and texture a submission
        /// uses until that submission completes (its trackers keep a
        /// reference) and destroys the raw resource only when the last
        /// reference goes; dropping a buffer whose map is pending aborts the
        /// map, whose callback then sends into a channel nobody reads.
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            let outcome = self.process_tile(image, format, levels, deadline, timing, payload);
            if AfterTile::of(&outcome) == AfterTile::Discard && self.resources.take().is_some() {
                tracing::debug!("GPU worker dropped its kept resources after a failed tile");
            }
            outcome
        }
    }

    /// What the worker does with its processor once the loop ends.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Ending {
        /// Drop it, releasing the GPU.
        Release,
        /// Leave it undropped for the rest of the process (see
        /// [`run_worker_loop`]).
        Abandon,
    }

    /// Why the worker loop ended.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum LoopEnd {
        /// The shutdown token was cancelled.
        ShutDown,
        /// Every sender was dropped and the buffered requests were served.
        Closed,
        /// The health was failed from outside the loop: the device was lost.
        DeviceLost,
        /// The processor panicked.
        Panicked,
        /// [`HangRule::misses`] tiles in a row missed; `hung` when any of
        /// them ran late rather than failed in time ([`Misses`]).
        Missed {
            /// Whether any of the misses ran late.
            hung: bool,
        },
    }

    impl LoopEnd {
        /// What becomes of the processor after this end: it is left
        /// undropped after a panic or misses with a late tile among them
        /// (the GPU may never finish its work), and dropped after every
        /// other end: a shutdown, a closed channel, a device lost (wgpu's
        /// wait for the queue returns at once on a lost device) and tiles
        /// that all failed in time.
        pub(super) fn ending(self) -> Ending {
            match self {
                Self::Panicked | Self::Missed { hung: true } => Ending::Abandon,
                Self::ShutDown | Self::Closed | Self::DeviceLost | Self::Missed { hung: false } => {
                    Ending::Release
                }
            }
        }
    }

    /// The worker loop, on the worker's thread; [`spawn_tile_worker`] says
    /// what ends it and how each request is answered.
    ///
    /// When the GPU failed by a hang or a panic, the processor is leaked
    /// rather than dropped (#420, [`LoopEnd::ending`]). Dropping
    /// [`WgpuTileProcessor`] drops the last handles to its device and queue,
    /// and wgpu-core's `Queue` drop waits for the GPU to go idle with no
    /// timeout: on a GPU that stopped making progress that pins this thread
    /// for good, and the runtime's drop at exit waits for it, so XEarthLayer
    /// could not quit. The same drop panics on any error but a lost device,
    /// outside the loop's `catch_unwind`, and after a panic nothing is known
    /// of the device. The GPU is never used again in the session, and the
    /// process's exit reclaims what is leaked.
    pub(super) fn run_worker_loop<P: TileProcessor>(
        mut processor: P,
        mut queues: WorkerQueues,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
        clock: SharedClock,
    ) {
        let end = serve(
            &mut processor,
            &mut queues,
            &shutdown,
            &health,
            rule,
            &*clock,
        );
        tracing::info!(?end, "GPU pipeline worker shutting down");
        match end.ending() {
            Ending::Release => drop(processor),
            Ending::Abandon => {
                tracing::debug!("GPU worker leaves the failed GPU's device undropped");
                std::mem::forget(processor);
            }
        }
    }

    /// The loop itself: serves requests until something ends it, and says
    /// why. The decisions are [`HangRule::waited_out`], [`Misses::judge`]
    /// and [`LoopEnd::ending`]; this acts on them.
    fn serve<P: TileProcessor>(
        processor: &mut P,
        queues: &mut WorkerQueues,
        shutdown: &CancellationToken,
        health: &SharedGpuHealth,
        rule: HangRule,
        clock: &dyn Clock,
    ) -> LoopEnd {
        let mut misses = Misses::default();
        loop {
            if shutdown.is_cancelled() {
                tracing::info!("GPU pipeline worker: shutdown signal received");
                let failed = queues.drain(|request| {
                    let _ = request
                        .response
                        .send(GpuEncodeReply::failed(SHUT_DOWN, GpuTileTiming::default()));
                });
                if failed > 0 {
                    tracing::debug!(failed, "GPU worker failed its queue at shutdown");
                }
                return LoopEnd::ShutDown;
            }
            // Failed from outside the loop: the device was lost.
            if let Some(reason) = health.failure() {
                hand_back_all(queues, reason);
                return LoopEnd::DeviceLost;
            }

            let (lane, request) = match queues.take() {
                Taken::Request(lane, request) => (lane, request),
                Taken::Empty => match wait_for_work(queues, shutdown, health) {
                    Woken::Request(lane, request) => (lane, request),
                    // A shutdown, a failure or a lane closed: the checks
                    // above and `take` see which.
                    Woken::Signal => continue,
                },
                Taken::Closed => return LoopEnd::Closed,
            };

            let taken = clock.now();
            let queue_wait = taken.saturating_duration_since(request.sent_at);
            match admit(
                lane,
                request.cancel.is_cancelled(),
                queue_wait,
                &rule,
                &request.claim,
            ) {
                Admission::Start => {}
                // A cancelled job's tile is not paid for yet: skipped, not
                // encoded (#422).
                Admission::Skip => {
                    tracing::debug!(
                        lane = lane.name(),
                        "GPU worker skipped a tile whose job was cancelled"
                    );
                    let _ = request.response.send(GpuEncodeReply::Cancelled);
                    continue;
                }
                Admission::CallerLeft => {
                    tracing::debug!(
                        lane = lane.name(),
                        "GPU worker skipped a tile whose caller left"
                    );
                    continue;
                }
                // The valve exists for the FUSE timeout: a prefetch request
                // has no deadline and waits for the GPU (#421). Not a miss:
                // the GPU never saw it.
                Admission::HandBack => {
                    let waited_ms = queue_wait.as_millis() as u64;
                    tracing::debug!(
                        waited_ms,
                        "GPU worker handed back a tile that waited too long"
                    );
                    hand_back(
                        request,
                        &format!(
                            "waited {waited_ms} ms in the queue, past the GPU's {} ms deadline",
                            rule.deadline.as_millis()
                        ),
                    );
                    continue;
                }
            }

            let GpuEncodeRequest {
                image,
                format,
                mipmap_count,
                sent_at: _,
                mut output,
                cancel: _,
                claim: _,
                response,
            } = request;

            let mut timing = GpuTileTiming {
                queue_wait,
                ..GpuTileTiming::default()
            };

            tracing::debug!(
                format = ?format,
                width = image.width(),
                height = image.height(),
                mipmap_count,
                lane = lane.name(),
                "GPU pipeline: processing full tile"
            );

            let tile_span = tracing::debug_span!(
                target: "profiling",
                "gpu_tile",
                width = image.width(),
                height = image.height(),
                levels = mipmap_count,
                lane = lane.name(),
            )
            .entered();
            let deadline = taken + rule.deadline;
            // The chain is written into the caller's file, after its
            // header, in place (#423).
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let payload = output.get_mut(layout::HEADER_LEN..).ok_or_else(|| {
                    DdsError::CompressionFailed(
                        "the DDS file is shorter than its header".to_string(),
                    )
                })?;
                processor.process(image, format, mipmap_count, deadline, &mut timing, payload)
            }))
            .map(|processed| processed.map(|()| output));
            timing.process = clock.now().saturating_duration_since(taken);
            drop(tile_span);

            let result = match result {
                Ok(result) => result,
                Err(panic_info) => {
                    let msg = crate::panic::panic_message(panic_info.as_ref());
                    let message = format!("GPU worker panicked: {msg}");
                    health.fail(message.clone());
                    // The image was inside the processor: this tile is lost.
                    let _ = response.send(GpuEncodeReply::failed(message, timing));
                    hand_back_all(queues, health.failure().unwrap_or_default());
                    return LoopEnd::Panicked;
                }
            };

            let error = result.as_ref().err().map(ToString::to_string);
            let verdict = misses.judge(rule, timing.process, error);
            let reply = GpuEncodeReply::Processed { result, timing };
            if verdict != Verdict::InTime {
                tracing::debug!(
                    misses = misses.count(),
                    ?verdict,
                    process_ms = timing.process.as_millis() as u64,
                    "GPU tile missed its deadline or failed"
                );
            }
            // On a miss the queue is handed back before the tile is
            // answered, so it is the queue that waited behind that tile,
            // never a request its caller sent after the reply (pinned by
            // `tests/gpu_hand_back_order.rs`).
            match verdict {
                Verdict::InTime => {
                    let _ = response.send(reply);
                }
                Verdict::Missed { late } => {
                    if late {
                        // The queue behind a slow tile goes to the CPU now
                        // rather than wait for the next one, on the lanes
                        // that have a deadline: prefetch waits (#421).
                        for lane in [GpuLane::Demand, GpuLane::Prefetch] {
                            if lane.has_deadline() {
                                hand_back_queued(
                                    queues.lane(lane),
                                    "a GPU tile ahead of it missed its deadline",
                                );
                            }
                        }
                    }
                    let _ = response.send(reply);
                }
                Verdict::Failed { reason, hung } => {
                    health.fail(reason);
                    hand_back_all(queues, health.failure().unwrap_or_default());
                    let _ = response.send(reply);
                    return LoopEnd::Missed { hung };
                }
            }
        }
    }

    /// What woke an idle worker ([`wait_for_work`]).
    pub(super) enum Woken {
        /// A request, the first to reach either empty channel; on a tie,
        /// demand's.
        Request(GpuLane, GpuEncodeRequest),
        /// The shutdown, the GPU's failure, or a lane's last sender gone.
        Signal,
    }

    /// Blocks the idle worker until either channel receives a request, the
    /// shutdown token is cancelled, the GPU fails or a lane closes (#423):
    /// no polling, so an idle worker never wakes on its own, and a request
    /// reaching it is taken at once. Called only when both channels are
    /// empty, so the first request to arrive is the one [`next_take`] would
    /// take; demand wins a tie (`biased`).
    ///
    /// Returns at once when both channels are closed: the last senders
    /// may go between the worker's look at the queues and this wait, and
    /// with neither channel left to wait on, only a shutdown or a failure
    /// would wake it, so the loop would never see that every sender is
    /// gone.
    ///
    /// tokio's channels and `CancellationToken` need no runtime to be
    /// awaited: the worker blocks on its own thread with `pollster`.
    pub(super) fn wait_for_work(
        queues: &mut WorkerQueues,
        shutdown: &CancellationToken,
        health: &SharedGpuHealth,
    ) -> Woken {
        let demand_open = state(&queues.demand) != QueueState::Closed;
        let prefetch_open = state(&queues.prefetch) != QueueState::Closed;
        if !demand_open && !prefetch_open {
            // `take` sees both closed and the loop ends.
            return Woken::Signal;
        }
        let WorkerQueues { demand, prefetch } = queues;
        let woken = |lane, request: Option<GpuEncodeRequest>| {
            request.map_or(Woken::Signal, |request| Woken::Request(lane, request))
        };
        pollster::block_on(async {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => Woken::Signal,
                _ = health.failed() => Woken::Signal,
                request = demand.recv(), if demand_open => woken(GpuLane::Demand, request),
                request = prefetch.recv(), if prefetch_open => woken(GpuLane::Prefetch, request),
            }
        })
    }

    /// What one tile the processor returned from means for the GPU
    /// ([`Misses::judge`]).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum Verdict {
        /// Succeeded in time: answer it and carry on; the misses are
        /// cleared.
        InTime,
        /// A miss, not yet enough to fail the GPU. A `late` one hands back
        /// the requests queued behind it.
        Missed {
            /// Whether it ran late rather than failed in time.
            late: bool,
        },
        /// The last of [`HangRule::misses`] in a row: the GPU is failed
        /// with `reason` and the loop ends ([`LoopEnd::Missed`]).
        Failed {
            /// The health's reason.
            reason: String,
            /// Whether any of the misses ran late.
            hung: bool,
        },
    }

    /// The worker's tiles in a row that missed: ended at or past the
    /// deadline, or failed in time. A tile that succeeds in time clears it.
    #[derive(Debug, Default)]
    pub(super) struct Misses {
        count: u32,
        /// How many of them failed in time, rather than ran late.
        failed: u32,
        /// The last in-time failure's error.
        last_error: Option<String>,
    }

    impl Misses {
        /// The misses in a row so far.
        pub(super) fn count(&self) -> u32 {
            self.count
        }

        /// Judges one tile by `rule`: its processor call took `took`, and
        /// failed with `error` if any. A late tile is a miss whatever its
        /// result (its error, if any, is the deadline's); a tile in time is a
        /// miss when it failed, and clears the misses when it succeeded.
        pub(super) fn judge(
            &mut self,
            rule: HangRule,
            took: Duration,
            error: Option<String>,
        ) -> Verdict {
            let late = rule.is_late(took);
            match (late, error) {
                (false, None) => {
                    *self = Self::default();
                    return Verdict::InTime;
                }
                (false, Some(error)) => {
                    self.failed += 1;
                    self.last_error = Some(error);
                }
                (true, _) => {}
            }
            self.count += 1;
            if self.count >= rule.misses {
                Verdict::Failed {
                    reason: self.reason(rule.deadline),
                    hung: self.hung(),
                }
            } else {
                Verdict::Missed { late }
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

    /// Closes both channels and hands back every request in either
    /// ([`WorkerQueues::drain`]), so no image is left behind with the
    /// receivers.
    ///
    /// Must be called off any runtime, on the worker's own thread.
    fn hand_back_all(queues: &mut WorkerQueues, reason: &str) {
        let handed = queues.drain(|request| hand_back(request, reason));
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
    /// Must be called off any runtime, on the worker's own thread.
    fn drain(
        rx: &mut mpsc::Receiver<GpuEncodeRequest>,
        answer: &mut impl FnMut(GpuEncodeRequest),
    ) -> usize {
        rx.close();
        let mut answered = 0;
        while let Some(request) = rx.blocking_recv() {
            answer(request);
            answered += 1;
        }
        answered
    }

    /// The worker thread's end, for [`GpuWorkerHandle`] to wait on: set
    /// when the thread's closure returns or unwinds.
    #[derive(Default)]
    struct Ended {
        flag: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
        /// Cancelled with the flag, for an async wait.
        token: CancellationToken,
    }

    /// Marks [`Ended`] when dropped, on every way out of the thread.
    struct EndGuard(Arc<Ended>);

    impl Drop for EndGuard {
        fn drop(&mut self) {
            if let Ok(mut flag) = self.0.flag.lock() {
                *flag = true;
            }
            self.0.changed.notify_all();
            self.0.token.cancel();
        }
    }

    /// The GPU worker's thread, named [`WORKER_THREAD`], and its shutdown
    /// token (#423): the one handle the service keeps to stop the worker
    /// and wait for it, from async code ([`stop`](Self::stop)) or from a
    /// `Drop` ([`stop_blocking`](Self::stop_blocking)).
    ///
    /// A wait never outlasts its bound: a worker in a tile ends after it,
    /// within the tile's deadline ([`HangRule`]), but a driver call that
    /// never returns would hold its thread for good, and the handle then
    /// leaves it, as the process exit reclaims it.
    pub struct GpuWorkerHandle {
        thread: Option<std::thread::JoinHandle<()>>,
        ended: Arc<Ended>,
        shutdown: CancellationToken,
    }

    impl GpuWorkerHandle {
        /// Whether the worker's thread has ended.
        pub fn is_finished(&self) -> bool {
            self.ended.token.is_cancelled()
        }

        /// Waits up to `within` for the worker to end, and joins its thread:
        /// `None` if it is still running, else how the thread ended (the
        /// loop itself never panics).
        pub async fn wait(&mut self, within: Duration) -> Option<std::thread::Result<()>> {
            tokio::time::timeout(within, self.ended.token.cancelled())
                .await
                .ok()?;
            Some(self.join())
        }

        /// [`wait`](Self::wait), blocking the calling thread.
        pub fn wait_blocking(&mut self, within: Duration) -> Option<std::thread::Result<()>> {
            let flag = self.ended.flag.lock().ok()?;
            let (flag, _) = self
                .ended
                .changed
                .wait_timeout_while(flag, within, |ended| !*ended)
                .ok()?;
            let ended = *flag;
            drop(flag);
            ended.then(|| self.join())
        }

        /// Cancels the worker's shutdown token, then [`wait`](Self::wait)s:
        /// the worker finishes the tile in progress, fails every request
        /// queued in either lane (#464) and ends.
        pub async fn stop(&mut self, within: Duration) -> Option<std::thread::Result<()>> {
            self.shutdown.cancel();
            self.wait(within).await
        }

        /// [`stop`](Self::stop), blocking the calling thread.
        pub fn stop_blocking(&mut self, within: Duration) -> Option<std::thread::Result<()>> {
            self.shutdown.cancel();
            self.wait_blocking(within)
        }

        fn join(&mut self) -> std::thread::Result<()> {
            self.thread
                .take()
                .map_or(Ok(()), std::thread::JoinHandle::join)
        }
    }

    /// Spawn the worker loop around `processor` on a thread of its own,
    /// named [`WORKER_THREAD`] (#423): out of the runtime's blocking pool,
    /// where it would hold a thread for the session. It reads `queues`: a
    /// demand request whenever the worker is free, a prefetch request only
    /// while the demand channel is empty (#421), and blocks while both are
    /// empty, never polling. A single receiver is a demand channel with no
    /// prefetch ([`WorkerQueues`]'s `From`). Each request is answered with
    /// the processor's result, or with its own image
    /// ([`GpuEncodeReply::Returned`]) when the GPU does not take it. The
    /// loop ends:
    ///
    /// - when the shutdown token is cancelled, at once if idle, else
    ///   between tiles: a tile in progress is finished and answered; every
    ///   queued request of either lane, and every one whose sender already
    ///   holds a slot, fails with "GPU worker shut down" (#464);
    /// - when every sender of both channels is dropped, after the buffered
    ///   requests;
    /// - when the GPU fails (#420): the processor panics (the request in
    ///   progress fails with the panic's message), `health` is failed from
    ///   outside (the device was lost), or `rule.misses` tiles in a row end
    ///   at or past `rule.deadline` or fail before it. `health` turns Failed
    ///   with the reason, and every queued request of either lane is handed
    ///   back.
    ///
    /// On the demand channel only (#421), a tile past its deadline that
    /// does not fail the GPU hands back the demand requests queued behind
    /// it, and a demand request taken after waiting past the deadline in the
    /// queue is handed back unprocessed ([`HangRule::waited_out`]). A
    /// prefetch request is processed however long it waited.
    ///
    /// The processor is dropped when the loop ends, except after a GPU
    /// failed by a hang or a panic, when it is leaked so that its drop
    /// cannot pin the thread (see `run_worker_loop`).
    ///
    /// The loop reads the time on [`SystemClock`]; see
    /// [`spawn_tile_worker_with_clock`].
    ///
    /// # Errors
    ///
    /// The OS refused a thread.
    pub fn try_spawn_tile_worker<P: TileProcessor + 'static>(
        processor: P,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
    ) -> std::io::Result<GpuWorkerHandle> {
        try_spawn_tile_worker_with_clock(
            processor,
            queues,
            shutdown,
            health,
            rule,
            Arc::new(SystemClock),
        )
    }

    /// [`try_spawn_tile_worker`] on `clock`: the loop measures each
    /// request's queue wait from its `sent_at` and each tile's time on it,
    /// and gives the processor its deadline on it. The requests' senders
    /// must stamp `sent_at` on the same clock
    /// ([`GpuEncoderChannel::with_clock`]).
    ///
    /// # Errors
    ///
    /// The OS refused a thread.
    pub fn try_spawn_tile_worker_with_clock<P: TileProcessor + 'static>(
        processor: P,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
        clock: SharedClock,
    ) -> std::io::Result<GpuWorkerHandle> {
        let queues = queues.into();
        let ended = Arc::new(Ended::default());
        let guard = EndGuard(Arc::clone(&ended));
        let token = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name(WORKER_THREAD.to_string())
            .spawn(move || {
                let _ended = guard;
                run_worker_loop(processor, queues, shutdown, health, rule, clock)
            })?;
        Ok(GpuWorkerHandle {
            thread: Some(thread),
            ended,
            shutdown: token,
        })
    }

    /// [`try_spawn_tile_worker`], panicking if the OS refuses a thread: for
    /// tests.
    pub fn spawn_tile_worker<P: TileProcessor + 'static>(
        processor: P,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
    ) -> GpuWorkerHandle {
        try_spawn_tile_worker(processor, queues, shutdown, health, rule)
            .expect("the OS starts the GPU worker's thread")
    }

    /// [`try_spawn_tile_worker_with_clock`], panicking if the OS refuses a
    /// thread: for tests.
    pub fn spawn_tile_worker_with_clock<P: TileProcessor + 'static>(
        processor: P,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        rule: HangRule,
        clock: SharedClock,
    ) -> GpuWorkerHandle {
        try_spawn_tile_worker_with_clock(processor, queues, shutdown, health, rule, clock)
            .expect("the OS starts the GPU worker's thread")
    }

    /// Spawn the GPU pipeline worker: [`try_spawn_tile_worker`] with a
    /// [`WgpuTileProcessor`].
    ///
    /// The worker receives full tile requests via the channel, generates
    /// mipmaps internally, and compresses each level on the GPU. One message
    /// in, one result out — no per-level channel round-trips. It gives up on
    /// the GPU by the default [`HangRule`].
    ///
    /// # Errors
    ///
    /// The OS refused a thread.
    pub fn spawn_gpu_worker(
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: GpuBlockCompressor,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
    ) -> Result<GpuWorkerHandle, DdsError> {
        try_spawn_tile_worker(
            WgpuTileProcessor::new(device, queue, compressor),
            queues,
            shutdown,
            health,
            HangRule::default(),
        )
        .map_err(|e| DdsError::CompressionFailed(format!("GPU worker thread: {e}")))
    }

    /// Create the two lanes' [`GpuEncoderChannel`]s ([`gpu_lanes`]) and the
    /// one full-tile streaming worker that reads them (#421).
    ///
    /// Initializes GPU resources and spawns the worker thread. Returns the
    /// channel handles, the worker's handle, and the selection: the
    /// adapter chosen (for display; the worker owns the device) with the
    /// host's other adapters for the encoder policy. Both channels'
    /// [`health`](GpuEncoderChannel::health) is the one the worker and the
    /// device's lost callback publish. The prefetch channel holds
    /// `prefetch_capacity` requests. Cancelling `shutdown` stops the worker,
    /// as [`GpuWorkerHandle::stop`] does.
    ///
    /// Nothing else is started: the per-minute `GPU encode sample` lines of
    /// the channels' [`stats`](GpuEncoderChannel::stats) are started by the
    /// service wiring (`service::builder::create_encoder`), so a test that
    /// samples the stats itself loses nothing to them.
    ///
    /// # Errors
    ///
    /// Returns `DdsError` if GPU initialization fails.
    pub fn create_gpu_encoder_channels(
        gpu_device: &str,
        prefetch_capacity: usize,
        shutdown: CancellationToken,
    ) -> Result<
        (
            GpuEncoderChannels,
            GpuWorkerHandle,
            crate::system::GpuSelection,
        ),
        DdsError,
    > {
        let health = SharedGpuHealth::new();
        let (device, queue, compressor, selection) =
            crate::dds::create_gpu_resources(gpu_device, &health)?;

        tracing::info!(adapter = %selection.adapter, "GPU pipeline worker starting");

        let (channels, queues) = gpu_lanes(prefetch_capacity, health.clone(), shutdown.clone());
        let handle = spawn_gpu_worker(device, queue, compressor, queues, shutdown, health)?;
        Ok((channels, handle, selection))
    }
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::inner::{AfterTile, Ending, LoopEnd, Misses, Verdict};
    use super::*;
    use crate::dds::compressor::tests::varied;
    use crate::dds::compressor::{ChainOutcome, MipmapCompressor};
    use crate::dds::gpu_health::{device_lost, GpuHealth, SharedGpuHealth};
    use crate::dds::gpu_stats::GpuEncodeStats;
    use crate::dds::{
        DdsEncoder, DdsError, DdsFormat, IspcCompressor, MipmapStream, SoftwareCompressor,
    };
    use image::RgbaImage;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    // =========================================================================
    // Time in these tests
    // =========================================================================
    //
    // No test here passes or fails on how fast the machine runs. The rules
    // (`HangRule`, `Misses`, `LoopEnd`) are tested with plain durations. The
    // loop tests run the real loop on a `FakeClock` that moves only when a
    // fake processor (or the test) advances it, so a tile is late, or a
    // request waited out, because the test said so. Real time appears only
    // as generous failure guards (`reply_of`, `loop_ends`, `waits_for`).

    /// The production rule: its numbers no longer matter to a test, since
    /// the clock is fake.
    const RULE: HangRule = HangRule {
        deadline: TILE_DEADLINE,
        misses: MISSES_BEFORE_FAILED,
    };
    /// A tile's time just past the deadline: late.
    const LATE: Duration = TILE_DEADLINE.saturating_add(Duration::from_millis(1));
    /// A tile's time just inside the deadline: in time.
    const IN_TIME: Duration = TILE_DEADLINE.saturating_sub(Duration::from_millis(1));
    /// The generous guard on a reply or a loop's end: never the thing under
    /// test, and far from any expected time.
    const GUARD: Duration = Duration::from_secs(5);

    /// A clock that moves only when advanced. Clones share the time, so the
    /// test, its processors and the channel see one clock.
    #[derive(Clone)]
    struct FakeClock {
        base: Instant,
        offset_ns: Arc<AtomicU64>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset_ns: Arc::default(),
            }
        }

        fn advance(&self, by: Duration) {
            self.offset_ns
                .fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
        }

        /// How far it has been advanced in all.
        fn advanced(&self) -> Duration {
            Duration::from_nanos(self.offset_ns.load(Ordering::SeqCst))
        }

        fn shared(&self) -> SharedClock {
            Arc::new(self.clone())
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + self.advanced()
        }
    }

    // =========================================================================
    // The rules, given values (#420)
    // =========================================================================

    #[test]
    fn the_default_rule_is_a_two_second_deadline_and_three_misses() {
        assert_eq!(TILE_DEADLINE, Duration::from_secs(2));
        assert_eq!(MISSES_BEFORE_FAILED, 3);
        assert_eq!(HangRule::default(), RULE);
    }

    /// A request is waited out strictly past the deadline.
    #[test]
    fn a_request_is_waited_out_past_the_deadline() {
        let ms = Duration::from_millis;
        for (deadline, queue_wait, waited_out) in [
            (ms(2000), Duration::ZERO, false),
            (ms(2000), ms(1999), false),
            (ms(2000), ms(2000), false),
            (ms(2000), ms(2001), true),
            (ms(2000), ms(60_000), true),
            (ms(1000), ms(1001), true),
            (ms(1000), ms(999), false),
        ] {
            let rule = HangRule {
                deadline,
                misses: 3,
            };
            assert_eq!(
                rule.waited_out(queue_wait),
                waited_out,
                "{queue_wait:?} in the queue against a {deadline:?} deadline"
            );
        }
    }

    /// A tile is late at the deadline or past it.
    #[test]
    fn a_tile_is_late_at_the_deadline_or_past_it() {
        let ms = Duration::from_millis;
        for (deadline, took, late) in [
            (ms(2000), Duration::ZERO, false),
            (ms(2000), ms(1999), false),
            (ms(2000), ms(2000), true),
            (ms(2000), ms(2001), true),
            (ms(1000), ms(1000), true),
            (ms(1000), ms(999), false),
        ] {
            let rule = HangRule {
                deadline,
                misses: 3,
            };
            assert_eq!(
                rule.is_late(took),
                late,
                "a tile of {took:?} against a {deadline:?} deadline"
            );
        }
    }

    /// One tile as [`Misses::judge`] sees it: its processor time, and its
    /// error if it failed.
    type Tile = (Duration, Option<&'static str>);

    /// The verdict on each tile of `tiles`, in order, from fresh misses.
    fn verdicts(rule: HangRule, tiles: &[Tile]) -> Vec<Verdict> {
        let mut misses = Misses::default();
        tiles
            .iter()
            .map(|&(took, error)| misses.judge(rule, took, error.map(str::to_string)))
            .collect()
    }

    fn failed(reason: &str, hung: bool) -> Verdict {
        Verdict::Failed {
            reason: reason.to_string(),
            hung,
        }
    }

    /// Given the misses so far and one more tile, the verdict: in time,
    /// a miss (late or failed in time), or the GPU failed with its reason
    /// and whether any miss ran late.
    #[test]
    fn misses_judge_each_tile() {
        use Verdict::{InTime, Missed};
        let late = Missed { late: true };
        let fail = Missed { late: false };
        let ok = (IN_TIME, None);
        let slow = (LATE, None);
        let deadline = (TILE_DEADLINE, None);
        let error = |e: &'static str| -> Tile { (Duration::ZERO, Some(e)) };
        let missed_3 = "3 tiles in a row missed the GPU's 2000 ms deadline";
        let cases: &[(&str, &[Tile], Vec<Verdict>)] = &[
            ("in time", &[ok], vec![InTime]),
            ("late", &[slow], vec![late.clone()]),
            ("at the deadline", &[deadline], vec![late.clone()]),
            ("failed in time", &[error("e")], vec![fail.clone()]),
            (
                "two misses and a late tile",
                &[slow, slow, slow],
                vec![late.clone(), late.clone(), failed(missed_3, true)],
            ),
            (
                "two misses and a tile at the deadline",
                &[slow, slow, deadline],
                vec![late.clone(), late.clone(), failed(missed_3, true)],
            ),
            (
                "two failures and a third",
                &[error("e1"), error("e2"), error("e3")],
                vec![
                    fail.clone(),
                    fail.clone(),
                    failed(
                        "3 tiles in a row failed on the GPU, the last with: e3",
                        false,
                    ),
                ],
            ),
            (
                "failed, late, failed",
                &[error("e1"), slow, error("e3")],
                vec![
                    fail.clone(),
                    late.clone(),
                    failed(
                        "3 tiles in a row failed on the GPU or missed its 2000 ms deadline, \
                         the last failure: e3",
                        true,
                    ),
                ],
            ),
            (
                "late tiles that also failed count as late",
                &[
                    (LATE, Some("poll timed out")),
                    (LATE, Some("poll timed out")),
                    (LATE, Some("poll timed out")),
                ],
                vec![late.clone(), late.clone(), failed(missed_3, true)],
            ),
            (
                "a tile in time clears the misses",
                &[slow, slow, ok, slow, slow],
                vec![
                    late.clone(),
                    late.clone(),
                    InTime,
                    late.clone(),
                    late.clone(),
                ],
            ),
            (
                "a tile in time clears the failures",
                &[error("e"), error("e"), ok, error("e"), error("e")],
                vec![
                    fail.clone(),
                    fail.clone(),
                    InTime,
                    fail.clone(),
                    fail.clone(),
                ],
            ),
            (
                "a tile just inside the deadline is in time",
                &[slow, slow, ok],
                vec![late.clone(), late.clone(), InTime],
            ),
        ];
        for (case, tiles, expected) in cases {
            assert_eq!(&verdicts(RULE, tiles), expected, "{case}");
        }
    }

    /// The number of misses that fail the GPU is the rule's.
    #[test]
    fn the_rule_says_how_many_misses_fail_the_gpu() {
        let slow = (LATE, None);
        let one = HangRule {
            deadline: TILE_DEADLINE,
            misses: 1,
        };
        assert_eq!(
            verdicts(one, &[slow]),
            [failed(
                "1 tiles in a row missed the GPU's 2000 ms deadline",
                true
            )]
        );
        let five = HangRule {
            deadline: TILE_DEADLINE,
            misses: 5,
        };
        let v = verdicts(five, &[slow; 5]);
        assert_eq!(v[3], Verdict::Missed { late: true });
        assert_eq!(
            v[4],
            failed("5 tiles in a row missed the GPU's 2000 ms deadline", true)
        );
    }

    /// Given how the loop ended, what becomes of the processor: left
    /// undropped after a panic or a hang, dropped otherwise.
    #[test]
    fn how_the_loop_ended_decides_whether_the_processor_is_dropped() {
        for (end, ending) in [
            (LoopEnd::ShutDown, Ending::Release),
            (LoopEnd::Closed, Ending::Release),
            (LoopEnd::DeviceLost, Ending::Release),
            (LoopEnd::Missed { hung: false }, Ending::Release),
            (LoopEnd::Missed { hung: true }, Ending::Abandon),
            (LoopEnd::Panicked, Ending::Abandon),
        ] {
            assert_eq!(end.ending(), ending, "{end:?}");
        }
    }

    /// Given how a tile ended, whether the GPU processor keeps its
    /// resources for the next (#423 review): only after a success. A tile
    /// that timed out, failed its map or failed in any other way leaves
    /// the next tile to fresh ones.
    #[test]
    fn the_kept_resources_survive_a_good_tile_only() {
        let failed = |message: &str| Err(DdsError::CompressionFailed(message.to_string()));
        for (case, outcome, after) in [
            ("encoded", Ok(()), AfterTile::Keep),
            (
                "poll timed out",
                failed("GPU poll timed out at the tile's deadline, level 0"),
                AfterTile::Discard,
            ),
            (
                "map failed",
                failed("buffer mapping failed: Buffer is still mapped"),
                AfterTile::Discard,
            ),
            (
                "mapped range",
                failed("mapped range unavailable: out of range"),
                AfterTile::Discard,
            ),
            (
                "poll failed",
                failed("GPU poll failed: lost"),
                AfterTile::Discard,
            ),
            (
                "chain short of its slots",
                failed("the chain does not fill the file"),
                AfterTile::Discard,
            ),
            (
                "unsupported format",
                Err(DdsError::UnsupportedFormat("BC3".to_string())),
                AfterTile::Discard,
            ),
            (
                "bad dimensions",
                Err(DdsError::InvalidDimensions(0, 4)),
                AfterTile::Discard,
            ),
        ] {
            assert_eq!(AfterTile::of(&outcome), after, "{case}");
        }
    }

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
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            crate::dds::encoder::compress_chain_into(
                &SoftwareCompressor,
                MipmapStream::new(image, levels),
                format,
                payload,
            )
        }
    }

    /// Tile work given as a closure, for processors that panic, take fake
    /// time, fail, wait on a gate or record order. Writes nothing into the
    /// payload: its zeros stand for the chain.
    struct FnProcessor<F>(F);

    impl<F> TileProcessor for FnProcessor<F>
    where
        F: FnMut(RgbaImage, &mut GpuTileTiming) -> Result<(), DdsError> + Send,
    {
        fn process(
            &mut self,
            image: RgbaImage,
            _format: DdsFormat,
            _levels: usize,
            _deadline: Instant,
            timing: &mut GpuTileTiming,
            _payload: &mut [u8],
        ) -> Result<(), DdsError> {
            (self.0)(image, timing)
        }
    }

    /// Every tile takes `took` on `clock`, counted in `calls`, and succeeds.
    fn taking(clock: &FakeClock, took: Duration, calls: &Arc<AtomicUsize>) -> impl TileProcessor {
        let (clock, calls) = (clock.clone(), Arc::clone(calls));
        FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
            calls.fetch_add(1, Ordering::SeqCst);
            clock.advance(took);
            Ok(())
        })
    }

    /// A processor that waits for the test's word before each tile, so the
    /// test can queue requests behind a tile the worker has already taken.
    /// A dropped sender lets every tile through. With `starts`, it reports
    /// each tile's width as it reaches the gate: the worker has then
    /// claimed the request and is inside the processor.
    struct Gated<P> {
        inner: P,
        gate: std::sync::mpsc::Receiver<()>,
        starts: Option<mpsc::UnboundedSender<u32>>,
    }

    impl<P: TileProcessor> TileProcessor for Gated<P> {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            if let Some(starts) = &self.starts {
                let _ = starts.send(image.width());
            }
            let _ = self.gate.recv();
            self.inner
                .process(image, format, levels, deadline, timing, payload)
        }
    }

    /// `inner` behind a gate, and the sender that lets one tile through
    /// per message.
    fn gated<P: TileProcessor>(inner: P) -> (Gated<P>, std::sync::mpsc::Sender<()>) {
        let (open, gate) = std::sync::mpsc::channel();
        let starts = None;
        (
            Gated {
                inner,
                gate,
                starts,
            },
            open,
        )
    }

    /// [`gated`], reporting each tile's width as it starts ([`next_start`]).
    fn gated_reporting<P: TileProcessor>(
        inner: P,
    ) -> (
        Gated<P>,
        std::sync::mpsc::Sender<()>,
        mpsc::UnboundedReceiver<u32>,
    ) {
        let (processor, open) = gated(inner);
        let (starts_tx, starts) = mpsc::unbounded_channel();
        let processor = Gated {
            starts: Some(starts_tx),
            ..processor
        };
        (processor, open, starts)
    }

    /// The width of the next tile the processor started, within the guard.
    async fn next_start(starts: &mut mpsc::UnboundedReceiver<u32>) -> u32 {
        tokio::time::timeout(GUARD, starts.recv())
            .await
            .expect("a tile starts within the guard")
            .expect("the worker is alive")
    }

    /// The worker loop around `processor` on `clock`, with the production
    /// rule.
    fn spawn_with<P: TileProcessor + 'static>(
        processor: P,
        queues: impl Into<WorkerQueues>,
        shutdown: CancellationToken,
        health: SharedGpuHealth,
        clock: &FakeClock,
    ) -> GpuWorkerHandle {
        spawn_tile_worker_with_clock(processor, queues, shutdown, health, RULE, clock.shared())
    }

    /// The DDS file of a `width` x `height` BC1 tile of `levels` levels,
    /// its header written as the encoder writes it.
    fn file(width: u32, height: u32, levels: usize) -> Vec<u8> {
        let levels = levels as u32;
        let len = crate::dds::layout::file_len(width, height, levels, DdsFormat::BC1);
        let mut file = vec![0; len];
        file[..crate::dds::layout::HEADER_LEN].copy_from_slice(
            &crate::dds::DdsHeader::new(width, height, levels, DdsFormat::BC1).to_bytes(),
        );
        file
    }

    /// The file of one `width` x 4 level: one BC1 block (8 bytes) per 4
    /// pixels of width after the header.
    fn file_len_of(width: u32) -> usize {
        crate::dds::layout::HEADER_LEN + 8 * width.div_ceil(4) as usize
    }

    /// A request stamped now on `clock`, whose image is `width` pixels wide
    /// so a processor can tell requests apart, and the receiver of its
    /// reply.
    fn request(
        clock: &FakeClock,
        width: u32,
    ) -> (GpuEncodeRequest, oneshot::Receiver<GpuEncodeReply>) {
        GpuEncodeRequest::new(
            RgbaImage::new(width, 4),
            DdsFormat::BC1,
            1,
            file(width, 4, 1),
            clock.now(),
            CancellationToken::new(),
        )
    }

    /// Queues requests of the given widths, stamped now on `clock`.
    fn queue(
        tx: &mpsc::Sender<GpuEncodeRequest>,
        clock: &FakeClock,
        widths: &[u32],
    ) -> Vec<oneshot::Receiver<GpuEncodeReply>> {
        widths
            .iter()
            .map(|&w| {
                let (req, reply) = request(clock, w);
                tx.try_send(req).expect("channel has room");
                reply
            })
            .collect()
    }

    /// Queues one request of `width`.
    fn one(
        tx: &mpsc::Sender<GpuEncodeRequest>,
        clock: &FakeClock,
        width: u32,
    ) -> oneshot::Receiver<GpuEncodeReply> {
        queue(tx, clock, &[width]).pop().unwrap()
    }

    /// Queues one request of `width` that waited `waited` before the worker
    /// can take it: stamped, then the clock advanced, then sent. An idle
    /// worker takes it with exactly that wait.
    fn one_after(
        tx: &mpsc::Sender<GpuEncodeRequest>,
        clock: &FakeClock,
        width: u32,
        waited: Duration,
    ) -> oneshot::Receiver<GpuEncodeReply> {
        let (req, reply) = request(clock, width);
        clock.advance(waited);
        tx.try_send(req).expect("channel has room");
        reply
    }

    async fn reply_of(reply: oneshot::Receiver<GpuEncodeReply>) -> GpuEncodeReply {
        tokio::time::timeout(GUARD, reply)
            .await
            .expect("a reply within the guard")
            .expect("the worker answered")
    }

    /// The processor's result and timing; panics on a handed-back reply.
    fn processed(reply: GpuEncodeReply) -> (Result<Vec<u8>, DdsError>, GpuTileTiming) {
        match reply {
            GpuEncodeReply::Processed { result, timing } => (result, timing),
            GpuEncodeReply::Returned { reason, .. } => {
                panic!("expected a processed tile, it was handed back: {reason}")
            }
            GpuEncodeReply::Cancelled => panic!("expected a processed tile, it was cancelled"),
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

    async fn loop_ends(mut handle: GpuWorkerHandle) {
        handle
            .wait(GUARD)
            .await
            .expect("the worker loop ended within the guard")
            .expect("the worker loop itself never panics");
    }

    /// Waits until `done` holds, polling; fails after the guard. A failure
    /// guard on a state, never a measure of time.
    async fn waits_for(what: &str, mut done: impl FnMut() -> bool) {
        // Real time here only bounds a wait for a state, as a guard.
        let give_up = Instant::now() + GUARD;
        while !done() {
            assert!(Instant::now() < give_up, "{what}: not within the guard");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// The compressed chain of an outcome; panics on a handed-back image.
    fn compressed(outcome: Result<ChainOutcome, DdsError>) -> Result<Vec<u8>, DdsError> {
        outcome.map(|outcome| match outcome {
            ChainOutcome::Compressed(data) => data,
            ChainOutcome::Returned { reason, .. } => {
                panic!("expected the GPU's chain, the image was handed back: {reason}")
            }
            ChainOutcome::Cancelled => panic!("expected the GPU's chain, it was cancelled"),
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
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8, 12]);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("kernel asserted on width {}", image.width())
        });
        let health = SharedGpuHealth::new();

        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        loop_ends(handle).await;

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
        queues: impl Into<WorkerQueues>,
        health: SharedGpuHealth,
        clock: &FakeClock,
    ) {
        super::inner::run_worker_loop(
            processor,
            queues.into(),
            CancellationToken::new(),
            health,
            RULE,
            clock.shared(),
        )
    }

    /// A device lost while the worker is idle (the callback fails the
    /// health from another thread) ends the loop and hands back what is
    /// queued; nothing more reaches the processor.
    #[tokio::test]
    async fn a_device_lost_while_idle_ends_the_loop() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = spawn_with(
            taking(&clock, Duration::ZERO, &calls),
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        // One tile answered: the loop is running, and idle once more.
        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();

        device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
        loop_ends(handle).await;

        assert!(tx.is_closed());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing after the loss");
        let channel = Arc::new(
            GpuEncoderChannel::new(tx, health, CancellationToken::new()).with_clock(clock.shared()),
        );
        let (image, reason) = returned_by(&channel, 8).await;
        assert_eq!(image.width(), 8);
        assert_eq!(reason, "GPU device lost: driver reset");
    }

    /// An idle loop runs until shut down, then ends.
    #[tokio::test]
    async fn shutdown_stops_an_idle_loop() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            SoftwareProcessor,
            rx,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );
        // One tile answered: nothing that ends the loop has happened, so
        // it is still running.
        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();
        assert!(!handle.is_finished(), "an idle loop waits for work");

        shutdown.cancel();
        loop_ends(handle).await;
    }

    /// Shutdown is seen between tiles: the tile in progress is finished and
    /// answered, the queued one is never started and fails (#464).
    #[tokio::test]
    async fn shutdown_stops_a_busy_loop_after_its_tile() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut replies = queue(&tx, &clock, &[4, 8]).into_iter();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<u32>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            started_tx.send(image.width()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        });
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            processor,
            rx,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );

        let started = tokio::task::spawn_blocking(move || {
            let first = started_rx.recv_timeout(GUARD);
            (first, started_rx)
        });
        let (first, started_rx) = started.await.unwrap();
        assert_eq!(first, Ok(4), "the first request is in progress");

        // Cancelled while the tile is in the processor.
        shutdown.cancel();
        release_tx.send(()).unwrap();
        loop_ends(handle).await;

        let (first, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            first.unwrap().len(),
            file_len_of(4),
            "the busy tile is answered"
        );
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
    /// drain began and sends after the worker closed the channel: a drain
    /// that stopped at an empty queue, dropping the receiver, left the
    /// request in a channel nobody reads, and `GpuEncoderChannel` keeps its
    /// sender for the session, so the caller waited forever.
    #[tokio::test]
    async fn shutdown_answers_a_request_sent_while_the_queue_drains() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let permit = tx.clone().reserve_owned().await.expect("a free slot");
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            SoftwareProcessor,
            rx,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );

        shutdown.cancel();
        // The drain closes the channel first.
        tokio::time::timeout(GUARD, tx.closed())
            .await
            .expect("the worker closed its channel");
        let (late, reply) = request(&clock, 4);
        permit.send(late);

        let (result, _) = processed(reply_of(reply).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU worker shut down"
        );
        loop_ends(handle).await;
        drop(tx);
    }

    /// The same after a panic (#464): a request sent while the failed
    /// worker drains its queue comes back with its image (#420).
    #[tokio::test]
    async fn a_panic_answers_a_request_sent_while_the_queue_drains() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let permit = tx.clone().reserve_owned().await.expect("a free slot");
        let in_progress = one(&tx, &clock, 4);
        let handle = spawn_with(
            FnProcessor(
                |_: RgbaImage, _: &mut GpuTileTiming| -> Result<(), DdsError> {
                    panic!("device lost")
                },
            ),
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );

        // The drain closes the channel first.
        tokio::time::timeout(GUARD, tx.closed())
            .await
            .expect("the worker closed its channel");
        let (late, reply) = request(&clock, 8);
        permit.send(late);

        let (image, reason) = returned(reply_of(reply).await);
        assert_eq!(
            (image.width(), reason.as_str()),
            (8, "GPU worker panicked: device lost")
        );
        assert!(processed(reply_of(in_progress).await).0.is_err());
        loop_ends(handle).await;
        drop(tx);
    }

    /// Requests already buffered are still served after the last sender is
    /// dropped; then the loop ends.
    #[tokio::test]
    async fn closed_channel_ends_the_loop_after_the_buffered_requests() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8]);
        drop(tx);

        let handle = spawn_with(
            SoftwareProcessor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        loop_ends(handle).await;

        // One BC1 block per 4 x 4: 8 bytes for the 4-wide, 16 for the 8-wide.
        let mut lengths = Vec::new();
        for reply in replies {
            lengths.push(processed(reply_of(reply).await).0.unwrap().len());
        }
        assert_eq!(lengths, [file_len_of(4), file_len_of(8)]);
    }

    #[tokio::test]
    async fn closed_channel_ends_an_idle_loop() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = spawn_with(
            SoftwareProcessor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        // One tile answered: the loop is running, and idle once more.
        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();
        drop(tx);
        loop_ends(handle).await;
    }

    /// The loop times the queue wait and the processor's call on its clock,
    /// and passes the processor's own stages through. Four requests queued
    /// together, each tile taking `TILE`: the one at position `i` waited
    /// exactly the `i` tiles ahead of it.
    #[tokio::test]
    async fn timings_are_filled_and_queue_wait_grows_with_queue_position() {
        const TILE: Duration = Duration::from_millis(20);
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8, 12, 16]);
        let ticking = clock.clone();
        let processor = FnProcessor(move |_: RgbaImage, timing: &mut GpuTileTiming| {
            ticking.advance(TILE);
            timing.downsample = Duration::from_millis(1);
            timing.upload = Duration::from_millis(2);
            timing.gpu = Duration::from_millis(3);
            timing.readback = Duration::from_millis(4);
            Ok(())
        });
        let _worker = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );

        for (i, reply) in replies.into_iter().enumerate() {
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
            assert_eq!(t.process, TILE, "process is the call");
            assert_eq!(t.queue_wait, TILE * i as u32, "position {i}");
        }
    }

    // =========================================================================
    // Two lanes, demand first (#421)
    // =========================================================================

    /// Raw senders for both lanes and the worker's side, each channel
    /// holding [`CHANNEL_CAPACITY`] requests.
    fn lanes() -> (
        mpsc::Sender<GpuEncodeRequest>,
        mpsc::Sender<GpuEncodeRequest>,
        WorkerQueues,
    ) {
        let (demand_tx, demand_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (prefetch_tx, prefetch_rx) = mpsc::channel(CHANNEL_CAPACITY);
        (
            demand_tx,
            prefetch_tx,
            WorkerQueues::new(demand_rx, prefetch_rx),
        )
    }

    /// The widths of the tiles a processor started, in order.
    type Started = Arc<std::sync::Mutex<Vec<u32>>>;

    /// A processor that records each tile's width and succeeds at once.
    fn recording() -> (impl TileProcessor, Started) {
        let started = Started::default();
        let seen = Arc::clone(&started);
        let processor = FnProcessor(move |image: RgbaImage, _: &mut GpuTileTiming| {
            seen.lock().unwrap().push(image.width());
            Ok(())
        });
        (processor, started)
    }

    /// Whether the worker has taken every request `tx` sent: none is left
    /// buffered.
    fn all_taken(tx: &mpsc::Sender<GpuEncodeRequest>) -> bool {
        tx.capacity() == tx.max_capacity()
    }

    /// The lane rule, given each channel's state: demand whenever it holds
    /// a request, prefetch only while demand holds none, the end only when
    /// both are closed and empty.
    #[test]
    fn the_next_take_is_demand_first_and_prefetch_only_when_demand_is_empty() {
        use QueueState::{Closed, Empty, Ready};
        let demand = NextTake::Take(GpuLane::Demand);
        let prefetch = NextTake::Take(GpuLane::Prefetch);
        for (d, p, next) in [
            (Ready, Ready, demand),
            (Ready, Empty, demand),
            (Ready, Closed, demand),
            (Empty, Ready, prefetch),
            (Closed, Ready, prefetch),
            (Empty, Empty, NextTake::Wait),
            (Empty, Closed, NextTake::Wait),
            (Closed, Empty, NextTake::Wait),
            (Closed, Closed, NextTake::End),
        ] {
            assert_eq!(next_take(d, p), next, "demand {d:?}, prefetch {p:?}");
        }
    }

    /// Only demand has a deadline: the queue-age valve and the hand-back
    /// behind a late tile are for the FUSE timeout (#421).
    #[test]
    fn only_the_demand_lane_has_a_deadline() {
        assert!(GpuLane::Demand.has_deadline());
        assert!(!GpuLane::Prefetch.has_deadline());
    }

    /// The acceptance test of #421, through the real loop: while the worker
    /// is busy with a prefetch tile, 14 prefetch requests and then one
    /// demand request are queued (14 is the prefetch lane's CPU pool at
    /// reference sizing). The demand request is the next one processed: the
    /// most it waited behind prefetch is the one tile in progress.
    #[tokio::test]
    async fn a_demand_request_is_taken_before_the_prefetch_queue_ahead_of_it() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let (inner, started) = recording();
        let (processor, open) = gated(inner);
        let mut replies = queue(&prefetch_tx, &clock, &[1000]);
        let _worker = spawn_with(
            processor,
            queues,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        waits_for("the busy tile is taken", || all_taken(&prefetch_tx)).await;

        let prefetch: Vec<u32> = (1..=14).map(|i| 4 * i).collect();
        replies.extend(queue(&prefetch_tx, &clock, &prefetch));
        replies.extend(queue(&demand_tx, &clock, &[2000]));
        drop(open);
        for reply in replies {
            processed(reply_of(reply).await).0.unwrap();
        }

        let order = started.lock().unwrap().clone();
        let expected: Vec<u32> = [1000, 2000].into_iter().chain(prefetch).collect();
        assert_eq!(
            order, expected,
            "the demand tile is next after the busy one"
        );
    }

    /// The queue-age valve applies to the demand channel only (#421): a
    /// prefetch request that waited past the deadline is still processed on
    /// the GPU, while a demand request that waited as long is handed back.
    #[tokio::test]
    async fn a_prefetch_request_past_the_deadline_is_processed_and_a_demand_one_handed_back() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let (stale_demand, demand_reply) = request(&clock, 4);
        let (stale_prefetch, prefetch_reply) = request(&clock, 8);
        clock.advance(LATE);
        demand_tx.try_send(stale_demand).unwrap();
        prefetch_tx.try_send(stale_prefetch).unwrap();
        let (processor, started) = recording();
        let health = SharedGpuHealth::new();
        let _worker = spawn_with(
            processor,
            queues,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        let (image, reason) = returned(reply_of(demand_reply).await);
        assert_eq!(image.width(), 4);
        assert!(reason.starts_with("waited "), "{reason}");
        let (result, timing) = processed(reply_of(prefetch_reply).await);
        assert!(result.is_ok(), "the prefetch tile is encoded");
        assert_eq!(timing.queue_wait, LATE);
        assert_eq!(
            *started.lock().unwrap(),
            [8],
            "only prefetch reached the GPU"
        );
        assert_eq!(health.health(), GpuHealth::Healthy);
    }

    /// A late tile hands back the demand requests queued behind it, not the
    /// prefetch ones: prefetch has no deadline and waits for the GPU (#421).
    #[tokio::test]
    async fn a_late_tile_hands_back_the_demand_queue_only() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let late = one(&demand_tx, &clock, 4);
        let queued_demand = one(&demand_tx, &clock, 8);
        let queued_prefetch = one(&prefetch_tx, &clock, 12);
        let calls = Arc::new(AtomicUsize::new(0));
        let health = SharedGpuHealth::new();
        let _worker = spawn_with(
            taking(&clock, LATE, &calls),
            queues,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        processed(reply_of(late).await).0.unwrap();
        let (image, reason) = returned(reply_of(queued_demand).await);
        assert_eq!(
            (image.width(), reason.as_str()),
            (8, "a GPU tile ahead of it missed its deadline")
        );
        processed(reply_of(queued_prefetch).await).0.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(health.health(), GpuHealth::Healthy, "two misses, not three");
    }

    /// A GPU failure hands back the queue of both lanes (#420, #421): every
    /// image comes back to its caller, and both channels are closed.
    #[tokio::test]
    async fn a_panic_hands_back_the_queues_of_both_lanes() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let mut replies = queue(&demand_tx, &clock, &[4, 8]).into_iter();
        let prefetch_replies = queue(&prefetch_tx, &clock, &[12, 16]);
        let handle = spawn_with(
            FnProcessor(
                |_: RgbaImage, _: &mut GpuTileTiming| -> Result<(), DdsError> {
                    panic!("device lost")
                },
            ),
            queues,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        loop_ends(handle).await;

        assert!(processed(reply_of(replies.next().unwrap()).await)
            .0
            .is_err());
        for (reply, width) in replies.chain(prefetch_replies).zip([8, 12, 16]) {
            let (image, reason) = returned(reply_of(reply).await);
            assert_eq!(
                (image.width(), reason.as_str()),
                (width, "GPU worker panicked: device lost")
            );
        }
        assert!(demand_tx.is_closed() && prefetch_tx.is_closed());
    }

    /// Shutdown fails the queue of both lanes, and answers a prefetch
    /// request sent while the drain runs (#464 over two channels): its
    /// sender reserved a slot before the drain began.
    #[tokio::test]
    async fn shutdown_answers_both_lanes_and_a_prefetch_request_sent_while_draining() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let permit = prefetch_tx
            .clone()
            .reserve_owned()
            .await
            .expect("a free slot");
        let (inner, started) = recording();
        let (processor, open) = gated(inner);
        let in_progress = one(&demand_tx, &clock, 4);
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            processor,
            queues,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );
        waits_for("the tile is taken", || all_taken(&demand_tx)).await;
        let queued_prefetch = one(&prefetch_tx, &clock, 8);
        let queued_demand = one(&demand_tx, &clock, 12);

        shutdown.cancel();
        open.send(()).unwrap();
        processed(reply_of(in_progress).await).0.unwrap();
        for reply in [queued_demand, queued_prefetch] {
            let (result, _) = processed(reply_of(reply).await);
            assert_eq!(
                error_text(&result),
                "Compression failed: GPU worker shut down"
            );
        }
        // The drain is now waiting on the reserved prefetch slot.
        assert!(!handle.is_finished());
        let (late, reply) = request(&clock, 16);
        permit.send(late);
        let (result, _) = processed(reply_of(reply).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU worker shut down"
        );
        loop_ends(handle).await;
        assert_eq!(*started.lock().unwrap(), [4], "nothing queued was started");
    }

    /// The loop ends when every sender of both channels is gone, not when
    /// one lane's are: with the demand senders dropped it still serves
    /// prefetch.
    #[tokio::test]
    async fn the_loop_ends_only_when_both_lanes_are_closed() {
        let clock = FakeClock::new();
        let (demand_tx, prefetch_tx, queues) = lanes();
        let (processor, _) = recording();
        let handle = spawn_with(
            processor,
            queues,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        drop(demand_tx);

        processed(reply_of(one(&prefetch_tx, &clock, 4)).await)
            .0
            .unwrap();
        assert!(!handle.is_finished(), "the prefetch lane is still open");
        drop(prefetch_tx);
        loop_ends(handle).await;
    }

    /// The prefetch channel holds as many requests as the prefetch lane's
    /// CPU pool (#421), and at least one; the demand channel holds
    /// [`CHANNEL_CAPACITY`]. Both handles share the worker's health.
    #[test]
    fn the_prefetch_channel_is_bounded_by_its_capacity() {
        let health = SharedGpuHealth::new();
        let (channels, queues) = gpu_lanes(14, health.clone(), CancellationToken::new());
        assert_eq!(queues.demand.max_capacity(), CHANNEL_CAPACITY);
        assert_eq!(queues.prefetch.max_capacity(), 14);
        health.fail("test");
        assert!(channels.demand.health().is_failed());
        assert!(channels.prefetch.health().is_failed());

        let (_, queues) = gpu_lanes(0, SharedGpuHealth::new(), CancellationToken::new());
        assert_eq!(queues.prefetch.max_capacity(), 1);
    }

    #[test]
    fn the_lanes_are_named_in_the_log() {
        assert_eq!(GpuLane::Demand.name(), "demand");
        assert_eq!(GpuLane::Prefetch.name(), "prefetch");
    }

    // =========================================================================
    // Awaited, and cancelled before the tile starts (#422)
    // =========================================================================

    /// The worker's decision on a request it took, given its lane, its
    /// job's cancel, its wait in the queue and who has claimed it.
    #[test]
    fn a_taken_request_is_admitted_by_lane_cancel_wait_and_claim() {
        let fresh = RequestClaim::default;
        let left = || {
            let claim = RequestClaim::default();
            assert!(claim.abandon());
            claim
        };
        use Admission::{CallerLeft, HandBack, Skip, Start};
        use GpuLane::{Demand, Prefetch};
        for (lane, cancelled, wait, claim, admission) in [
            (Demand, false, IN_TIME, fresh(), Start),
            (Prefetch, false, IN_TIME, fresh(), Start),
            // The valve: demand only (#421).
            (Demand, false, LATE, fresh(), HandBack),
            (Prefetch, false, LATE, fresh(), Start),
            // A cancelled job is skipped on either lane, waited out or not.
            (Demand, true, IN_TIME, fresh(), Skip),
            (Prefetch, true, LATE, fresh(), Skip),
            (Demand, true, LATE, fresh(), Skip),
            // Its caller left first.
            (Demand, false, IN_TIME, left(), CallerLeft),
            (Prefetch, false, LATE, left(), CallerLeft),
        ] {
            assert_eq!(
                admit(lane, cancelled, wait, &RULE, &claim),
                admission,
                "{lane:?}, cancelled {cancelled}, waited {wait:?}"
            );
            assert_eq!(claim.is_started(), admission == Start, "{lane:?}");
        }
    }

    /// Whoever claims a queued request first has it: the worker starting
    /// the tile, or the caller leaving on a cancel.
    #[test]
    fn a_request_is_claimed_once_by_the_worker_or_its_caller() {
        let claim = RequestClaim::default();
        assert!(!claim.is_started());
        assert!(claim.start(), "the worker starts a queued tile");
        assert!(claim.is_started());
        assert!(!claim.abandon(), "its caller cannot leave a started tile");
        assert!(!claim.start());

        let claim = RequestClaim::default();
        assert!(claim.abandon(), "a caller leaves a queued tile");
        assert!(!claim.start(), "the worker cannot start a tile left");
        assert!(!claim.is_started());
    }

    /// A channel handle on `clock`, its raw sender (to queue tiles the
    /// handle did not send) and the worker's side.
    fn handle_and_raw(
        clock: &FakeClock,
    ) -> (
        Arc<GpuEncoderChannel>,
        mpsc::Sender<GpuEncodeRequest>,
        mpsc::Receiver<GpuEncodeRequest>,
    ) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let channel = handle(tx.clone()).with_clock(clock.shared());
        (Arc::new(channel), tx, rx)
    }

    /// Yields until `raw`'s channel holds `n` requests: the futures that
    /// send them run on this thread, and each sends on its first poll.
    async fn queued(raw: &mpsc::Sender<GpuEncodeRequest>, n: usize) {
        for _ in 0..10_000 {
            if raw.max_capacity() - raw.capacity() == n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{n} requests were never queued");
    }

    /// A job cancelled while its tile waits in the queue: its caller
    /// returns `Cancelled` at once, without waiting for the worker (still
    /// held on a tile), and the worker never starts that tile. It is
    /// counted as cancelled, not as a tile, failure or fallback.
    #[tokio::test]
    async fn a_caller_cancelled_while_queued_leaves_and_the_tile_is_never_started() {
        let clock = FakeClock::new();
        let (channel, raw, rx) = handle_and_raw(&clock);
        let (inner, started) = recording();
        let (processor, open) = gated(inner);
        let busy = one(&raw, &clock, 4);
        let _worker = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        waits_for("the busy tile is taken", || all_taken(&raw)).await;

        let cancel = CancellationToken::new();
        let caller = {
            let (channel, cancel) = (Arc::clone(&channel), cancel.clone());
            tokio::spawn(async move {
                channel
                    .submit_mipmap_chain(
                        RgbaImage::new(8, 4),
                        DdsFormat::BC1,
                        1,
                        file(8, 4, 1),
                        cancel,
                    )
                    .await
            })
        };
        queued(&raw, 1).await;
        cancel.cancel();
        let outcome = tokio::time::timeout(GUARD, caller)
            .await
            .expect("the caller leaves while the worker is held")
            .unwrap();
        assert!(
            matches!(outcome, Ok(ChainOutcome::Cancelled)),
            "{outcome:?}"
        );

        drop(open);
        processed(reply_of(busy).await).0.unwrap();
        // A tile behind it shows the worker went past the cancelled one.
        processed(reply_of(one(&raw, &clock, 12)).await).0.unwrap();
        assert_eq!(
            *started.lock().unwrap(),
            [4, 12],
            "the cancelled tile never started"
        );

        let s = channel.stats().take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.failed, s.fallback, s.cancelled), (0, 0, 0, 1));
        assert_eq!(s.depth_now, 0, "the caller that left is not counted");
    }

    /// A job cancelled while the GPU encodes its tile: the tile is paid
    /// for, so its caller waits for it and gets it (#385 then caches it).
    #[tokio::test]
    async fn a_caller_cancelled_while_its_tile_is_encoded_waits_for_it() {
        let clock = FakeClock::new();
        let (channel, _raw, rx) = handle_and_raw(&clock);
        let (inner, _) = recording();
        let (processor, open, mut starts) = gated_reporting(inner);
        let _worker = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );

        let cancel = CancellationToken::new();
        let mut caller = Box::pin(channel.submit_mipmap_chain(
            RgbaImage::new(8, 4),
            DdsFormat::BC1,
            1,
            file(8, 4, 1),
            cancel.clone(),
        ));
        assert!(futures::poll!(&mut caller).is_pending());
        // Inside the processor, held at the gate: the worker claimed the
        // request before it started the tile. Taken off the channel is not
        // enough: a cancel between the take and the claim lets the caller
        // leave first.
        assert_eq!(next_start(&mut starts).await, 8);
        cancel.cancel();
        assert!(
            futures::poll!(&mut caller).is_pending(),
            "a started tile is waited for"
        );

        drop(open);
        let outcome = tokio::time::timeout(GUARD, caller)
            .await
            .expect("the tile is answered");
        assert_eq!(compressed(outcome).unwrap().len(), file_len_of(8));
    }

    /// A job already cancelled is never sent; one cancelled while the
    /// channel is full leaves its send, and the request goes with it.
    #[tokio::test]
    async fn a_cancelled_caller_sends_nothing() {
        let clock = FakeClock::new();
        let (tx, mut rx) = mpsc::channel(1);
        let channel = handle(tx.clone()).with_clock(clock.shared());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = channel
            .submit_mipmap_chain(
                RgbaImage::new(4, 4),
                DdsFormat::BC1,
                1,
                file(4, 4, 1),
                cancel,
            )
            .await;
        assert!(
            matches!(outcome, Ok(ChainOutcome::Cancelled)),
            "{outcome:?}"
        );
        assert!(rx.try_recv().is_err(), "nothing was sent");

        let _full = one(&tx, &clock, 4);
        let cancel = CancellationToken::new();
        let mut caller = Box::pin(channel.submit_mipmap_chain(
            RgbaImage::new(8, 4),
            DdsFormat::BC1,
            1,
            file(8, 4, 1),
            cancel.clone(),
        ));
        assert!(
            futures::poll!(&mut caller).is_pending(),
            "waiting for a slot"
        );
        cancel.cancel();
        assert!(matches!(caller.await, Ok(ChainOutcome::Cancelled)));
        assert_eq!(rx.try_recv().unwrap().image.width(), 4);
        assert!(
            rx.try_recv().is_err(),
            "the cancelled request never arrived"
        );
        assert_eq!(channel.stats().take_sample().unwrap().cancelled, 2);
    }

    /// The worker skips a request whose job was cancelled before it took
    /// it, answering `Cancelled` without starting the tile, and goes on.
    #[tokio::test]
    async fn the_worker_skips_a_request_whose_job_was_cancelled() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let (skipped, skipped_reply) = GpuEncodeRequest::new(
            RgbaImage::new(4, 4),
            DdsFormat::BC1,
            1,
            file(4, 4, 1),
            clock.now(),
            cancel.clone(),
        );
        cancel.cancel();
        tx.try_send(skipped).unwrap();
        let next = one(&tx, &clock, 8);
        let (processor, started) = recording();
        let _worker = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );

        assert!(matches!(
            reply_of(skipped_reply).await,
            GpuEncodeReply::Cancelled
        ));
        processed(reply_of(next).await).0.unwrap();
        assert_eq!(*started.lock().unwrap(), [8]);
    }

    /// The acceptance test of #422: an encode waiting on the worker
    /// occupies no blocking-pool thread. The runtime's one blocking thread
    /// is held for the whole test, the worker runs on a thread of its own,
    /// and a burst of encodes through the executor's adapter, the
    /// production path, is answered all the same. Waited for on blocking
    /// threads, as before #422, none of them could start.
    #[test]
    fn an_encode_waiting_on_the_gpu_occupies_no_blocking_thread() {
        use crate::executor::{TextureEncoderAdapter, TextureEncoderAsync, TokioExecutor};
        use crate::texture::DdsTextureEncoder;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let (processor, started) = recording();
        let worker = {
            let (health, clock) = (health.clone(), clock.clone());
            std::thread::spawn(move || run_worker_loop_here(processor, rx, health, &clock))
        };
        let channel =
            GpuEncoderChannel::new(tx, health, CancellationToken::new()).with_clock(clock.shared());
        let adapter = Arc::new(TextureEncoderAdapter::new(Arc::new(
            crate::texture::DdsTextureEncoder::new(DdsFormat::BC1)
                .with_mipmap_count(1)
                .with_mipmap_compressor(Arc::new(channel)),
        )));
        let _: &DdsTextureEncoder = adapter.encoder();

        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        runtime.block_on(async {
            let held = tokio::task::spawn_blocking(move || hold_rx.recv());
            let burst: Vec<_> = (1..=6)
                .map(|i| {
                    let adapter = Arc::clone(&adapter);
                    tokio::spawn(async move {
                        let cancel = CancellationToken::new();
                        adapter
                            .encode(RgbaImage::new(4 * i, 4), &cancel, &TokioExecutor::new())
                            .await
                    })
                })
                .collect();
            for (i, encode) in (1..=6).zip(burst) {
                let dds = tokio::time::timeout(GUARD, encode)
                    .await
                    .expect("answered with no blocking thread free")
                    .unwrap()
                    .expect("the blocking executor was not needed")
                    .expect("the tile is encoded");
                assert_eq!(dds.len(), file_len_of(4 * i));
            }
            hold_tx.send(()).unwrap();
            held.await.unwrap().unwrap();
        });
        assert_eq!(started.lock().unwrap().len(), 6);
        drop(adapter);
        worker.join().unwrap();
    }

    // =========================================================================
    // The worker's thread, idle wait and output in place (#423)
    // =========================================================================

    /// What wakes an idle worker, given the channels' state: a request
    /// (demand first when both hold one), the shutdown or the GPU's failure.
    /// (A lane whose last sender goes while it waits wakes it too; that is
    /// the loop's end, in `the_loop_ends_only_when_both_lanes_are_closed`.)
    #[test]
    fn an_idle_worker_wakes_for_a_request_a_shutdown_or_a_failure() {
        use super::inner::{wait_for_work, Woken};
        let woken_lane = |woken: Woken| match woken {
            Woken::Request(lane, request) => Some((lane, request.image.width())),
            Woken::Signal => None,
        };
        let clock = FakeClock::new();
        let (shutdown, health) = (CancellationToken::new(), SharedGpuHealth::new());

        let (demand_tx, prefetch_tx, mut queues) = lanes();
        let _ = one(&prefetch_tx, &clock, 8);
        let woken = wait_for_work(&mut queues, &shutdown, &health);
        assert_eq!(woken_lane(woken), Some((GpuLane::Prefetch, 8)));
        let _ = one(&prefetch_tx, &clock, 8);
        let _ = one(&demand_tx, &clock, 4);
        let woken = wait_for_work(&mut queues, &shutdown, &health);
        assert_eq!(
            woken_lane(woken),
            Some((GpuLane::Demand, 4)),
            "a tie: demand"
        );

        let (_demand_tx, _prefetch_tx, mut queues) = lanes();
        let failed = SharedGpuHealth::new();
        failed.fail("device lost");
        assert!(woken_lane(wait_for_work(&mut queues, &shutdown, &failed)).is_none());
        let stopped = CancellationToken::new();
        stopped.cancel();
        assert!(woken_lane(wait_for_work(&mut queues, &stopped, &health)).is_none());
    }

    /// Given both lanes closed, an idle worker does not wait: every sender
    /// went between its look at the queues and its wait, and nothing would
    /// ever wake it but a shutdown or a failure. It returns at once, and
    /// the loop sees both channels closed and ends.
    #[test]
    fn an_idle_worker_with_both_lanes_closed_does_not_wait() {
        use super::inner::{wait_for_work, Woken};
        let (demand_tx, prefetch_tx, mut queues) = lanes();
        drop((demand_tx, prefetch_tx));
        // On a thread of its own, so a regression fails within the guard
        // rather than hang the run.
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let woken = wait_for_work(
                &mut queues,
                &CancellationToken::new(),
                &SharedGpuHealth::new(),
            );
            let _ = done_tx.send(matches!(woken, Woken::Signal));
        });
        assert_eq!(
            done.recv_timeout(GUARD),
            Ok(true),
            "woken at once, with no request"
        );
    }

    /// The worker blocks while idle and is woken by what it waits for: a
    /// request sent to an idle worker is answered, and a shutdown ends an
    /// idle worker. (Nothing measures the wake-up: there is no poll
    /// interval left to measure.)
    #[tokio::test]
    async fn an_idle_worker_answers_a_request_and_ends_on_a_shutdown() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, started) = recording();
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            processor,
            rx,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );

        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();
        assert_eq!(*started.lock().unwrap(), [4]);
        shutdown.cancel();
        loop_ends(handle).await;
    }

    /// The worker runs on a thread of its own, named for `top` and
    /// profilers, not on the runtime's blocking pool.
    #[tokio::test]
    async fn the_worker_runs_on_its_named_thread() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let name = Arc::new(std::sync::Mutex::new(None));
        let seen = Arc::clone(&name);
        let _worker = spawn_with(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                *seen.lock().unwrap() = std::thread::current().name().map(str::to_string);
                Ok(())
            }),
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();
        assert_eq!(name.lock().unwrap().as_deref(), Some(WORKER_THREAD));
    }

    /// Stopping the worker through its handle, as the service's shutdown
    /// does, finishes the tile in progress, fails the queue and ends the
    /// worker: within one tile, and never before it.
    #[tokio::test]
    async fn stopping_the_worker_ends_it_after_the_tile_in_progress() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (inner, started) = recording();
        let (processor, open) = gated(inner);
        let busy = one(&tx, &clock, 4);
        let mut handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        waits_for("the tile is taken", || all_taken(&tx)).await;
        let queued = one(&tx, &clock, 8);

        {
            let mut stopping = Box::pin(handle.stop(GUARD));
            assert!(
                futures::poll!(&mut stopping).is_pending(),
                "the tile in progress is finished first"
            );
            open.send(()).unwrap();
            stopping
                .await
                .expect("ended within the guard")
                .expect("the loop never panics");
        }
        assert!(handle.is_finished());
        processed(reply_of(busy).await).0.unwrap();
        let (result, _) = processed(reply_of(queued).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU worker shut down"
        );
        assert_eq!(*started.lock().unwrap(), [4]);
    }

    /// The blocking stop, as the service's `Drop` uses it: an idle worker
    /// ends; a second wait finds it ended.
    #[test]
    fn a_blocking_stop_ends_an_idle_worker() {
        let (_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, _) = recording();
        let mut handle = spawn_tile_worker(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            RULE,
        );
        let ended = handle.stop_blocking(GUARD);
        assert!(matches!(ended, Some(Ok(()))), "ended within the guard");
        assert!(handle.is_finished());
        assert!(matches!(handle.wait_blocking(Duration::ZERO), Some(Ok(()))));
    }

    /// The payload in place (#423): the file the encoder allocated at its
    /// exact size goes to the worker with the request, the processor writes
    /// the chain into it after the header, and the same allocation comes
    /// back as the tile: no spare capacity, and no copy after the
    /// processor's.
    #[test]
    fn the_chain_is_written_into_the_encoders_file_in_place() {
        /// Writes its byte across the payload, once, and records where.
        struct Fills(Arc<std::sync::Mutex<Vec<(usize, usize)>>>);
        impl TileProcessor for Fills {
            fn process(
                &mut self,
                _image: RgbaImage,
                _format: DdsFormat,
                _levels: usize,
                _deadline: Instant,
                _timing: &mut GpuTileTiming,
                payload: &mut [u8],
            ) -> Result<(), DdsError> {
                payload.fill(0x5A);
                self.0
                    .lock()
                    .unwrap()
                    .push((payload.as_ptr() as usize, payload.len()));
                Ok(())
            }
        }
        let clock = FakeClock::new();
        let writes = Arc::default();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let worker = std::thread::spawn({
            let (processor, clock) = (Fills(Arc::clone(&writes)), clock.clone());
            move || run_worker_loop_here(processor, rx, SharedGpuHealth::new(), &clock)
        });
        let channel = Arc::new(handle(tx).with_clock(clock.shared()));

        let dds = gpu_encoder(&channel).encode(varied(64, 64)).unwrap();

        let header_len = crate::dds::layout::HEADER_LEN;
        let expected = crate::dds::layout::file_len(64, 64, 7, DdsFormat::BC1);
        assert_eq!((dds.len(), dds.capacity()), (expected, expected));
        assert!(dds[..header_len] == ispc_encode(varied(64, 64))[..header_len]);
        assert!(dds[header_len..].iter().all(|&b| b == 0x5A));
        let payload_at = dds.as_ptr() as usize + header_len;
        assert_eq!(
            *writes.lock().unwrap(),
            [(payload_at, expected - header_len)],
            "one write, into the returned file"
        );
        drop(channel);
        worker.join().unwrap();
    }

    /// A file that comes back from the backend at another length is an
    /// error, never a tile.
    #[test]
    fn a_file_of_the_wrong_length_is_an_error() {
        struct Short;
        impl MipmapCompressor for Short {
            fn compress_mipmap_chain(
                &self,
                _image: RgbaImage,
                _format: DdsFormat,
                _mipmap_count: usize,
                mut output: Vec<u8>,
            ) -> Result<ChainOutcome, DdsError> {
                output.pop();
                Ok(ChainOutcome::Compressed(output))
            }
            fn name(&self) -> &str {
                "short"
            }
        }
        let encoder = DdsEncoder::new(DdsFormat::BC1)
            .with_mipmap_compressor(Arc::new(Short), Arc::new(IspcCompressor));
        let error = encoder.encode(varied(64, 64)).unwrap_err().to_string();
        assert!(error.contains("short returned a"), "{error}");
    }

    /// The processor is given the deadline: the rule's deadline from when
    /// the worker took the tile, whatever the tile waited in the queue.
    #[tokio::test]
    async fn the_processor_is_given_the_deadline_from_when_its_tile_starts() {
        struct Deadlines(FakeClock, std::sync::mpsc::Sender<Duration>);
        impl TileProcessor for Deadlines {
            fn process(
                &mut self,
                _image: RgbaImage,
                _format: DdsFormat,
                _levels: usize,
                deadline: Instant,
                _timing: &mut GpuTileTiming,
                _payload: &mut [u8],
            ) -> Result<(), DdsError> {
                self.1.send(deadline - self.0.now()).unwrap();
                self.0.advance(Duration::from_millis(500));
                Ok(())
            }
        }
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8]);
        let (left_tx, left_rx) = std::sync::mpsc::channel();
        let _worker = spawn_with(
            Deadlines(clock.clone(), left_tx),
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        let mut waits = Vec::new();
        for reply in replies {
            waits.push(processed(reply_of(reply).await).1.queue_wait);
        }
        assert_eq!(waits, [Duration::ZERO, Duration::from_millis(500)]);
        let left: Vec<_> = left_rx.try_iter().collect();
        assert_eq!(
            left,
            [TILE_DEADLINE, TILE_DEADLINE],
            "the whole deadline at the start of each call"
        );
    }

    // =========================================================================
    // The hung-GPU rule in the loop (#420)
    // =========================================================================

    /// A tile past its deadline is answered, and the requests queued behind
    /// it come back at once with their images rather than wait for the next
    /// tile: no caller waits longer than that one tile. One miss does not
    /// fail the GPU.
    #[tokio::test]
    async fn a_missed_deadline_hands_the_queue_back_without_failing_the_gpu() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8, 12, 16, 20]);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let _worker = spawn_with(
            taking(&clock, LATE, &calls),
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        let mut replies = replies.into_iter();
        let (first, timing) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            first.unwrap().len(),
            file_len_of(4),
            "the slow tile is still answered"
        );
        assert_eq!(timing.process, LATE);
        for (reply, width) in replies.zip([8, 12, 16, 20]) {
            let (image, reason) = returned(reply_of(reply).await);
            assert_eq!(image.width(), width);
            assert_eq!(reason, "a GPU tile ahead of it missed its deadline");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only the slow tile ran");
        assert_eq!(clock.advanced(), LATE, "every caller waited one tile");
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
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let (processor, open) = gated(taking(&clock, LATE, &calls));
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        // Two misses, one tile at a time: each is answered.
        for width in [4, 8] {
            open.send(()).unwrap();
            processed(reply_of(one(&tx, &clock, width)).await)
                .0
                .unwrap();
            assert!(!health.is_failed());
        }
        // The third, with two more queued behind it before it ends.
        let mut replies = queue(&tx, &clock, &[12, 16, 20]).into_iter();
        open.send(()).unwrap();
        processed(reply_of(replies.next().unwrap()).await)
            .0
            .unwrap();
        let reason = "3 tiles in a row missed the GPU's 2000 ms deadline";
        for (reply, width) in replies.zip([16, 20]) {
            let (image, why) = returned(reply_of(reply).await);
            assert_eq!((image.width(), why.as_str()), (width, reason));
        }
        loop_ends(handle).await;
        assert_eq!(health.failure(), Some(reason));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let channel = Arc::new(
            GpuEncoderChannel::new(tx, health, CancellationToken::new()).with_clock(clock.shared()),
        );
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

    /// Tiles that fail in time are judged too: a destroyed device fails
    /// each tile at once, with no panic and no hang (seen on hardware with
    /// `Device::destroy`). The third fails the GPU with the last error, and
    /// the queue behind it comes back.
    #[tokio::test]
    async fn failed_tiles_in_a_row_fail_the_gpu_and_hand_the_queue_back() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let (processor, open) = gated(refusing(&calls));
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        for width in [4, 8] {
            open.send(()).unwrap();
            let (result, _) = processed(reply_of(one(&tx, &clock, width)).await);
            assert!(result.is_err());
            assert!(!health.is_failed(), "two failures are not a lost GPU");
        }
        // The third, with one more queued behind it before it ends.
        let mut replies = queue(&tx, &clock, &[12, 16]).into_iter();
        open.send(()).unwrap();
        let (result, _) = processed(reply_of(replies.next().unwrap()).await);
        assert_eq!(
            error_text(&result),
            "Compression failed: GPU poll failed: submission 2 refused"
        );
        let reason = "3 tiles in a row failed on the GPU, the last with: \
                      Compression failed: GPU poll failed: submission 2 refused";
        let (image, why) = returned(reply_of(replies.next().unwrap()).await);
        assert_eq!((image.width(), why.as_str()), (16, reason));
        loop_ends(handle).await;
        assert_eq!(health.failure(), Some(reason));
    }

    // =========================================================================
    // The queue-age valve in the loop (#420)
    // =========================================================================

    /// A GPU slower than the deadline over the queue depth but never late:
    /// without the valve the k-th caller of a full queue waits k tiles.
    /// Twelve requests sent together, each tile `TILE` on the fake clock:
    /// the tile taken at 2000 ms (not past the deadline) is the last on the
    /// GPU; at 2500 ms every one left has waited past it and comes back
    /// with its own image. No caller waited longer than the deadline plus
    /// one tile, and the GPU stays healthy.
    #[tokio::test]
    async fn no_caller_waits_longer_than_the_deadline_plus_one_tile() {
        const TILE: Duration = Duration::from_millis(500);
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let widths: Vec<u32> = (1..=12).map(|i| 4 * i).collect();
        let replies = queue(&tx, &clock, &widths);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let _worker = spawn_with(
            taking(&clock, TILE, &calls),
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );

        let (mut waits, mut back) = (Vec::new(), Vec::new());
        for (reply, width) in replies.into_iter().zip(widths) {
            match reply_of(reply).await {
                GpuEncodeReply::Processed { result, timing } => {
                    result.unwrap();
                    waits.push(timing.queue_wait);
                }
                GpuEncodeReply::Returned { image, reason } => {
                    assert_eq!(image.width(), width, "each caller gets its own image");
                    back.push(reason);
                }
                GpuEncodeReply::Cancelled => panic!("nothing was cancelled"),
            }
        }
        assert_eq!(
            waits,
            (0..5).map(|i| TILE * i).collect::<Vec<_>>(),
            "the tiles taken by the deadline ran"
        );
        assert_eq!(
            back,
            vec!["waited 2500 ms in the queue, past the GPU's 2000 ms deadline"; 7]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 5, "the rest never ran");
        assert_eq!(
            clock.advanced(),
            TILE_DEADLINE + TILE,
            "the last caller was answered at the deadline plus one tile"
        );
        assert_eq!(health.health(), GpuHealth::Healthy, "no tile was late");
    }

    /// A request handed back for its wait never reaches the processor and
    /// is not a miss: it neither counts towards the misses in a row nor
    /// clears them.
    #[tokio::test]
    async fn a_request_handed_back_for_its_wait_is_not_a_miss() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = spawn_with(
            taking(&clock, LATE, &calls),
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        let stale = |width| one_after(&tx, &clock, width, LATE);

        // Late, waited out, late: two misses, not three.
        processed(reply_of(one(&tx, &clock, 4)).await).0.unwrap();
        let (image, reason) = returned(reply_of(stale(16)).await);
        assert_eq!(image.width(), 16, "its own image comes back");
        assert_eq!(
            reason,
            "waited 2001 ms in the queue, past the GPU's 2000 ms deadline"
        );
        processed(reply_of(one(&tx, &clock, 8)).await).0.unwrap();
        assert_eq!(health.health(), GpuHealth::Healthy, "a wait is not a miss");
        // Waited out, then a third late tile: the misses were not cleared.
        returned(reply_of(stale(20)).await);
        processed(reply_of(one(&tx, &clock, 12)).await).0.unwrap();
        loop_ends(handle).await;
        assert!(health.is_failed(), "nor does a wait clear the misses");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "only the late tiles ran");
    }

    // =========================================================================
    // What the loop does with its processor at the end (#420)
    // =========================================================================

    /// A processor that reports when its drop starts, then holds the drop
    /// until released, as dropping a hung GPU's queue would hold it
    /// forever. The release is also given when its sender is dropped, so a
    /// failing test never leaves the drop held.
    struct DropProbe<P> {
        inner: P,
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl<P: TileProcessor> TileProcessor for DropProbe<P> {
        fn process(
            &mut self,
            image: RgbaImage,
            format: DdsFormat,
            levels: usize,
            deadline: Instant,
            timing: &mut GpuTileTiming,
            payload: &mut [u8],
        ) -> Result<(), DdsError> {
            self.inner
                .process(image, format, levels, deadline, timing, payload)
        }
    }

    impl<P> Drop for DropProbe<P> {
        fn drop(&mut self) {
            let _ = self.started.send(());
            let _ = self.release.recv();
        }
    }

    /// The probe's two ends, for the test.
    struct Probe {
        started: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    impl Probe {
        /// Whether the processor's drop has started.
        fn dropped(&self) -> bool {
            self.started.try_recv().is_ok()
        }
    }

    /// `inner` in a probe whose drop holds until the test releases it.
    fn held<P: TileProcessor>(inner: P) -> (DropProbe<P>, Probe) {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let probe = DropProbe {
            inner,
            started: started_tx,
            release: release_rx,
        };
        (probe, Probe { started, release })
    }

    /// `inner` in a probe whose drop is released in advance.
    fn released<P: TileProcessor>(inner: P) -> (DropProbe<P>, Probe) {
        let (probe, ends) = held(inner);
        ends.release.send(()).unwrap();
        (probe, ends)
    }

    /// A GPU failed by late tiles is a GPU that may never finish its work,
    /// and wgpu's `Queue` drop waits for it with no timeout: the worker
    /// leaves the processor undropped, and its loop ends. A loop that
    /// dropped it would be held in the drop until the guard fails it.
    #[tokio::test]
    async fn a_gpu_failed_by_late_tiles_is_left_undropped() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let calls = Arc::new(AtomicUsize::new(0));
        let (processor, probe) = held(taking(&clock, LATE, &calls));
        let health = SharedGpuHealth::new();
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        // One at a time: a late tile hands back the queue behind it.
        for width in [4, 8, 12] {
            reply_of(one(&tx, &clock, width)).await;
        }
        assert!(health.is_failed());
        loop_ends(handle).await;
        assert!(!probe.dropped(), "the processor was not dropped");
    }

    /// The same after a panic: nothing is known of the device it left.
    #[tokio::test]
    async fn a_processor_that_panicked_is_left_undropped() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let reply = one(&tx, &clock, 4);
        let (processor, probe) = held(FnProcessor(
            |_: RgbaImage, _: &mut GpuTileTiming| -> Result<(), DdsError> { panic!("device lost") },
        ));
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        reply_of(reply).await;
        loop_ends(handle).await;
        assert!(!probe.dropped(), "the processor was not dropped");
    }

    /// Every other end releases the GPU: a shutdown, a closed channel, a
    /// device lost (its queue's drop returns at once), and tiles that fail
    /// in time (a destroyed device).
    #[tokio::test]
    async fn every_other_end_drops_the_processor() {
        let clock = FakeClock::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let ok = || taking(&clock, Duration::ZERO, &calls);

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, probe) = released(ok());
        let shutdown = CancellationToken::new();
        let handle = spawn_with(
            processor,
            rx,
            shutdown.clone(),
            SharedGpuHealth::new(),
            &clock,
        );
        shutdown.cancel();
        loop_ends(handle).await;
        assert!(probe.dropped(), "shutdown");
        drop(tx);

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, probe) = released(ok());
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        drop(tx);
        loop_ends(handle).await;
        assert!(probe.dropped(), "closed channel");

        let (_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (processor, probe) = released(ok());
        let health = SharedGpuHealth::new();
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
        loop_ends(handle).await;
        assert!(probe.dropped(), "device lost");

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let replies = queue(&tx, &clock, &[4, 8, 12]);
        let (processor, probe) = released(refusing(&calls));
        let health = SharedGpuHealth::new();
        let handle = spawn_with(
            processor,
            rx,
            CancellationToken::new(),
            health.clone(),
            &clock,
        );
        for reply in replies {
            reply_of(reply).await;
        }
        loop_ends(handle).await;
        assert!(health.is_failed());
        assert!(probe.dropped(), "tiles failed in time");
    }

    // =========================================================================
    // The channel handle
    // =========================================================================

    #[test]
    fn test_channel_capacity_constant() {
        assert_eq!(CHANNEL_CAPACITY, 32);
    }

    fn handle(tx: mpsc::Sender<GpuEncodeRequest>) -> GpuEncoderChannel {
        GpuEncoderChannel::new(tx, SharedGpuHealth::new(), CancellationToken::new())
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
            ch.compress_mipmap_chain(
                RgbaImage::new(width, 4),
                DdsFormat::BC1,
                1,
                file(width, 4, 1),
            )
        })
        .await
        .expect("spawn_blocking should not panic");
        match outcome {
            Ok(ChainOutcome::Returned { image, reason }) => (image, reason),
            other => panic!("expected the image back, got {other:?}"),
        }
    }

    /// The worker is gone (its receiver dropped) while nobody shut it down:
    /// it stopped unexpectedly (a panic outside the loop's `catch_unwind`).
    /// The send error returns the request and the image comes back to the
    /// caller (#420), and the GPU is failed, so the Status pane stops
    /// showing it and no later tile is sent.
    #[tokio::test]
    async fn a_send_refused_while_not_shutting_down_fails_the_gpu() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let channel = Arc::new(handle(tx));
        drop(rx);

        let (image, reason) = returned_by(&channel, 8).await;
        assert_eq!(image.width(), 8);
        assert_eq!(reason, "GPU worker stopped unexpectedly");
        assert_eq!(
            channel.health().failure(),
            Some("GPU worker stopped unexpectedly")
        );
        let (image, reason) = returned_by(&channel, 12).await;
        assert_eq!(
            (image.width(), reason.as_str()),
            (12, "GPU worker stopped unexpectedly")
        );
    }

    /// A send refused after the worker's shutdown token was cancelled is a
    /// worker shut down on purpose: the image comes back, the GPU is not
    /// failed.
    #[tokio::test]
    async fn a_send_refused_after_shutdown_hands_the_image_back_only() {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let channel = Arc::new(GpuEncoderChannel::new(
            tx,
            SharedGpuHealth::new(),
            shutdown.clone(),
        ));
        shutdown.cancel();
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
    /// health and `clock`.
    fn channel_on<P: TileProcessor + 'static>(
        processor: P,
        clock: &FakeClock,
    ) -> (GpuEncoderChannel, GpuWorkerHandle) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let shutdown = CancellationToken::new();
        let worker = spawn_with(processor, rx, shutdown.clone(), health.clone(), clock);
        let channel = GpuEncoderChannel::new(tx, health, shutdown).with_clock(clock.shared());
        (channel, worker)
    }

    /// [`channel_on`] a clock nobody advances: every tile is in time.
    fn channel_with<P: TileProcessor + 'static>(
        processor: P,
    ) -> (GpuEncoderChannel, GpuWorkerHandle) {
        channel_on(processor, &FakeClock::new())
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
        })
        .await
        .expect("spawn_blocking should not panic");

        let data = compressed(result).expect("compress should succeed");
        assert_eq!(data.len(), file_len_of(4)); // 1 block × 8 bytes for BC1
    }

    #[tokio::test]
    async fn test_worker_processes_multiple_sequential_requests() {
        let (channel, _worker) = channel_with(SoftwareProcessor);
        let channel = Arc::new(channel);

        for i in 0..3 {
            let ch = Arc::clone(&channel);
            let result = tokio::task::spawn_blocking(move || {
                let image = RgbaImage::new(4, 4);
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
            })
            .await
            .expect("spawn_blocking should not panic");

            let data =
                compressed(result).unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
            assert_eq!(data.len(), file_len_of(4));
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
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            let data = compressed(result).expect("concurrent compress should succeed");
            assert_eq!(data.len(), file_len_of(4)); // 1 BC1 block = 8 bytes
        }
    }

    /// Dropping the channel handle closes the channel and ends the loop.
    #[tokio::test]
    async fn test_worker_stops_when_channel_dropped() {
        let (channel, worker) = channel_with(SoftwareProcessor);
        drop(channel);
        loop_ends(worker).await;
    }

    /// An unsupported format is refused by the caller's side of the channel:
    /// nothing is sent, so the worker never sees it and no GPU is needed.
    #[tokio::test]
    async fn channel_rejects_unsupported_format_without_sending() {
        let (channel, mut rx) = mock_channel();
        let mut rx = rx.take().expect("receiver");

        let result = tokio::task::spawn_blocking(move || {
            channel.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC3, 1, file(4, 4, 1))
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
            |_: RgbaImage, _: &mut GpuTileTiming| -> Result<(), DdsError> {
                panic!("simulated kernel panic")
            },
        ));

        let result = tokio::task::spawn_blocking(move || {
            let image = RgbaImage::new(4, 4);
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
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

    /// Waits until `n` requests are buffered in the channel `raw` sends on,
    /// polling. The sleep and the 5 s bound only guard a wait for a state:
    /// no outcome depends on how long it takes.
    fn buffered(raw: &mpsc::Sender<GpuEncodeRequest>, n: usize) {
        let give_up = Instant::now() + GUARD;
        while raw.max_capacity() - raw.capacity() != n {
            assert!(Instant::now() < give_up, "{n} requests were never queued");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A panic fails the tile in progress; the queued tiles and every later
    /// one are encoded by the fallback, byte for byte what ISPC makes; the
    /// health is Failed with the panic's message. (Its one WARN is counted
    /// in `tests/gpu_fallback_warning.rs`, a process of its own.)
    #[test]
    fn after_a_panic_the_queue_and_every_later_tile_are_the_ispc_encode() {
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let raw = tx.clone();
        let health = SharedGpuHealth::new();
        let channel = Arc::new(
            GpuEncoderChannel::new(tx, health.clone(), CancellationToken::new())
                .with_clock(clock.shared()),
        );
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
            let worker =
                scope.spawn(|| run_worker_loop_here(processor, rx, health.clone(), &clock));
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
            ch.compress_mipmap_chain(RgbaImage::new(4, 4), DdsFormat::BC1, 1, file(4, 4, 1))
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
        let clock = FakeClock::new();
        let ticking = clock.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let (channel, _worker) = channel_on(
            FnProcessor(move |_: RgbaImage, timing: &mut GpuTileTiming| {
                if seen.fetch_add(1, Ordering::SeqCst) == 1 {
                    return Err(DdsError::CompressionFailed("simulated".into()));
                }
                ticking.advance(Duration::from_millis(5));
                timing.downsample = Duration::from_millis(1);
                timing.upload = Duration::from_millis(2);
                timing.gpu = Duration::from_millis(3);
                timing.readback = Duration::from_millis(4);
                Ok(())
            }),
            &clock,
        );
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
        // On the fake clock the tile took 5 ms in the processor and none in
        // the queue, and the round trip is exactly the two.
        assert_eq!(max(s.process), 5_000, "process is the call");
        assert_eq!(max(s.queue_wait), 0);
        assert_eq!(max(s.total), 5_000, "the round trip: {s:?}");
    }

    // =========================================================================
    // Queue depth (#419 item 3)
    // =========================================================================

    /// Waits until `stats` shows `depth` requests (a guard on a state).
    async fn depth_reaches(stats: &GpuEncodeStats, depth: usize) {
        waits_for(&format!("depth {depth}"), || stats.depth() == depth).await;
    }

    /// The depth counts every caller from before its send to its reply:
    /// in progress, buffered, and blocked in `blocking_send` on a full
    /// channel, which the channel's own capacity does not see. It returns
    /// to zero after the burst, and the sample keeps the burst's peak.
    #[tokio::test]
    async fn queue_depth_rises_under_a_burst_and_returns_to_zero() {
        // A one-slot channel: of five callers, one is in progress, one is
        // buffered and three are blocked in their send.
        let clock = FakeClock::new();
        let (tx, rx) = mpsc::channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let _worker = spawn_with(
            FnProcessor(move |_: RgbaImage, _: &mut GpuTileTiming| {
                release_rx.recv().unwrap();
                Ok(())
            }),
            rx,
            CancellationToken::new(),
            SharedGpuHealth::new(),
            &clock,
        );
        let raw = tx.clone();
        let channel = Arc::new(handle(tx).with_clock(clock.shared()));
        let stats = Arc::clone(channel.stats());

        let callers: Vec<_> = (0..5)
            .map(|_| {
                let ch = Arc::clone(&channel);
                tokio::task::spawn_blocking(move || {
                    compressed(ch.compress_mipmap_chain(
                        RgbaImage::new(4, 4),
                        DdsFormat::BC1,
                        1,
                        file(4, 4, 1),
                    ))
                })
            })
            .collect();

        depth_reaches(&stats, 5).await;
        // The counter is raised before the send: wait for the buffered
        // request to reach the channel's one slot too.
        waits_for("the slot filled", || raw.capacity() == 0).await;
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
    /// in the sample: a worker gone before replying.
    #[tokio::test]
    async fn dropped_replies_count_as_failures() {
        // A worker that takes one request and drops it unanswered.
        let (tx, mut rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let worker = tokio::task::spawn_blocking(move || drop(rx.blocking_recv()));
        let channel = Arc::new(handle(tx));
        let stats = Arc::clone(channel.stats());

        let err = encode_one(&channel).await.expect_err("reply dropped");
        assert!(err.to_string().contains("dropped response channel"));
        worker.await.unwrap();

        let s = stats.take_sample().expect("a failure");
        assert_eq!((s.tiles(), s.failed, s.depth_now), (0, 1, 0));
    }

    #[tokio::test]
    #[ignore] // Requires GPU hardware
    async fn test_create_gpu_encoder_channels_convenience() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (channels, worker_handle, _adapter) =
            match create_gpu_encoder_channels(&test_gpu_device(), 4, shutdown) {
                Ok(r) => r,
                Err(_) => return,
            };

        // Verify both channels are connected
        assert!(channels.demand.is_connected());
        assert!(channels.prefetch.is_connected());
        assert!(!channels.demand.health().is_failed());

        // Drop both channels, verify worker stops: 10 s only guards a
        // hang, a real device's release may be slow.
        drop(channels);
        let mut worker_handle = worker_handle;
        worker_handle
            .wait(std::time::Duration::from_secs(10))
            .await
            .expect("worker should stop within timeout")
            .expect("worker should not panic");
    }

    // =========================================================================
    // Full-tile GPU pipeline tests (require GPU hardware)
    // =========================================================================

    /// The adapter the hardware tests use, as `texture.gpu_device`:
    /// `XEL_TEST_GPU_DEVICE`, `integrated` by default.
    fn test_gpu_device() -> String {
        std::env::var("XEL_TEST_GPU_DEVICE").unwrap_or_else(|_| "integrated".to_string())
    }

    /// Helper to create GPU resources for pipeline tests.
    fn gpu_resources_or_skip() -> Option<(
        wgpu::Device,
        wgpu::Queue,
        block_compression::GpuBlockCompressor,
    )> {
        use crate::dds::create_gpu_resources;
        match create_gpu_resources(&test_gpu_device(), &SharedGpuHealth::new()) {
            Ok((device, queue, compressor, _name)) => Some((device, queue, compressor)),
            Err(_) => None, // Skip if no GPU
        }
    }

    /// A channel on a real GPU worker.
    fn gpu_channel(
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: block_compression::GpuBlockCompressor,
    ) -> (GpuEncoderChannel, GpuWorkerHandle) {
        let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
        let health = SharedGpuHealth::new();
        let shutdown = CancellationToken::new();
        let worker = spawn_gpu_worker(
            device,
            queue,
            compressor,
            rx,
            shutdown.clone(),
            health.clone(),
        )
        .expect("a worker thread");
        (GpuEncoderChannel::new(tx, health, shutdown), worker)
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
        })
        .await
        .unwrap();

        let data = compressed(result).expect("pipeline compress should succeed");
        // 1 block × 8 bytes for BC1
        assert_eq!(data.len(), file_len_of(4));
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
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
            })
            .await
            .unwrap();

            let data =
                compressed(result).unwrap_or_else(|e| panic!("request {i} should succeed: {e}"));
            assert_eq!(data.len(), file_len_of(4));
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
                ch.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            let data = compressed(result).expect("concurrent pipeline compress should succeed");
            assert_eq!(data.len(), file_len_of(4));
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
            ch.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4, 4, 1))
        })
        .await
        .unwrap();
        assert_eq!(compressed(bc1).unwrap().len(), file_len_of(4));
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

        // Drop sender to close channel: 10 s only guards a hang, a real
        // device's release may be slow.
        drop(channel);

        let mut worker = worker;
        worker
            .wait(std::time::Duration::from_secs(10))
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
            channel.compress_mipmap_chain(image, DdsFormat::BC1, 1, file(4096, 4096, 1))
        })
        .await
        .unwrap();

        let data = compressed(result).expect("GPU compression should succeed");
        // BC1: 1024×1024 blocks × 8 bytes = 8388608 bytes
        assert_eq!(data.len(), crate::dds::layout::HEADER_LEN + 8_388_608);
    }

    /// A tile that fails on the GPU leaves nothing behind for the next
    /// (#423 review): one good tile, one whose deadline has already passed
    /// (its poll times out with the readback buffer's map still pending),
    /// then five good tiles, every one encoded and byte for byte a fresh
    /// processor's. With the kept resources reused after the failure, each
    /// of the five failed (`BufferStillMapped`, then a submission index
    /// that was never returned), and the GPU was off for the session.
    #[test]
    #[ignore] // Requires GPU hardware
    fn a_failed_tile_leaves_the_next_tiles_to_fresh_resources() {
        const SIZE: u32 = 4096;
        let levels = crate::dds::layout::full_chain_levels(SIZE, SIZE);
        let Some((device, queue, compressor)) = gpu_resources_or_skip() else {
            return;
        };
        let Some((fresh_device, fresh_queue, fresh_compressor)) = gpu_resources_or_skip() else {
            return;
        };
        let mut worker = WgpuTileProcessor::new(device, queue, compressor);
        let mut fresh = WgpuTileProcessor::new(fresh_device, fresh_queue, fresh_compressor);
        let encode = |processor: &mut WgpuTileProcessor, deadline: Instant| {
            let mut payload =
                vec![0; crate::dds::layout::chain_len(SIZE, SIZE, levels, DdsFormat::BC1)];
            processor
                .process(
                    varied(SIZE, SIZE),
                    DdsFormat::BC1,
                    levels as usize,
                    deadline,
                    &mut GpuTileTiming::default(),
                    &mut payload,
                )
                .map(|()| payload)
        };
        // Generous: only a guard against a hung GPU.
        let in_time = || Instant::now() + Duration::from_secs(60);

        let expected = encode(&mut fresh, in_time()).expect("the fresh processor's tile");
        let first = encode(&mut worker, in_time()).expect("the first tile");
        assert!(first == expected, "the first tile is the fresh one's");
        let late = encode(&mut worker, Instant::now());
        assert!(
            late.as_ref()
                .is_err_and(|e| e.to_string().contains("timed out")),
            "the tile past its deadline times out: {:?}",
            late.map(|p| p.len())
        );
        for tile in 1..=5 {
            let payload = encode(&mut worker, in_time())
                .unwrap_or_else(|e| panic!("tile {tile} after the late one: {e}"));
            assert!(payload == expected, "tile {tile} is the fresh one's");
        }
    }
}
