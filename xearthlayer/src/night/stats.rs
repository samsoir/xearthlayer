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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moka::notification::RemovalCause;
use tracing::debug;

use super::admission::GenerationClass;
use super::disk::EntryReject;
use super::layout::PayloadPart;
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
    /// The visible levels from the payload cache.
    MemoryHit,
    /// The visible levels from another read's load of them, in flight when
    /// this read asked: neither a hit nor a load of its own.
    Joined,
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
            Self::Joined => "joined",
            Self::DiskHit => "disk",
            Self::Generated => "generated",
            Self::Black => "black",
        }
    }
}

/// What one lookup of a payload part in memory did (#432: the head and the
/// tail are looked up apart, each part a read wants once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadLookup {
    /// The part was in memory when asked for, and no load ran for it.
    Hit,
    /// The part was not in memory when asked for, and this read's load did
    /// not run: the read joined another's load in flight. Also, rarely, a
    /// part kept between the two checks.
    Joined,
    /// This read's load ran: a miss.
    Loaded,
}

/// What one pre-generation did (#433), once it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PregenOutcome {
    /// It generated the payload.
    Generated,
    /// The small levels were in memory already: nothing loaded.
    InMemory,
    /// Loaded from the lit disk tier into memory: nothing generated.
    OnDisk,
    /// A read's load of the footprint was in flight: it joined it.
    Joined,
    /// Nothing to keep: a recipe unreadable, generation failed, or refused
    /// at shutdown.
    Black,
}

impl PregenOutcome {
    /// Its name in the debug lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::InMemory => "memory",
            Self::OnDisk => "disk",
            Self::Joined => "joined",
            Self::Black => "black",
        }
    }
}

/// Entries and bytes of each part of the payload memory now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PayloadOccupancy {
    pub head_entries: u64,
    pub head_bytes: u64,
    pub tail_entries: u64,
    pub tail_bytes: u64,
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
/// back. One reason per payload, the first in declaration order that
/// applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBackSkip {
    /// A road tile was still building at the deadline: kept nowhere.
    RoadsLate,
    /// No fingerprint could be computed: kept in memory only.
    NoFingerprint,
    /// A road tile failed: kept in memory only.
    RoadFailed,
    /// A road tile's roads were read from DSFs other than those the lit
    /// fingerprint names: a DSF changed while the session ran (#430). Kept
    /// in memory only.
    RoadSourcesChanged,
    /// A Laminar art lookup for the payload met a doubt: a file given up on
    /// after I/O errors this session, or one the lit fingerprint does not
    /// cover. Kept in memory only.
    ArtFailed,
}

/// What one generation did, recorded once when it ends.
#[derive(Debug, Clone, Copy)]
pub struct GenerationRecord {
    /// The class its permit was admitted as (#433).
    pub class: GenerationClass,
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

/// What one probe did, recorded once when it answered or found another
/// probe's answer after its permit wait.
#[derive(Debug, Clone, Copy)]
pub struct ProbeRecord {
    /// Waiting for the footprint's road tiles; `None` when it did not wait:
    /// without road lights, or when its recipe lit it or another probe's
    /// answer was found first (only a footprint the recipe leaves dark
    /// waits for roads, #428).
    pub road_wait: Option<Duration>,
    /// From asking for a probe permit to holding one.
    pub permit_wait: Duration,
    /// Its answer, when it was the one committed (`None` when another
    /// probe's answer was committed first).
    pub committed: Option<ProbeAnswer>,
}

/// Cumulative counts since the source was built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LitStatsSnapshot {
    /// By [`ReadOutcome`], in its declaration order.
    pub reads: [u64; 7],
    /// Committed probe answers by [`ProbeAnswer`]: recipe, road, dark.
    pub probes: [u64; 3],
    /// Generations by their road tiles: complete, timed out, failed.
    pub generation_roads: [u64; 3],
    /// Generations by the [`GenerationClass`] their permit was admitted as.
    pub generation_classes: [u64; 3],
    /// Generations that gave no payload (served black).
    pub generations_black: u64,
    /// Generations whose roads timed out and were all ready at the permit.
    pub roads_ready_at_permit: u64,
    /// Write-backs skipped by [`WriteBackSkip`], in its declaration order.
    pub write_backs_skipped: [u64; 5],
    /// Write-backs dropped because as many as the bound were pending.
    pub write_backs_dropped: u64,
    /// Footprints queued for pre-generation (#433).
    pub pregen_queued: u64,
    /// Footprints not queued for pre-generation: the queue was full.
    pub pregen_dropped: u64,
    /// Pre-generations taken off the queue to run.
    pub pregen_started: u64,
    /// Pre-generations still queued at shutdown, which never ran.
    pub pregen_cancelled: u64,
    /// Pre-generations that ran, by [`PregenOutcome`].
    pub pregenerations: [u64; 5],
    /// See [`PayloadLookup::Hit`], by [`PayloadPart`]: head, tail.
    pub payload_hits: [u64; 2],
    /// See [`PayloadLookup::Joined`], by part.
    pub payload_joins: [u64; 2],
    /// See [`PayloadLookup::Loaded`], by part.
    pub payload_misses: [u64; 2],
    /// Parts evicted for size, by part.
    pub payload_evictions: [u64; 2],
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

