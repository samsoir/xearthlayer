//! Latency summary over one sample window.
//!
//! The no-harm measure for the prefetch lane is FUSE demand latency per
//! 60 s sample (#296). A window collects durations and `take()` closes the
//! sample, so each log line describes only its own interval.

/// Median, 95th percentile and maximum of one closed window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub median_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub count: usize,
}

/// Smallest bucket bound, and the width ratio between neighbouring bounds.
const HIST_BASE_US: f64 = 1_000.0;
const HIST_RATIO: f64 = 1.05;
/// 1 ms * 1.05^299 is about 36 minutes, past the 10 minute design range;
/// anything longer lands in the last bucket.
const HIST_BUCKETS: usize = 300;

/// Whole-session latency in fixed memory (#296).
///
/// The per-sample window sorts and discards; a flight lasts hours, so the
/// session figures come from log-spaced buckets (about 5% relative width)
/// plus an exact maximum and count. Percentiles are bucket upper bounds,
/// clamped to the maximum.
#[derive(Clone, Debug)]
pub struct LatencyHistogram {
    buckets: Box<[u64; HIST_BUCKETS]>,
    count: usize,
    max_us: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: Box::new([0; HIST_BUCKETS]),
            count: 0,
            max_us: 0,
        }
    }
}

impl LatencyHistogram {
    pub fn record(&mut self, duration_us: u64) {
        let index = if (duration_us as f64) <= HIST_BASE_US {
            0
        } else {
            let i = ((duration_us as f64 / HIST_BASE_US).ln() / HIST_RATIO.ln()).ceil();
            (i as usize).min(HIST_BUCKETS - 1)
        };
        self.buckets[index] += 1;
        self.count += 1;
        self.max_us = self.max_us.max(duration_us);
    }

    /// Figures over everything recorded so far; `None` when nothing was.
    pub fn summary(&self) -> Option<LatencySummary> {
        if self.count == 0 {
            return None;
        }
        // Nearest rank over the buckets, reported as the bucket's upper bound.
        let rank = |p: f64| {
            let target = ((p * self.count as f64).ceil() as usize).clamp(1, self.count);
            let mut seen = 0;
            for (i, n) in self.buckets.iter().enumerate() {
                seen += *n as usize;
                if seen >= target {
                    let bound = (HIST_BASE_US * HIST_RATIO.powi(i as i32)).ceil() as u64;
                    return bound.min(self.max_us);
                }
            }
            self.max_us
        };
        Some(LatencySummary {
            median_us: rank(0.5),
            p95_us: rank(0.95),
            max_us: self.max_us,
            count: self.count,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatencyWindow {
    samples: Vec<u64>,
}

impl LatencyWindow {
    pub fn record(&mut self, duration_us: u64) {
        self.samples.push(duration_us);
    }

    /// Closes the window: summarises it and starts a new one. `None` when
    /// nothing was recorded.
    pub fn take(&mut self) -> Option<LatencySummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = std::mem::take(&mut self.samples);
        sorted.sort_unstable();
        let n = sorted.len();
        // Nearest rank: the smallest value with at least p of the samples at
        // or below it.
        let rank = |p: f64| sorted[((p * n as f64).ceil() as usize).clamp(1, n) - 1];
        Some(LatencySummary {
            median_us: rank(0.5),
            p95_us: rank(0.95),
            max_us: sorted[n - 1],
            count: n,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_has_no_summary() {
        assert!(LatencyWindow::default().take().is_none());
    }

    #[test]
    fn summary_is_nearest_rank_and_window_resets() {
        let mut w = LatencyWindow::default();
        for ms in 1..=100u64 {
            w.record(ms * 1000);
        }
        let s = w.take().unwrap();
        assert_eq!(
            (s.median_us, s.p95_us, s.max_us, s.count),
            (50_000, 95_000, 100_000, 100)
        );
        assert!(w.take().is_none(), "take() starts a new sample");
    }

    #[test]
    fn empty_histogram_has_no_summary() {
        assert!(LatencyHistogram::default().summary().is_none());
    }

    #[test]
    fn histogram_keeps_exact_max_and_count() {
        let mut h = LatencyHistogram::default();
        for us in [5_000u64, 123_457, 80_000] {
            h.record(us);
        }
        let s = h.summary().unwrap();
        assert_eq!((s.max_us, s.count), (123_457, 3));
    }

    #[test]
    fn histogram_percentiles_are_within_five_percent() {
        let mut h = LatencyHistogram::default();
        for ms in 1..=1000u64 {
            h.record(ms * 1000);
        }
        let s = h.summary().unwrap();
        let within =
            |got: u64, exact: u64| (got as f64 - exact as f64).abs() / exact as f64 <= 0.05;
        assert!(within(s.median_us, 500_000), "median {}", s.median_us);
        assert!(within(s.p95_us, 950_000), "p95 {}", s.p95_us);
        assert_eq!(s.max_us, 1_000_000);
    }

    #[test]
    fn histogram_counts_values_beyond_the_top_bucket() {
        let mut h = LatencyHistogram::default();
        h.record(10_000);
        h.record(3_600_000_000);
        let s = h.summary().unwrap();
        assert_eq!((s.max_us, s.count), (3_600_000_000, 2));
        assert!(s.p95_us <= s.max_us);
    }

    #[test]
    fn histogram_handles_sub_millisecond_and_zero() {
        let mut h = LatencyHistogram::default();
        h.record(0);
        h.record(300);
        let s = h.summary().unwrap();
        assert_eq!((s.max_us, s.count), (300, 2));
    }
}
