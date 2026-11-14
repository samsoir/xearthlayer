//! Makes a 1 degree tile's merged roads ready for the lit path.
//!
//! For each tile it finds the DSFs X-Plane loads (the pack index, built
//! once), reads their road networks on the blocking pool from the highest
//! priority down, merges them and keeps the result in a byte-bounded memory
//! cache. A build runs in its own task, so a caller that stops waiting never
//! cancels it and concurrent callers share it. Extractions of 7z DSFs are
//! kept in the lit disk tier, because decompressing a mesh costs far more
//! than reading the entry back; raw DSFs are parsed again each session.
//!
//! Callers wait per tile, not per call: the first `footprint` for a tile
//! and purpose sets that tile's deadline, and no call waits past it. X-Plane
//! stats thousands of lit names, so a wait per call would multiply.
//!
//! Blocking work is bounded by [`ROAD_EXTRACT_PERMITS`]: a build holds one
//! permit for its whole tile and no other lit permit, and hands it to the
//! detached write of its cache entries, so at most that many tiles' entries
//! are ever waiting in memory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use dashmap::{DashMap, DashSet};
use futures::future::{join_all, BoxFuture, FutureExt, Shared};
use moka::notification::RemovalCause;
use moka::policy::EvictionPolicy;
use sha2::{Digest, Sha256};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use super::codec::{decode_roads, encode_roads};
use super::dsf::{read_dsf_file, DsfFileStats};
use super::is_lit_subtype;
use super::merge::{MergedRoads, Merger};
use super::packs::PackIndex;
use super::stats::{BuildRecord, Stats, WaitCounters};
use crate::cache::Cache;
use crate::night::{decode_entry_unsized, encode_entry, FileIdentity, LitFingerprint};

pub use super::stats::StatsSnapshot;

/// Concurrent tile builds (each followed by the write of its cache
/// entries). The permits sit inside the blocking pool's
/// `BLOCKING_POOL_HEADROOM`, beside the lit ones.
pub const ROAD_EXTRACT_PERMITS: usize = 2;
/// How long the lit probe of a tile's first footprint may wait for it.
pub const ROAD_PROBE_WAIT: Duration = Duration::from_secs(2);
/// How long the lit generation of a tile's first footprint may wait for it.
pub const ROAD_GENERATE_WAIT: Duration = Duration::from_secs(10);
/// Memory for merged tiles, weighted by [`MergedRoads::bytes`].
pub const ROAD_TILE_CACHE_BYTES: u64 = 128 * 1024 * 1024;
/// Stamped into every disk cache entry: bump it when what an extraction
/// keeps changes (the reader, the subtype filter or the codec's input).
pub const ROAD_EXTRACT_VERSION: u32 = 1;
/// Between two `Night roads: summary` lines.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

/// A 1 degree tile, `(lat, lon)` of its south-west corner.
type Tile = (i32, i32);

/// What asked for a tile first; logged with `Night roads: tile ready`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    DsfOpen,
    PrefetchHint,
    Probe,
    Generate,
}

/// What a `footprint` call is for, which sets how long its tiles are
/// waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    Probe,
    Generate,
}

impl Purpose {
    fn trigger(self) -> Trigger {
        match self {
            Self::Probe => Trigger::Probe,
            Self::Generate => Trigger::Generate,
        }
    }
}

/// Waits, memory and permits of a [`RoadService`]; `Default` is the
/// constants.
#[derive(Debug, Clone)]
pub struct RoadServiceConfig {
    pub probe_wait: Duration,
    pub generate_wait: Duration,
    pub tile_cache_bytes: u64,
    /// Concurrent builds, 1 to [`ROAD_EXTRACT_PERMITS`] (clamped: the night
    /// permit budget has no room for more).
    pub extract_permits: usize,
    /// Every tile build sleeps this long first (deadline tests).
    #[cfg(test)]
    pub build_delay: Duration,
}

impl Default for RoadServiceConfig {
    fn default() -> Self {
        Self {
            probe_wait: ROAD_PROBE_WAIT,
            generate_wait: ROAD_GENERATE_WAIT,
            tile_cache_bytes: ROAD_TILE_CACHE_BYTES,
            extract_permits: ROAD_EXTRACT_PERMITS,
            #[cfg(test)]
            build_delay: Duration::ZERO,
        }
    }
}

impl RoadServiceConfig {
    fn wait(&self, purpose: Purpose) -> Duration {
        match purpose {
            Purpose::Probe => self.probe_wait,
            Purpose::Generate => self.generate_wait,
        }
    }
}

/// A DSF as it is on disk; hashed into the lit fingerprint and the road
/// cache entry's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DsfIdentity {
    pub path: String,
    pub len: u64,
    pub mtime_nanos: u128,
}

impl DsfIdentity {
    /// Blocking: one `stat`.
    fn of(path: &Path) -> std::io::Result<Self> {
        let id = FileIdentity::from_metadata(&std::fs::metadata(path)?);
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            len: id.len,
            mtime_nanos: id.mtime_nanos,
        })
    }
}

/// The merged roads of a footprint's tiles that are ready, in the order
/// asked.
#[derive(Debug, Clone, Default)]
pub struct FootprintRoads {
    pub tiles: Vec<Arc<MergedRoads>>,
    /// False when any tile is missing, for any reason.
    pub complete: bool,
    /// True when a tile was still building when its deadline passed. A
    /// failed tile is not a timeout: it has no roads this session.
    pub timed_out: bool,
}

/// How one tile of a footprint ended.
enum TileWait {
    Ready(Arc<MergedRoads>),
    Failed,
    TimedOut,
}

/// A tile build in flight, awaited by any number of callers.
type Pending = Shared<BoxFuture<'static, Option<Arc<MergedRoads>>>>;

/// Merged roads per tile, built on demand. See the module documentation.
pub struct RoadService {
    install: PathBuf,
    /// Packs the index leaves out (XEL's own mounts).
    skip: Vec<PathBuf>,
    /// The lit disk tier; `None` when caching is off.
    disk: Option<Arc<dyn Cache>>,
    config: RoadServiceConfig,
    /// The pack index build, started by the first caller in its own task so
    /// a cancelled caller neither stops nor restarts it.
    index: OnceLock<Shared<BoxFuture<'static, Arc<PackIndex>>>>,
    /// Ready tiles. A failed build caches nothing.
    tiles: moka::future::Cache<Tile, Arc<MergedRoads>>,
    /// Builds in flight, one per tile.
    pending: DashMap<Tile, Pending>,
    /// Tiles with an unreadable DSF: no roads for the session.
    failed: DashSet<Tile>,
    /// When callers stop waiting for a tile, per purpose; set by the first.
    deadlines: DashMap<(Tile, Purpose), Instant>,
    /// Every source's identity, taken once per tile; concurrent first
    /// callers share one pass.
    identities: DashMap<Tile, Arc<OnceCell<Arc<[DsfIdentity]>>>>,
    /// When each tile was first ready, for the lead time.
    ready_at: DashMap<Tile, Instant>,
    /// Held by a build, then by the write of its entries.
    extract: Arc<Semaphore>,
    stats: Stats,
    summary_started: AtomicBool,
    #[cfg(test)]
    test: Arc<TestProbes>,
}

/// Counters only tests read.
#[cfg(test)]
#[derive(Default)]
struct TestProbes {
    writes_pending: std::sync::atomic::AtomicUsize,
    writes_peak: std::sync::atomic::AtomicUsize,
    index_builds: std::sync::atomic::AtomicUsize,
    identity_passes: std::sync::atomic::AtomicUsize,
    leads_noted: std::sync::atomic::AtomicUsize,
}

/// A source that stopped a tile build.
struct Unreadable {
    path: String,
    error: String,
}

/// A tile's merge and what it cost.
struct Merged {
    roads: MergedRoads,
    /// Disk entries to write once the tile is known to be good.
    writes: Vec<(String, Bytes)>,
    /// Sources that existed and were read.
    dsfs: usize,
    /// Sources never reached because a zone covered the tile.
    skipped: usize,
}

/// One DSF read from its file, after it was merged.
struct Extracted {
    stats: DsfFileStats,
    segments_kept: usize,
    exclusions: usize,
    entry: Option<Bytes>,
    encode_us: u64,
}

