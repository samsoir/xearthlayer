//! Counters and latency windows of the lit source, and the summary line it
//! logs every minute (`Night lighting: summary`), built as the road
//! service's (`roads/stats.rs`). Counters are cumulative for `snapshot`;
//! the summary reports one window at a time and says nothing for a window
//! in which nothing happened.
//!
//! Every record is one call at the event it counts; the mutex holding the
//! latency windows is a leaf, never held across an await or while anything
//! else is acquired.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use moka::notification::RemovalCause;
use tracing::debug;

use super::disk::EntryReject;
use super::service::RoadsState;
use crate::metrics::{LatencySummary, LatencyWindow};

/// Where the bytes of one lit read came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The footprint is dark: the minimal black DDS.
    Dark,
    /// A read of a lit file that did not reach the visible levels: the
    /// header and black levels, no payload asked for.
    Blank,
    /// The visible levels without loading them: from the payload cache, or
    /// from another read's load of them that this read joined.
    MemoryHit,
    /// The visible levels loaded from the lit disk tier.
    DiskHit,
    /// The visible levels generated.
    Generated,
    /// The read reached the visible levels and they are black: a recipe is
    /// unreadable or generation failed.
    Black,
}

impl ReadOutcome {
    /// Its name in the debug lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Blank => "blank",
            Self::MemoryHit => "memory",
            Self::DiskHit => "disk",
            Self::Generated => "generated",
            Self::Black => "black",
        }
    }
}

/// What a probe answered, and from what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// Lit by its recipe (checked first).
    Recipe,
    /// Lit by a road alone.
    Road,
    Dark,
}

impl ProbeAnswer {
    pub fn is_lit(self) -> bool {
        self != Self::Dark
    }

    /// Its name in the debug lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Recipe => "recipe",
            Self::Road => "road",
            Self::Dark => "dark",
        }
    }
}

/// Why a lit disk tier lookup did not serve a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskMiss {
    /// No entry under the key.
    Absent,
    /// The tier returned an error.
    ReadError,
    /// See [`EntryReject::Corrupt`].
    Corrupt,
    /// An entry stamped with another fingerprint.
    Fingerprint,
    /// An entry stamped with another payload length.
    Length,
    /// No fingerprint could be computed, so no entry was looked up.
    NoFingerprint,
}

impl From<EntryReject> for DiskMiss {
    fn from(reject: EntryReject) -> Self {
        match reject {
            EntryReject::Corrupt => Self::Corrupt,
            EntryReject::Fingerprint => Self::Fingerprint,
            EntryReject::Length => Self::Length,
        }
    }
}

/// Why a payload generated with a lit disk tier present was not written
/// back: the first reason found, in the order the source checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBackSkip {
    /// No fingerprint could be computed. Kept in memory unless its roads
    /// were late.
    NoFingerprint,
    /// A road tile was still building at the deadline: kept nowhere.
    RoadsLate,
    /// A road tile failed: kept in memory only.
    RoadFailed,
    /// An art file failed to read this session: kept in memory only.
    ArtFailed,
}

/// What one generation did, recorded once when it ends.
#[derive(Debug, Clone, Copy)]
pub struct GenerationRecord {
    /// From asking for a generation permit to holding one.
    pub permit_wait: Duration,
    /// From holding the permit to the payload (or its failure).
    pub generation: Duration,
    /// Its road tiles when it asked for the permit.
    pub roads: RoadsState,
    /// The roads timed out, and every road tile was ready by the time the
    /// permit was held (#427).
    pub roads_ready_at_permit: bool,
    /// Whether it gave a payload (`false`: served black).
    pub payload: bool,
}

/// What one probe did, recorded once when it answered.
#[derive(Debug, Clone, Copy)]
pub struct ProbeRecord {
    /// Waiting for the footprint's road tiles.
    pub road_wait: Duration,
    /// From asking for a probe permit to holding one.
    pub permit_wait: Duration,
    /// Its answer, when it was the one committed.
    pub committed: Option<ProbeAnswer>,
}

