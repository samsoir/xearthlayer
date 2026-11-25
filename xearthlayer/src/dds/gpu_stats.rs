//! Per-sample figures of the GPU encode path (#419): the stages of each
//! tile, and how many requests are in the channel.
//!
//! [`GpuEncoderChannel`](super::gpu_channel::GpuEncoderChannel) records
//! every request it sends: the stages of each encoded tile, and a count of
//! the requests that came back without one. With `texture.compressor =
//! hybrid` it also counts the tiles it encoded with ISPC instead. In the service, a task started
//! by `service::builder::create_encoder` closes the window every
//! `metrics::MEMORY_SAMPLE_INTERVAL` (the memory sample's cadence) and
//! logs one `GPU encode sample` line. Anything else holding a channel must
//! call [`GpuEncodeStats::take_sample`] itself; until it does, each window
//! keeps at most [`WINDOW_CAP`] tiles. The numbers go to the log and, as
//! spans, to the profile trace only: no metric events, telemetry or
//! dashboard.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::gpu_channel::GpuTileTiming;
use crate::metrics::{LatencySummary, LatencyWindow, MEMORY_SAMPLE_INTERVAL};

/// Tiles one window summarises at most. Past it a tile is only counted
/// ([`GpuSample::unsampled`]), so a channel nobody samples stays bounded.
/// A 60 s sample at 50 tiles/s holds 3,000.
pub const WINDOW_CAP: usize = 10_000;

/// The stage windows of one sample, each in microseconds.
#[derive(Default)]
struct StageWindows {
    /// Tiles in the windows below, at most [`WINDOW_CAP`].
    recorded: usize,
    /// Encoded tiles past the cap.
    unsampled: u64,
    queue_wait: LatencyWindow,
    downsample: LatencyWindow,
    upload: LatencyWindow,
    gpu: LatencyWindow,
    readback: LatencyWindow,
    process: LatencyWindow,
    total: LatencyWindow,
}

/// One closed sample. Stage summaries cover the tiles encoded in it (the
/// first [`WINDOW_CAP`]); a request that came back without a tile counts in
/// [`failed`](Self::failed) only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuSample {
    pub queue_wait: Option<LatencySummary>,
    pub downsample: Option<LatencySummary>,
    pub upload: Option<LatencySummary>,
    pub gpu: Option<LatencySummary>,
    pub readback: Option<LatencySummary>,
    /// The worker's whole call per tile.
    pub process: Option<LatencySummary>,
    /// The caller's round trip, from starting its send to the reply.
    pub total: Option<LatencySummary>,
    /// Encoded tiles past [`WINDOW_CAP`], in no summary.
    pub unsampled: u64,
    /// Requests that came back without a tile: an error from the worker,
    /// a worker not running or gone before replying, or a handle already
    /// shut down.
    pub failed: u64,
    /// Tiles `texture.compressor = hybrid` encoded with ISPC on the calling
    /// thread because the worker's queue was taken or the worker had
    /// stopped. Not in [`tiles`](Self::tiles), which counts the worker's.
    pub overflowed: u64,
    /// Requests in the channel when the sample closed (see
    /// [`GpuEncodeStats::depth`]).
    pub depth_now: usize,
    /// The most requests in the channel at once during the sample.
    pub depth_max: usize,
}

impl GpuSample {
    /// Tiles encoded in the sample.
    pub fn tiles(&self) -> u64 {
        self.total.map_or(0, |s| s.count as u64) + self.unsampled
    }

    /// Nothing was encoded, overflowed, failed or waiting during the sample.
    fn is_idle(&self) -> bool {
        self.tiles() == 0
            && self.overflowed == 0
            && self.failed == 0
            && self.depth_max == 0
            && self.depth_now == 0
    }
}