/// Ends a build's `pending` entry however its task ends. A task that ends
/// without an answer (a panic in the build's async part) fails the tile for
/// the session, so later callers get `None` at once instead of waiting on a
/// dead build.
struct PendingGuard {
    svc: Arc<RoadService>,
    tile: Tile,
    answered: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        // `failed` first: `start` re-checks it under the `pending` lock.
        if !self.answered && self.svc.failed.insert(self.tile) {
            debug!(
                lat = self.tile.0,
                lon = self.tile.1,
                "Night roads: tile build ended without an answer; no road lights this session"
            );
        }
        self.svc.pending.remove(&self.tile);
    }
}

impl RoadService {
    pub fn new(install: PathBuf, skip: Vec<PathBuf>, disk: Option<Arc<dyn Cache>>) -> Arc<Self> {
        Self::with_config(install, skip, disk, RoadServiceConfig::default())
    }

    pub(crate) fn with_config(
        install: PathBuf,
        skip: Vec<PathBuf>,
        disk: Option<Arc<dyn Cache>>,
        config: RoadServiceConfig,
    ) -> Arc<Self> {
        // LRU, not moka's default TinyLFU: admission by popularity would
        // reject a tile just built once the cache is full, and every request
        // for it would rebuild it.
        let tiles = moka::future::Cache::builder()
            .eviction_policy(EvictionPolicy::lru())
            .max_capacity(config.tile_cache_bytes)
            .weigher(|_, v: &Arc<MergedRoads>| v.bytes().try_into().unwrap_or(u32::MAX))
            .eviction_listener(|tile: Arc<Tile>, roads: Arc<MergedRoads>, cause| {
                if cause == RemovalCause::Size {
                    debug!(
                        lat = tile.0,
                        lon = tile.1,
                        bytes_held = roads.bytes(),
                        "Night roads: LRU"
                    );
                }
            })
            .build();
        let permits = config.extract_permits.clamp(1, ROAD_EXTRACT_PERMITS);
        Arc::new(Self {
            install,
            skip,
            disk,
            config,
            index: OnceLock::new(),
            tiles,
            pending: DashMap::new(),
            failed: DashSet::new(),
            deadlines: DashMap::new(),
            identities: DashMap::new(),
            ready_at: DashMap::new(),
            extract: Arc::new(Semaphore::new(permits)),
            stats: Stats::default(),
            summary_started: AtomicBool::new(false),
            #[cfg(test)]
            test: Arc::default(),
        })
    }

    /// Starts building `tile` unless it is ready, failed or building.
    /// Returns at once; the build runs in its own task.
    pub fn ensure(self: &Arc<Self>, tile: Tile, trigger: Trigger) {
        if !self.tiles.contains_key(&tile) {
            let _ = self.start(tile, trigger);
        }
    }

    /// The ready roads of `tiles`, waiting for each tile at most until its
    /// deadline for `purpose` (set by the first call that asked for it).
    /// Records the call's wait and timeout, and the lead time of each tile
    /// this call is the first probe of.
    pub async fn footprint(self: &Arc<Self>, tiles: &[Tile], purpose: Purpose) -> FootprintRoads {
        let started = Instant::now();
        let wait = self.config.wait(purpose);
        let found = join_all(tiles.iter().map(|&tile| {
            let (deadline, first) = match self.deadlines.entry((tile, purpose)) {
                Entry::Occupied(e) => (*e.get(), false),
                Entry::Vacant(e) => (*e.insert(started + wait), true),
            };
            async move {
                let (state, waited) = self.tile_by(tile, purpose.trigger(), deadline).await;
                (tile, first, state, waited)
            }
        }))
        .await;
        let now = Instant::now();
        let mut out = FootprintRoads {
            tiles: Vec::with_capacity(found.len()),
            complete: true,
            timed_out: false,
        };
        let mut waited = false;
        for (tile, first, state, tile_waited) in found {
            waited |= tile_waited;
            if first && purpose == Purpose::Probe {
                self.note_lead(tile, started, now);
            }
            match state {
                TileWait::Ready(roads) => out.tiles.push(roads),
                TileWait::Failed => out.complete = false,
                TileWait::TimedOut => {
                    out.complete = false;
                    out.timed_out = true;
                }
            }
        }
        self.waits(purpose).record(waited, out.timed_out);
        if waited || out.timed_out {
            debug!(
                purpose = ?purpose,
                tiles = tiles.len(),
                waited_ms = now.duration_since(started).as_millis() as u64,
                timed_out = out.timed_out,
                complete = out.complete,
                "Night roads: footprint waited"
            );
        }
        out
    }

    /// The identities of every DSF of `tile`, highest priority first,
    /// taken once per tile per session (X-Plane does not change a DSF it
    /// has loaded). Metadata only; a missing file is left out. The `stat`s
    /// run on the blocking pool under the caller's permit, if any: this
    /// takes none. A pass that fails (a panic) answers empty for this call
    /// only and is not kept.
    pub async fn identities(self: &Arc<Self>, tile: Tile) -> Arc<[DsfIdentity]> {
        let cell = Arc::clone(&self.identities.entry(tile).or_default());
        let ids = cell
            .get_or_try_init(|| async {
                let paths = self.source_paths(tile).await;
                #[cfg(test)]
                self.test.identity_passes.fetch_add(1, Ordering::SeqCst);
                tokio::task::spawn_blocking(move || {
                    paths
                        .iter()
                        .filter_map(|p| DsfIdentity::of(p).ok())
                        .collect::<Vec<_>>()
                })
                .await
                .map(Arc::<[DsfIdentity]>::from)
            })
            .await;
        match ids {
            Ok(ids) => Arc::clone(ids),
            Err(e) => {
                debug!(
                    lat = tile.0,
                    lon = tile.1,
                    error = %e,
                    "Night roads: DSF identities unavailable for this call"
                );
                Arc::from(Vec::new())
            }
        }
    }