/// Cumulative counts since the source was built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LitStatsSnapshot {
    /// By [`ReadOutcome`], in its declaration order.
    pub reads: [u64; 6],
    /// Committed probe answers by [`ProbeAnswer`]: recipe, road, dark.
    pub probes: [u64; 3],
    /// Generations by their road tiles: complete, timed out, failed.
    pub generation_roads: [u64; 3],
    /// Generations that gave no payload (served black).
    pub generations_black: u64,
    /// Generations whose roads timed out and were all ready at the permit.
    pub roads_ready_at_permit: u64,
    /// Write-backs skipped by [`WriteBackSkip`], in its declaration order.
    pub write_backs_skipped: [u64; 4],
    /// Payload cache lookups whose load did not run.
    pub payload_hits: u64,
    /// Payload cache lookups whose load ran.
    pub payload_misses: u64,
    /// Payloads evicted for size.
    pub payload_evictions: u64,
    pub disk_hits: u64,
    /// By [`DiskMiss`], in its declaration order.
    pub disk_misses: [u64; 6],
    pub disk_writes: u64,
    pub disk_write_failures: u64,
}

impl LitStatsSnapshot {
    pub fn reads(&self, outcome: ReadOutcome) -> u64 {
        self.reads[outcome as usize]
    }

    pub fn probes(&self, answer: ProbeAnswer) -> u64 {
        self.probes[answer as usize]
    }

    pub fn generations(&self) -> u64 {
        self.generation_roads.iter().sum()
    }

    pub fn generation_roads(&self, roads: RoadsState) -> u64 {
        self.generation_roads[roads as usize]
    }

    pub fn write_backs_skipped(&self, skip: WriteBackSkip) -> u64 {
        self.write_backs_skipped[skip as usize]
    }

    pub fn disk_misses(&self, miss: DiskMiss) -> u64 {
        self.disk_misses[miss as usize]
    }

    /// The counts since `earlier`.
    fn since(&self, earlier: &Self) -> Self {
        fn each<const N: usize>(a: [u64; N], b: [u64; N]) -> [u64; N] {
            std::array::from_fn(|i| a[i] - b[i])
        }
        Self {
            reads: each(self.reads, earlier.reads),
            probes: each(self.probes, earlier.probes),
            generation_roads: each(self.generation_roads, earlier.generation_roads),
            generations_black: self.generations_black - earlier.generations_black,
            roads_ready_at_permit: self.roads_ready_at_permit - earlier.roads_ready_at_permit,
            write_backs_skipped: each(self.write_backs_skipped, earlier.write_backs_skipped),
            payload_hits: self.payload_hits - earlier.payload_hits,
            payload_misses: self.payload_misses - earlier.payload_misses,
            payload_evictions: self.payload_evictions - earlier.payload_evictions,
            disk_hits: self.disk_hits - earlier.disk_hits,
            disk_misses: each(self.disk_misses, earlier.disk_misses),
            disk_writes: self.disk_writes - earlier.disk_writes,
            disk_write_failures: self.disk_write_failures - earlier.disk_write_failures,
        }
    }
}

/// One closed summary window.
#[derive(Debug, Clone, PartialEq)]
pub struct LitWindow {
    /// Counts within the window.
    pub delta: LitStatsSnapshot,
    /// Read latency by [`ReadOutcome`], probe included.
    pub reads: [Option<LatencySummary>; 6],
    pub generation_permit_wait: Option<LatencySummary>,
    pub generation: Option<LatencySummary>,
    pub probe_road_wait: Option<LatencySummary>,
    pub probe_permit_wait: Option<LatencySummary>,
    /// Waits for the disk permit of a lookup (write-backs are not timed).
    pub disk_permit_wait: Option<LatencySummary>,
    /// The most generations waiting for a permit at once.
    pub generations_waiting_peak: u64,
    /// The most write-backs in flight at once, waiting for their permit
    /// or writing.
    pub write_backs_pending_peak: u64,
}

impl LitWindow {
    pub fn read(&self, outcome: ReadOutcome) -> Option<LatencySummary> {
        self.reads[outcome as usize]
    }
}

#[derive(Default)]
struct Window {
    reads: [LatencyWindow; 6],
    generation_permit_wait: LatencyWindow,
    generation: LatencyWindow,
    probe_road_wait: LatencyWindow,
    probe_permit_wait: LatencyWindow,
    disk_permit_wait: LatencyWindow,
    /// The counters when the previous window closed.
    last: LitStatsSnapshot,
}