    pub fn generation_class(&self, class: GenerationClass) -> u64 {
        self.generation_classes[class as usize]
    }

    pub fn pregenerations(&self, outcome: PregenOutcome) -> u64 {
        self.pregenerations[outcome as usize]
    }

    pub fn write_backs_skipped(&self, skip: WriteBackSkip) -> u64 {
        self.write_backs_skipped[skip as usize]
    }

    pub fn disk_misses(&self, miss: DiskMiss) -> u64 {
        self.disk_misses[miss as usize]
    }

    pub fn payload_hits(&self, part: PayloadPart) -> u64 {
        self.payload_hits[part as usize]
    }

    pub fn payload_joins(&self, part: PayloadPart) -> u64 {
        self.payload_joins[part as usize]
    }

    pub fn payload_misses(&self, part: PayloadPart) -> u64 {
        self.payload_misses[part as usize]
    }

    pub fn payload_evictions(&self, part: PayloadPart) -> u64 {
        self.payload_evictions[part as usize]
    }

    /// Hits over every lookup of `part`, joins and loads included; `None`
    /// without any.
    pub fn payload_hit_rate(&self, part: PayloadPart) -> Option<f64> {
        hit_rate(
            self.payload_hits(part),
            self.payload_joins(part) + self.payload_misses(part),
        )
    }

    /// Hits over the disk lookups that could hit: without a fingerprint no
    /// entry is looked up, and a read error says nothing of the entry.
    pub fn disk_hit_rate(&self) -> Option<f64> {
        let could_hit = [
            DiskMiss::Absent,
            DiskMiss::Corrupt,
            DiskMiss::Fingerprint,
            DiskMiss::Length,
        ];
        let misses = could_hit.iter().map(|&m| self.disk_misses(m)).sum();
        hit_rate(self.disk_hits, misses)
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
            generation_classes: each(self.generation_classes, earlier.generation_classes),
            generations_black: self.generations_black - earlier.generations_black,
            roads_ready_at_permit: self.roads_ready_at_permit - earlier.roads_ready_at_permit,
            write_backs_skipped: each(self.write_backs_skipped, earlier.write_backs_skipped),
            write_backs_dropped: self.write_backs_dropped - earlier.write_backs_dropped,
            pregen_queued: self.pregen_queued - earlier.pregen_queued,
            pregen_dropped: self.pregen_dropped - earlier.pregen_dropped,
            pregen_started: self.pregen_started - earlier.pregen_started,
            pregen_cancelled: self.pregen_cancelled - earlier.pregen_cancelled,
            pregenerations: each(self.pregenerations, earlier.pregenerations),
            payload_hits: each(self.payload_hits, earlier.payload_hits),
            payload_joins: each(self.payload_joins, earlier.payload_joins),
            payload_misses: each(self.payload_misses, earlier.payload_misses),
            payload_evictions: each(self.payload_evictions, earlier.payload_evictions),
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
    pub reads: [Option<LatencySummary>; 7],
    pub generation_permit_wait: Option<LatencySummary>,
    /// The same waits by the class each permit was admitted as.
    pub generation_permit_wait_by_class: [Option<LatencySummary>; 3],
    pub generation: Option<LatencySummary>,
    /// Probe waits for road tiles: probes of footprints their recipe left
    /// dark, with road lights only.
    pub probe_road_wait: Option<LatencySummary>,
    pub probe_permit_wait: Option<LatencySummary>,
    /// Waits for the disk permit of a lookup (write-backs are not timed).
    pub disk_permit_wait: Option<LatencySummary>,
    /// The most generations waiting for a permit at once.
    pub generations_waiting_peak: u64,
    /// The most write-backs in flight at once, waiting for their permit
    /// or writing.
    pub write_backs_pending_peak: u64,
    /// The most footprints queued for pre-generation at once.
    pub pregen_queue_peak: u64,
}

impl LitWindow {
    pub fn read(&self, outcome: ReadOutcome) -> Option<LatencySummary> {
        self.reads[outcome as usize]
    }

