//! Counters and latency windows of the road service, and the summary line
//! it logs every minute. Counters are cumulative for `snapshot`; the
//! summary reports one window at a time and says nothing for a window in
//! which nothing happened.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tracing::debug;

use crate::metrics::{LatencySummary, LatencyWindow};

/// Cumulative counts since the service started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Compressed DSFs whose extraction was read from the disk cache.
    pub cache_hits: u64,
    /// Compressed DSFs read from the file with a disk cache present.
    pub cache_misses: u64,
    /// Probes that waited for roads or ended without them.
    pub probe_waits: u64,
    /// Probes whose roads were not all ready by their deadline.
    pub probe_timeouts: u64,
    /// Probes that committed an answer without all their roads.
    pub answered_before_roads: u64,
    pub tiles_ready: u64,
    pub tiles_failed: u64,
}

impl StatsSnapshot {
    /// The counts since `earlier`.
    fn since(&self, earlier: &Self) -> Self {
        Self {
            cache_hits: self.cache_hits - earlier.cache_hits,
            cache_misses: self.cache_misses - earlier.cache_misses,
            probe_waits: self.probe_waits - earlier.probe_waits,
            probe_timeouts: self.probe_timeouts - earlier.probe_timeouts,
            answered_before_roads: self.answered_before_roads - earlier.answered_before_roads,
            tiles_ready: self.tiles_ready - earlier.tiles_ready,
            tiles_failed: self.tiles_failed - earlier.tiles_failed,
        }
    }
}

/// What one tile build did, recorded once after its permit is released.
#[derive(Debug, Clone, Default)]
pub struct BuildRecord {
    pub ready: bool,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub permit_wait_us: u64,
    /// Wall time of each DSF read from its file.
    pub extract_us: Vec<u64>,
    /// Wall time spent on the blocking pool.
    pub cpu_us: u64,
    /// From the build's start to its tile being merged.
    pub total_us: u64,
}

/// One closed summary window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSummary {
    /// Counts within the window.
    pub delta: StatsSnapshot,
    pub extract: Option<LatencySummary>,
    pub tile_ready: Option<LatencySummary>,
    pub permit_wait: Option<LatencySummary>,
    pub cpu_ms: u64,
}

#[derive(Default)]
struct Window {
    extract: LatencyWindow,
    tile_ready: LatencyWindow,
    permit_wait: LatencyWindow,
    cpu_us: u64,
    /// The counters when the previous window closed.
    last: StatsSnapshot,
}

/// The road service's statistics. The mutex is a leaf: it is never held
/// across an await or while anything else is acquired.
#[derive(Default)]
pub struct Stats {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    probe_waits: AtomicU64,
    probe_timeouts: AtomicU64,
    answered_before_roads: AtomicU64,
    tiles_ready: AtomicU64,
    tiles_failed: AtomicU64,
    window: Mutex<Window>,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

impl Stats {
    pub fn record_build(&self, b: &BuildRecord) {
        add(&self.cache_hits, b.cache_hits);
        add(&self.cache_misses, b.cache_misses);
        add(
            if b.ready {
                &self.tiles_ready
            } else {
                &self.tiles_failed
            },
            1,
        );
        let Ok(mut w) = self.window.lock() else {
            return;
        };
        for &us in &b.extract_us {
            w.extract.record(us);
        }
        if b.ready {
            w.tile_ready.record(b.total_us);
        }
        w.permit_wait.record(b.permit_wait_us);
        w.cpu_us += b.cpu_us;
    }