/// Counters indexed by an enum's declaration order.
struct Tally<const N: usize>([AtomicU64; N]);

impl<const N: usize> Default for Tally<N> {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

impl<const N: usize> Tally<N> {
    fn add(&self, i: usize) {
        add(&self.0[i]);
    }

    fn get(&self) -> [u64; N] {
        std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed))
    }
}

fn add(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// How many are in some state now, and the most at once since the window
/// opened.
#[derive(Default)]
pub struct Gauge {
    now: AtomicU64,
    peak: AtomicU64,
}

impl Gauge {
    /// One more until the guard drops.
    pub fn enter(&self) -> GaugeGuard<'_> {
        let now = self.now.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(now, Ordering::Relaxed);
        GaugeGuard(self)
    }

    /// How many now.
    pub fn now(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }

    /// The window's peak; the next window starts from the current count.
    fn take_peak(&self) -> u64 {
        self.peak.swap(self.now(), Ordering::Relaxed)
    }
}

/// Holds one place in a [`Gauge`].
pub struct GaugeGuard<'a>(&'a Gauge);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::Relaxed);
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The lit source's statistics.
#[derive(Default)]
pub struct LitStats {
    reads: Tally<6>,
    probes: Tally<3>,
    generation_roads: Tally<3>,
    generations_black: AtomicU64,
    roads_ready_at_permit: AtomicU64,
    write_backs_skipped: Tally<4>,
    payload_hits: AtomicU64,
    payload_misses: AtomicU64,
    payload_evictions: AtomicU64,
    disk_hits: AtomicU64,
    disk_misses: Tally<6>,
    disk_writes: AtomicU64,
    disk_write_failures: AtomicU64,
    /// Generations waiting for a permit.
    pub generations_waiting: Gauge,
    /// Write-backs in flight.
    pub write_backs_pending: Gauge,
    window: Mutex<Window>,
}

impl LitStats {
    /// A lit read served by `outcome`, `elapsed` after it was asked for.
    pub fn record_read(&self, outcome: ReadOutcome, elapsed: Duration) {
        self.reads.add(outcome as usize);
        if let Ok(mut w) = self.window.lock() {
            w.reads[outcome as usize].record(micros(elapsed));
        }
    }

    pub fn record_probe(&self, p: &ProbeRecord) {
        if let Some(answer) = p.committed {
            self.probes.add(answer as usize);
        }
        if let Ok(mut w) = self.window.lock() {
            w.probe_road_wait.record(micros(p.road_wait));
            w.probe_permit_wait.record(micros(p.permit_wait));
        }
    }

    pub fn record_generation(&self, g: &GenerationRecord) {
        self.generation_roads.add(g.roads as usize);
        if !g.payload {
            add(&self.generations_black);
        }
        if g.roads_ready_at_permit {
            add(&self.roads_ready_at_permit);
        }
        if let Ok(mut w) = self.window.lock() {
            w.generation_permit_wait.record(micros(g.permit_wait));
            w.generation.record(micros(g.generation));
        }
    }

    /// A payload cache lookup: `loaded` when its load ran (a miss).
    pub fn record_payload_lookup(&self, loaded: bool) {
        add(if loaded {
            &self.payload_misses
        } else {
            &self.payload_hits
        });
    }

    /// A payload left the cache. Only evictions for size are counted: the
    /// cache invalidates nothing, has no expiry and never replaces a value.
    pub fn record_payload_removal(&self, cause: RemovalCause) {
        if cause == RemovalCause::Size {
            add(&self.payload_evictions);
        }
    }

    /// A lit disk tier lookup that waited `permit_wait` for its permit and
    /// hit, or missed for `miss`.
    pub fn record_disk_lookup(&self, permit_wait: Duration, miss: Option<DiskMiss>) {
        match miss {
            None => add(&self.disk_hits),
            Some(miss) => self.disk_misses.add(miss as usize),
        }
        if let Ok(mut w) = self.window.lock() {
            w.disk_permit_wait.record(micros(permit_wait));
        }
    }

    /// A write-back that ended, written or not.
    pub fn record_disk_write(&self, written: bool) {
        add(if written {
            &self.disk_writes
        } else {
            &self.disk_write_failures
        });
    }

    pub fn record_write_back_skipped(&self, skip: WriteBackSkip) {
        self.write_backs_skipped.add(skip as usize);
    }