    pub fn generation_permit_wait_of(&self, class: GenerationClass) -> Option<LatencySummary> {
        self.generation_permit_wait_by_class[class as usize]
    }
}

/// The samples of one window, taken out whole under the lock and
/// summarised (sorted) after it is released.
#[derive(Default)]
struct Samples {
    reads: [LatencyWindow; 7],
    generation_permit_wait: LatencyWindow,
    generation_permit_wait_by_class: [LatencyWindow; 3],
    generation: LatencyWindow,
    probe_road_wait: LatencyWindow,
    probe_permit_wait: LatencyWindow,
    disk_permit_wait: LatencyWindow,
}

#[derive(Default)]
struct Window {
    samples: Samples,
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
        self.arrive();
        GaugeGuard(self)
    }

    fn arrive(&self) {
        let now = self.now.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// One more, unless `bound` are in already ([`admits`]): whether it
    /// arrived. Atomic, so concurrent callers never pass the bound.
    fn arrive_within(&self, bound: u64) -> bool {
        let arrived = self
            .now
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                admits(now, bound).then_some(now + 1)
            });
        match arrived {
            Ok(before) => {
                self.peak.fetch_max(before + 1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }

    fn leave(&self) {
        self.now.fetch_sub(1, Ordering::Relaxed);
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
        self.0.leave();
    }
}

/// One write-back in flight, from before its task is spawned until the
/// guard drops; it owns the statistics so it can move into the task.
pub struct PendingWriteBack(Arc<LitStats>);

impl PendingWriteBack {
    /// The write ended, written or not.
    pub fn finished(&self, written: bool) {
        add(if written {
            &self.0.disk_writes
        } else {
            &self.0.disk_write_failures
        });
    }
}

impl Drop for PendingWriteBack {
    fn drop(&mut self) {
        self.0.write_backs_pending.leave();
    }
}

/// Whether one more may join `now` already in, with at most `bound` at once.
fn admits(now: u64, bound: u64) -> bool {
    now < bound
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The lit source's statistics.
#[derive(Default)]
pub struct LitStats {
    reads: Tally<7>,
    probes: Tally<3>,
    generation_roads: Tally<3>,
    generation_classes: Tally<3>,
    generations_black: AtomicU64,
    roads_ready_at_permit: AtomicU64,
    write_backs_skipped: Tally<5>,
    write_backs_dropped: AtomicU64,
    pregen_queued: AtomicU64,
    pregen_dropped: AtomicU64,
    pregen_started: AtomicU64,
    pregen_cancelled: AtomicU64,
    pregenerations: Tally<5>,
    /// Footprints queued for pre-generation.
    pregen_queue: Gauge,
    payload_hits: Tally<2>,
    payload_joins: Tally<2>,
    payload_misses: Tally<2>,
    payload_evictions: Tally<2>,
    disk_hits: AtomicU64,
    disk_misses: Tally<6>,
    disk_writes: AtomicU64,
    disk_write_failures: AtomicU64,
    /// Generations waiting for a permit.
    pub generations_waiting: Gauge,
    /// Write-backs in flight (see [`LitStats::write_back`]).
    write_backs_pending: Gauge,
    window: Mutex<Window>,
}

impl LitStats {
    /// A lit read served by `outcome`, `elapsed` after it was asked for.
    pub fn record_read(&self, outcome: ReadOutcome, elapsed: Duration) {
        self.reads.add(outcome as usize);
        if let Ok(mut w) = self.window.lock() {
            w.samples.reads[outcome as usize].record(micros(elapsed));
        }
    }

    pub fn record_probe(&self, p: &ProbeRecord) {
        if let Some(answer) = p.committed {
            self.probes.add(answer as usize);
        }
        if let Ok(mut w) = self.window.lock() {
            if let Some(road_wait) = p.road_wait {
                w.samples.probe_road_wait.record(micros(road_wait));
            }
            w.samples.probe_permit_wait.record(micros(p.permit_wait));
        }
    }

    pub fn record_generation(&self, g: &GenerationRecord) {
        self.generation_roads.add(g.roads as usize);
        self.generation_classes.add(g.class as usize);
        if !g.payload {
            add(&self.generations_black);
        }
        if g.roads_ready_at_permit {
            add(&self.roads_ready_at_permit);
        }
        if let Ok(mut w) = self.window.lock() {
            w.samples
                .generation_permit_wait
                .record(micros(g.permit_wait));
            w.samples.generation_permit_wait_by_class[g.class as usize]
                .record(micros(g.permit_wait));
            w.samples.generation.record(micros(g.generation));
        }
    }

    /// A lookup of `part` in the payload memory.
    pub fn record_payload_lookup(&self, part: PayloadPart, lookup: PayloadLookup) {
        match lookup {
            PayloadLookup::Hit => &self.payload_hits,
            PayloadLookup::Joined => &self.payload_joins,
            PayloadLookup::Loaded => &self.payload_misses,
        }
        .add(part as usize);
    }

    /// A `part` left the payload memory. Only evictions for size are
    /// counted: the memory invalidates nothing, has no expiry and never
    /// replaces a part it holds.
    pub fn record_payload_removal(&self, part: PayloadPart, cause: RemovalCause) {
        if cause == RemovalCause::Size {
            self.payload_evictions.add(part as usize);
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
            w.samples.disk_permit_wait.record(micros(permit_wait));
        }
    }

    /// A write-back about to be spawned, when fewer than `bound` are
    /// pending: pending until the guard drops, which records how it ended
    /// through [`PendingWriteBack::finished`]. `None`, counted as dropped,
    /// at the bound.
    pub fn write_back(self: &Arc<Self>, bound: u64) -> Option<PendingWriteBack> {
        if self.write_backs_pending.arrive_within(bound) {
            Some(PendingWriteBack(Arc::clone(self)))
        } else {
            add(&self.write_backs_dropped);
            None
        }
    }

    /// A footprint queued for pre-generation.
    pub fn record_pregen_queued(&self) {
        add(&self.pregen_queued);
        self.pregen_queue.arrive();
    }

    /// A footprint not queued: the queue was full.
    pub fn record_pregen_dropped(&self) {
        add(&self.pregen_dropped);
    }

    /// A queued footprint taken off the queue to run.
    pub fn record_pregen_started(&self) {
        add(&self.pregen_started);
        self.pregen_queue.leave();
    }

    /// `n` queued footprints cleared at shutdown.
    pub fn record_pregen_cancelled(&self, n: usize) {
        for _ in 0..n {
            add(&self.pregen_cancelled);
            self.pregen_queue.leave();
        }
    }

    /// A pre-generation that ran ended with `outcome`.
    pub fn record_pregen(&self, outcome: PregenOutcome) {
        self.pregenerations.add(outcome as usize);
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
            generation_classes: self.generation_classes.get(),
            generations_black: get(&self.generations_black),
            roads_ready_at_permit: get(&self.roads_ready_at_permit),
            write_backs_skipped: self.write_backs_skipped.get(),
            write_backs_dropped: get(&self.write_backs_dropped),
            pregen_queued: get(&self.pregen_queued),
            pregen_dropped: get(&self.pregen_dropped),
            pregen_started: get(&self.pregen_started),
            pregen_cancelled: get(&self.pregen_cancelled),
            pregenerations: self.pregenerations.get(),
            payload_hits: self.payload_hits.get(),
            payload_joins: self.payload_joins.get(),
            payload_misses: self.payload_misses.get(),
            payload_evictions: self.payload_evictions.get(),
            disk_hits: get(&self.disk_hits),
            disk_misses: self.disk_misses.get(),
            disk_writes: get(&self.disk_writes),
            disk_write_failures: get(&self.disk_write_failures),
        }
    }

    /// Closes the window: its counts and latencies, or `None` when nothing
    /// happened in it. The peaks alone do not make a window active.
    /// The samples are moved out under the lock and sorted after it, so
    /// reads recording meanwhile do not wait for the sort.
    pub fn take_window(&self) -> Option<LitWindow> {
        let now = self.snapshot();
        let (delta, mut samples) = {
            let mut w = self.window.lock().ok()?;
            let delta = now.since(&w.last);
            w.last = now;
            (delta, std::mem::take(&mut w.samples))
        };
        let summary = LitWindow {
            delta,
            reads: std::array::from_fn(|i| samples.reads[i].take()),
            generation_permit_wait: samples.generation_permit_wait.take(),
            generation_permit_wait_by_class: std::array::from_fn(|i| {
                samples.generation_permit_wait_by_class[i].take()
            }),
            generation: samples.generation.take(),
            probe_road_wait: samples.probe_road_wait.take(),
            probe_permit_wait: samples.probe_permit_wait.take(),
            disk_permit_wait: samples.disk_permit_wait.take(),
            generations_waiting_peak: self.generations_waiting.take_peak(),
            write_backs_pending_peak: self.write_backs_pending.take_peak(),
            pregen_queue_peak: self.pregen_queue.take_peak(),
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
    /// happened. `memory` is the payload memory's occupancy now:
    /// `payload_entries` and `payload_bytes` are both parts', then each
    /// part's apart.
    pub fn log_summary(&self, memory: PayloadOccupancy) {
        let Some(w) = self.take_window() else {
            return;
        };
        let d = w.delta;
        let ms = |s: Option<LatencySummary>| s.map(|s| tracing::field::display(Millis(s)));
        let read = |o: ReadOutcome| ms(w.read(o));
        let skipped = |s: WriteBackSkip| d.write_backs_skipped(s);
        let miss = |m: DiskMiss| d.disk_misses(m);
        let (head, tail) = (PayloadPart::Head, PayloadPart::Tail);
        let class = |c: GenerationClass| d.generation_class(c);
        let class_wait = |c: GenerationClass| ms(w.generation_permit_wait_of(c));
        use GenerationClass::{HeadRead, Pregeneration, TailRead};
        debug!(
            reads_dark = d.reads(ReadOutcome::Dark),
            reads_blank = d.reads(ReadOutcome::Blank),
            reads_memory = d.reads(ReadOutcome::MemoryHit),
            reads_joined = d.reads(ReadOutcome::Joined),
            reads_disk = d.reads(ReadOutcome::DiskHit),
            reads_generated = d.reads(ReadOutcome::Generated),
            reads_black = d.reads(ReadOutcome::Black),
            read_dark_ms = read(ReadOutcome::Dark),
            read_blank_ms = read(ReadOutcome::Blank),
            read_memory_ms = read(ReadOutcome::MemoryHit),
            read_joined_ms = read(ReadOutcome::Joined),
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
            generations_head_read = class(HeadRead),
            generations_tail_read = class(TailRead),
            generations_pregeneration = class(Pregeneration),
            generation_permit_wait_head_read_ms = class_wait(HeadRead),
            generation_permit_wait_tail_read_ms = class_wait(TailRead),
            generation_permit_wait_pregeneration_ms = class_wait(Pregeneration),
            generation_ms = ms(w.generation),
            generations_waiting_peak = w.generations_waiting_peak,
            roads_complete = d.generation_roads(RoadsState::Complete),
            roads_timed_out = d.generation_roads(RoadsState::TimedOut),
            roads_failed = d.generation_roads(RoadsState::Failed),
            roads_ready_at_permit = d.roads_ready_at_permit,
            write_back_skipped_roads_late = skipped(WriteBackSkip::RoadsLate),
            write_back_skipped_no_fingerprint = skipped(WriteBackSkip::NoFingerprint),
            write_back_skipped_road_failed = skipped(WriteBackSkip::RoadFailed),
            write_back_skipped_road_sources_changed = skipped(WriteBackSkip::RoadSourcesChanged),
            write_back_skipped_art_failed = skipped(WriteBackSkip::ArtFailed),
            write_backs_dropped = d.write_backs_dropped,
            pregen_queued = d.pregen_queued,
            pregen_dropped = d.pregen_dropped,
            pregen_started = d.pregen_started,
            pregen_cancelled = d.pregen_cancelled,
            pregen_generated = d.pregenerations(PregenOutcome::Generated),
            pregen_skipped_memory = d.pregenerations(PregenOutcome::InMemory),
            pregen_skipped_disk = d.pregenerations(PregenOutcome::OnDisk),
            pregen_joined = d.pregenerations(PregenOutcome::Joined),
            pregen_black = d.pregenerations(PregenOutcome::Black),
            pregen_queue_peak = w.pregen_queue_peak,
            payload_head_hits = d.payload_hits(head),
            payload_head_joins = d.payload_joins(head),
            payload_head_misses = d.payload_misses(head),
            payload_head_hit_rate = d.payload_hit_rate(head),
            payload_head_evictions = d.payload_evictions(head),
            payload_tail_hits = d.payload_hits(tail),
            payload_tail_joins = d.payload_joins(tail),
            payload_tail_misses = d.payload_misses(tail),
            payload_tail_hit_rate = d.payload_hit_rate(tail),
            payload_tail_evictions = d.payload_evictions(tail),
            payload_entries = memory.head_entries + memory.tail_entries,
            payload_bytes = memory.head_bytes + memory.tail_bytes,
            payload_head_entries = memory.head_entries,
            payload_head_bytes = memory.head_bytes,
            payload_tail_entries = memory.tail_entries,
            payload_tail_bytes = memory.tail_bytes,
            disk_hits = d.disk_hits,
            disk_hit_rate = d.disk_hit_rate(),
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
            class: GenerationClass::TailRead,
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
        assert_eq!(
            ReadOutcome::Joined as usize,
            ReadOutcome::MemoryHit as usize + 1
        );
        assert_eq!(ProbeAnswer::Dark as usize + 1, s.probes.len());
        assert_eq!(RoadsState::Failed as usize + 1, s.generation_roads.len());
        assert_eq!(
            GenerationClass::Pregeneration as usize + 1,
            s.generation_classes.len()
        );
        for (i, class) in GenerationClass::ALL.into_iter().enumerate() {
            assert_eq!(class as usize, i);
        }
        assert_eq!(
            WriteBackSkip::ArtFailed as usize + 1,
            s.write_backs_skipped.len()
        );
        assert_eq!(DiskMiss::NoFingerprint as usize + 1, s.disk_misses.len());
        assert_eq!(PregenOutcome::Black as usize + 1, s.pregenerations.len());
        for parts in [
            s.payload_hits,
            s.payload_joins,
            s.payload_misses,
            s.payload_evictions,
        ] {
            assert_eq!(PayloadPart::Tail as usize + 1, parts.len());
        }
    }

    #[test]
    fn a_window_where_nothing_happened_has_no_summary() {
        let s = LitStats::default();
        assert_eq!(s.take_window(), None);
        s.log_summary(PayloadOccupancy::default()); // logs nothing, and does not panic
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
    fn generation_permit_waits_are_timed_by_class() {
        let s = LitStats::default();
        let at = |class, wait| GenerationRecord {
            class,
            permit_wait: wait,
            ..generation(RoadsState::Complete, true)
        };
        s.record_generation(&at(GenerationClass::HeadRead, MS));
        s.record_generation(&at(GenerationClass::Pregeneration, 7 * MS));
        s.record_generation(&at(GenerationClass::Pregeneration, 9 * MS));
        assert_eq!(s.snapshot().generation_classes, [1, 0, 2]);
        let w = s.take_window().unwrap();
        assert_eq!(w.generation_permit_wait.unwrap().count, 3, "all of them");
        let of = |c| w.generation_permit_wait_of(c);
        assert_eq!(of(GenerationClass::HeadRead).unwrap().max_us, 1_000);
        assert_eq!(of(GenerationClass::TailRead), None);
        let pregen = of(GenerationClass::Pregeneration).unwrap();
        assert_eq!((pregen.count, pregen.max_us), (2, 9_000));
    }

    #[test]
    fn pregenerations_are_counted_from_queue_to_outcome_and_the_queue_peaks() {
        let s = LitStats::default();
        for _ in 0..3 {
            s.record_pregen_queued();
        }
        s.record_pregen_dropped();
        s.record_pregen_started();
        s.record_pregen(PregenOutcome::Generated);
        s.record_pregen_started();
        s.record_pregen(PregenOutcome::OnDisk);
        s.record_pregen_cancelled(1);
        let snap = s.snapshot();
        assert_eq!(
            (
                snap.pregen_queued,
                snap.pregen_dropped,
                snap.pregen_started,
                snap.pregen_cancelled
            ),
            (3, 1, 2, 1)
        );
        assert_eq!(snap.pregenerations, [1, 0, 1, 0, 0]);
        assert_eq!(s.pregen_queue.now(), 0, "every queued one left");
        let w = s.take_window().unwrap();
        assert_eq!(w.pregen_queue_peak, 3);
        s.log_summary(PayloadOccupancy::default());
    }

    #[test]
    fn a_window_reports_only_its_own_counts() {
        let s = LitStats::default();
        s.record_payload_lookup(PayloadPart::Tail, PayloadLookup::Loaded);
        s.take_window().unwrap();
        s.record_payload_lookup(PayloadPart::Tail, PayloadLookup::Hit);
        s.record_payload_removal(PayloadPart::Head, RemovalCause::Size);
        s.record_payload_removal(PayloadPart::Head, RemovalCause::Explicit);
        let w = s.take_window().unwrap();
        assert_eq!(
            w.delta,
            LitStatsSnapshot {
                payload_hits: [0, 1],
                payload_evictions: [1, 0],
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
        let stats = Arc::new(s);
        let s = stats.as_ref();
        stats.write_back(2).unwrap().finished(true);
        stats.write_back(2).unwrap().finished(false);
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
        let probe = |road_wait, committed| ProbeRecord {
            road_wait,
            permit_wait: MS,
            committed,
        };
        s.record_probe(&probe(Some(5 * MS), Some(ProbeAnswer::Road)));
        s.record_probe(&probe(Some(5 * MS), None));
        s.record_probe(&probe(None, Some(ProbeAnswer::Dark)));
        assert_eq!(s.snapshot().probes, [0, 1, 1]);
        let w = s.take_window().unwrap();
        assert_eq!(w.probe_road_wait.unwrap().count, 2, "with roads only");
        assert_eq!(w.probe_permit_wait.unwrap().count, 3, "every probe waited");
        assert_eq!(w.probe_permit_wait.unwrap().max_us, 1_000);
    }

    #[test]
    fn a_pending_write_back_counts_until_it_drops() {
        let s = Arc::new(LitStats::default());
        let pending = s.write_back(1).unwrap();
        assert_eq!(s.write_backs_pending.now(), 1);
        drop(pending);
        assert_eq!(s.write_backs_pending.now(), 0);
        let snap = s.snapshot();
        assert_eq!((snap.disk_writes, snap.disk_write_failures), (0, 0));
        assert_eq!(s.write_backs_pending.take_peak(), 1);
    }

    #[test]
    fn a_write_back_past_the_bound_is_dropped_and_counted() {
        assert!(admits(15, 16));
        assert!(!admits(16, 16));
        let s = Arc::new(LitStats::default());
        let held: Vec<_> = (0..3).map(|_| s.write_back(3).unwrap()).collect();
        assert!(s.write_back(3).is_none(), "at the bound");
        assert_eq!(s.snapshot().write_backs_dropped, 1);
        drop(held);
        assert!(s.write_back(3).is_some(), "room again");
        assert_eq!(s.write_backs_pending.take_peak(), 3, "never past the bound");
    }

    #[test]
    fn the_payload_hit_rate_of_a_part_counts_hits_over_its_lookups() {
        let s = LitStats::default();
        let tail = PayloadPart::Tail;
        assert_eq!(s.snapshot().payload_hit_rate(tail), None);
        s.record_payload_lookup(tail, PayloadLookup::Hit);
        s.record_payload_lookup(tail, PayloadLookup::Joined);
        s.record_payload_lookup(tail, PayloadLookup::Joined);
        s.record_payload_lookup(tail, PayloadLookup::Loaded);
        s.record_payload_lookup(PayloadPart::Head, PayloadLookup::Loaded);
        let snap = s.snapshot();
        assert_eq!(
            (
                snap.payload_hits(tail),
                snap.payload_joins(tail),
                snap.payload_misses(tail)
            ),
            (1, 2, 1)
        );
        assert_eq!(snap.payload_hit_rate(tail), Some(0.25));
        assert_eq!(snap.payload_hit_rate(PayloadPart::Head), Some(0.0));
        assert_eq!(snap.payload_misses, [1, 1]);
    }

    #[test]
    fn the_disk_hit_rate_leaves_out_lookups_that_could_not_hit() {
        let s = LitStats::default();
        s.record_disk_lookup(MS, None);
        s.record_disk_lookup(MS, Some(DiskMiss::Absent));
        s.record_disk_lookup(MS, Some(DiskMiss::NoFingerprint));
        s.record_disk_lookup(MS, Some(DiskMiss::ReadError));
        assert_eq!(s.snapshot().disk_hit_rate(), Some(0.5));
        assert_eq!(LitStats::default().snapshot().disk_hit_rate(), None);
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
