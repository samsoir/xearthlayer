//! The installed panic hook with caught panics (#418).
//!
//! A process of its own: the hook is process-wide, and installed in the
//! library's unit tests it would report every expected panic of every test.
//!
//! The hook used to unmount the scenery on every panic, so one caught panic in
//! a tokio task or a `spawn_blocking` closure left XEarthLayer running with
//! nothing mounted. These tests install the real hook with a writer they read
//! back, panic where the service catches panics, and check that the report
//! was written for each and that the runtime carries on; for the GPU worker
//! (#420), that tiles go on being encoded, on the CPU. What the report says
//! is tested in `xearthlayer::panic`, the unmount on a main-thread panic in
//! `manager::mounts`.

use std::io::{self, Write};
use std::sync::Mutex;

use xearthlayer::panic as panic_handler;

/// Every report the hook wrote in this process.
static REPORTS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The hook's writer: appends to [`REPORTS`].
struct Reports;

impl Write for Reports {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        REPORTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn install() {
    panic_handler::install(|| Reports);
}

fn reported(message: &str) -> bool {
    let reports = REPORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&reports).contains(&format!("Message: {message}"))
}

#[test]
fn panics_caught_by_tokio_are_reported_and_the_runtime_keeps_serving() {
    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("runtime builds");

    let task = runtime.block_on(runtime.spawn(async { panic!("panic in a FUSE task") }));
    let blocking = runtime.block_on(
        runtime.spawn_blocking(|| -> u32 { panic!("panic in an encode on the blocking pool") }),
    );

    assert!(task.expect_err("the task panicked").is_panic());
    assert!(blocking.expect_err("the closure panicked").is_panic());
    assert!(reported("panic in a FUSE task"));
    assert!(reported("panic in an encode on the blocking pool"));

    // Neither blocked nor aborted: the next task and blocking call run.
    let next = runtime.block_on(async {
        let encoded = tokio::task::spawn_blocking(|| 7u32).await.unwrap();
        tokio::spawn(async move { encoded * 6 }).await.unwrap()
    });
    assert_eq!(next, 42);
}

#[test]
fn a_panic_caught_on_a_plain_thread_is_reported_and_the_thread_carries_on() {
    install();

    // The GPU worker's pattern: catch per item and keep going.
    let worker = std::thread::spawn(|| {
        let caught = std::panic::catch_unwind(|| panic!("panic in one tile"));
        assert!(caught.is_err());
        "worker carried on"
    });

    assert_eq!(worker.join().unwrap(), "worker carried on");
    assert!(reported("panic in one tile"));
}

/// The GPU worker's own panic (#420), under the installed hook: a processor
/// that panics as `Device::poll` does on a lost device. The hook reports it
/// and unmounts nothing; the real worker loop catches it, fails the health
/// and ends; the runtime carries on and the tile after it is encoded on the
/// fallback.
#[test]
fn a_gpu_worker_panic_is_reported_and_tiles_fall_back_to_the_cpu() {
    use std::sync::Arc;
    use std::time::Instant;

    use image::RgbaImage;
    use tokio_util::sync::CancellationToken;
    use xearthlayer::dds::gpu_channel::{
        spawn_tile_worker_with_clock, Clock, GpuEncoderChannel, GpuTileTiming, HangRule,
        SharedClock, TileProcessor, CHANNEL_CAPACITY,
    };
    use xearthlayer::dds::gpu_health::SharedGpuHealth;
    use xearthlayer::dds::{DdsEncoder, DdsError, DdsFormat, IspcCompressor, MipmapCompressor};

    /// A clock that never moves, shared by the channel and its worker: the
    /// tile is never waited out, however slowly the test runs.
    struct Frozen(Instant);

    impl Clock for Frozen {
        fn now(&self) -> Instant {
            self.0
        }
    }

    struct LosesTheDevice;

    impl TileProcessor for LosesTheDevice {
        fn process(
            &mut self,
            _image: RgbaImage,
            _format: DdsFormat,
            _levels: usize,
            _deadline: Instant,
            _timing: &mut GpuTileTiming,
            _payload: &mut [u8],
        ) -> Result<(), DdsError> {
            panic!("panic in the GPU worker: Device::poll: device lost")
        }
    }

    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("runtime builds");

    let clock: SharedClock = Arc::new(Frozen(Instant::now()));
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let health = SharedGpuHealth::new();
    let channel = Arc::new(
        GpuEncoderChannel::new(tx, health.clone(), CancellationToken::new())
            .with_clock(Arc::clone(&clock)),
    );
    let encoder = Arc::new(DdsEncoder::new(DdsFormat::BC1).with_mipmap_compressor(
        Arc::clone(&channel) as Arc<dyn MipmapCompressor>,
        Arc::new(IspcCompressor),
    ));
    let mut worker = {
        let _guard = runtime.enter();
        spawn_tile_worker_with_clock(
            LosesTheDevice,
            rx,
            CancellationToken::new(),
            health.clone(),
            HangRule::default(),
            clock,
        )
    };

    let encode = |encoder: &Arc<DdsEncoder>| {
        let encoder = Arc::clone(encoder);
        runtime
            .block_on(runtime.spawn_blocking(move || encoder.encode(RgbaImage::new(64, 64))))
            .expect("the encode itself never panics")
    };
    assert!(
        encode(&encoder).is_err(),
        "the tile in the processor is lost"
    );
    worker
        .wait_blocking(std::time::Duration::from_secs(5))
        .expect("the loop ended")
        .expect("the loop caught the panic and ended");

    assert!(reported(
        "panic in the GPU worker: Device::poll: device lost"
    ));
    assert_eq!(
        health.failure(),
        Some("GPU worker panicked: panic in the GPU worker: Device::poll: device lost")
    );
    let expected = DdsEncoder::new(DdsFormat::BC1)
        .with_compressor(Arc::new(IspcCompressor))
        .encode(RgbaImage::new(64, 64))
        .unwrap();
    assert_eq!(
        encode(&encoder).expect("the next tile is encoded on the fallback"),
        expected
    );
}