    pub fn snapshot(&self) -> LitStatsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LitStatsSnapshot {
            reads: self.reads.get(),
            probes: self.probes.get(),
            generation_roads: self.generation_roads.get(),
            generations_black: get(&self.generations_black),
            roads_ready_at_permit: get(&self.roads_ready_at_permit),
            write_backs_skipped: self.write_backs_skipped.get(),
            payload_hits: get(&self.payload_hits),
            payload_misses: get(&self.payload_misses),
            payload_evictions: get(&self.payload_evictions),
            disk_hits: get(&self.disk_hits),
            disk_misses: self.disk_misses.get(),
            disk_writes: get(&self.disk_writes),
            disk_write_failures: get(&self.disk_write_failures),
        }
    }

    /// Closes the window: its counts and latencies, or `None` when nothing
    /// happened in it. The peaks alone do not make a window active.
    pub fn take_window(&self) -> Option<LitWindow> {
        let now = self.snapshot();
        let mut w = self.window.lock().ok()?;
        let delta = now.since(&w.last);
        w.last = now;
        let summary = LitWindow {
            delta,
            reads: std::array::from_fn(|i| w.reads[i].take()),
            generation_permit_wait: w.generation_permit_wait.take(),
            generation: w.generation.take(),
            probe_road_wait: w.probe_road_wait.take(),
            probe_permit_wait: w.probe_permit_wait.take(),
            disk_permit_wait: w.disk_permit_wait.take(),
            generations_waiting_peak: self.generations_waiting.take_peak(),
            write_backs_pending_peak: self.write_backs_pending.take_peak(),
        };
        let idle = summary.delta == LitStatsSnapshot::default()
            && summary.reads.iter().all(Option::is_none)
            && [
                summary.generation_permit_wait,
                summary.generation,
                summary.probe_road_wait,
                summary.probe_permit_wait,
                summary.disk_permit_wait,
            ]
            .iter()
            .all(Option::is_none);
        (!idle).then_some(summary)
    }

    /// Logs the window as `Night lighting: summary`, unless nothing
    /// happened. `payload_entries` and `payload_bytes` are the payload
    /// cache's entry count and weighted size now.
    pub fn log_summary(&self, payload_entries: u64, payload_bytes: u64) {
        let Some(w) = self.take_window() else {
            return;
        };
        let d = w.delta;
        let ms = |s: Option<LatencySummary>| s.map(|s| tracing::field::display(Millis(s)));
        let read = |o: ReadOutcome| ms(w.read(o));
        let skipped = |s: WriteBackSkip| d.write_backs_skipped(s);
        let miss = |m: DiskMiss| d.disk_misses(m);
        let disk_misses: u64 = d.disk_misses.iter().sum();
        debug!(
            reads_dark = d.reads(ReadOutcome::Dark),
            reads_blank = d.reads(ReadOutcome::Blank),
            reads_memory = d.reads(ReadOutcome::MemoryHit),
            reads_disk = d.reads(ReadOutcome::DiskHit),
            reads_generated = d.reads(ReadOutcome::Generated),
            reads_black = d.reads(ReadOutcome::Black),
            read_dark_ms = read(ReadOutcome::Dark),
            read_blank_ms = read(ReadOutcome::Blank),
            read_memory_ms = read(ReadOutcome::MemoryHit),
            read_disk_ms = read(ReadOutcome::DiskHit),
            read_generated_ms = read(ReadOutcome::Generated),
            read_black_ms = read(ReadOutcome::Black),
            probes_recipe = d.probes(ProbeAnswer::Recipe),
            probes_road = d.probes(ProbeAnswer::Road),
            probes_dark = d.probes(ProbeAnswer::Dark),
            probe_road_wait_ms = ms(w.probe_road_wait),
            probe_permit_wait_ms = ms(w.probe_permit_wait),
            generations = d.generations(),
            generations_black = d.generations_black,
            generation_permit_wait_ms = ms(w.generation_permit_wait),
            generation_ms = ms(w.generation),
            generations_waiting_peak = w.generations_waiting_peak,
            roads_complete = d.generation_roads(RoadsState::Complete),
            roads_timed_out = d.generation_roads(RoadsState::TimedOut),
            roads_failed = d.generation_roads(RoadsState::Failed),
            roads_ready_at_permit = d.roads_ready_at_permit,
            memory_only_road_failed = skipped(WriteBackSkip::RoadFailed),
            memory_only_art_failed = skipped(WriteBackSkip::ArtFailed),
            memory_only_no_fingerprint = skipped(WriteBackSkip::NoFingerprint),
            write_backs_skipped = d.write_backs_skipped.iter().sum::<u64>(),
            payload_hits = d.payload_hits,
            payload_misses = d.payload_misses,
            payload_hit_rate = hit_rate(d.payload_hits, d.payload_misses),
            payload_evictions = d.payload_evictions,
            payload_entries,
            payload_bytes,
            disk_hits = d.disk_hits,
            disk_misses,
            disk_hit_rate = hit_rate(d.disk_hits, disk_misses),
            disk_miss_absent = miss(DiskMiss::Absent),
            disk_miss_error = miss(DiskMiss::ReadError),
            disk_miss_corrupt = miss(DiskMiss::Corrupt),
            disk_miss_fingerprint = miss(DiskMiss::Fingerprint),
            disk_miss_length = miss(DiskMiss::Length),
            disk_miss_no_fingerprint = miss(DiskMiss::NoFingerprint),
            disk_writes = d.disk_writes,
            disk_write_failures = d.disk_write_failures,
            write_backs_pending_peak = w.write_backs_pending_peak,
            disk_permit_wait_ms = ms(w.disk_permit_wait),
            "Night lighting: summary"
        );
    }
}