    /// The lit source committed a dark answer for a footprint whose probe
    /// timed out: a late road could have lit it.
    pub fn note_dark_answer_before_roads(&self) {
        self.stats.record_dark_answer();
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Whether the tile cache holds the tile.
    pub fn is_ready(&self, tile: Tile) -> bool {
        self.tiles.contains_key(&tile)
    }

    fn waits(&self, purpose: Purpose) -> &WaitCounters {
        match purpose {
            Purpose::Probe => &self.stats.probe,
            Purpose::Generate => &self.stats.generate,
        }
    }

    /// Logs how long before (positive) or after (negative) its first probe
    /// at `first_probe` the tile was ready.
    fn note_lead(&self, tile: Tile, first_probe: Instant, now: Instant) {
        let ready = self.ready_at.get(&tile).map(|r| *r);
        #[cfg(test)]
        self.test.leads_noted.fetch_add(1, Ordering::SeqCst);
        debug!(
            lat = tile.0,
            lon = tile.1,
            ms = lead_ms(first_probe, ready, now),
            "Night roads: lead time"
        );
    }

    /// How the tile ended for a caller waiting until `deadline`, and whether
    /// the caller waited for it. Starts its build if needed.
    async fn tile_by(
        self: &Arc<Self>,
        tile: Tile,
        trigger: Trigger,
        deadline: Instant,
    ) -> (TileWait, bool) {
        if let Some(ready) = self.tiles.get(&tile).await {
            return (TileWait::Ready(ready), false);
        }
        let Some(pending) = self.start(tile, trigger) else {
            return (TileWait::Failed, false);
        };
        let Some(until) = wait_until(deadline, Instant::now()) else {
            return (TileWait::TimedOut, false);
        };
        let state = match tokio::time::timeout_at(until.into(), pending).await {
            Ok(Some(roads)) => TileWait::Ready(roads),
            Ok(None) => TileWait::Failed,
            Err(_) => TileWait::TimedOut,
        };
        (state, true)
    }

    /// The build of `tile`, started unless one is in flight; `None` for a
    /// failed tile. The build is its own task, so dropping the returned
    /// future never cancels it.
    fn start(self: &Arc<Self>, tile: Tile, trigger: Trigger) -> Option<Pending> {
        if self.failed.contains(&tile) {
            return None;
        }
        self.start_summary();
        match self.pending.entry(tile) {
            Entry::Occupied(e) => Some(e.get().clone()),
            Entry::Vacant(e) => {
                // A build that failed after the check above marked the tile
                // before its guard removed the entry this lock now guards.
                if self.failed.contains(&tile) {
                    return None;
                }
                let svc = Arc::clone(self);
                let build = tokio::spawn(async move {
                    let mut guard = PendingGuard {
                        svc,
                        tile,
                        answered: false,
                    };
                    let builder = Arc::clone(&guard.svc);
                    let roads = guard
                        .svc
                        .tiles
                        .optionally_get_with(tile, async move {
                            builder.build_tile(tile, trigger).await.map(Arc::new)
                        })
                        .await;
                    guard.answered = true;
                    roads
                });
                let pending = async move { build.await.ok().flatten() }.boxed().shared();
                Some(e.insert(pending).clone())
            }
        }
    }

    /// Reads and merges the tile's sources under one extract permit, which
    /// then passes to the write of the tile's cache entries. Any unreadable
    /// source fails the tile for the session.
    async fn build_tile(self: Arc<Self>, tile: Tile, trigger: Trigger) -> Option<MergedRoads> {
        #[cfg(test)]
        tokio::time::sleep(self.config.build_delay).await;
        let started = Instant::now();
        let sources = self.source_paths(tile).await;
        let asked = Instant::now();
        let permit = Arc::clone(&self.extract).acquire_owned().await.ok()?;
        let mut record = BuildRecord {
            permit_wait_us: micros(asked.elapsed()),
            ..BuildRecord::default()
        };
        let mut merged = self.merge_sources(tile, &sources, &mut record).await;
        let writes = match &mut merged {
            Ok(m) => std::mem::take(&mut m.writes),
            Err(_) => Vec::new(),
        };
        self.write_back(writes, permit);
        record.total_us = micros(started.elapsed());
        record.ready = merged.is_ok();
        self.stats.record_build(&record);
        match merged {
            Ok(m) => {
                self.ready_at.entry(tile).or_insert_with(Instant::now);
                debug!(
                    lat = tile.0,
                    lon = tile.1,
                    trigger = ?trigger,
                    dsfs = m.dsfs,
                    cache_hits = record.cache_hits,
                    cache_misses = record.cache_misses,
                    merged_segments = m.roads.len(),
                    removed = m.roads.removed,
                    skipped_below_cover = m.skipped,
                    bytes = m.roads.bytes(),
                    total_ms = record.total_us / 1000,
                    "Night roads: tile ready"
                );
                Some(m.roads)
            }
            Err(u) => {
                if self.failed.insert(tile) {
                    warn!(
                        lat = tile.0,
                        lon = tile.1,
                        path = %u.path,
                        error = %u.error,
                        "Night roads: DSF unreadable; tile has no road lights this session"
                    );
                }
                None
            }
        }
    }

    /// Feeds the tile's sources to a merger, highest priority first, until
    /// a zone covers the tile. Each extraction is dropped once merged (and
    /// encoded, for a 7z), so at most one is alive at a time.
    async fn merge_sources(
        &self,
        tile: Tile,
        sources: &[PathBuf],
        record: &mut BuildRecord,
    ) -> Result<Merged, Unreadable> {
        let mut merger = Merger::new(tile);
        let mut writes = vec![];
        let (mut dsfs, mut reached) = (0, 0);
        for path in sources {
            reached += 1;
            let p = path.clone();
            let (id, cpu) = on_blocking_pool(&path.to_string_lossy(), move || {
                timed(|| DsfIdentity::of(&p))
            })
            .await?;
            record.cpu_us += cpu;
            let id = match id {
                Ok(id) => id,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Unreadable {
                        path: path.to_string_lossy().into_owned(),
                        error: e.to_string(),
                    })
                }
            };
            dsfs += 1;
            let (key, fp) = (cache_key(tile, &id.path), entry_fingerprint(&id));

            let lookup = Instant::now();
            let stored = match &self.disk {
                Some(disk) => disk.get(&key).await.ok().flatten(),
                None => None,
            };
            let read_ms = lookup.elapsed().as_millis() as u64;
            if let Some(entry) = stored {
                let (m, segments, decode_us, cpu) = on_blocking_pool(&id.path, move || {
                    let t = Instant::now();
                    let roads = decode_entry_unsized(entry, &fp).and_then(|b| decode_roads(&b));
                    let decode_us = micros(t.elapsed());
                    let segments = roads.map(|r| {
                        merger.add(&r);
                        r.chains.len()
                    });
                    (merger, segments, decode_us, micros(t.elapsed()))
                })
                .await?;
                merger = m;
                record.cpu_us += cpu;
                if let Some(segments) = segments {
                    record.cache_hits += 1;
                    debug!(
                        path = %id.path,
                        read_ms,
                        decode_ms = decode_us / 1000,
                        segments,
                        "Night roads: DSF cache hit"
                    );
                    if merger.covered() {
                        break;
                    }
                    continue;
                }
            }

            let cache = self.disk.is_some();
            let p = path.clone();
            let (m, read, read_us, cpu) = on_blocking_pool(&id.path, move || {
                let t = Instant::now();
                let read = read_dsf_file(&p, is_lit_subtype);
                let read_us = micros(t.elapsed());
                let read = read.map(|(roads, stats)| {
                    let (payload, mut encode_us) =
                        timed(|| (cache && stats.compressed).then(|| encode_roads(&roads)));
                    merger.add(&roads);
                    let (segments_kept, exclusions) = (roads.chains.len(), roads.exclusions.len());
                    drop(roads);
                    let (entry, us) = timed(|| payload.map(|p| encode_entry(&fp, &p)));
                    encode_us += us;
                    Extracted {
                        stats,
                        segments_kept,
                        exclusions,
                        entry,
                        encode_us,
                    }
                });
                (merger, read, read_us, micros(t.elapsed()))
            })
            .await?;
            merger = m;
            record.cpu_us += cpu;
            record.extract_us.push(read_us);
            let x = read.map_err(|e| Unreadable {
                path: id.path.clone(),
                error: e.to_string(),
            })?;
            if cache && x.stats.compressed {
                record.cache_misses += 1;
            }
            debug!(
                path = %id.path,
                file_bytes = x.stats.file_bytes,
                read_bytes = x.stats.read_bytes,
                compressed = x.stats.compressed,
                ms = read_us / 1000,
                encode_ms = x.encode_us / 1000,
                segments_kept = x.segments_kept,
                exclusions = x.exclusions,
                pool_points = x.stats.pool_points,
                walk_visits = x.stats.walk_visits,
                cached = x.entry.is_some(),
                "Night roads: DSF extracted"
            );
            writes.extend(x.entry.map(|e| (key, e)));
            if merger.covered() {
                break;
            }
        }
        let (roads, cpu) = on_blocking_pool("merge", move || timed(|| merger.finish())).await?;
        record.cpu_us += cpu;
        Ok(Merged {
            roads,
            writes,
            dsfs,
            skipped: sources.len() - reached,
        })
    }

    /// The tile's DSF paths, highest priority first.
    async fn source_paths(&self, tile: Tile) -> Vec<PathBuf> {
        let index = self.index().await;
        index
            .sources(tile.0, tile.1)
            .into_iter()
            .map(|s| s.path)
            .collect()
    }

    /// The pack index, built once in its own blocking task: a caller that
    /// is cancelled leaves the build running for the next. A one-off
    /// directory listing per session; it takes no permit, so a caller
    /// holding one (a lit disk permit around `identities`) never holds two.
    async fn index(&self) -> Arc<PackIndex> {
        let build = self.index.get_or_init(|| {
            let (install, skip) = (self.install.clone(), self.skip.clone());
            #[cfg(test)]
            let test = Arc::clone(&self.test);
            let task = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                test.index_builds.fetch_add(1, Ordering::SeqCst);
                PackIndex::build(&install, &skip)
            });
            async move {
                let (index, s) = task.await.unwrap_or_default();
                debug!(
                    packs = s.packs,
                    enabled = s.enabled,
                    dsfs = s.dsfs,
                    tiles = s.tiles,
                    bytes = index.bytes(),
                    ms = s.ms,
                    "Night roads: pack index built"
                );
                Arc::new(index)
            }
            .boxed()
            .shared()
        });
        build.clone().await
    }

    /// Writes a built tile's cache entries in one detached task that takes
    /// over the build's permit and releases it when done: an entry never
    /// queues behind other builds, and at most the permit count of tiles'
    /// entries are ever waiting. With nothing to write the permit is
    /// released here.
    fn write_back(&self, entries: Vec<(String, Bytes)>, permit: OwnedSemaphorePermit) {
        let Some(disk) = self.disk.clone().filter(|_| !entries.is_empty()) else {
            return;
        };
        #[cfg(test)]
        let test = {
            let n = self.test.writes_pending.fetch_add(1, Ordering::SeqCst) + 1;
            self.test.writes_peak.fetch_max(n, Ordering::SeqCst);
            Arc::clone(&self.test)
        };
        tokio::spawn(async move {
            for (key, entry) in entries {
                if let Err(e) = disk.set(&key, entry).await {
                    debug!(error = %e, key, "Night roads: road cache write failed");
                }
            }
            #[cfg(test)]
            test.writes_pending.fetch_sub(1, Ordering::SeqCst);
            drop(permit);
        });
    }

    /// Starts the minute summary once. The task holds only a `Weak`, so it
    /// ends at the first tick after the service is dropped.
    fn start_summary(self: &Arc<Self>) {
        if self.summary_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SUMMARY_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(svc) = weak.upgrade() else {
                    break;
                };
                svc.stats.log_summary(svc.tiles.weighted_size());
            }
        });
    }

    #[cfg(test)]
    async fn writes_settled(&self) {
        while self.test.writes_pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

/// Runs `f` on the blocking pool; a panic there stops the build as an
/// unreadable `path` would.
async fn on_blocking_pool<T: Send + 'static>(
    path: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unreadable> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Unreadable {
            path: path.to_owned(),
            error: e.to_string(),
        })
}

