//! FUSE demand over the recent past, for the dashboard's latency line (#481).
//!
//! The line once showed the whole session, so a cold start's waits stayed on
//! it for the rest of a flight that went well. It now describes the last few
//! samples. The metrics daemon already closes a sample every
//! [`MEMORY_SAMPLE_INTERVAL`] for the `Prefetch sample` line; the ring keeps
//! the last [`RECENT_SAMPLES`] of them, plus the open one, so the window rolls
//! on the daemon's own cadence with no second clock.
//!
//! Each sample keeps its waits as a [`LatencyHistogram`], which merges bucket
//! by bucket: summaries could not be combined into the window's percentiles.
//! Counters are cumulative in the daemon's state, so each sample keeps only
//! their values when it opened, and the window's counts are differences.

use std::collections::VecDeque;
use std::time::Duration;

use super::daemon::MEMORY_SAMPLE_INTERVAL;
use super::latency::{LatencyHistogram, LatencySummary};

/// Closed samples the recent window keeps, beside the open one.
pub const RECENT_SAMPLES: usize = 5;

/// The span the recent window covers at least, once a session is that long:
/// its closed samples. The open sample adds up to one more interval, so a
/// window with no waits had none for at least this long.
pub const RECENT_WINDOW: Duration =
    Duration::from_secs(MEMORY_SAMPLE_INTERVAL.as_secs() * RECENT_SAMPLES as u64);

/// FUSE demand counters: cumulative in the daemon, a difference in a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemandCounts {
    /// FUSE requests served from the memory or DDS disk cache.
    pub cached: u64,
    /// FUSE requests that needed a generation.
    pub generated: u64,
    /// FUSE waits abandoned at the generation timeout.
    pub timeouts: usize,
}

impl DemandCounts {
    /// What was counted between `start` and `self`.
    fn since(self, start: DemandCounts) -> DemandCounts {
        DemandCounts {
            cached: self.cached.saturating_sub(start.cached),
            generated: self.generated.saturating_sub(start.generated),
            timeouts: self.timeouts.saturating_sub(start.timeouts),
        }
    }
}

/// FUSE demand over the recent window, as the dashboard reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecentFuseDemand {
    /// The waits for a generation in the window; `None` when nothing waited.
    pub latency: Option<LatencySummary>,
    /// Requests served from cache, generations and timeouts in the window.
    pub counts: DemandCounts,
    /// How long ago the session's last wait for a generation ended; `None`
    /// before the first.
    pub since_last_wait: Option<Duration>,
}

/// One closed sample: its waits, and the counters when it opened.
#[derive(Clone, Debug, Default)]
struct Sample {
    waits: LatencyHistogram,
    opened_at: DemandCounts,
}

/// The last [`RECENT_SAMPLES`] closed samples and the open one, owned by the
/// metrics daemon.
#[derive(Clone, Debug, Default)]
pub(crate) struct RecentDemandRing {
    closed: VecDeque<Sample>,
    open: Sample,
}

impl RecentDemandRing {
    /// A FUSE wait for a generation, whatever its outcome, timeouts included.
    pub(crate) fn record_wait(&mut self, duration_us: u64) {
        self.open.waits.record(duration_us);
    }

    /// Closes the open sample, with the counters as they are now, and drops
    /// the oldest closed sample once more than [`RECENT_SAMPLES`] are kept.
    pub(crate) fn close(&mut self, now: DemandCounts) {
        let next = Sample {
            waits: LatencyHistogram::default(),
            opened_at: now,
        };
        self.closed
            .push_back(std::mem::replace(&mut self.open, next));
        while self.closed.len() > RECENT_SAMPLES {
            self.closed.pop_front();
        }
    }