/// A latency summary as `median/p95/max` in milliseconds, to the
/// microsecond. The summary leaves a field out when its window has no
/// sample.
#[derive(Debug, Clone, Copy)]
struct Millis(LatencySummary);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |us: u64| us as f64 / 1000.0;
        write!(
            f,
            "{:.3}/{:.3}/{:.3}",
            ms(self.0.median_us),
            ms(self.0.p95_us),
            ms(self.0.max_us)
        )
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

    const MS: Duration = Duration::from_millis(1);

    fn generation(roads: RoadsState, payload: bool) -> GenerationRecord {
        GenerationRecord {
            permit_wait: 3 * MS,
            generation: 40 * MS,
            roads,
            roads_ready_at_permit: false,
            payload,
        }
    }

    #[test]
    fn every_variant_has_its_counter() {
        // Counters are indexed by declaration order: the last variant of each
        // enum is the last counter of its array.
        let s = LitStatsSnapshot::default();
        assert_eq!(ReadOutcome::Black as usize + 1, s.reads.len());
        assert_eq!(ProbeAnswer::Dark as usize + 1, s.probes.len());
        assert_eq!(RoadsState::Failed as usize + 1, s.generation_roads.len());
        assert_eq!(
            WriteBackSkip::ArtFailed as usize + 1,
            s.write_backs_skipped.len()
        );
        assert_eq!(DiskMiss::NoFingerprint as usize + 1, s.disk_misses.len());
    }

    #[test]
    fn a_window_where_nothing_happened_has_no_summary() {
        let s = LitStats::default();
        assert_eq!(s.take_window(), None);
        s.log_summary(0, 0); // logs nothing, and does not panic
    }

    #[test]
    fn a_peak_alone_does_not_make_a_window_active() {
        let s = LitStats::default();
        let _waiting = s.generations_waiting.enter();
        assert_eq!(s.take_window(), None);
    }

    #[test]
    fn reads_are_counted_and_timed_by_outcome() {
        let s = LitStats::default();
        s.record_read(ReadOutcome::MemoryHit, MS);
        s.record_read(ReadOutcome::MemoryHit, 3 * MS);
        s.record_read(ReadOutcome::Generated, 90 * MS);
        let snap = s.snapshot();
        assert_eq!(snap.reads(ReadOutcome::MemoryHit), 2);
        assert_eq!(snap.reads(ReadOutcome::Generated), 1);
        assert_eq!(snap.reads(ReadOutcome::Dark), 0);
        let w = s.take_window().unwrap();
        assert_eq!(w.delta, snap);
        let memory = w.read(ReadOutcome::MemoryHit).unwrap();
        assert_eq!((memory.count, memory.max_us), (2, 3_000));
        assert_eq!(w.read(ReadOutcome::Generated).unwrap().max_us, 90_000);
        assert_eq!(w.read(ReadOutcome::DiskHit), None);
        assert_eq!(s.take_window(), None, "the window closed");
        assert_eq!(s.snapshot(), snap, "the counters are cumulative");
    }

    #[test]
    fn generations_are_counted_by_roads_and_timed() {
        let s = LitStats::default();
        s.record_generation(&generation(RoadsState::Complete, true));
        s.record_generation(&GenerationRecord {
            roads_ready_at_permit: true,
            ..generation(RoadsState::TimedOut, true)
        });
        s.record_generation(&generation(RoadsState::Failed, false));
        let snap = s.snapshot();
        assert_eq!(snap.generations(), 3);
        assert_eq!(snap.generation_roads, [1, 1, 1]);
        assert_eq!((snap.generations_black, snap.roads_ready_at_permit), (1, 1));
        let w = s.take_window().unwrap();
        assert_eq!(w.generation_permit_wait.unwrap().count, 3);
        assert_eq!(w.generation.unwrap().max_us, 40_000);
    }

    #[test]
    fn a_window_reports_only_its_own_counts() {
        let s = LitStats::default();
        s.record_payload_lookup(true);
        s.take_window().unwrap();
        s.record_payload_lookup(false);
        s.record_payload_removal(RemovalCause::Size);
        s.record_payload_removal(RemovalCause::Explicit);
        let w = s.take_window().unwrap();
        assert_eq!(
            w.delta,
            LitStatsSnapshot {
                payload_hits: 1,
                payload_evictions: 1,
                ..LitStatsSnapshot::default()
            }
        );
    }

    #[test]
    fn disk_lookups_writes_and_skips_are_counted_by_reason() {
        let s = LitStats::default();
        s.record_disk_lookup(MS, None);
        s.record_disk_lookup(MS, Some(DiskMiss::Fingerprint));
        s.record_disk_lookup(MS, Some(EntryReject::Length.into()));
        s.record_disk_write(true);
        s.record_disk_write(false);
        s.record_write_back_skipped(WriteBackSkip::ArtFailed);
        let snap = s.snapshot();
        assert_eq!(snap.disk_hits, 1);
        assert_eq!(snap.disk_misses(DiskMiss::Fingerprint), 1);
        assert_eq!(snap.disk_misses(DiskMiss::Length), 1);
        assert_eq!(snap.disk_misses(DiskMiss::Absent), 0);
        assert_eq!((snap.disk_writes, snap.disk_write_failures), (1, 1));
        assert_eq!(snap.write_backs_skipped(WriteBackSkip::ArtFailed), 1);
        assert_eq!(s.take_window().unwrap().disk_permit_wait.unwrap().count, 3);
    }

    #[test]
    fn only_a_committed_probe_answer_is_counted() {
        let s = LitStats::default();
        let probe = |committed| ProbeRecord {
            road_wait: 5 * MS,
            permit_wait: MS,
            committed,
        };
        s.record_probe(&probe(Some(ProbeAnswer::Road)));
        s.record_probe(&probe(None));
        assert_eq!(s.snapshot().probes, [0, 1, 0]);
        let w = s.take_window().unwrap();
        assert_eq!(w.probe_road_wait.unwrap().count, 2, "both waited");
        assert_eq!(w.probe_permit_wait.unwrap().max_us, 1_000);
    }

    #[test]
    fn a_gauge_peaks_per_window() {
        let g = Gauge::default();
        let a = g.enter();
        let b = g.enter();
        drop(b);
        assert_eq!(g.take_peak(), 2);
        assert_eq!(g.take_peak(), 1, "the next window starts from the count");
        drop(a);
        assert_eq!(g.take_peak(), 1);
        assert_eq!(g.take_peak(), 0);
    }

    #[test]
    fn latencies_print_as_milliseconds_to_the_microsecond() {
        let s = LatencySummary {
            median_us: 12,
            p95_us: 1_500,
            max_us: 250_000,
            count: 3,
        };
        assert_eq!(Millis(s).to_string(), "0.012/1.500/250.000");
    }

    #[test]
    fn hit_rate_is_over_lookups_that_could_hit() {
        assert_eq!(hit_rate(0, 0), None);
        assert_eq!(hit_rate(3, 1), Some(0.75));
    }
}