/// `f`'s result and its wall time in microseconds.
fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let started = Instant::now();
    let out = f();
    (out, micros(started.elapsed()))
}

fn micros(d: Duration) -> u64 {
    d.as_micros().try_into().unwrap_or(u64::MAX)
}

/// The disk cache key of a DSF's extraction: `roads:{lat}:{lon}:{hex16}`,
/// the tile (so the entry lands in the tile's region directory) and the
/// first 8 bytes of the path's SHA-256.
fn cache_key(tile: Tile, path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("roads:{}:{}:{hex}", tile.0, tile.1)
}

/// The fingerprint a DSF's cache entry carries: a rewritten file, or a new
/// extraction version, makes the stored entry a miss.
fn entry_fingerprint(id: &DsfIdentity) -> LitFingerprint {
    let mut h = Sha256::new();
    h.update(ROAD_EXTRACT_VERSION.to_le_bytes());
    h.update(id.path.as_bytes());
    h.update(id.len.to_le_bytes());
    h.update(id.mtime_nanos.to_le_bytes());
    LitFingerprint(h.finalize().into())
}

/// The instant to wait until for a tile whose deadline is `deadline`;
/// `None` once it has passed.
fn wait_until(deadline: Instant, now: Instant) -> Option<Instant> {
    (deadline > now).then_some(deadline)
}

