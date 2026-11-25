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
    /// Compressed DSFs read in full from the file with a disk cache
    /// present (a base mesh read only to its header is not one).
    pub cache_misses: u64,
    /// Probe footprints that waited for a tile or timed out.
    pub probe_waits: u64,
    /// Probe footprints with a tile still building past its deadline.
    pub probe_timeouts: u64,
    /// Generation footprints that waited for a tile or timed out.
    pub generate_waits: u64,
    /// Generation footprints with a tile still building past its deadline.
    pub generate_timeouts: u64,
    /// Dark lit answers committed while the probe's roads were late.
    pub answered_before_roads: u64,
    pub tiles_ready: u64,
    pub tiles_failed: u64,
    /// Road cache entries never written because the write backlog was
    /// full: the next session extracts their DSFs again.
    pub road_writes_dropped: u64,
}

impl StatsSnapshot {
    /// The counts since `earlier`.
    fn since(&self, earlier: &Self) -> Self {
        Self {
            cache_hits: self.cache_hits - earlier.cache_hits,
            cache_misses: self.cache_misses - earlier.cache_misses,
            probe_waits: self.probe_waits - earlier.probe_waits,
            probe_timeouts: self.probe_timeouts - earlier.probe_timeouts,
            generate_waits: self.generate_waits - earlier.generate_waits,
            generate_timeouts: self.generate_timeouts - earlier.generate_timeouts,
            answered_before_roads: self.answered_before_roads - earlier.answered_before_roads,
            tiles_ready: self.tiles_ready - earlier.tiles_ready,
            tiles_failed: self.tiles_failed - earlier.tiles_failed,
            road_writes_dropped: self.road_writes_dropped - earlier.road_writes_dropped,
        }
    }
}

/// What one tile build did, recorded once when the build ends, after its
/// permit is released and before its cache entries are written.
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
    /// Footprints of each purpose.
    pub probe: WaitCounters,
    pub generate: WaitCounters,
    answered_before_roads: AtomicU64,
    tiles_ready: AtomicU64,
    tiles_failed: AtomicU64,
    road_writes_dropped: AtomicU64,
    window: Mutex<Window>,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Waits and timeouts of the footprints of one purpose.
#[derive(Default)]
pub struct WaitCounters {
    waits: AtomicU64,
    timeouts: AtomicU64,
}

impl WaitCounters {
    /// A footprint that `waited` for a tile, and whether a tile was still
    /// building past its deadline. One that did neither is not counted.
    pub fn record(&self, waited: bool, timed_out: bool) {
        if waited || timed_out {
            add(&self.waits, 1);
        }
        if timed_out {
            add(&self.timeouts, 1);
        }
    }

    fn get(&self) -> (u64, u64) {
        (
            self.waits.load(Ordering::Relaxed),
            self.timeouts.load(Ordering::Relaxed),
        )
    }
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

    /// A dark lit answer committed while the probe's roads were late.
    pub fn record_dark_answer(&self) {
        add(&self.answered_before_roads, 1);
    }

    /// A built tile's `entries` road cache entries dropped, the write
    /// backlog being full.
    pub fn record_writes_dropped(&self, entries: u64) {
        add(&self.road_writes_dropped, entries);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let (probe_waits, probe_timeouts) = self.probe.get();
        let (generate_waits, generate_timeouts) = self.generate.get();
        StatsSnapshot {
            cache_hits: get(&self.cache_hits),
            cache_misses: get(&self.cache_misses),
            probe_waits,
            probe_timeouts,
            generate_waits,
            generate_timeouts,
            answered_before_roads: get(&self.answered_before_roads),
            tiles_ready: get(&self.tiles_ready),
            tiles_failed: get(&self.tiles_failed),
            road_writes_dropped: get(&self.road_writes_dropped),
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
            generate_waits = d.generate_waits,
            generate_timeouts = d.generate_timeouts,
            answered_before_roads = d.answered_before_roads,
            road_writes_dropped = d.road_writes_dropped,
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
        s.probe.record(true, true);
        s.record_dark_answer();
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
    fn a_footprint_that_neither_waited_nor_timed_out_is_not_counted() {
        let s = Stats::default();
        s.probe.record(false, false);
        assert_eq!(s.snapshot(), StatsSnapshot::default());
        s.probe.record(true, false);
        s.generate.record(false, true);
        let snap = s.snapshot();
        assert_eq!((snap.probe_waits, snap.probe_timeouts), (1, 0));
        assert_eq!((snap.generate_waits, snap.generate_timeouts), (1, 1));
        assert_eq!(snap.answered_before_roads, 0);
    }

    #[test]
    fn dropped_road_writes_are_counted_and_reported_per_window() {
        let s = Stats::default();
        s.record_writes_dropped(3);
        let w = s.take_window().expect("a drop is something that happened");
        assert_eq!(
            w.delta,
            StatsSnapshot {
                road_writes_dropped: 3,
                ..StatsSnapshot::default()
            }
        );
        s.record_writes_dropped(2);
        assert_eq!(s.take_window().unwrap().delta.road_writes_dropped, 2);
        assert_eq!(s.snapshot().road_writes_dropped, 5, "cumulative");
    }

    #[test]
    fn hit_rate_is_over_lookups_that_could_hit() {
        assert_eq!(hit_rate(0, 0), None);
        assert_eq!(hit_rate(3, 1), Some(0.75));
    }
}