/// The GPU path's stage windows and queue depth, shared by every caller of
/// one channel.
#[derive(Default)]
pub struct GpuEncodeStats {
    window: Mutex<StageWindows>,
    failed: AtomicU64,
    overflowed: AtomicU64,
    depth: AtomicUsize,
    depth_peak: AtomicUsize,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

impl GpuEncodeStats {
    /// Records one encoded tile: its stages and the caller's round trip.
    pub fn record_encoded(&self, timing: &GpuTileTiming, total: Duration) {
        let Ok(mut w) = self.window.lock() else {
            return;
        };
        if w.recorded >= WINDOW_CAP {
            w.unsampled += 1;
            return;
        }
        w.recorded += 1;
        w.queue_wait.record(micros(timing.queue_wait));
        w.downsample.record(micros(timing.downsample));
        w.upload.record(micros(timing.upload));
        w.gpu.record(micros(timing.gpu));
        w.readback.record(micros(timing.readback));
        w.process.record(micros(timing.process));
        w.total.record(micros(total));
    }

    /// Records one request that came back without a tile.
    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one tile encoded with ISPC instead of the worker.
    pub fn record_overflowed(&self) {
        self.overflowed.fetch_add(1, Ordering::Relaxed);
    }

    /// Requests in the channel now: buffered, blocked in `blocking_send` on
    /// a full channel, and in progress. The channel's own
    /// `max_capacity() - capacity()` sees only the buffered ones.
    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Counts one request into the channel until the guard drops: raised
    /// before the send, lowered once the reply (or the failure) is back.
    pub fn enter(&self) -> DepthGuard<'_> {
        let now = self.depth.fetch_add(1, Ordering::Relaxed) + 1;
        self.depth_peak.fetch_max(now, Ordering::Relaxed);
        DepthGuard(self)
    }

    /// Closes the sample: its figures, and a new sample started. `None`
    /// when nothing was encoded, overflowed, failed or waiting in it.
    pub fn take_sample(&self) -> Option<GpuSample> {
        let depth_now = self.depth();
        // The next sample's peak starts from what is in the channel now.
        let depth_max = self.depth_peak.swap(depth_now, Ordering::Relaxed);
        let failed = self.failed.swap(0, Ordering::Relaxed);
        let overflowed = self.overflowed.swap(0, Ordering::Relaxed);
        let mut w = self.window.lock().ok()?;
        w.recorded = 0;
        let sample = GpuSample {
            queue_wait: w.queue_wait.take(),
            downsample: w.downsample.take(),
            upload: w.upload.take(),
            gpu: w.gpu.take(),
            readback: w.readback.take(),
            process: w.process.take(),
            total: w.total.take(),
            unsampled: std::mem::take(&mut w.unsampled),
            failed,
            overflowed,
            depth_now,
            depth_max: depth_max.max(depth_now),
        };
        (!sample.is_idle()).then_some(sample)
    }

    /// Logs the sample as `GPU encode sample`, unless it was idle.
    pub fn log_sample(&self) {
        let Some(s) = self.take_sample() else {
            return;
        };
        // Milliseconds to two decimals: the small stages are under 1 ms.
        let ms = |us: u64| (us as f64 / 10.0).round() / 100.0;
        let fig = |s: Option<LatencySummary>| {
            s.map_or((0.0, 0.0, 0.0), |s| {
                (ms(s.median_us), ms(s.p95_us), ms(s.max_us))
            })
        };
        let (total_p50_ms, total_p95_ms, total_max_ms) = fig(s.total);
        let (queue_wait_p50_ms, queue_wait_p95_ms, queue_wait_max_ms) = fig(s.queue_wait);
        let (downsample_p50_ms, downsample_p95_ms, downsample_max_ms) = fig(s.downsample);
        let (upload_p50_ms, upload_p95_ms, upload_max_ms) = fig(s.upload);
        let (gpu_p50_ms, gpu_p95_ms, gpu_max_ms) = fig(s.gpu);
        let (readback_p50_ms, readback_p95_ms, readback_max_ms) = fig(s.readback);
        let (process_p50_ms, process_p95_ms, process_max_ms) = fig(s.process);
        tracing::info!(
            tiles = s.tiles(),
            unsampled = s.unsampled,
            failed = s.failed,
            ispc_overflow = s.overflowed,
            depth_now = s.depth_now,
            depth_max = s.depth_max,
            total_p50_ms,
            total_p95_ms,
            total_max_ms,
            queue_wait_p50_ms,
            queue_wait_p95_ms,
            queue_wait_max_ms,
            downsample_p50_ms,
            downsample_p95_ms,
            downsample_max_ms,
            upload_p50_ms,
            upload_p95_ms,
            upload_max_ms,
            gpu_p50_ms,
            gpu_p95_ms,
            gpu_max_ms,
            readback_p50_ms,
            readback_p95_ms,
            readback_max_ms,
            process_p50_ms,
            process_p95_ms,
            process_max_ms,
            "GPU encode sample"
        );
    }
}