    /// The window as published: every sample kept, the open one included, so
    /// a wait shows as soon as it ends rather than when its sample closes.
    pub(crate) fn summary(
        &self,
        now: DemandCounts,
        since_last_wait: Option<Duration>,
    ) -> RecentFuseDemand {
        let mut waits = self.open.waits.clone();
        for sample in &self.closed {
            waits.merge(&sample.waits);
        }
        let opened_at = self
            .closed
            .front()
            .map_or(self.open.opened_at, |oldest| oldest.opened_at);
        RecentFuseDemand {
            latency: waits.summary(),
            counts: now.since(opened_at),
            since_last_wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(cached: u64, generated: u64, timeouts: usize) -> DemandCounts {
        DemandCounts {
            cached,
            generated,
            timeouts,
        }
    }

    #[test]
    fn the_window_is_five_minutes() {
        assert_eq!(RECENT_WINDOW, Duration::from_secs(300));
    }

    #[test]
    fn an_empty_ring_has_no_latency_and_no_counts() {
        let ring = RecentDemandRing::default();
        assert_eq!(
            ring.summary(DemandCounts::default(), None),
            RecentFuseDemand::default()
        );
    }

    #[test]
    fn a_wait_in_the_open_sample_is_in_the_window_at_once() {
        let mut ring = RecentDemandRing::default();
        ring.record_wait(40_000);
        let recent = ring.summary(counts(0, 1, 0), Some(Duration::ZERO));
        let latency = recent.latency.expect("the open sample counts");
        assert_eq!((latency.count, latency.max_us), (1, 40_000));
        assert_eq!(recent.counts, counts(0, 1, 0));
        assert_eq!(recent.since_last_wait, Some(Duration::ZERO));
    }

    #[test]
    fn the_window_merges_its_samples_rather_than_their_summaries() {
        let mut ring = RecentDemandRing::default();
        // One slow sample and four fast ones: a median of the five medians
        // would read 10 ms, the merged median reads the slow sample's.
        for _ in 0..50 {
            ring.record_wait(2_000_000);
        }
        ring.close(counts(0, 50, 0));
        for _ in 0..4 {
            for _ in 0..10 {
                ring.record_wait(10_000);
            }
            ring.close(counts(0, 0, 0));
        }
        let latency = ring.summary(counts(0, 90, 0), None).latency.unwrap();
        assert_eq!(latency.count, 90);
        assert_eq!(latency.max_us, 2_000_000);
        assert!(latency.median_us > 1_000_000, "{latency:?}");
    }

    #[test]
    fn a_sample_leaves_the_window_after_five_more_close() {
        let mut ring = RecentDemandRing::default();
        ring.record_wait(10_000_000);
        ring.close(counts(0, 1, 1));
        for _ in 0..4 {
            ring.close(counts(0, 1, 1));
        }
        let held = ring.summary(counts(0, 1, 1), None);
        assert_eq!(
            held.latency.map(|s| s.max_us),
            Some(10_000_000),
            "five closed samples are kept"
        );

        ring.close(counts(0, 1, 1));
        let recent = ring.summary(counts(0, 1, 1), Some(Duration::from_secs(400)));
        assert_eq!(recent.latency, None, "the sixth close drops it");
        assert_eq!(recent.counts, counts(0, 0, 0));
        assert_eq!(recent.since_last_wait, Some(Duration::from_secs(400)));
    }

    #[test]
    fn counts_are_those_since_the_oldest_kept_sample_opened() {
        let mut ring = RecentDemandRing::default();
        // Before the ring is full the window starts with the session.
        ring.close(counts(100, 10, 1));
        assert_eq!(
            ring.summary(counts(150, 12, 1), None).counts,
            counts(150, 12, 1)
        );

        // Six closes later the oldest kept sample opened at the first close.
        for n in 1..=5u64 {
            ring.close(counts(100 + n * 10, 10 + n, 1));
        }
        assert_eq!(
            ring.summary(counts(170, 16, 2), None).counts,
            counts(70, 6, 1)
        );
    }

    #[test]
    fn counters_that_went_backwards_read_as_none() {
        let mut ring = RecentDemandRing::default();
        ring.close(counts(10, 10, 1));
        for _ in 0..RECENT_SAMPLES {
            ring.close(counts(10, 10, 1));
        }
        assert_eq!(
            ring.summary(counts(0, 0, 0), None).counts,
            DemandCounts::default()
        );
    }
}