    /// A probe that waited `waited_ms` for its roads, and whether they were
    /// all there in time. One that neither waited nor missed is not counted.
    pub fn record_probe(&self, waited_ms: u64, in_time: bool) {
        if waited_ms > 0 || !in_time {
            add(&self.probe_waits, 1);
        }
        if !in_time {
            add(&self.probe_timeouts, 1);
            add(&self.answered_before_roads, 1);
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StatsSnapshot {
            cache_hits: get(&self.cache_hits),
            cache_misses: get(&self.cache_misses),
            probe_waits: get(&self.probe_waits),
            probe_timeouts: get(&self.probe_timeouts),
            answered_before_roads: get(&self.answered_before_roads),
            tiles_ready: get(&self.tiles_ready),
            tiles_failed: get(&self.tiles_failed),
        }
    }

    /// Closes the window: its counts and latencies, or `None` when nothing
    /// happened in it.
    pub fn take_window(&self) -> Option<WindowSummary> {
        let now = self.snapshot();
        let mut w = self.window.lock().ok()?;
        let delta = now.since(&w.last);
        w.last = now;
        let summary = WindowSummary {
            delta,
            extract: w.extract.take(),
            tile_ready: w.tile_ready.take(),
            permit_wait: w.permit_wait.take(),
            cpu_ms: std::mem::take(&mut w.cpu_us) / 1000,
        };
        let idle = summary.delta == StatsSnapshot::default()
            && summary.extract.is_none()
            && summary.tile_ready.is_none()
            && summary.permit_wait.is_none();
        (!idle).then_some(summary)
    }

    /// Logs the window as `Night roads: summary`, unless nothing happened.
    pub fn log_summary(&self, tile_cache_bytes: u64) {
        let Some(w) = self.take_window() else {
            return;
        };
        let ms = |s: Option<LatencySummary>| {
            s.map_or((0, 0, 0, 0), |s| {
                (
                    s.median_us / 1000,
                    s.p95_us / 1000,
                    s.max_us / 1000,
                    s.count,
                )
            })
        };
        let (extract_median_ms, extract_p95_ms, extract_max_ms, extracted) = ms(w.extract);
        let (ready_median_ms, ready_p95_ms, ready_max_ms, _) = ms(w.tile_ready);
        let (_, permit_wait_p95_ms, _, _) = ms(w.permit_wait);
        let d = w.delta;
        debug!(
            tile_cache_bytes,
            cache_hits = d.cache_hits,
            cache_misses = d.cache_misses,
            cache_hit_rate = hit_rate(d.cache_hits, d.cache_misses),
            tiles_ready = d.tiles_ready,
            tiles_failed = d.tiles_failed,
            probe_waits = d.probe_waits,
            probe_timeouts = d.probe_timeouts,
            answered_before_roads = d.answered_before_roads,
            extracted,
            extract_median_ms,
            extract_p95_ms,
            extract_max_ms,
            ready_median_ms,
            ready_p95_ms,
            ready_max_ms,
            permit_wait_p95_ms,
            road_cpu_ms = w.cpu_ms,
            "Night roads: summary"
        );
    }
}

/// Hits over the lookups that could hit; `None` without any.
fn hit_rate(hits: u64, misses: u64) -> Option<f64> {
    let total = hits + misses;
    (total > 0).then(|| hits as f64 / total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(ready: bool, hits: u64, misses: u64) -> BuildRecord {
        BuildRecord {
            ready,
            cache_hits: hits,
            cache_misses: misses,
            permit_wait_us: 2_000,
            extract_us: vec![5_000, 7_000],
            cpu_us: 12_000,
            total_us: 15_000,
        }
    }

    #[test]
    fn a_window_where_nothing_happened_has_no_summary() {
        let s = Stats::default();
        assert_eq!(s.take_window(), None);
    }

    #[test]
    fn builds_are_counted_and_their_latencies_summarised_once() {
        let s = Stats::default();
        s.record_build(&build(true, 1, 1));
        s.record_build(&build(false, 0, 0));
        let snap = s.snapshot();
        assert_eq!(
            (
                snap.tiles_ready,
                snap.tiles_failed,
                snap.cache_hits,
                snap.cache_misses
            ),
            (1, 1, 1, 1)
        );
        let w = s.take_window().unwrap();
        assert_eq!(w.delta, snap);
        assert_eq!(w.extract.unwrap().count, 4);
        assert_eq!(w.tile_ready.unwrap().count, 1, "a failed tile is not ready");
        assert_eq!(w.permit_wait.unwrap().count, 2);
        assert_eq!(w.cpu_ms, 24);
        assert_eq!(s.take_window(), None, "the window closed");
        assert_eq!(s.snapshot(), snap, "the counters are cumulative");
    }

    #[test]
    fn a_window_reports_only_its_own_counts() {
        let s = Stats::default();
        s.record_build(&build(true, 2, 0));
        s.take_window().unwrap();
        s.record_probe(5, false);
        let w = s.take_window().unwrap();
        assert_eq!(
            w.delta,
            StatsSnapshot {
                probe_waits: 1,
                probe_timeouts: 1,
                answered_before_roads: 1,
                ..StatsSnapshot::default()
            }
        );
        assert_eq!(w.tile_ready, None);
    }

    #[test]
    fn a_probe_that_did_not_wait_and_got_its_roads_is_not_counted() {
        let s = Stats::default();
        s.record_probe(0, true);
        assert_eq!(s.snapshot(), StatsSnapshot::default());
        s.record_probe(3, true);
        let snap = s.snapshot();
        assert_eq!((snap.probe_waits, snap.probe_timeouts), (1, 0));
        assert_eq!(snap.answered_before_roads, 0);
    }

    #[test]
    fn hit_rate_is_over_lookups_that_could_hit() {
        assert_eq!(hit_rate(0, 0), None);
        assert_eq!(hit_rate(3, 1), Some(0.75));
    }
}