/// One request counted in [`GpuEncodeStats::depth`] while it lives.
pub struct DepthGuard<'a>(&'a GpuEncodeStats);

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.0.depth.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Logs a sample of `stats` every [`MEMORY_SAMPLE_INTERVAL`] until
/// `shutdown` is cancelled or the stats are dropped with their channel.
/// Must be called inside a Tokio runtime.
pub(crate) fn start_sample_log(
    stats: Weak<GpuEncodeStats>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    sample_log_every(MEMORY_SAMPLE_INTERVAL, stats, shutdown)
}

fn sample_log_every(
    period: Duration,
    stats: Weak<GpuEncodeStats>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            let Some(stats) = stats.upgrade() else {
                break;
            };
            stats.log_sample();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn timing(scale: u64) -> GpuTileTiming {
        GpuTileTiming {
            queue_wait: ms(scale),
            downsample: ms(2 * scale),
            upload: ms(3 * scale),
            gpu: ms(4 * scale),
            readback: ms(5 * scale),
            process: ms(14 * scale),
        }
    }

    #[test]
    fn an_idle_sample_is_none() {
        assert!(GpuEncodeStats::default().take_sample().is_none());
    }

    #[test]
    fn a_sample_summarises_each_stage_and_counts_failures_apart() {
        let stats = GpuEncodeStats::default();
        stats.record_encoded(&timing(1), ms(16));
        stats.record_encoded(&timing(2), ms(31));
        stats.record_failed();

        let s = stats.take_sample().expect("a busy sample");
        assert_eq!(s.tiles(), 2);
        assert_eq!(s.failed, 1);
        let max = |s: Option<LatencySummary>| s.map(|s| (s.max_us, s.count));
        assert_eq!(max(s.queue_wait), Some((2_000, 2)));
        assert_eq!(max(s.downsample), Some((4_000, 2)));
        assert_eq!(max(s.upload), Some((6_000, 2)));
        assert_eq!(max(s.gpu), Some((8_000, 2)));
        assert_eq!(max(s.readback), Some((10_000, 2)));
        assert_eq!(max(s.process), Some((28_000, 2)));
        assert_eq!(max(s.total), Some((31_000, 2)));
        assert_eq!(s.total.unwrap().median_us, 16_000);

        assert!(stats.take_sample().is_none(), "taking starts a new sample");
    }

    /// A channel nobody samples stays bounded: past the cap a tile is only
    /// counted, and the next window starts empty.
    #[test]
    fn a_window_summarises_at_most_its_cap_and_counts_the_rest() {
        let stats = GpuEncodeStats::default();
        for _ in 0..WINDOW_CAP + 3 {
            stats.record_encoded(&timing(1), ms(16));
        }
        let s = stats.take_sample().expect("a busy sample");
        assert_eq!(s.total.unwrap().count, WINDOW_CAP);
        assert_eq!(s.unsampled, 3);
        assert_eq!(s.tiles(), WINDOW_CAP as u64 + 3);

        stats.record_encoded(&timing(1), ms(16));
        let s = stats.take_sample().expect("a busy sample");
        assert_eq!((s.total.unwrap().count, s.unsampled), (1, 0));
    }

    #[test]
    fn a_sample_of_failures_only_is_not_idle() {
        let stats = GpuEncodeStats::default();
        stats.record_failed();
        let s = stats.take_sample().expect("a failure is not idle");
        assert_eq!((s.tiles(), s.failed), (0, 1));
    }

    #[test]
    fn depth_rises_with_each_request_and_falls_as_each_is_answered() {
        let stats = GpuEncodeStats::default();
        let a = stats.enter();
        let b = stats.enter();
        let c = stats.enter();
        assert_eq!(stats.depth(), 3);
        drop(b);
        assert_eq!(stats.depth(), 2);

        let s = stats.take_sample().expect("requests are waiting");
        assert_eq!((s.depth_now, s.depth_max), (2, 3));
        drop((a, c));
        assert_eq!(stats.depth(), 0);

        // The next sample's peak starts from the depth at the last close.
        let s = stats.take_sample().expect("two were in the channel");
        assert_eq!((s.depth_now, s.depth_max), (0, 2));
        assert!(stats.take_sample().is_none());
    }

    /// Every field of every event emitted while `f` runs.
    fn capture_events(f: impl FnOnce()) -> Vec<HashMap<String, String>> {
        use tracing_subscriber::layer::SubscriberExt;

        struct Visitor<'a>(&'a mut HashMap<String, String>);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_string(), format!("{v:?}"));
            }
        }
        struct Layer(Arc<Mutex<Vec<HashMap<String, String>>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = HashMap::new();
                event.record(&mut Visitor(&mut fields));
                self.0.lock().unwrap().push(fields);
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Layer(Arc::clone(&events)));
        tracing::subscriber::with_default(subscriber, f);
        let captured = events.lock().unwrap().clone();
        captured
    }

    #[test]
    fn the_sample_line_carries_every_stage_in_milliseconds() {
        let stats = GpuEncodeStats::default();
        stats.record_encoded(&timing(1), Duration::from_micros(16_234));

        let events = capture_events(|| stats.log_sample());
        let line = events
            .iter()
            .find(|e| e.get("message").map(String::as_str) == Some("GPU encode sample"))
            .expect("a sample line");
        for (field, value) in [
            ("tiles", "1"),
            ("unsampled", "0"),
            ("failed", "0"),
            ("ispc_overflow", "0"),
            ("depth_now", "0"),
            ("depth_max", "0"),
            ("total_max_ms", "16.23"),
            ("queue_wait_p50_ms", "1.0"),
            ("downsample_p95_ms", "2.0"),
            ("upload_max_ms", "3.0"),
            ("gpu_p50_ms", "4.0"),
            ("readback_p50_ms", "5.0"),
            ("process_p50_ms", "14.0"),
        ] {
            assert_eq!(line.get(field).map(String::as_str), Some(value), "{field}");
        }
    }

    #[test]
    fn overflowed_tiles_are_counted_apart_from_the_workers() {
        let stats = GpuEncodeStats::default();
        stats.record_overflowed();
        stats.record_overflowed();
        stats.record_encoded(&timing(1), ms(16));

        let s = stats.take_sample().expect("a busy sample");
        assert_eq!((s.tiles(), s.overflowed), (1, 2));
        assert!(stats.take_sample().is_none(), "the count starts again");
    }

    #[test]
    fn a_sample_of_overflowed_tiles_only_is_logged() {
        let stats = GpuEncodeStats::default();
        stats.record_overflowed();

        let events = capture_events(|| stats.log_sample());
        let line = events
            .iter()
            .find(|e| e.get("message").map(String::as_str) == Some("GPU encode sample"))
            .expect("a sample line");
        assert_eq!(line.get("tiles").map(String::as_str), Some("0"));
        assert_eq!(line.get("ispc_overflow").map(String::as_str), Some("1"));
    }

    #[test]
    fn an_idle_sample_logs_nothing() {
        let stats = GpuEncodeStats::default();
        assert!(capture_events(|| stats.log_sample()).is_empty());
    }

    #[tokio::test]
    async fn the_sample_log_ends_at_shutdown() {
        let stats = Arc::new(GpuEncodeStats::default());
        let shutdown = CancellationToken::new();
        let task = start_sample_log(Arc::downgrade(&stats), shutdown.clone());
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the task ends at once")
            .unwrap();
    }

    #[tokio::test]
    async fn the_sample_log_ends_at_the_tick_after_its_stats_are_dropped() {
        let stats = Arc::new(GpuEncodeStats::default());
        let period = Duration::from_millis(10);
        let task = sample_log_every(period, Arc::downgrade(&stats), CancellationToken::new());
        tokio::time::sleep(period * 3).await;
        assert!(!task.is_finished(), "the task ticks while its stats live");
        drop(stats);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the task ends at its next tick")
            .unwrap();
    }
}