/// Milliseconds from the tile being ready to its first probe: positive when
/// the roads came first, negative when the probe did (still not ready
/// counts up to `now`).
fn lead_ms(first_probe: Instant, ready_at: Option<Instant>, now: Instant) -> i64 {
    let signed = |a: Instant, b: Instant| match a.checked_duration_since(b) {
        Some(d) => d.as_millis() as i64,
        None => -(b.duration_since(a).as_millis() as i64),
    };
    signed(first_probe, ready_at.unwrap_or(now))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::night::roads::dsf::test_dsf::{install as write_install, sevenz, Builder};

    const TILE: (i32, i32) = (46, 6);

    fn high(zone: Option<&str>) -> Builder {
        let mut b = Builder::new(46, 6);
        if let Some(z) = zone {
            b.props.push(("sim/exclude_net".into(), z.into()));
        }
        b.road(46, 6, 100, &[(6.1, 46.1), (6.2, 46.2)]);
        b
    }

    fn low() -> Builder {
        let mut b = Builder::new(46, 6);
        b.road(46, 6, 50, &[(6.5, 46.5), (6.6, 46.6)]);
        b
    }

    /// install/ with scenery_packs listing High above Low, both with +46+006.
    fn install_with(high: Vec<u8>, low: Vec<u8>) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        write_install(t.path(), TILE, &[("High", Some(high)), ("Low", Some(low))]);
        t
    }

    fn install(high_zone: Option<&str>) -> tempfile::TempDir {
        install_with(high(high_zone).bytes(), low().bytes())
    }

    fn dsf(t: &tempfile::TempDir, pack: &str) -> PathBuf {
        t.path()
            .join("Custom Scenery")
            .join(pack)
            .join("Earth nav data/+40+000/+46+006.dsf")
    }

    fn disk() -> Arc<dyn Cache> {
        Arc::new(crate::cache::MemoryCacheProvider::new(64 << 20, None))
    }

    fn config(probe_ms: u64, generate_ms: u64, delay_ms: u64) -> RoadServiceConfig {
        RoadServiceConfig {
            probe_wait: Duration::from_millis(probe_ms),
            generate_wait: Duration::from_millis(generate_ms),
            build_delay: Duration::from_millis(delay_ms),
            ..RoadServiceConfig::default()
        }
    }

    /// A tile-cache budget that holds `tiles` test tiles of at most two
    /// chains of two points each, whatever a tile's fixed overhead is. The
    /// tests that use it assert their real tiles fit it.
    fn budget_for(tiles: usize) -> u64 {
        // Per chain: two points of two u16 offsets, a chain byte and a
        // four-cell box (a tile this small has no cell index).
        let chain = 2 * 2 * 2 + 1 + 4;
        (tiles * (MergedRoads::empty(TILE).bytes() + 2 * chain)) as u64
    }

    /// The disk cache's entry count once pending writes and maintenance ran.
    async fn entries(svc: &RoadService, d: &Arc<dyn Cache>) -> u64 {
        svc.writes_settled().await;
        d.gc().await.unwrap();
        d.entry_count()
    }

    // Pure pieces.

    #[test]
    fn cache_keys_name_the_tile_and_a_path_digest() {
        let k = cache_key((-34, -71), "/x/+40+000/-34-071.dsf");
        let parts: Vec<&str> = k.split(':').collect();
        assert_eq!(parts[..3], ["roads", "-34", "-71"]);
        assert_eq!(parts[3].len(), 16);
        assert!(parts[3]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(k, cache_key((-34, -71), "/y/+40+000/-34-071.dsf"));
        assert_eq!(
            crate::cache::lru_index::key_to_region(&k).as_deref(),
            Some("-34-071")
        );
    }

    #[test]
    fn every_identity_field_moves_the_entry_fingerprint() {
        let id = DsfIdentity {
            path: "/a.dsf".into(),
            len: 10,
            mtime_nanos: 5,
        };
        let base = entry_fingerprint(&id);
        assert_eq!(base, entry_fingerprint(&id.clone()));
        for other in [
            DsfIdentity {
                path: "/b.dsf".into(),
                ..id.clone()
            },
            DsfIdentity {
                len: 11,
                ..id.clone()
            },
            DsfIdentity {
                mtime_nanos: 6,
                ..id.clone()
            },
        ] {
            assert_ne!(base, entry_fingerprint(&other), "{other:?}");
        }
    }

    #[test]
    fn a_tile_is_waited_for_only_until_its_deadline() {
        let now = Instant::now();
        let later = now + Duration::from_millis(5);
        assert_eq!(wait_until(later, now), Some(later));
        assert_eq!(wait_until(now, now), None);
        assert_eq!(wait_until(now, later), None);
    }

    #[test]
    fn lead_time_is_positive_when_the_roads_came_first() {
        let t0 = Instant::now();
        let ms = |n| Duration::from_millis(n);
        assert_eq!(lead_ms(t0 + ms(300), Some(t0), t0 + ms(900)), 300);
        assert_eq!(lead_ms(t0, Some(t0 + ms(250)), t0 + ms(900)), -250);
        assert_eq!(lead_ms(t0, None, t0 + ms(900)), -900, "still not ready");
    }

    #[test]
    fn purposes_have_their_own_waits_and_triggers() {
        let c = config(10, 20, 0);
        assert_eq!(c.wait(Purpose::Probe), Duration::from_millis(10));
        assert_eq!(c.wait(Purpose::Generate), Duration::from_millis(20));
        assert!(matches!(Purpose::Probe.trigger(), Trigger::Probe));
        assert!(matches!(Purpose::Generate.trigger(), Trigger::Generate));
        let d = RoadServiceConfig::default();
        assert_eq!(d.probe_wait, ROAD_PROBE_WAIT);
        assert_eq!(d.generate_wait, ROAD_GENERATE_WAIT);
        assert_eq!(d.tile_cache_bytes, ROAD_TILE_CACHE_BYTES);
        assert_eq!(d.build_delay, Duration::ZERO);
    }

    // Merging.

    #[tokio::test]
    async fn merges_both_packs_without_exclusions() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], Some(disk()));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete);
        assert_eq!(r.tiles[0].len(), 2);
        assert!(svc.is_ready(TILE));
    }

    #[tokio::test]
    async fn a_whole_tile_exclusion_hides_and_skips_lower_packs() {
        let t = install(Some("6/46/7/47"));
        // Corrupt the low DSF: if it were read, the tile would fail.
        std::fs::write(dsf(&t, "Low"), b"garbage").unwrap();
        let svc = RoadService::new(t.path().into(), vec![], Some(disk()));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete, "the low DSF is never read");
        assert_eq!(r.tiles[0].len(), 1);
    }

    #[tokio::test]
    async fn a_tile_with_no_sources_is_complete_and_empty() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let r = svc.footprint(&[(10, 10)], Purpose::Probe).await;
        assert!(r.complete);
        assert!(r.tiles[0].is_empty());
    }

    #[tokio::test]
    async fn a_source_removed_since_the_index_was_built_is_skipped() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        assert_eq!(svc.identities(TILE).await.len(), 2, "index built");
        std::fs::remove_file(dsf(&t, "Low")).unwrap();
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete);
        assert_eq!(r.tiles[0].len(), 1);
    }

    // Failure.

    #[tokio::test]
    async fn an_unreadable_dsf_fails_the_whole_tile_and_writes_no_entry() {
        // The high DSF is a valid 7z, so it would be cached if the tile succeeded.
        let t = install_with(sevenz(&high(None).bytes()), b"garbage".to_vec());
        let d = disk();
        let svc = RoadService::new(t.path().into(), vec![], Some(d.clone()));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(!r.complete);
        assert!(r.tiles.is_empty());
        assert!(!svc.is_ready(TILE));
        assert_eq!(entries(&svc, &d).await, 0);
        assert_eq!(svc.stats_snapshot().tiles_failed, 1);
    }

    #[tokio::test]
    async fn a_failed_tile_answers_at_once_and_incomplete_later() {
        let t = install_with(high(None).bytes(), b"garbage".to_vec());
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(5_000, 5_000, 0));
        assert!(!svc.footprint(&[TILE], Purpose::Probe).await.complete);
        let started = Instant::now();
        let r = svc.footprint(&[TILE], Purpose::Generate).await;
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!r.complete);
        assert!(r.tiles.is_empty());
        assert_eq!(svc.stats_snapshot().tiles_failed, 1, "not rebuilt");
    }

    #[tokio::test]
    async fn a_failed_tile_keeps_the_other_tiles_of_a_footprint() {
        let t = install_with(high(None).bytes(), b"garbage".to_vec());
        let svc = RoadService::new(t.path().into(), vec![], None);
        let r = svc.footprint(&[TILE, (10, 10)], Purpose::Probe).await;
        assert!(!r.complete);
        assert_eq!(r.tiles.len(), 1);
        assert_eq!(r.tiles[0].tile, (10, 10));
    }

    // Disk cache.

    #[tokio::test]
    async fn a_raw_dsf_is_never_written_to_the_disk_cache() {
        let t = install(None);
        let d = disk();
        let svc = RoadService::new(t.path().into(), vec![], Some(d.clone()));
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert_eq!(entries(&svc, &d).await, 0);
        let s = svc.stats_snapshot();
        assert_eq!((s.cache_hits, s.cache_misses), (0, 0));
    }

    #[tokio::test]
    async fn a_second_service_reads_a_compressed_extraction_from_the_disk_cache() {
        let t = install_with(sevenz(&high(None).bytes()), low().bytes());
        let d = disk();
        let a = RoadService::new(t.path().into(), vec![], Some(d.clone()));
        let first = a.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(a.stats_snapshot().cache_misses, 1);
        assert_eq!(entries(&a, &d).await, 1, "only the 7z DSF");
        let id = &a.identities(TILE).await[0];
        let entry = d.get(&cache_key(TILE, &id.path)).await.unwrap().unwrap();
        assert_eq!(&entry[..4], crate::cache::integrity::LIT_ENTRY_MAGIC);

        let b = RoadService::new(t.path().into(), vec![], Some(d));
        let second = b.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(b.stats_snapshot().cache_hits, 1);
        assert_eq!(b.stats_snapshot().cache_misses, 0);
        assert_eq!(second.tiles[0], first.tiles[0], "same merged roads");
    }

    #[tokio::test]
    async fn a_cache_entry_for_another_version_of_the_file_is_ignored() {
        let t = install_with(sevenz(&high(None).bytes()), low().bytes());
        let d = disk();
        let a = RoadService::new(t.path().into(), vec![], Some(d.clone()));
        a.footprint(&[TILE], Purpose::Probe).await;
        a.writes_settled().await;
        let mut changed = high(None);
        changed.road(46, 6, 100, &[(6.3, 46.3), (6.4, 46.4), (6.45, 46.45)]);
        std::fs::write(dsf(&t, "High"), sevenz(&changed.bytes())).unwrap();

        let b = RoadService::new(t.path().into(), vec![], Some(d));
        let r = b.footprint(&[TILE], Purpose::Probe).await;
        let s = b.stats_snapshot();
        assert_eq!((s.cache_hits, s.cache_misses), (0, 1));
        assert_eq!(r.tiles[0].len(), 3, "read from the new file");
    }

    // Identities.

    #[tokio::test]
    async fn identities_are_ordered_cached_and_change_with_the_file() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let a = svc.identities(TILE).await;
        assert_eq!(a.len(), 2);
        assert!(a[0].path.contains("High"));
        assert!(a[1].path.contains("Low"));
        std::fs::write(dsf(&t, "Low"), b"changed length").unwrap();
        assert_eq!(svc.identities(TILE).await, a, "cached for the session");
        let fresh = RoadService::new(t.path().into(), vec![], None);
        let b = fresh.identities(TILE).await;
        assert_ne!(a, b);
        assert_eq!(a[0], b[0]);
        assert!(fresh.identities((10, 10)).await.is_empty());
    }

    // Coalescing, readiness and the cache.

    #[tokio::test]
    async fn concurrent_footprints_build_a_tile_once() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(5_000, 5_000, 30));
        let (a, b) = tokio::join!(
            svc.footprint(&[TILE], Purpose::Probe),
            svc.footprint(&[TILE], Purpose::Generate)
        );
        assert!(a.complete && b.complete);
        assert!(Arc::ptr_eq(&a.tiles[0], &b.tiles[0]));
        assert_eq!(svc.stats_snapshot().tiles_ready, 1);
    }

    #[tokio::test]
    async fn callers_that_stop_waiting_share_one_build_and_never_cancel_it() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(30, 30, 150));
        assert!(!svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert!(!svc.footprint(&[TILE], Purpose::Generate).await.complete);
        svc.ensure(TILE, Trigger::DsfOpen);
        let a = svc.start(TILE, Trigger::Probe).unwrap();
        assert!(a.ptr_eq(&svc.start(TILE, Trigger::Generate).unwrap()));
        assert_eq!(svc.pending.len(), 1, "one build in flight");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !svc.is_ready(TILE) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(svc.is_ready(TILE), "the build outlived its callers");
        assert_eq!(svc.stats_snapshot().tiles_ready, 1);
        tokio::task::yield_now().await;
        assert!(svc.pending.is_empty());
    }

    #[tokio::test]
    async fn pending_cache_writes_never_exceed_the_permits() {
        // A burst of 7z tiles queues on one permit; a finished tile's entry
        // must not wait in memory behind the rest of the backlog.
        let t = tempfile::tempdir().unwrap();
        let tiles = [(46, 6), (47, 6), (48, 6), (49, 6), (50, 6)];
        for &(lat, lon) in &tiles {
            let mut b = Builder::new(lat, lon);
            let (x, y) = (f64::from(lon), f64::from(lat));
            b.road(lat, lon, 100, &[(x + 0.1, y + 0.1), (x + 0.2, y + 0.2)]);
            write_install(t.path(), (lat, lon), &[("High", Some(sevenz(&b.bytes())))]);
        }
        let d = disk();
        let c = RoadServiceConfig {
            extract_permits: 1,
            ..config(5_000, 5_000, 0)
        };
        let svc = RoadService::with_config(t.path().into(), vec![], Some(d.clone()), c);
        for &tile in &tiles {
            svc.ensure(tile, Trigger::DsfOpen);
        }
        let r = svc.footprint(&tiles, Purpose::Generate).await;
        assert!(r.complete);
        assert_eq!(entries(&svc, &d).await, tiles.len() as u64, "all written");
        assert_eq!(svc.test.writes_peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ensure_alone_makes_the_tile_ready() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        svc.ensure(TILE, Trigger::DsfOpen);
        svc.ensure(TILE, Trigger::DsfOpen);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !svc.is_ready(TILE) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(svc.is_ready(TILE));
        assert_eq!(svc.stats_snapshot().tiles_ready, 1);
    }

    #[tokio::test]
    async fn the_tile_cache_evicts_by_weight_and_rebuilds() {
        let t = install(None);
        let other = (47, 6);
        let mut h = Builder::new(47, 6);
        h.road(47, 6, 100, &[(6.1, 47.1), (6.2, 47.2)]);
        let mut l = Builder::new(47, 6);
        l.road(47, 6, 50, &[(6.5, 47.5), (6.6, 47.6)]);
        write_install(
            t.path(),
            other,
            &[("High", Some(h.bytes())), ("Low", Some(l.bytes()))],
        );
        let budget = budget_for(1);
        let c = RoadServiceConfig {
            tile_cache_bytes: budget,
            ..config(5_000, 5_000, 0)
        };
        let svc = RoadService::with_config(t.path().into(), vec![], None, c);
        let a = svc.footprint(&[TILE], Purpose::Probe).await;
        let b = svc.footprint(&[other], Purpose::Probe).await;
        assert!(a.complete && b.complete);
        let (wa, wb) = (a.tiles[0].bytes() as u64, b.tiles[0].bytes() as u64);
        assert!(wa <= budget && wb <= budget, "either tile alone fits");
        assert!(wa + wb > budget, "both do not");
        svc.tiles.run_pending_tasks().await;
        assert!(svc.tiles.weighted_size() <= budget);
        let ready: Vec<_> = [TILE, other]
            .into_iter()
            .filter(|&t| svc.is_ready(t))
            .collect();
        assert_eq!(ready.len(), 1, "exactly one tile kept");
        let evicted = if ready[0] == TILE { other } else { TILE };
        let r = svc.footprint(&[evicted], Purpose::Probe).await;
        assert!(r.complete);
        assert_eq!(r.tiles[0].len(), 2);
        assert_eq!(svc.stats_snapshot().tiles_ready, 3, "rebuilt");
    }

    #[tokio::test]
    async fn a_tile_just_built_is_kept_over_a_popular_older_one() {
        // Admission by popularity would drop a new tile at once when the
        // cache is full and rebuild it on every request.
        let t = install(None);
        let other = (47, 6);
        let mut h = Builder::new(47, 6);
        h.road(47, 6, 100, &[(6.1, 47.1), (6.2, 47.2)]);
        write_install(t.path(), other, &[("High", Some(h.bytes()))]);
        let budget = budget_for(1);
        let c = RoadServiceConfig {
            tile_cache_bytes: budget,
            ..config(5_000, 5_000, 0)
        };
        let svc = RoadService::with_config(t.path().into(), vec![], None, c);
        let mut old = 0;
        for _ in 0..5 {
            let r = svc.footprint(&[TILE], Purpose::Probe).await;
            assert!(r.complete);
            old = r.tiles[0].bytes() as u64;
        }
        let new = svc.footprint(&[other], Purpose::Probe).await;
        assert!(new.complete);
        let new = new.tiles[0].bytes() as u64;
        assert!(old <= budget && new <= budget, "either tile alone fits");
        assert!(old + new > budget, "both do not");
        svc.tiles.run_pending_tasks().await;
        assert!(svc.is_ready(other));
        assert!(!svc.is_ready(TILE));
    }

    // Per-tile deadlines.

    #[tokio::test]
    async fn a_probe_after_the_first_probes_deadline_does_not_wait() {
        let t = install(None);
        let svc =
            RoadService::with_config(t.path().into(), vec![], None, config(100, 5_000, 2_000));
        let first = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(!first.complete);
        let started = Instant::now();
        let second = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(!second.complete);
        assert!(second.timed_out, "still building past its deadline");
        assert!(second.tiles.is_empty());
        assert_eq!(svc.stats_snapshot().probe_timeouts, 2);
    }

    #[tokio::test]
    async fn a_probe_during_the_build_waits_only_the_remaining_time() {
        let t = install(None);
        let wait = Duration::from_millis(200);
        let svc =
            RoadService::with_config(t.path().into(), vec![], None, config(200, 5_000, 2_000));
        let t0 = Instant::now();
        let s = Arc::clone(&svc);
        let first = tokio::spawn(async move { s.footprint(&[TILE], Purpose::Probe).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        let second = svc.footprint(&[TILE], Purpose::Probe).await;
        let ended = Instant::now();
        assert!(!second.complete);
        assert!(
            ended >= t0 + wait,
            "waited until the first probe's deadline"
        );
        assert!(ended - started < wait, "not a wait of its own");
        assert!(!first.await.unwrap().complete);
    }

    #[tokio::test]
    async fn probe_and_generate_deadlines_are_independent() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(30, 5_000, 200));
        assert!(!svc.footprint(&[TILE], Purpose::Probe).await.complete);
        let r = svc.footprint(&[TILE], Purpose::Generate).await;
        assert!(r.complete, "the generate wait is its own");
        assert!(
            svc.footprint(&[TILE], Purpose::Probe).await.complete,
            "ready now"
        );
    }

    // Instrumentation.

    #[tokio::test]
    async fn a_footprint_past_a_building_tiles_deadline_times_out() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(30, 5_000, 300));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(!r.complete && r.timed_out);
        let s = svc.stats_snapshot();
        assert_eq!((s.probe_waits, s.probe_timeouts), (1, 1));
        assert_eq!((s.generate_waits, s.generate_timeouts), (0, 0));
        assert_eq!(s.answered_before_roads, 0, "only the lit source knows");
    }

    #[tokio::test]
    async fn generate_waits_and_timeouts_are_counted_apart() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(5_000, 30, 300));
        assert!(svc.footprint(&[TILE], Purpose::Generate).await.timed_out);
        let s = svc.stats_snapshot();
        assert_eq!((s.generate_waits, s.generate_timeouts), (1, 1));
        assert_eq!((s.probe_waits, s.probe_timeouts), (0, 0));
    }

    #[tokio::test]
    async fn a_wait_that_ends_ready_is_a_wait_and_no_timeout() {
        let t = install(None);
        let svc = RoadService::with_config(t.path().into(), vec![], None, config(5_000, 5_000, 30));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete && !r.timed_out);
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        let s = svc.stats_snapshot();
        assert_eq!(
            (s.probe_waits, s.probe_timeouts),
            (1, 0),
            "a ready tile is no wait"
        );
    }

    #[tokio::test]
    async fn a_failed_tile_is_incomplete_but_not_timed_out() {
        let t = install_with(high(None).bytes(), b"garbage".to_vec());
        let svc = RoadService::new(t.path().into(), vec![], None);
        for _ in 0..2 {
            let r = svc.footprint(&[TILE], Purpose::Probe).await;
            assert!(!r.complete && !r.timed_out);
        }
        assert_eq!(svc.stats_snapshot().probe_timeouts, 0);
    }

    #[tokio::test]
    async fn dark_answers_before_roads_are_counted() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        svc.note_dark_answer_before_roads();
        svc.note_dark_answer_before_roads();
        assert_eq!(
            svc.stats_snapshot(),
            StatsSnapshot {
                answered_before_roads: 2,
                ..StatsSnapshot::default()
            }
        );
    }

    #[tokio::test]
    async fn lead_time_is_noted_for_the_first_probe_of_each_tile() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let noted = || svc.test.leads_noted.load(Ordering::SeqCst);
        svc.footprint(&[TILE], Purpose::Generate).await;
        assert_eq!(noted(), 0, "not a probe");
        svc.footprint(&[TILE, (10, 10)], Purpose::Probe).await;
        assert_eq!(noted(), 2);
        svc.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(noted(), 2, "once per tile");
    }

    #[tokio::test]
    async fn a_cancelled_first_caller_does_not_restart_the_pack_index() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let _ = tokio::time::timeout(Duration::ZERO, svc.identities(TILE)).await;
        assert_eq!(svc.identities(TILE).await.len(), 2);
        assert_eq!(svc.test.index_builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_identity_calls_share_one_stat_pass() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let (a, b) = tokio::join!(svc.identities(TILE), svc.identities(TILE));
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(svc.test.identity_passes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_build_that_ends_without_an_answer_fails_its_tile() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        for (answered, tile) in [(true, (1, 1)), (false, (2, 2))] {
            svc.pending
                .insert(tile, futures::future::ready(None).boxed().shared());
            drop(PendingGuard {
                svc: Arc::clone(&svc),
                tile,
                answered,
            });
            assert!(!svc.pending.contains_key(&tile));
            assert_eq!(svc.failed.contains(&tile), !answered);
        }
    }

    /// Waits until no build is in flight.
    async fn builds_settled(svc: &RoadService) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !svc.pending.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_concurrent_callers_build_a_tile_once() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        let calls = (0..32).map(|i| {
            let s = Arc::clone(&svc);
            tokio::spawn(async move {
                if i % 2 == 0 {
                    s.ensure(TILE, Trigger::DsfOpen);
                }
                s.footprint(&[TILE], Purpose::Generate).await.complete
            })
        });
        for done in join_all(calls).await {
            assert!(done.unwrap());
        }
        builds_settled(&svc).await;
        assert_eq!(svc.stats_snapshot().tiles_ready, 1);
        assert!(svc.pending.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_concurrent_callers_fail_a_tile_once() {
        let t = install_with(high(None).bytes(), b"garbage".to_vec());
        let svc = RoadService::new(t.path().into(), vec![], None);
        let calls = (0..32).map(|i| {
            let s = Arc::clone(&svc);
            tokio::spawn(async move {
                if i % 2 == 0 {
                    s.ensure(TILE, Trigger::DsfOpen);
                }
                s.footprint(&[TILE], Purpose::Generate).await.complete
            })
        });
        for done in join_all(calls).await {
            assert!(!done.unwrap());
        }
        builds_settled(&svc).await;
        assert_eq!(svc.stats_snapshot().tiles_failed, 1);
        assert!(svc.pending.is_empty());
    }

    #[tokio::test]
    async fn the_summary_task_does_not_keep_the_service_alive() {
        let t = install(None);
        let svc = RoadService::new(t.path().into(), vec![], None);
        svc.ensure(TILE, Trigger::DsfOpen);
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert!(svc.summary_started.load(Ordering::Relaxed));
        let weak = Arc::downgrade(&svc);
        drop(svc);
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_none());
    }

    /// Measurements over the developer's X-Plane installation, which is
    /// only read; every test returns at once where it is absent. Release
    /// build, one test per process so memory readings are its own:
    /// `cargo test --release -p xearthlayer --lib --
    /// night::roads::service::tests::real::<name> --exact --ignored
    /// --nocapture`.
    pub(crate) mod real {
        use super::*;
        use crate::xworld_pro::SCENERY_PACKS_FILE;

        const INSTALL: &str = "/media/FlightSim/X-Plane 12-beta";
        const GENEVA: Tile = (46, 6);
        const TOKYO: Tile = (35, 139);
        /// No DSF covers it (South Pacific): readying it builds the pack
        /// index alone.
        const OCEAN: Tile = (-50, -130);

        /// Whether the installation is there; says so when it is not.
        fn installed() -> bool {
            let ini = Path::new(INSTALL)
                .join("Custom Scenery")
                .join(SCENERY_PACKS_FILE);
            let ok = ini.is_file();
            if !ok {
                println!("no X-Plane installation at {INSTALL}; skipped");
            }
            ok
        }

        /// XEL's own mount, which the service is told to leave out.
        fn xel_mounts() -> Vec<PathBuf> {
            vec![Path::new(INSTALL).join("Custom Scenery/zzXEL_ortho")]
        }

        /// Every simHeaven pack, and XEL's mount: what is left of a tile is
        /// its other packs and Global Scenery.
        fn without_simheaven() -> Vec<PathBuf> {
            let cs = Path::new(INSTALL).join("Custom Scenery");
            let mut skip = xel_mounts();
            for e in std::fs::read_dir(&cs).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with("simHeaven_") {
                    skip.push(cs.join(e.file_name()));
                }
            }
            skip
        }

        /// A service over the installation whose waits never cut a
        /// measurement short.
        fn service(skip: Vec<PathBuf>, disk: Option<Arc<dyn Cache>>) -> Arc<RoadService> {
            RoadService::with_config(
                INSTALL.into(),
                skip,
                disk,
                RoadServiceConfig {
                    probe_wait: Duration::from_secs(600),
                    generate_wait: Duration::from_secs(600),
                    ..RoadServiceConfig::default()
                },
            )
        }

        /// A disk tier in memory, large enough never to evict.
        fn memory_disk() -> Arc<dyn Cache> {
            Arc::new(crate::cache::MemoryCacheProvider::new(4 << 30, None))
        }

        /// Prints the service's `Night roads:` events (debug) to stdout.
        fn print_road_events() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter("xearthlayer::night::roads=debug")
                .with_target(false)
                .with_ansi(false)
                .without_time()
                .with_writer(std::io::stdout)
                .try_init();
        }

        /// `tiles` made ready for generation, and the wall time in ms. Every
        /// tile must be ready, or the numbers measure nothing.
        async fn ready(svc: &Arc<RoadService>, tiles: &[Tile]) -> (FootprintRoads, f64) {
            let started = Instant::now();
            let roads = svc.footprint(tiles, Purpose::Generate).await;
            assert!(roads.complete, "every tile ready");
            (roads, started.elapsed().as_secs_f64() * 1e3)
        }

        /// The merged roads of `tiles` from the installation (XEL's mount
        /// left out, no disk tier), or `None` without it.
        pub(crate) fn merged_roads(tiles: &[Tile]) -> Option<Vec<Arc<MergedRoads>>> {
            if !installed() {
                return None;
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let roads = rt.block_on(async {
                let svc = service(xel_mounts(), None);
                let roads = svc.footprint(tiles, Purpose::Generate).await;
                assert!(roads.complete, "every tile ready");
                roads.tiles
            });
            Some(roads)
        }

        fn mib(bytes: impl Into<f64>) -> String {
            format!("{:.1} MiB", bytes.into() / f64::from(1u32 << 20))
        }

        /// Resident memory of the process, in bytes.
        fn resident() -> f64 {
            memory_stats::memory_stats().map_or(f64::NAN, |s| s.physical_mem as f64)
        }

        /// The process's peak resident memory (Linux `VmHWM`), in bytes.
        fn peak_resident() -> f64 {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
                    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
                    Some(kb * 1024.0)
                })
                .unwrap_or(f64::NAN)
        }

        /// Readiness of Geneva and Tokyo with every installed pack, without
        /// and then with a disk tier, and the probe's crossing query on
        /// Tokyo's merged roads.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore]
        async fn tiles_ready_over_the_real_install() {
            if !installed() {
                return;
            }
            print_road_events();
            let index = tokio::task::spawn_blocking(|| {
                let started = Instant::now();
                let (idx, s) = PackIndex::build(Path::new(INSTALL), &xel_mounts());
                (s, idx.bytes(), started.elapsed().as_secs_f64() * 1e3)
            })
            .await
            .unwrap();
            println!(
                "pack index, built alone: {:?}, {} bytes ({}), {:.1} ms",
                index.0,
                index.1,
                mib(index.1 as f64),
                index.2
            );

            let mut tokyo = None;
            for (label, disk) in [("no disk tier", None), ("disk tier", Some(memory_disk()))] {
                let svc = service(xel_mounts(), disk.clone());
                let (_, ms) = ready(&svc, &[OCEAN]).await;
                println!("{label}: pack index through the service {ms:.1} ms");
                for tile in [GENEVA, TOKYO] {
                    let considered = svc.source_paths(tile).await.len();
                    let (roads, ms) = ready(&svc, &[tile]).await;
                    let r = &roads.tiles[0];
                    println!(
                        "{label}: tile {tile:?} cold {ms:.1} ms, {considered} sources, \
                         {} merged segments, {} removed, {} bytes ({})",
                        r.len(),
                        r.removed,
                        r.bytes(),
                        mib(r.bytes() as f64)
                    );
                    if tile == TOKYO {
                        tokyo = Some(Arc::clone(r));
                    }
                }
                if let Some(d) = &disk {
                    svc.writes_settled().await;
                    d.gc().await.unwrap();
                    println!(
                        "{label}: {} entries, {} bytes on the tier",
                        d.entry_count(),
                        d.size_bytes()
                    );
                }
                println!("{label}: {:?}", svc.stats_snapshot());
            }

            // The probe asks whether any road box meets a ZL16 footprint
            // grown by the glow reach; time it over random footprints.
            let tokyo = tokyo.unwrap();
            let boxes: Vec<_> = (0..10_000)
                .map(|i| {
                    use crate::night::generator::noise::{hash2, unit};
                    use crate::night::roads::field::{reach_deg, tests::name_at_zoom};
                    let lat = f64::from(TOKYO.0) + unit(hash2(i, 0, 1));
                    let lon = f64::from(TOKYO.1) + unit(hash2(i, 1, 1));
                    let n = name_at_zoom(lat, lon, 16);
                    let f = crate::night::Footprint::from_name(&n);
                    let r = reach_deg(&n, 1024, &crate::night::roads::RoadModel::DEFAULT);
                    (f.south - r, f.west - r, f.north + r, f.east + r)
                })
                .collect();
            let started = Instant::now();
            let crossed = boxes
                .iter()
                .filter(|&&(s, w, n, e)| std::hint::black_box(&tokyo).crosses(s, w, n, e))
                .count();
            let ns = started.elapsed().as_nanos() as f64 / boxes.len() as f64;
            println!(
                "crosses on Tokyo ({} merged segments): {ns:.0} ns per call, {crossed} of {} crossed",
                tokyo.len(),
                boxes.len()
            );
        }

        /// Geneva without its simHeaven packs falls back to the packs below
        /// and Global Scenery's 7z mesh: cold (decompression, a cache miss
        /// that writes the mesh's entry), then a second service reading the
        /// same disk tier (hits only).
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore]
        async fn global_scenery_fallback_for_geneva() {
            if !installed() {
                return;
            }
            print_road_events();
            let skip = without_simheaven();
            println!("{} packs left out", skip.len());
            let d = memory_disk();
            for pass in ["cold", "warm"] {
                let svc = service(skip.clone(), Some(Arc::clone(&d)));
                ready(&svc, &[OCEAN]).await;
                let sources = svc.source_paths(GENEVA).await;
                let (roads, ms) = ready(&svc, &[GENEVA]).await;
                let r = &roads.tiles[0];
                svc.writes_settled().await;
                let s = svc.stats_snapshot();
                println!(
                    "{pass}: {ms:.1} ms, {} sources, {} merged segments, {} bytes, \
                     cache hits {}, misses {}",
                    sources.len(),
                    r.len(),
                    r.bytes(),
                    s.cache_hits,
                    s.cache_misses
                );
                for path in &sources {
                    let key = cache_key(GENEVA, &path.to_string_lossy());
                    if let Some(entry) = d.get(&key).await.unwrap() {
                        println!("  entry {} for {}", entry.len(), path.display());
                    }
                }
                // Global Scenery is the last source, and it was read: only
                // a read 7z leaves an entry.
                let global = sources.last().expect("Geneva has sources");
                assert!(
                    global.starts_with(Path::new(INSTALL).join("Global Scenery")),
                    "{}",
                    global.display()
                );
                let key = cache_key(GENEVA, &global.to_string_lossy());
                assert!(d.get(&key).await.unwrap().is_some(), "the mesh was read");
                match pass {
                    "cold" => assert!(s.cache_misses > 0, "the cold pass decompressed"),
                    _ => assert_eq!(s.cache_misses, 0, "the warm pass read entries only"),
                }
            }
        }

        /// Returns freed memory the allocator still holds to the system, so
        /// what remains resident is live.
        fn trim() {
            #[cfg(all(target_os = "linux", target_env = "gnu"))]
            // SAFETY: `malloc_trim` only releases free memory.
            unsafe {
                libc::malloc_trim(0);
            }
        }

        /// Starts rayon's global pool. `run` has it running long before a
        /// road tile is asked for (the ortho union index and the cache
        /// scans use it), so its threads are not the pack index's cost.
        fn warm_rayon() {
            use rayon::prelude::*;
            let n: usize = (0..100_000usize)
                .into_par_iter()
                .map(|k| std::hint::black_box(vec![k as u8; 64]).len())
                .sum();
            std::hint::black_box(n);
        }

        /// Resident memory around building the pack index and readying the
        /// 3 x 3 tiles centred on `centre`, then after the service is
        /// dropped, and after the allocator gives back what it holds free.
        /// The allocator is configured as `run` configures it, and rayon's
        /// pool is already running.
        async fn memory_of_a_block(centre: Tile) {
            if !installed() {
                return;
            }
            crate::metrics::configure_allocator();
            warm_rayon();
            // And a blocking-pool thread, which `run` has too.
            tokio::task::spawn_blocking(|| ()).await.unwrap();
            let start = resident();
            let svc = service(xel_mounts(), None);
            ready(&svc, &[OCEAN]).await;
            let indexed = resident();
            let index = svc.index().await.bytes();
            let tiles: Vec<Tile> = (-1..=1)
                .flat_map(|a| (-1..=1).map(move |b| (centre.0 + a, centre.1 + b)))
                .collect();
            let (roads, ms) = ready(&svc, &tiles).await;
            let merged: usize = roads.tiles.iter().map(|r| r.bytes()).sum();
            svc.tiles.run_pending_tasks().await;
            let held = svc.tiles.weighted_size();
            let readied = resident();
            let peak = peak_resident();
            drop(roads);
            drop(svc);
            tokio::time::sleep(Duration::from_millis(200)).await;
            let dropped = resident();
            trim();
            let trimmed = resident();
            println!(
                "3 x 3 around {centre:?}: ready in {ms:.0} ms; merged {} (tile cache holds {}); \
                 pack index {}; resident: start {}, pack index +{}, tiles +{}, \
                 after drop +{} over start, after trim +{}; peak {}",
                mib(merged as f64),
                mib(held as f64),
                mib(index as f64),
                mib(start),
                mib(indexed - start),
                mib(readied - indexed),
                mib(dropped - start),
                mib(trimmed - start),
                mib(peak)
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore]
        async fn memory_of_a_3x3_block_around_geneva() {
            memory_of_a_block(GENEVA).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore]
        async fn memory_of_a_3x3_block_around_tokyo() {
            memory_of_a_block(TOKYO).await;
        }
    }
}
