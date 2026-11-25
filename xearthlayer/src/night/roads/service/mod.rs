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
//! Only overlay DSFs give networks and exclusion zones, as X-Plane shows
//! the networks of every overlay of a tile but of one base mesh only, the
//! highest priority one: XEL's own wherever lit textures are served, which
//! carries no roads and which the pack index leaves out. Any other base
//! mesh is read only to its header, and a tile no overlay gives a road has
//! no road light: see [`RoadService::merge_sources`].
//!
//! Callers wait per build, not per call: the first `footprint` of a tile's
//! build for a purpose sets that build's deadline, and no call waits past
//! it. X-Plane stats thousands of lit names, so a wait per call would
//! multiply. The wait's clock starts when the build gets its extract
//! permit, or at the first ask if that is later, so a build queued behind
//! others does not spend its callers' budget in the queue; a cap from the
//! first ask bounds the whole wait (#429). A tile rebuilt after eviction is
//! a new build with new deadlines.
//!
//! Blocking work is bounded by [`ROAD_EXTRACT_PERMITS`]: a build holds one
//! permit for its whole tile and no other lit permit, and releases it before
//! the detached write of its cache entries, so a queued build never waits
//! for another tile's disk writes. Each entry's write takes one of the lit
//! disk tier's permits, shared with the lit source, and gives it back before
//! the next, so a lit disk lookup waits for one entry's write, never a whole
//! tile's. At most [`ROAD_WRITE_BACKLOG`] tiles' entries are ever waiting in
//! memory: past it a tile's entries are dropped, and the next session
//! extracts them again.

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
use tokio::sync::{watch, OnceCell, Semaphore};
use tracing::{debug, warn};

use super::codec::{decode_roads, encode_roads};
use super::dsf::{read_dsf_open, DsfError, DsfFileStats, DsfRoads, ReadMode};
use super::is_lit_subtype;
use super::merge::{MergedRoads, Merger};
use super::packs::PackIndex;
use super::stats::{BuildRecord, Stats, WaitCounters};
use crate::cache::Cache;
use crate::night::{
    decode_entry_unsized, encode_entry, FileIdentity, LitDiskPermits, LitFingerprint,
};

pub use super::stats::StatsSnapshot;

/// Concurrent tile builds. The permits sit inside the blocking pool's
/// `BLOCKING_POOL_HEADROOM`, beside the lit ones; the write of a build's
/// cache entries takes a lit disk permit instead.
pub const ROAD_EXTRACT_PERMITS: usize = 2;
/// How long the lit probe of a tile's first footprint may wait for it once
/// its build holds an extract permit.
pub const ROAD_PROBE_WAIT: Duration = Duration::from_secs(2);
/// The longest the lit probes of a tile's build wait for it from the first
/// of them, its queue for an extract permit included: the longest stall a
/// `stat` of a lit name sees per tile.
pub const ROAD_PROBE_CAP: Duration = Duration::from_secs(5);
/// How long the lit generation of a tile's first footprint may wait for it.
/// It is also the generation's cap, so a generation's deadline is its first
/// ask plus this, the permit's start or not: a generation whose roads came
/// late takes them at its permit instead (#427).
pub const ROAD_GENERATE_WAIT: Duration = Duration::from_secs(10);
/// Tiles whose cache entries may wait for their write at once; a built
/// tile's entries past it are dropped (the next session extracts them
/// again). As many as there are builds, as when a build's permit bounded
/// its write.
pub const ROAD_WRITE_BACKLOG: usize = ROAD_EXTRACT_PERMITS;
/// Memory for merged tiles, weighted by [`MergedRoads::bytes`].
pub const ROAD_TILE_CACHE_BYTES: u64 = 128 * 1024 * 1024;
/// Stamped into every road disk cache entry and hashed into its key: bump
/// it when what one DSF's extraction keeps, or which DSFs are stored,
/// changes (the reader, the subtype filter, the codec's input or layout). Version 3
/// keeps the glowing classes only; version 2 also kept local streets,
/// minor roads and footways; version 1 also stored the base meshes a
/// fallback read. Version 4 stores no network definition name (the codec's
/// layout changed with it). Older keys are never looked up again. It guards
/// road entries only; lit payloads are guarded by `ROADS_VERSION`.
pub const ROAD_EXTRACT_VERSION: u32 = 4;
/// DSF-open hints are ignored while this many builds are pending, so a
/// burst of hints cannot queue ahead of the builds a probe or a generation
/// is waiting on. Builds those ask for are never refused.
pub const ROAD_HINT_BACKLOG: usize = 8;
/// Between two `Night roads: summary` lines.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

/// A 1 degree tile, `(lat, lon)` of its south-west corner.
type Tile = (i32, i32);

/// What asked for a tile first; logged with `Night roads: tile ready`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    DsfOpen,
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
    /// At least `probe_wait`.
    pub probe_cap: Duration,
    pub generate_wait: Duration,
    /// At least `generate_wait`.
    pub generate_cap: Duration,
    pub tile_cache_bytes: u64,
    /// Concurrent builds, 1 to [`ROAD_EXTRACT_PERMITS`] (clamped: the night
    /// permit budget has no room for more).
    pub extract_permits: usize,
    /// Tiles whose cache entries may wait for their write at once.
    pub write_backlog: usize,
    /// When set, every tile build first takes one permit of it, so a test
    /// decides when each build may run (`add_permits`).
    #[cfg(test)]
    pub gate: Option<Arc<Semaphore>>,
    /// When set, every tile build takes one permit of it once it holds its
    /// extract permit, so a test decides how long a build holds that.
    #[cfg(test)]
    pub hold: Option<Arc<Semaphore>>,
}

impl Default for RoadServiceConfig {
    fn default() -> Self {
        Self {
            probe_wait: ROAD_PROBE_WAIT,
            probe_cap: ROAD_PROBE_CAP,
            generate_wait: ROAD_GENERATE_WAIT,
            generate_cap: ROAD_GENERATE_WAIT,
            tile_cache_bytes: ROAD_TILE_CACHE_BYTES,
            extract_permits: ROAD_EXTRACT_PERMITS,
            write_backlog: ROAD_WRITE_BACKLOG,
            #[cfg(test)]
            gate: None,
            #[cfg(test)]
            hold: None,
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

    fn cap(&self, purpose: Purpose) -> Duration {
        let cap = match purpose {
            Purpose::Probe => self.probe_cap,
            Purpose::Generate => self.generate_cap,
        };
        cap.max(self.wait(purpose))
    }

    /// When callers of `purpose` stop waiting for a build first asked for
    /// at `asked`, which got its extract permit at `permitted` (`None`
    /// while it has none): the purpose's wait from the permit, or from the
    /// ask when that is later, and never past the cap from the ask.
    fn deadline(&self, purpose: Purpose, asked: Instant, permitted: Option<Instant>) -> Instant {
        let cap = asked + self.cap(purpose);
        match permitted {
            Some(at) => cap.min(at.max(asked) + self.wait(purpose)),
            None => cap,
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
        Ok(Self::from_metadata(path, &std::fs::metadata(path)?))
    }

    fn from_metadata(path: &Path, metadata: &std::fs::Metadata) -> Self {
        let id = FileIdentity::from_metadata(metadata);
        Self {
            path: path.to_string_lossy().into_owned(),
            len: id.len,
            mtime_nanos: id.mtime_nanos,
        }
    }
}

/// Opens the DSF at `path` and takes its identity from the open file (one
/// `fstat`), so the identity is that of the bytes read through the handle
/// even when the path is replaced meanwhile. Blocking.
fn open_source(path: &Path) -> std::io::Result<(std::fs::File, DsfIdentity)> {
    let file = std::fs::File::open(path)?;
    let id = DsfIdentity::from_metadata(path, &file.metadata()?);
    Ok((file, id))
}

/// The DSFs a tile build read its roads from, highest priority first,
/// each as it was when read; a source that was gone is left out (#430).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFrom {
    pub tile: Tile,
    pub dsfs: Vec<DsfIdentity>,
    /// False when a zone covering the tile stopped the build before its
    /// last source: the sources after `dsfs` were never read.
    pub whole: bool,
}

impl ReadFrom {
    /// Whether these are the DSFs `identities` names (an identity pass of
    /// the tile, [`RoadService::identities`]): the same DSFs in the same
    /// order, or, for a build a zone stopped, the DSFs it read followed by
    /// others, which it never reached and which cannot change its roads.
    pub fn agrees_with(&self, identities: &[DsfIdentity]) -> bool {
        if self.whole {
            self.dsfs == identities
        } else {
            identities.starts_with(&self.dsfs)
        }
    }
}

/// A ready tile: its merged roads and the DSFs they were read from.
#[derive(Debug, Clone)]
pub struct ReadyTile {
    pub roads: Arc<MergedRoads>,
    pub read_from: Arc<ReadFrom>,
}

impl ReadyTile {
    /// `roads` as if read from no DSF.
    #[cfg(test)]
    pub(crate) fn unread(roads: Arc<MergedRoads>) -> Self {
        let read_from = Arc::new(ReadFrom {
            tile: roads.tile,
            dsfs: Vec::new(),
            whole: true,
        });
        Self { roads, read_from }
    }
}

/// The merged roads of a footprint's tiles that are ready, in the order
/// asked.
#[derive(Debug, Clone, Default)]
pub struct FootprintRoads {
    pub tiles: Vec<Arc<MergedRoads>>,
    /// What each of `tiles` was read from, in the same order.
    pub read_from: Vec<Arc<ReadFrom>>,
    /// False when any tile is missing, for any reason.
    pub complete: bool,
    /// True when a tile was still building when its deadline passed. A
    /// failed tile is not a timeout: it has no roads this session.
    pub timed_out: bool,
}

/// Where a tile is at one instant, as [`RoadService::tiles_now`] finds it.
#[derive(Debug, Clone)]
pub enum TileNow {
    Ready(ReadyTile),
    /// A build is in flight.
    Building,
    /// No roads this session.
    Failed,
    /// Neither ready nor building: evicted, or never asked for.
    Absent,
}

/// How one tile of a footprint ended.
enum TileWait {
    Ready(ReadyTile),
    Failed,
    TimedOut,
}

/// A tile build's answer, awaited by any number of callers.
type Pending = Shared<BoxFuture<'static, Option<ReadyTile>>>;

/// A tile build in flight: its answer, and when it got its extract permit
/// (`None` until then), which starts the clock of its callers' waits.
#[derive(Clone)]
struct Build {
    roads: Pending,
    permitted: watch::Receiver<Option<Instant>>,
}

impl Build {
    /// A build that has answered `roads` and never got a permit.
    #[cfg(test)]
    fn finished(roads: Option<ReadyTile>) -> Self {
        Self {
            roads: futures::future::ready(roads).boxed().shared(),
            permitted: watch::channel(None).1,
        }
    }
}

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
    tiles: moka::future::Cache<Tile, ReadyTile>,
    /// Builds in flight, one per tile.
    pending: DashMap<Tile, Build>,
    /// Tiles with an unreadable DSF: no roads for the session.
    failed: DashSet<Tile>,
    /// When the first caller of each purpose asked for a tile's current
    /// build, which with the build's permit sets the purpose's deadline;
    /// cleared when a new build starts.
    first_asked: DashMap<(Tile, Purpose), Instant>,
    /// Tiles a probe has asked for, for the lead time: the first probe's
    /// instant while the tile is not ready yet, `None` once noted.
    probed: DashMap<Tile, Option<Instant>>,
    /// Every source's identity, taken once per tile; concurrent first
    /// callers share one pass. A failed pass is not kept.
    identities: DashMap<Tile, Arc<OnceCell<Arc<[DsfIdentity]>>>>,
    /// When each tile was first ready, for the lead time.
    ready_at: DashMap<Tile, Instant>,
    /// Held by a build.
    extract: Arc<Semaphore>,
    /// The lit disk tier's permits, given at construction and shared with
    /// the lit source; held by the write of a build's entries.
    disk_io: LitDiskPermits,
    /// Tiles whose entries are waiting for their write or being written.
    writes_pending: Arc<std::sync::atomic::AtomicUsize>,
    stats: Stats,
    summary_started: AtomicBool,
    #[cfg(test)]
    test: Arc<TestProbes>,
}

/// Counters only tests read.
#[cfg(test)]
#[derive(Default)]
struct TestProbes {
    writes_peak: std::sync::atomic::AtomicUsize,
    writes_dropped: std::sync::atomic::AtomicUsize,
    /// Road entry writes waiting for a lit disk permit.
    writes_awaiting_permit: std::sync::atomic::AtomicUsize,
    index_builds: std::sync::atomic::AtomicUsize,
    identity_passes: std::sync::atomic::AtomicUsize,
    leads_noted: std::sync::atomic::AtomicUsize,
    /// `start` marks its tile failed between its two checks, as a build
    /// failing on another thread at that moment would.
    fail_between_checks: AtomicBool,
}

/// A source that stopped a tile build.
struct Unreadable {
    path: String,
    error: String,
}

/// A tile's merge and what it cost.
struct Merged {
    roads: MergedRoads,
    /// The DSFs merged or passed over, as read.
    read_from: ReadFrom,
    /// Disk entries to write once the tile is known to be good.
    writes: Vec<(String, Bytes)>,
    /// Sources that existed and were reached.
    dsfs: usize,
    /// Overlays merged.
    overlays_read: usize,
    /// Base meshes passed over at their header.
    base_skipped: usize,
    /// Sources never reached because a zone covered the tile.
    skipped: usize,
}

/// One DSF read from its file, after it was offered to the merger.
struct Extracted {
    /// The file's identity, from the handle it was read through.
    read_id: DsfIdentity,
    stats: DsfFileStats,
    absorbed: Absorbed,
    /// A base mesh read only to its header.
    head_only: bool,
    segments_kept: usize,
    exclusions: usize,
    entry: Option<Bytes>,
    encode_us: u64,
}

/// What a source gave a tile's merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Absorbed {
    Merged,
    /// A base mesh, passed over.
    Base,
    /// A DSF whose header names another tile than the one it is filed as.
    Mismatch((i32, i32)),
}

/// Merges `roads`, a source of `tile`, when it is an overlay of the tile.
fn absorb(merger: &mut Merger, tile: Tile, roads: &DsfRoads) -> Absorbed {
    if roads.tile != tile {
        Absorbed::Mismatch(roads.tile)
    } else if !roads.overlay {
        Absorbed::Base
    } else {
        merger.add(roads);
        Absorbed::Merged
    }
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
    /// A service over the X-Plane installation at `install`, leaving out
    /// the packs in `skip`. `disk` is the lit disk tier (`None` when caching
    /// is off), whose writes take `disk_permits`: the session's one set,
    /// shared with the lit source.
    pub fn new(
        install: PathBuf,
        skip: Vec<PathBuf>,
        disk: Option<Arc<dyn Cache>>,
        disk_permits: LitDiskPermits,
    ) -> Arc<Self> {
        Self::with_config(
            install,
            skip,
            disk,
            disk_permits,
            RoadServiceConfig::default(),
        )
    }

    pub(crate) fn with_config(
        install: PathBuf,
        skip: Vec<PathBuf>,
        disk: Option<Arc<dyn Cache>>,
        disk_permits: LitDiskPermits,
        config: RoadServiceConfig,
    ) -> Arc<Self> {
        // LRU, not moka's default TinyLFU: admission by popularity would
        // reject a tile just built once the cache is full, and every request
        // for it would rebuild it.
        let tiles = moka::future::Cache::builder()
            .eviction_policy(EvictionPolicy::lru())
            .max_capacity(config.tile_cache_bytes)
            .weigher(|_, v: &ReadyTile| v.roads.bytes().try_into().unwrap_or(u32::MAX))
            .eviction_listener(|tile: Arc<Tile>, ready: ReadyTile, cause| {
                if cause == RemovalCause::Size {
                    debug!(
                        lat = tile.0,
                        lon = tile.1,
                        bytes_held = ready.roads.bytes(),
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
            first_asked: DashMap::new(),
            probed: DashMap::new(),
            identities: DashMap::new(),
            ready_at: DashMap::new(),
            extract: Arc::new(Semaphore::new(permits)),
            disk_io: disk_permits,
            writes_pending: Arc::default(),
            stats: Stats::default(),
            summary_started: AtomicBool::new(false),
            #[cfg(test)]
            test: Arc::default(),
        })
    }

    /// Starts building `tile` unless it is ready, failed or building.
    /// Returns at once; the build runs in its own task. A DSF-open hint is
    /// ignored while [`ROAD_HINT_BACKLOG`] builds are pending. A probe's
    /// call counts as the tile's first probe for the lead time, as a probe's
    /// `footprint` does.
    pub fn ensure(self: &Arc<Self>, tile: Tile, trigger: Trigger) {
        if trigger == Trigger::Probe {
            self.note_probe(tile, Instant::now());
        }
        if self.tiles.contains_key(&tile) {
            return;
        }
        if trigger == Trigger::DsfOpen && !self.pending.contains_key(&tile) {
            let pending = self.pending.len();
            if pending >= ROAD_HINT_BACKLOG {
                debug!(
                    lat = tile.0,
                    lon = tile.1,
                    pending,
                    "Night roads: hint ignored"
                );
                return;
            }
        }
        let _ = self.start(tile, trigger);
    }

    /// The ready roads of `tiles`, waiting for each tile at most until its
    /// build's deadline for `purpose` (set by the first call that waited on
    /// that build). Records the call's wait and timeout, and the lead time
    /// of each tile this call is the first probe of.
    pub async fn footprint(self: &Arc<Self>, tiles: &[Tile], purpose: Purpose) -> FootprintRoads {
        let started = Instant::now();
        if purpose == Purpose::Probe {
            for &tile in tiles {
                self.note_probe(tile, started);
            }
        }
        let found = join_all(tiles.iter().map(|&tile| async move {
            let (state, waited) = self.tile_by(tile, purpose, started).await;
            (tile, state, waited)
        }))
        .await;
        let now = Instant::now();
        let mut out = FootprintRoads {
            tiles: Vec::with_capacity(found.len()),
            read_from: Vec::with_capacity(found.len()),
            complete: true,
            timed_out: false,
        };
        let mut waited = false;
        for (_, state, tile_waited) in found {
            waited |= tile_waited;
            match state {
                TileWait::Ready(ready) => {
                    out.tiles.push(ready.roads);
                    out.read_from.push(ready.read_from);
                }
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
    /// taken once per tile per session. Metadata only; a file that is gone
    /// (NotFound) is left out. A DSF can still change while the session
    /// runs (a pack updated between flights, an overlay link rebuilt), so
    /// these need not be the DSFs a tile's roads were read from: the lit
    /// source writes a payload under them only when they agree with the
    /// build's [`ReadFrom`] (#430). `None` when the pass failed: a source whose `stat` failed for
    /// another reason, or a panic. A failed pass is not kept, so a later
    /// call tries again. The `stat`s run on the blocking pool under the
    /// caller's permit, if any: this takes none.
    pub async fn identities(self: &Arc<Self>, tile: Tile) -> Option<Arc<[DsfIdentity]>> {
        let cell = Arc::clone(&self.identities.entry(tile).or_default());
        let ids = cell
            .get_or_try_init(|| async {
                let paths = self.source_paths(tile).await;
                #[cfg(test)]
                self.test.identity_passes.fetch_add(1, Ordering::SeqCst);
                tokio::task::spawn_blocking(move || stat_all(&paths))
                    .await
                    .map_err(|e| e.to_string())?
                    .map(Arc::<[DsfIdentity]>::from)
                    .map_err(|e| e.to_string())
            })
            .await;
        match ids {
            Ok(ids) => Some(Arc::clone(ids)),
            Err(error) => {
                debug!(
                    lat = tile.0,
                    lon = tile.1,
                    error,
                    "Night roads: DSF identities unavailable for this call"
                );
                None
            }
        }
    }

    /// How each of `tiles` is now, in the order asked, from the tile cache,
    /// the builds in flight and the failed tiles. Starts no build, never
    /// waits and records no wait: a generation holding its permit looks
    /// once more at the tiles its wait left out (#427), and must not wait
    /// for roads while holding it.
    pub async fn tiles_now(&self, tiles: &[Tile]) -> Vec<TileNow> {
        let mut out = Vec::with_capacity(tiles.len());
        for &tile in tiles {
            let now = if let Some(ready) = self.tiles.get(&tile).await {
                TileNow::Ready(ready)
            } else if let Some(build) = self.pending.get(&tile).map(|b| b.roads.clone()) {
                match settled(&build) {
                    TileWait::Ready(ready) => TileNow::Ready(ready),
                    TileWait::Failed => TileNow::Failed,
                    TileWait::TimedOut => TileNow::Building,
                }
            } else if self.failed.contains(&tile) {
                TileNow::Failed
            } else {
                TileNow::Absent
            };
            out.push(now);
        }
        out
    }

    /// The lit source committed a dark answer for a footprint whose probe
    /// timed out: a late road could have lit it.
    pub fn note_dark_answer_before_roads(&self) {
        self.stats.record_dark_answer();
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// The lit disk tier's permits this service was given.
    pub(crate) fn disk_permits(&self) -> &LitDiskPermits {
        &self.disk_io
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

    /// A probe asked for `tile` at `at`. The tile's first probe notes its
    /// lead time: at once when the tile has been ready, else when its build
    /// makes it ready ([`Self::note_ready`]), so a negative lead is how long
    /// after the probe the roads came, whoever waited for them. A tile that
    /// never becomes ready notes none.
    fn note_probe(&self, tile: Tile, at: Instant) {
        // The entry's lock is held while `ready_at` is read: a build that
        // makes the tile ready meanwhile sets `ready_at` first, then waits
        // for this lock in `note_ready`, so the lead is noted exactly once.
        let Entry::Vacant(e) = self.probed.entry(tile) else {
            return;
        };
        let ready = self.ready_at.get(&tile).map(|r| *r);
        match ready {
            Some(ready) => {
                e.insert(None);
                self.note_lead(tile, at, ready);
            }
            None => {
                e.insert(Some(at));
            }
        }
    }

    /// The tile was ready at `ready` (its first time): notes the lead time
    /// of a probe that asked before.
    fn note_ready(&self, tile: Tile, ready: Instant) {
        let probed = match self.probed.get_mut(&tile) {
            Some(mut first) => first.take(),
            None => None,
        };
        if let Some(first_probe) = probed {
            self.note_lead(tile, first_probe, ready);
        }
    }

    /// Logs how long before (positive) or after (negative) its first probe
    /// at `first_probe` the tile was ready at `ready`.
    fn note_lead(&self, tile: Tile, first_probe: Instant, ready: Instant) {
        #[cfg(test)]
        self.test.leads_noted.fetch_add(1, Ordering::SeqCst);
        debug!(
            lat = tile.0,
            lon = tile.1,
            ms = lead_ms(first_probe, ready),
            "Night roads: lead time"
        );
    }

    /// How the tile ended for a caller that asked at `asked`, and whether
    /// the caller waited for it. Starts its build if needed, then waits
    /// until the build's deadline for `purpose`, which moves when the build
    /// gets its extract permit.
    async fn tile_by(
        self: &Arc<Self>,
        tile: Tile,
        purpose: Purpose,
        asked: Instant,
    ) -> (TileWait, bool) {
        if let Some(ready) = self.tiles.get(&tile).await {
            return (TileWait::Ready(ready), false);
        }
        let Some(build) = self.start(tile, purpose.trigger()) else {
            return (TileWait::Failed, false);
        };
        let first = *self.first_asked.entry((tile, purpose)).or_insert(asked);
        let mut permitted = build.permitted.clone();
        // False once the build has dropped its side: it has ended.
        let mut watching = true;
        let mut waited = false;
        loop {
            let at = *permitted.borrow_and_update();
            let deadline = self.config.deadline(purpose, first, at);
            let Some(until) = wait_until(deadline, Instant::now()) else {
                return (settled(&build.roads), waited);
            };
            waited = true;
            tokio::select! {
                biased;
                roads = build.roads.clone() => {
                    let state = match roads {
                        Some(roads) => TileWait::Ready(roads),
                        None => TileWait::Failed,
                    };
                    return (state, true);
                }
                () = tokio::time::sleep_until(until.into()) => {
                    return (settled(&build.roads), true);
                }
                changed = permitted.changed(), if watching && at.is_none() => {
                    watching = changed.is_ok();
                }
            }
        }
    }

    /// The deadline of `tile`'s current build for `purpose`, once a caller
    /// has asked for it.
    #[cfg(test)]
    fn deadline(&self, tile: Tile, purpose: Purpose) -> Option<Instant> {
        let first = *self.first_asked.get(&(tile, purpose))?;
        let at = self.pending.get(&tile).and_then(|b| *b.permitted.borrow());
        Some(self.config.deadline(purpose, first, at))
    }

    /// The build of `tile`, started unless one is in flight; `None` for a
    /// failed tile. The build is its own task, so dropping the returned
    /// future never cancels it. A new build clears the tile's deadlines,
    /// so its first caller sets fresh ones.
    fn start(self: &Arc<Self>, tile: Tile, trigger: Trigger) -> Option<Build> {
        if self.failed.contains(&tile) {
            return None;
        }
        self.start_summary();
        #[cfg(test)]
        if self.test.fail_between_checks.load(Ordering::SeqCst) {
            self.failed.insert(tile);
        }
        match self.pending.entry(tile) {
            Entry::Occupied(e) => Some(e.get().clone()),
            Entry::Vacant(e) => {
                // A build that failed after the check above marked the tile
                // before its guard removed the entry this lock now guards.
                if self.failed.contains(&tile) {
                    return None;
                }
                for purpose in [Purpose::Probe, Purpose::Generate] {
                    self.first_asked.remove(&(tile, purpose));
                }
                let (permitted_tx, permitted) = watch::channel(None);
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
                            builder.build_tile(tile, trigger, permitted_tx).await
                        })
                        .await;
                    guard.answered = true;
                    roads
                });
                let roads = async move { build.await.ok().flatten() }.boxed().shared();
                Some(e.insert(Build { roads, permitted }).clone())
            }
        }
    }

    /// Reads and merges the tile's sources under one extract permit, and
    /// says when it got it through `permitted`. The permit is released
    /// before the write of the tile's cache entries. Any unreadable source
    /// fails the tile for the session.
    async fn build_tile(
        self: Arc<Self>,
        tile: Tile,
        trigger: Trigger,
        permitted: watch::Sender<Option<Instant>>,
    ) -> Option<ReadyTile> {
        #[cfg(test)]
        if let Some(gate) = &self.config.gate {
            if let Ok(p) = gate.acquire().await {
                p.forget();
            }
        }
        let started = Instant::now();
        let sources = self.source_paths(tile).await;
        let asked = Instant::now();
        let permit = Arc::clone(&self.extract).acquire_owned().await.ok()?;
        permitted.send_replace(Some(Instant::now()));
        #[cfg(test)]
        if let Some(hold) = &self.config.hold {
            if let Ok(p) = hold.acquire().await {
                p.forget();
            }
        }
        let mut record = BuildRecord {
            permit_wait_us: micros(asked.elapsed()),
            ..BuildRecord::default()
        };
        let mut merged = self.merge_sources(tile, &sources, &mut record).await;
        drop(permit);
        let writes = match &mut merged {
            Ok(m) => std::mem::take(&mut m.writes),
            Err(_) => Vec::new(),
        };
        self.write_back(tile, writes);
        record.total_us = micros(started.elapsed());
        record.ready = merged.is_ok();
        self.stats.record_build(&record);
        match merged {
            Ok(m) => {
                let now = Instant::now();
                // `ready_at` before `probed` (see `note_probe`); its guard
                // is dropped first.
                let first = match self.ready_at.entry(tile) {
                    Entry::Vacant(e) => {
                        e.insert(now);
                        true
                    }
                    Entry::Occupied(_) => false,
                };
                if first {
                    self.note_ready(tile, now);
                }
                debug!(
                    lat = tile.0,
                    lon = tile.1,
                    trigger = ?trigger,
                    dsfs = m.dsfs,
                    overlays_read = m.overlays_read,
                    base_skipped = m.base_skipped,
                    cache_hits = record.cache_hits,
                    cache_misses = record.cache_misses,
                    merged_segments = m.roads.len(),
                    removed = m.roads.removed,
                    skipped_below_cover = m.skipped,
                    bytes = m.roads.bytes(),
                    total_ms = record.total_us / 1000,
                    "Night roads: tile ready"
                );
                Some(ReadyTile {
                    roads: Arc::new(m.roads),
                    read_from: Arc::new(m.read_from),
                })
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

    /// Feeds the tile's sources to a merger, each extraction dropped once
    /// merged (and encoded, for a 7z), so at most one is alive at a time.
    ///
    /// Every overlay is read, highest priority first, until a zone covers
    /// the tile. A base mesh stops at its header and gives nothing, wherever
    /// it ranks: X-Plane draws the network of the one base mesh it shows,
    /// XEL's own, which has none. A source whose header names another tile
    /// is skipped.
    async fn merge_sources(
        &self,
        tile: Tile,
        sources: &[PathBuf],
        record: &mut BuildRecord,
    ) -> Result<Merged, Unreadable> {
        let mut merger = Merger::new(tile);
        let mut writes = vec![];
        let mut read = Vec::with_capacity(sources.len());
        let (mut dsfs, mut reached, mut overlays_read, mut base_skipped) = (0, 0, 0, 0);
        for path in sources {
            reached += 1;
            let Some(stated) = self.identity_of(path, record).await? else {
                continue;
            };
            dsfs += 1;
            let (absorbed, id);
            (merger, absorbed, id) = self
                .take(merger, tile, path, &stated, record, &mut writes)
                .await?;
            read.push(id.clone());
            match absorbed {
                Absorbed::Merged => {
                    overlays_read += 1;
                    if merger.covered() {
                        break;
                    }
                }
                Absorbed::Base => base_skipped += 1,
                Absorbed::Mismatch(at) => note_mismatch(tile, &id, at),
            }
        }
        let (roads, cpu) = on_blocking_pool("merge", move || timed(|| merger.finish())).await?;
        record.cpu_us += cpu;
        Ok(Merged {
            roads,
            read_from: ReadFrom {
                tile,
                dsfs: read,
                whole: reached == sources.len(),
            },
            writes,
            dsfs,
            overlays_read,
            base_skipped,
            skipped: sources.len() - reached,
        })
    }

    /// The identity of a source, `None` when it is gone. Any other `stat`
    /// failure fails the tile.
    async fn identity_of(
        &self,
        path: &Path,
        record: &mut BuildRecord,
    ) -> Result<Option<DsfIdentity>, Unreadable> {
        let p = path.to_path_buf();
        let (id, cpu) = on_blocking_pool(&path.to_string_lossy(), move || {
            timed(|| DsfIdentity::of(&p))
        })
        .await?;
        record.cpu_us += cpu;
        match id {
            Ok(id) => Ok(Some(id)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Unreadable {
                path: path.to_string_lossy().into_owned(),
                error: e.to_string(),
            }),
        }
    }

    /// Offers one source to `merger`: from its stored extraction when the
    /// disk tier has one for the source as `id` (its `stat`) says it is,
    /// else from its file. Only an overlay's extraction is ever stored, so
    /// a base mesh costs a lookup that misses and a read of its header. A
    /// compressed overlay read from its file leaves its entry in `writes`.
    /// Also returns the identity of what was taken: `id` for a stored
    /// extraction, else that of the file read, taken from its handle, so a
    /// file replaced since `id` is never stamped with it (#430).
    async fn take(
        &self,
        mut merger: Merger,
        tile: Tile,
        path: &Path,
        id: &DsfIdentity,
        record: &mut BuildRecord,
        writes: &mut Vec<(String, Bytes)>,
    ) -> Result<(Merger, Absorbed, DsfIdentity), Unreadable> {
        let key = cache_key(tile, &id.path);
        let fp = entry_fingerprint(id);
        let lookup = Instant::now();
        let stored = match &self.disk {
            Some(disk) => disk
                .get(&key)
                .await
                .ok()
                .flatten()
                .and_then(|e| decode_entry_unsized(e, &fp)),
            None => None,
        };
        let read_ms = lookup.elapsed().as_millis() as u64;
        if let Some(payload) = stored {
            let (m, hit, decode_us) = on_blocking_pool(&id.path, move || {
                let t = Instant::now();
                let hit = decode_roads(&payload).map(|r| {
                    let absorbed = absorb(&mut merger, tile, &r);
                    (absorbed, r.chains.len())
                });
                (merger, hit, micros(t.elapsed()))
            })
            .await?;
            merger = m;
            record.cpu_us += decode_us;
            if let Some((absorbed, segments)) = hit {
                record.cache_hits += 1;
                debug!(
                    path = %id.path,
                    read_ms,
                    decode_ms = decode_us / 1000,
                    segments,
                    "Night roads: DSF cache hit"
                );
                return Ok((merger, absorbed, id.clone()));
            }
        }

        let cache = self.disk.is_some();
        let p = path.to_path_buf();
        let (m, read, read_us, cpu) = on_blocking_pool(&id.path, move || {
            let t = Instant::now();
            let read = open_source(&p)
                .map_err(|e| DsfError::Io(e.to_string()))
                .and_then(|(file, read_id)| {
                    read_dsf_open(file, is_lit_subtype, ReadMode::OverlaysOnly)
                        .map(|read| (read, read_id))
                });
            let read_us = micros(t.elapsed());
            let read = read.map(|((roads, stats), read_id)| {
                let head_only = !roads.overlay;
                let (payload, mut encode_us) = timed(|| {
                    (cache && stats.compressed && !head_only).then(|| encode_roads(&roads))
                });
                let absorbed = absorb(&mut merger, tile, &roads);
                let (segments_kept, exclusions) = (roads.chains.len(), roads.exclusions.len());
                drop(roads);
                let (entry, us) =
                    timed(|| payload.map(|p| encode_entry(&entry_fingerprint(&read_id), &p)));
                encode_us += us;
                Extracted {
                    read_id,
                    stats,
                    absorbed,
                    head_only,
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
        if cache && x.stats.compressed && !x.head_only {
            record.cache_misses += 1;
        }
        debug!(
            path = %id.path,
            file_bytes = x.stats.file_bytes,
            read_bytes = x.stats.read_bytes,
            compressed = x.stats.compressed,
            head_only = x.head_only,
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
        Ok((merger, x.absorbed, x.read_id))
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

    /// Writes a built tile's cache entries in one detached task, once the
    /// build's extract permit is released: a queued build never waits for a
    /// write. Each entry takes a lit disk permit of its own and gives it back
    /// before the next, so a lit disk lookup queued meanwhile waits for one
    /// entry's write, never for the rest of the tile's. At most
    /// `config.write_backlog` tiles' entries wait at once; past it the
    /// tile's entries are dropped, and the next session extracts them again.
    fn write_back(&self, tile: Tile, entries: Vec<(String, Bytes)>) {
        let Some(disk) = self.disk.clone().filter(|_| !entries.is_empty()) else {
            return;
        };
        let backlog = self.config.write_backlog;
        let admitted = self
            .writes_pending
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < backlog).then_some(n + 1)
            });
        if admitted.is_err() {
            #[cfg(test)]
            self.test.writes_dropped.fetch_add(1, Ordering::SeqCst);
            debug!(
                lat = tile.0,
                lon = tile.1,
                entries = entries.len(),
                backlog,
                "Night roads: road cache writes dropped; too many waiting"
            );
            return;
        }
        #[cfg(test)]
        if let Ok(before) = admitted {
            self.test
                .writes_peak
                .fetch_max(before + 1, Ordering::SeqCst);
        }
        // Counted until the task ends, however it ends.
        let slot = WriteSlot(Arc::clone(&self.writes_pending));
        let disk_io = self.disk_io.clone();
        #[cfg(test)]
        let test = Arc::clone(&self.test);
        tokio::spawn(async move {
            let _slot = slot;
            for (key, entry) in entries {
                #[cfg(test)]
                test.writes_awaiting_permit.fetch_add(1, Ordering::SeqCst);
                let permit = disk_io.acquire().await;
                #[cfg(test)]
                test.writes_awaiting_permit.fetch_sub(1, Ordering::SeqCst);
                let Some(_permit) = permit else {
                    return;
                };
                if let Err(e) = disk.set(&key, entry).await {
                    debug!(error = %e, key, "Night roads: road cache write failed");
                }
            }
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

    /// Builds in flight.
    #[cfg(test)]
    pub(crate) fn builds_pending(&self) -> usize {
        self.pending.len()
    }

    /// Drops a ready tile from the tile cache, as the LRU would.
    #[cfg(test)]
    pub(crate) async fn evict(&self, tile: Tile) {
        self.tiles.invalidate(&tile).await;
    }

    /// Free extract permits.
    #[cfg(test)]
    pub(crate) fn extract_permits_free(&self) -> usize {
        self.extract.available_permits()
    }

    #[cfg(test)]
    async fn writes_settled(&self) {
        while self.writes_pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

/// One tile's place in the write backlog, given back when it drops.
struct WriteSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for WriteSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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

/// How a waiter whose deadline has passed finds the tile: a build that
/// has just resolved is used, not reported as a timeout.
fn settled(pending: &Pending) -> TileWait {
    match pending.clone().now_or_never() {
        Some(Some(roads)) => TileWait::Ready(roads),
        Some(None) => TileWait::Failed,
        None => TileWait::TimedOut,
    }
}

/// The identities of `paths`, in order. A file that is gone is left out;
/// any other `stat` failure fails the pass. Blocking.
fn stat_all(paths: &[PathBuf]) -> std::io::Result<Vec<DsfIdentity>> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        match DsfIdentity::of(path) {
            Ok(id) => out.push(id),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// A source filed as `tile` whose header says `at`: it is skipped.
fn note_mismatch(tile: Tile, id: &DsfIdentity, at: (i32, i32)) {
    debug!(
        lat = tile.0,
        lon = tile.1,
        path = %id.path,
        dsf_lat = at.0,
        dsf_lon = at.1,
        "Night roads: DSF tile mismatch"
    );
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
/// first 8 bytes of the SHA-256 of [`ROAD_EXTRACT_VERSION`] and the path.
/// An entry of an earlier version is never looked up, so never loaded.
fn cache_key(tile: Tile, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(ROAD_EXTRACT_VERSION.to_le_bytes());
    h.update(path.as_bytes());
    let digest = h.finalize();
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
/// the roads came first, negative when the probe did.
fn lead_ms(first_probe: Instant, ready_at: Instant) -> i64 {
    match first_probe.checked_duration_since(ready_at) {
        Some(d) => d.as_millis() as i64,
        None => -(ready_at.duration_since(first_probe).as_millis() as i64),
    }
}

#[cfg(test)]
pub(crate) mod real;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cache::{GcResult, ServiceCacheError};
    use crate::night::roads::dsf::test_dsf::{install as write_install, sevenz, Builder};

    const TILE: (i32, i32) = (46, 6);

    /// An overlay DSF for `(lat, lon)`, as road packs ship them.
    fn overlay(lat: i32, lon: i32) -> Builder {
        let mut b = Builder::new(lat, lon);
        b.props.push(("sim/overlay".into(), "1".into()));
        b
    }

    fn high(zone: Option<&str>) -> Builder {
        let mut b = overlay(46, 6);
        if let Some(z) = zone {
            b.props.push(("sim/exclude_net".into(), z.into()));
        }
        b.road(46, 6, 100, &[(6.1, 46.1), (6.2, 46.2)]);
        b
    }

    fn low() -> Builder {
        let mut b = overlay(46, 6);
        b.road(46, 6, 30, &[(6.5, 46.5), (6.6, 46.6)]);
        b
    }

    /// A base mesh (not an overlay) for the tile with one road at `lon`.
    fn base_mesh(lon: f64) -> Builder {
        let mut b = Builder::new(46, 6);
        b.road(46, 6, 30, &[(lon, 46.3), (lon, 46.4)]);
        b
    }

    /// `dsf` with every byte after its HEAD atom replaced by garbage: it
    /// reads only as far as HEAD, so reading it further fails its tile.
    fn past_head_garbage(dsf: Vec<u8>) -> Vec<u8> {
        let head = 12 + u32::from_le_bytes(dsf[16..20].try_into().unwrap()) as usize;
        let mut out = dsf;
        out[head..].fill(0xFF);
        out
    }

    /// The tile's merged chains as (lon, lat) of their first point, sorted.
    fn starts(r: &MergedRoads) -> Vec<(i64, i64)> {
        let mut out = vec![];
        r.for_each_chain_near(46.0, 6.0, 47.0, 7.0, |c| {
            let (x, y) = c.points().next().unwrap();
            out.push(((x * 100.0).round() as i64, (y * 100.0).round() as i64));
        });
        out.sort();
        out
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

    /// A disk tier that records each key whose `get` returned an entry, and
    /// whose `set`s, with a gate, each wait for one of its permits.
    pub(super) struct Loads {
        inner: Arc<dyn Cache>,
        loaded: std::sync::Mutex<Vec<String>>,
        set_gate: Option<Arc<Semaphore>>,
        sets_waiting: std::sync::atomic::AtomicUsize,
        sets_done: std::sync::atomic::AtomicUsize,
    }

    impl Loads {
        pub(super) fn new(inner: Arc<dyn Cache>) -> Self {
            Self {
                inner,
                loaded: std::sync::Mutex::default(),
                set_gate: None,
                sets_waiting: Default::default(),
                sets_done: Default::default(),
            }
        }

        /// Each `set` waits for one permit of `gate` (`add_permits`).
        fn with_set_gate(inner: Arc<dyn Cache>, gate: Arc<Semaphore>) -> Self {
            Self {
                set_gate: Some(gate),
                ..Self::new(inner)
            }
        }

        pub(super) fn loaded(&self) -> Vec<String> {
            self.loaded.lock().unwrap().clone()
        }

        /// `set`s waiting at the gate.
        fn sets_waiting(&self) -> usize {
            self.sets_waiting.load(Ordering::SeqCst)
        }

        /// `set`s that have returned.
        fn sets_done(&self) -> usize {
            self.sets_done.load(Ordering::SeqCst)
        }
    }

    impl Cache for Loads {
        fn set(&self, key: &str, value: Bytes) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
            let key = key.to_owned();
            async move {
                if let Some(gate) = &self.set_gate {
                    self.sets_waiting.fetch_add(1, Ordering::SeqCst);
                    if let Ok(p) = gate.acquire().await {
                        p.forget();
                    }
                    self.sets_waiting.fetch_sub(1, Ordering::SeqCst);
                }
                let set = self.inner.set(&key, value).await;
                self.sets_done.fetch_add(1, Ordering::SeqCst);
                set
            }
            .boxed()
        }

        fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Bytes>, ServiceCacheError>> {
            let key = key.to_owned();
            async move {
                let got = self.inner.get(&key).await?;
                if got.is_some() {
                    self.loaded.lock().unwrap().push(key);
                }
                Ok(got)
            }
            .boxed()
        }

        fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
            self.inner.delete(key)
        }

        fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
            self.inner.contains(key)
        }

        fn contains_sync(&self, key: &str) -> bool {
            self.inner.contains_sync(key)
        }

        fn size_bytes(&self) -> u64 {
            self.inner.size_bytes()
        }

        fn entry_count(&self) -> u64 {
            self.inner.entry_count()
        }

        fn max_size_bytes(&self) -> u64 {
            self.inner.max_size_bytes()
        }

        fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
            self.inner.set_max_size(size_bytes)
        }

        fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>> {
            self.inner.gc()
        }
    }

    /// Waits of `probe_ms` and `generate_ms` whose caps are the waits: a
    /// deadline is then the first ask plus the wait, the permit's start or
    /// not.
    fn config(probe_ms: u64, generate_ms: u64) -> RoadServiceConfig {
        let (probe, generate) = (
            Duration::from_millis(probe_ms),
            Duration::from_millis(generate_ms),
        );
        RoadServiceConfig {
            probe_wait: probe,
            probe_cap: probe,
            generate_wait: generate,
            generate_cap: generate,
            ..RoadServiceConfig::default()
        }
    }

    /// Makes the deadline of `tile`'s build for `purpose` already past, as
    /// if its first caller had asked a whole cap ago.
    fn deadline_passed(svc: &RoadService, tile: Tile, purpose: Purpose) {
        let asked = Instant::now().checked_sub(svc.config.cap(purpose)).unwrap();
        svc.first_asked.insert((tile, purpose), asked);
    }

    /// `config` whose builds wait for the returned gate: each
    /// `add_permits(1)` lets one build run.
    fn gated(probe_ms: u64, generate_ms: u64) -> (RoadServiceConfig, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let c = RoadServiceConfig {
            gate: Some(Arc::clone(&gate)),
            ..config(probe_ms, generate_ms)
        };
        (c, gate)
    }

    /// A wait long enough that a build never outlasts it: a test that
    /// expects the roads waits this long and returns as soon as they are
    /// ready.
    const LONG: u64 = 10_000;

    /// A service that waits `LONG` for its roads, so a test that expects
    /// them does not depend on how fast the machine builds a tile.
    fn patient(install: &Path, disk: Option<Arc<dyn Cache>>) -> Arc<RoadService> {
        RoadService::with_config(
            install.into(),
            vec![],
            disk,
            LitDiskPermits::new(),
            config(LONG, LONG),
        )
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
        assert_eq!(lead_ms(t0 + ms(300), t0), 300);
        assert_eq!(lead_ms(t0, t0 + ms(250)), -250);
    }

    #[test]
    fn a_deadline_runs_from_the_builds_permit_within_the_cap() {
        let ms = Duration::from_millis;
        let c = RoadServiceConfig {
            probe_wait: ms(2_000),
            probe_cap: ms(5_000),
            ..config(LONG, LONG)
        };
        let asked = Instant::now();
        let deadline = |permitted| c.deadline(Purpose::Probe, asked, permitted);
        assert_eq!(deadline(None), asked + ms(5_000), "queued: the cap");
        assert_eq!(deadline(Some(asked + ms(1_000))), asked + ms(3_000));
        assert_eq!(
            deadline(Some(asked + ms(4_000))),
            asked + ms(5_000),
            "never past the cap"
        );
        let before = asked.checked_sub(ms(500)).unwrap();
        assert_eq!(
            deadline(Some(before)),
            asked + ms(2_000),
            "a permit before the ask: from the ask"
        );
        // A cap equal to the wait is the deadline before #429.
        let same = config(2_000, LONG);
        for permitted in [None, Some(before), Some(asked + ms(1_000))] {
            assert_eq!(
                same.deadline(Purpose::Probe, asked, permitted),
                asked + ms(2_000)
            );
        }
        // A cap below the wait is the wait.
        let low = RoadServiceConfig {
            probe_cap: ms(1),
            ..config(2_000, LONG)
        };
        assert_eq!(low.deadline(Purpose::Probe, asked, None), asked + ms(2_000));
        // The defaults: a probe capped at its cap, a generation as before.
        let d = RoadServiceConfig::default();
        assert_eq!(d.cap(Purpose::Probe), ROAD_PROBE_CAP);
        assert_eq!(d.cap(Purpose::Generate), ROAD_GENERATE_WAIT);
        assert!(ROAD_PROBE_CAP > ROAD_PROBE_WAIT);
    }

    #[test]
    fn purposes_have_their_own_waits_and_triggers() {
        let c = config(10, 20);
        assert_eq!(c.wait(Purpose::Probe), Duration::from_millis(10));
        assert_eq!(c.wait(Purpose::Generate), Duration::from_millis(20));
        assert!(matches!(Purpose::Probe.trigger(), Trigger::Probe));
        assert!(matches!(Purpose::Generate.trigger(), Trigger::Generate));
        let d = RoadServiceConfig::default();
        assert_eq!(d.probe_wait, ROAD_PROBE_WAIT);
        assert_eq!(d.generate_wait, ROAD_GENERATE_WAIT);
        assert_eq!(d.tile_cache_bytes, ROAD_TILE_CACHE_BYTES);
        assert_eq!(d.write_backlog, ROAD_WRITE_BACKLOG);
        assert!(d.gate.is_none() && d.hold.is_none());
    }

    // Merging.

    #[tokio::test]
    async fn merges_both_packs_without_exclusions() {
        let t = install(None);
        let svc = patient(t.path(), Some(disk()));
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
        let svc = patient(t.path(), Some(disk()));
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete, "the low DSF is never read");
        assert_eq!(r.tiles[0].len(), 1);
    }

    #[tokio::test]
    async fn a_tile_with_no_sources_is_complete_and_empty() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[(10, 10)], Purpose::Probe).await;
        assert!(r.complete);
        assert!(r.tiles[0].is_empty());
    }

    #[tokio::test]
    async fn a_source_removed_since_the_index_was_built_is_skipped() {
        let t = install(None);
        let svc = patient(t.path(), None);
        assert_eq!(svc.identities(TILE).await.unwrap().len(), 2, "index built");
        std::fs::remove_file(dsf(&t, "Low")).unwrap();
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete);
        assert_eq!(r.tiles[0].len(), 1);
    }

    // Overlays and base meshes.

    #[tokio::test]
    async fn overlays_give_the_roads_and_a_base_mesh_is_read_only_to_its_header() {
        // Past its HEAD the base mesh is garbage: reading on fails the tile.
        let t = tempfile::tempdir().unwrap();
        let base = past_head_garbage(base_mesh(6.8).bytes());
        write_install(
            t.path(),
            TILE,
            &[("Overlay", Some(high(None).bytes())), ("Mesh", Some(base))],
        );
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete, "the base mesh was not read past HEAD");
        assert_eq!(
            starts(&r.tiles[0]),
            vec![(610, 4610)],
            "the overlay's road only"
        );
    }

    #[tokio::test]
    async fn a_tile_whose_only_network_is_in_a_base_mesh_has_no_roads() {
        // Past their HEAD the base meshes are garbage, raw and 7z: reading
        // either further fails the tile.
        let mut no_network = overlay(46, 6);
        no_network
            .props
            .push(("sim/exclude_obj".into(), "6/46/7/47".into()));
        let mut unlit = overlay(46, 6);
        unlit.road(46, 6, 74, &[(6.1, 46.1), (6.2, 46.2)]);
        for o in [no_network, unlit] {
            let t = tempfile::tempdir().unwrap();
            write_install(
                t.path(),
                TILE,
                &[
                    ("Overlay", Some(o.bytes())),
                    ("MeshA", Some(past_head_garbage(base_mesh(6.7).bytes()))),
                    (
                        "MeshB",
                        Some(sevenz(&past_head_garbage(base_mesh(6.8).bytes()))),
                    ),
                ],
            );
            let svc = patient(t.path(), Some(disk()));
            let r = svc.footprint(&[TILE], Purpose::Probe).await;
            assert!(r.complete, "no base mesh was read past its HEAD");
            assert!(r.tiles[0].is_empty(), "{:?}", starts(&r.tiles[0]));
        }
    }

    #[tokio::test]
    async fn a_base_mesh_above_the_overlays_is_read_only_to_its_header() {
        let base = past_head_garbage(base_mesh(6.8).bytes());
        let t = tempfile::tempdir().unwrap();
        write_install(
            t.path(),
            TILE,
            &[("Mesh", Some(base)), ("Overlay", Some(low().bytes()))],
        );
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete, "the mesh was read only to its HEAD");
        assert_eq!(starts(&r.tiles[0]), vec![(650, 4650)], "the overlay's road");
    }

    #[tokio::test]
    async fn a_base_mesh_skipped_at_its_header_writes_no_entry() {
        let t = tempfile::tempdir().unwrap();
        write_install(
            t.path(),
            TILE,
            &[
                ("Overlay", Some(high(None).bytes())),
                ("Mesh", Some(sevenz(&base_mesh(6.8).bytes()))),
            ],
        );
        let d = disk();
        let svc = patient(t.path(), Some(d.clone()));
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert_eq!(entries(&svc, &d).await, 0);
        let s = svc.stats_snapshot();
        assert_eq!((s.cache_hits, s.cache_misses), (0, 0));
    }

    /// The key extraction version 1 gave a DSF's entry: the path's digest
    /// alone. That version also stored base meshes, for a fallback.
    pub(super) fn version_1_key(tile: Tile, path: &str) -> String {
        let digest = Sha256::digest(path.as_bytes());
        let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        format!("roads:{}:{}:{hex}", tile.0, tile.1)
    }

    #[tokio::test]
    async fn a_base_mesh_entry_from_an_earlier_session_is_never_loaded() {
        let t = tempfile::tempdir().unwrap();
        write_install(
            t.path(),
            TILE,
            &[
                ("Overlay", Some(sevenz(&high(None).bytes()))),
                (
                    "Mesh",
                    Some(sevenz(&past_head_garbage(base_mesh(6.8).bytes()))),
                ),
            ],
        );
        let mesh = dsf(&t, "Mesh").to_string_lossy().into_owned();
        let overlay = dsf(&t, "Overlay").to_string_lossy().into_owned();
        let d = Arc::new(Loads::new(disk()));
        d.set(&version_1_key(TILE, &mesh), Bytes::from_static(b"garbage"))
            .await
            .unwrap();
        for session in ["first", "second"] {
            let svc = patient(t.path(), Some(d.clone() as Arc<dyn Cache>));
            let r = svc.footprint(&[TILE], Purpose::Probe).await;
            assert!(r.complete, "{session}: the mesh was read only to its HEAD");
            assert_eq!(starts(&r.tiles[0]), vec![(610, 4610)], "{session}");
            svc.writes_settled().await;
        }
        assert_eq!(
            d.loaded(),
            vec![cache_key(TILE, &overlay)],
            "the overlay's entry, once; never the mesh's"
        );
    }
    #[tokio::test]
    async fn a_dsf_filed_under_another_tile_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let mut misnamed = overlay(47, 6);
        misnamed.road(47, 6, 100, &[(6.1, 47.1), (6.2, 47.2)]);
        write_install(
            t.path(),
            TILE,
            &[
                ("Wrong", Some(misnamed.bytes())),
                ("Low", Some(low().bytes())),
            ],
        );
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(r.complete, "not a failure");
        assert_eq!(
            starts(&r.tiles[0]),
            vec![(650, 4650)],
            "the right tile's roads only"
        );
    }

    // Hints.

    #[tokio::test]
    async fn dsf_hints_are_ignored_past_the_backlog_but_probes_are_not() {
        let t = install(None);
        let (c, _gate) = gated(30, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        // The gate stays shut, so every build stays pending.
        let tiles: Vec<Tile> = (0..ROAD_HINT_BACKLOG as i32 + 1).map(|k| (10, k)).collect();
        for &tile in &tiles {
            svc.ensure(tile, Trigger::DsfOpen);
        }
        assert_eq!(svc.pending.len(), ROAD_HINT_BACKLOG);
        let last = *tiles.last().unwrap();
        assert!(
            !svc.pending.contains_key(&last),
            "the hint past the backlog"
        );
        svc.ensure(tiles[0], Trigger::DsfOpen);
        assert_eq!(
            svc.pending.len(),
            ROAD_HINT_BACKLOG,
            "a pending tile is no new build"
        );
        let r = svc.footprint(&[last], Purpose::Probe).await;
        assert!(r.timed_out, "the probe started its build and waited");
        assert!(svc.pending.contains_key(&last));
    }

    // Failure.

    #[tokio::test]
    async fn an_unreadable_dsf_fails_the_whole_tile_and_writes_no_entry() {
        // The high DSF is a valid 7z, so it would be cached if the tile succeeded.
        let t = install_with(sevenz(&high(None).bytes()), b"garbage".to_vec());
        let d = disk();
        let svc = patient(t.path(), Some(d.clone()));
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
        let svc = patient(t.path(), None);
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
        let svc = patient(t.path(), None);
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
        let svc = patient(t.path(), Some(d.clone()));
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert_eq!(entries(&svc, &d).await, 0);
        let s = svc.stats_snapshot();
        assert_eq!((s.cache_hits, s.cache_misses), (0, 0));
    }

    #[tokio::test]
    async fn a_second_service_reads_a_compressed_extraction_from_the_disk_cache() {
        let t = install_with(sevenz(&high(None).bytes()), low().bytes());
        let d = disk();
        let a = patient(t.path(), Some(d.clone()));
        let first = a.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(a.stats_snapshot().cache_misses, 1);
        assert_eq!(entries(&a, &d).await, 1, "only the 7z DSF");
        let id = &a.identities(TILE).await.unwrap()[0];
        let entry = d.get(&cache_key(TILE, &id.path)).await.unwrap().unwrap();
        assert_eq!(&entry[..4], crate::cache::integrity::LIT_ENTRY_MAGIC);

        let b = patient(t.path(), Some(d));
        let second = b.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(b.stats_snapshot().cache_hits, 1);
        assert_eq!(b.stats_snapshot().cache_misses, 0);
        assert_eq!(second.tiles[0], first.tiles[0], "same merged roads");
    }

    #[tokio::test]
    async fn a_cache_entry_for_another_version_of_the_file_is_ignored() {
        let t = install_with(sevenz(&high(None).bytes()), low().bytes());
        let d = disk();
        let a = patient(t.path(), Some(d.clone()));
        a.footprint(&[TILE], Purpose::Probe).await;
        a.writes_settled().await;
        let mut changed = high(None);
        changed.road(46, 6, 100, &[(6.3, 46.3), (6.4, 46.4), (6.45, 46.45)]);
        std::fs::write(dsf(&t, "High"), sevenz(&changed.bytes())).unwrap();

        let b = patient(t.path(), Some(d));
        let r = b.footprint(&[TILE], Purpose::Probe).await;
        let s = b.stats_snapshot();
        assert_eq!((s.cache_hits, s.cache_misses), (0, 1));
        assert_eq!(r.tiles[0].len(), 3, "read from the new file");
    }

    // Identities.

    #[tokio::test]
    async fn identities_are_ordered_cached_and_change_with_the_file() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let a = svc.identities(TILE).await.unwrap();
        assert_eq!(a.len(), 2);
        assert!(a[0].path.contains("High"));
        assert!(a[1].path.contains("Low"));
        std::fs::write(dsf(&t, "Low"), b"changed length").unwrap();
        assert_eq!(
            svc.identities(TILE).await,
            Some(a.clone()),
            "cached for the session"
        );
        let fresh = patient(t.path(), None);
        let b = fresh.identities(TILE).await.unwrap();
        assert_ne!(a, b);
        assert_eq!(a[0], b[0]);
        assert!(fresh.identities((10, 10)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_source_gone_since_the_index_was_built_is_left_out_of_the_identities() {
        let t = install(None);
        let svc = patient(t.path(), None);
        assert!(svc.identities((10, 10)).await.is_some(), "index built");
        std::fs::remove_file(dsf(&t, "Low")).unwrap();
        let ids = svc.identities(TILE).await.unwrap();
        assert_eq!(ids.len(), 1);
        assert!(ids[0].path.contains("High"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identities_fail_when_a_source_cannot_be_stated() {
        use std::os::unix::fs::PermissionsExt;
        let t = install(None);
        let svc = patient(t.path(), None);
        assert!(svc.identities((10, 10)).await.is_some(), "index built");
        let block = dsf(&t, "Low").parent().unwrap().to_path_buf();
        let mode = |m| std::fs::set_permissions(&block, std::fs::Permissions::from_mode(m));
        mode(0o000).unwrap();
        if std::fs::metadata(dsf(&t, "Low")).is_ok() {
            mode(0o755).unwrap();
            return; // root: permissions do not apply
        }
        let failed = svc.identities(TILE).await;
        mode(0o755).unwrap();
        assert_eq!(failed, None, "not a tile without that DSF");
        assert_eq!(
            svc.identities(TILE).await.unwrap().len(),
            2,
            "a failed pass is not kept"
        );
    }

    // What a build read (#430).

    #[test]
    fn a_sources_identity_is_that_of_the_file_read_through_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("+46+006.dsf");
        let first = high(None).bytes();
        std::fs::write(&path, &first).unwrap();
        let (file, id) = open_source(&path).unwrap();
        // The path is replaced by another file between the open and the read.
        let mut other = overlay(46, 6);
        other.road(46, 6, 100, &[(6.3, 46.3), (6.4, 46.4)]);
        other.road(46, 6, 100, &[(6.5, 46.5), (6.6, 46.6), (6.7, 46.7)]);
        let replacement = dir.path().join("new.dsf");
        std::fs::write(&replacement, other.bytes()).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(
            DsfIdentity::of(&path).unwrap(),
            id,
            "the path names the new file"
        );
        assert_eq!(id.len, first.len() as u64, "the identity is the old file's");
        let (roads, _) = read_dsf_open(file, is_lit_subtype, ReadMode::OverlaysOnly).unwrap();
        assert_eq!(roads.chains.len(), 1, "and so are the roads read");
    }

    #[test]
    fn what_a_build_read_agrees_with_the_same_dsfs_or_a_prefix_it_stopped_at() {
        let id = |path: &str, len| DsfIdentity {
            path: path.into(),
            len,
            mtime_nanos: 1,
        };
        let (a, b, c) = (id("/a", 1), id("/b", 2), id("/c", 3));
        let read = |dsfs: &[&DsfIdentity], whole| ReadFrom {
            tile: TILE,
            dsfs: dsfs.iter().map(|&d| d.clone()).collect(),
            whole,
        };
        let pass = [a.clone(), b.clone(), c.clone()];
        assert!(read(&[&a, &b, &c], true).agrees_with(&pass));
        assert!(
            !read(&[&a, &b], true).agrees_with(&pass),
            "one appeared since"
        );
        assert!(read(&[&a, &b], false).agrees_with(&pass), "never reached c");
        assert!(
            !read(&[&a, &c], false).agrees_with(&pass),
            "b appeared since"
        );
        let changed = [a.clone(), id("/b", 9), c.clone()];
        assert!(!read(&[&a, &b, &c], true).agrees_with(&changed));
        assert!(!read(&[&a, &b], false).agrees_with(&changed));
        assert!(read(&[], true).agrees_with(&[]));
    }

    #[tokio::test]
    async fn a_build_records_what_it_read() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        let read = &r.read_from[0];
        assert_eq!(read.tile, TILE);
        assert!(read.whole);
        assert_eq!(read.dsfs[..], svc.identities(TILE).await.unwrap()[..]);

        // A zone over the whole tile stops the build at the high pack.
        let t = install(Some("6/46/7/47"));
        let svc = patient(t.path(), None);
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        let read = &r.read_from[0];
        let pass = svc.identities(TILE).await.unwrap();
        assert!(!read.whole);
        assert_eq!(read.dsfs.len(), 1);
        assert_eq!(pass.len(), 2);
        assert!(read.agrees_with(&pass));
    }

    // Coalescing, readiness and the cache.

    #[tokio::test]
    async fn concurrent_footprints_build_a_tile_once() {
        let t = install(None);
        let (c, gate) = gated(LONG, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        // Both callers are waiting on the build before the gate opens.
        let (a, b, ()) = tokio::join!(
            svc.footprint(&[TILE], Purpose::Probe),
            svc.footprint(&[TILE], Purpose::Generate),
            async { gate.add_permits(1) }
        );
        assert!(a.complete && b.complete);
        assert!(Arc::ptr_eq(&a.tiles[0], &b.tiles[0]));
        assert_eq!(svc.stats_snapshot().tiles_ready, 1);
    }

    #[tokio::test]
    async fn callers_that_stop_waiting_share_one_build_and_never_cancel_it() {
        let t = install(None);
        let (c, gate) = gated(30, 30);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        // The gate is shut: both callers give up on a build still waiting.
        assert!(!svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert!(!svc.footprint(&[TILE], Purpose::Generate).await.complete);
        svc.ensure(TILE, Trigger::DsfOpen);
        let a = svc.start(TILE, Trigger::Probe).unwrap();
        assert!(a
            .roads
            .ptr_eq(&svc.start(TILE, Trigger::Generate).unwrap().roads));
        assert_eq!(svc.pending.len(), 1, "one build in flight");
        gate.add_permits(1);
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
    async fn pending_cache_writes_never_exceed_the_backlog() {
        // A burst of 7z tiles queues on one permit; a finished tile's entry
        // must not wait in memory behind the rest of the burst.
        let t = tempfile::tempdir().unwrap();
        let tiles = [(46, 6), (47, 6), (48, 6), (49, 6), (50, 6)];
        for &(lat, lon) in &tiles {
            let mut b = overlay(lat, lon);
            let (x, y) = (f64::from(lon), f64::from(lat));
            b.road(lat, lon, 100, &[(x + 0.1, y + 0.1), (x + 0.2, y + 0.2)]);
            write_install(t.path(), (lat, lon), &[("High", Some(sevenz(&b.bytes())))]);
        }
        let d = disk();
        let c = RoadServiceConfig {
            extract_permits: 1,
            ..config(LONG, LONG)
        };
        let svc = RoadService::with_config(
            t.path().into(),
            vec![],
            Some(d.clone()),
            LitDiskPermits::new(),
            c,
        );
        for &tile in &tiles {
            svc.ensure(tile, Trigger::DsfOpen);
        }
        let r = svc.footprint(&tiles, Purpose::Generate).await;
        assert!(r.complete);
        assert_eq!(entries(&svc, &d).await, tiles.len() as u64, "all written");
        assert!(svc.test.writes_peak.load(Ordering::SeqCst) <= ROAD_WRITE_BACKLOG);
    }

    /// A 7z overlay with one motorway for each of `tiles`, as road packs
    /// ship them, so each build leaves an entry to write.
    fn sevenz_install(tiles: &[Tile]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for &(lat, lon) in tiles {
            let mut b = overlay(lat, lon);
            let (x, y) = (f64::from(lon), f64::from(lat));
            b.road(lat, lon, 100, &[(x + 0.1, y + 0.1), (x + 0.2, y + 0.2)]);
            write_install(t.path(), (lat, lon), &[("High", Some(sevenz(&b.bytes())))]);
        }
        t
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_tiles_cache_write_never_holds_a_build() {
        // #429: a build released its extract permit only once its entries
        // were written, so a build queued on that permit waited for
        // another tile's disk writes.
        let (u, t_) = ((46, 6), (47, 6));
        let t = sevenz_install(&[u, t_]);
        let gate = Arc::new(Semaphore::new(0));
        let d = Arc::new(Loads::with_set_gate(disk(), Arc::clone(&gate)));
        let c = RoadServiceConfig {
            extract_permits: 1,
            write_backlog: 1,
            ..config(LONG, LONG)
        };
        let svc = RoadService::with_config(
            t.path().into(),
            vec![],
            Some(d.clone() as _),
            LitDiskPermits::new(),
            c,
        );
        assert!(svc.footprint(&[u], Purpose::Generate).await.complete);
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.sets_waiting() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(d.sets_waiting(), 1, "U's write is blocked");
        // The bound is far below the gate's: only a build waiting for the
        // write could reach it.
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            svc.footprint(&[t_], Purpose::Generate),
        )
        .await
        .expect("T builds while U's write is blocked");
        assert!(r.complete);
        assert_eq!(d.sets_waiting(), 1, "U's write is still blocked");
        assert_eq!(
            svc.test.writes_dropped.load(Ordering::SeqCst),
            1,
            "T's entries: the backlog is full"
        );
        gate.add_permits(1);
        assert_eq!(entries(&svc, &(d.clone() as _)).await, 1, "U's entry");
        assert_eq!(svc.test.writes_peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn road_cache_writes_take_the_lit_disk_permits() {
        let tiles = [(46, 6), (47, 6)];
        let t = sevenz_install(&tiles);
        let gate = Arc::new(Semaphore::new(0));
        let d = Arc::new(Loads::with_set_gate(disk(), Arc::clone(&gate)));
        let permits = LitDiskPermits::new();
        let svc = RoadService::with_config(
            t.path().into(),
            vec![],
            Some(d.clone() as _),
            permits.clone(),
            config(LONG, LONG),
        );
        let disk_io = permits.semaphore();
        let all = disk_io.available_permits() as u32;
        assert_eq!(all as usize, crate::night::service::LIT_DISK_PERMITS);
        let held = disk_io.acquire_many(all).await.unwrap();
        assert!(svc.footprint(&tiles, Purpose::Generate).await.complete);
        until("both tiles' writes wait for a disk permit", || {
            svc.test.writes_awaiting_permit.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(d.sets_waiting() + d.sets_done(), 0, "no write without one");
        gate.add_permits(tiles.len());
        drop(held);
        assert_eq!(entries(&svc, &(d.clone() as _)).await, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lit_disk_lookup_waits_for_one_road_entry_not_a_whole_tile() {
        // Three 7z overlays: the tile's write has three entries.
        let t = tempfile::tempdir().unwrap();
        let mut third = overlay(46, 6);
        third.road(46, 6, 100, &[(6.8, 46.8), (6.9, 46.9)]);
        write_install(
            t.path(),
            TILE,
            &[
                ("High", Some(sevenz(&high(None).bytes()))),
                ("Low", Some(sevenz(&low().bytes()))),
                ("Third", Some(sevenz(&third.bytes()))),
            ],
        );
        let gate = Arc::new(Semaphore::new(0));
        let d = Arc::new(Loads::with_set_gate(disk(), Arc::clone(&gate)));
        let permits = LitDiskPermits::new();
        let svc = RoadService::with_config(
            t.path().into(),
            vec![],
            Some(d.clone() as _),
            permits.clone(),
            config(LONG, LONG),
        );
        // Other lit disk work holds every permit but one.
        let others = permits
            .semaphore()
            .acquire_many(crate::night::service::LIT_DISK_PERMITS as u32 - 1)
            .await
            .unwrap();
        assert!(svc.footprint(&[TILE], Purpose::Generate).await.complete);
        until("the tile's first entry is being written", || {
            d.sets_waiting() == 1
        })
        .await;
        assert_eq!(
            permits.semaphore().available_permits(),
            0,
            "it holds the last"
        );
        // A lit disk lookup queues for a permit behind the entry's write.
        let mut lookup = Box::pin(permits.acquire());
        assert!((&mut lookup).now_or_never().is_none(), "queued");
        gate.add_permits(1);
        let permit = tokio::time::timeout(Duration::from_secs(5), lookup)
            .await
            .expect("the lookup waits for one entry's write, not the tile's")
            .expect("a permit");
        assert_eq!(d.sets_done(), 1, "the tile's other entries wait for it");
        assert_eq!(d.sets_waiting(), 0);
        drop((permit, others));
        gate.add_permits(2);
        assert_eq!(entries(&svc, &(d.clone() as _)).await, 3);
    }

    #[tokio::test]
    async fn ensure_alone_makes_the_tile_ready() {
        let t = install(None);
        let svc = patient(t.path(), None);
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
        let mut h = overlay(47, 6);
        h.road(47, 6, 100, &[(6.1, 47.1), (6.2, 47.2)]);
        let mut l = overlay(47, 6);
        l.road(47, 6, 30, &[(6.5, 47.5), (6.6, 47.6)]);
        write_install(
            t.path(),
            other,
            &[("High", Some(h.bytes())), ("Low", Some(l.bytes()))],
        );
        let budget = budget_for(1);
        let c = RoadServiceConfig {
            tile_cache_bytes: budget,
            ..config(LONG, LONG)
        };
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
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
        let mut h = overlay(47, 6);
        h.road(47, 6, 100, &[(6.1, 47.1), (6.2, 47.2)]);
        write_install(t.path(), other, &[("High", Some(h.bytes()))]);
        let budget = budget_for(1);
        let c = RoadServiceConfig {
            tile_cache_bytes: budget,
            ..config(LONG, LONG)
        };
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
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

    // Per-build deadlines.

    #[tokio::test]
    async fn a_probe_after_the_first_probes_deadline_does_not_wait() {
        let t = install(None);
        let wait = Duration::from_millis(500);
        let (c, _gate) = gated(500, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        // The gate stays shut: the build outlasts every deadline.
        let first = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(!first.complete && first.timed_out);
        let deadline = svc.deadline(TILE, Purpose::Probe).unwrap();
        let started = Instant::now();
        let second = svc.footprint(&[TILE], Purpose::Probe).await;
        // A wait of its own would last the whole probe wait; the margin is
        // that wait, far above any scheduling delay.
        assert!(started.elapsed() < wait);
        assert_eq!(
            svc.deadline(TILE, Purpose::Probe).unwrap(),
            deadline,
            "the build's deadline, not a new one"
        );
        assert!(!second.complete);
        assert!(second.timed_out, "still building past its deadline");
        assert!(second.tiles.is_empty());
        assert_eq!(svc.stats_snapshot().probe_timeouts, 2);
    }

    #[tokio::test]
    async fn a_probe_during_the_build_waits_only_the_remaining_time() {
        let t = install(None);
        let wait = Duration::from_millis(1_000);
        let (c, _gate) = gated(1_000, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        let s = Arc::clone(&svc);
        let first = tokio::spawn(async move { s.footprint(&[TILE], Purpose::Probe).await });
        // The second probe asks 400 ms after the first: had it a wait of its
        // own, it would end 400 ms after the build's deadline.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let started = Instant::now();
        let second = svc.footprint(&[TILE], Purpose::Probe).await;
        let ended = Instant::now();
        let deadline = svc.deadline(TILE, Purpose::Probe).unwrap();
        assert!(!second.complete && second.timed_out);
        assert!(ended >= deadline, "waited until the build's deadline");
        assert!(ended < started + wait, "not a wait of its own");
        assert!(!first.await.unwrap().complete);
    }

    #[tokio::test]
    async fn probe_and_generate_deadlines_are_independent() {
        let t = install(None);
        let (c, gate) = gated(30, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        assert!(!svc.footprint(&[TILE], Purpose::Probe).await.complete);
        let (r, ()) = tokio::join!(svc.footprint(&[TILE], Purpose::Generate), async {
            gate.add_permits(1)
        });
        assert!(r.complete, "the generate wait is its own");
        assert!(
            svc.footprint(&[TILE], Purpose::Probe).await.complete,
            "ready now"
        );
    }

    #[tokio::test]
    async fn an_evicted_tile_asked_for_after_its_first_deadline_is_waited_for() {
        let t = install(None);
        let (c, gate) = gated(LONG, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        let (first, ()) = tokio::join!(svc.footprint(&[TILE], Purpose::Probe), async {
            gate.add_permits(1)
        });
        assert!(first.complete);
        // Its first build's deadline has passed, and the tile is evicted.
        deadline_passed(&svc, TILE, Purpose::Probe);
        svc.tiles.invalidate(&TILE).await;
        assert!(!svc.is_ready(TILE));
        let (again, ()) = tokio::join!(svc.footprint(&[TILE], Purpose::Probe), async {
            gate.add_permits(1)
        });
        assert!(
            again.complete && !again.timed_out,
            "the rebuild is waited for"
        );
        assert_eq!(again.tiles[0].len(), 2);
        assert_eq!(svc.stats_snapshot().tiles_ready, 2, "rebuilt");
    }

    #[tokio::test]
    async fn a_build_that_resolved_as_the_deadline_passed_is_not_a_timeout() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let tile = (10, 10);
        let roads = Arc::new(MergedRoads::empty(tile));
        let done = Some(ReadyTile::unread(Arc::clone(&roads)));
        // A finished build whose tile is not (or no longer) in the cache,
        // its deadline already past.
        svc.pending.insert(tile, Build::finished(done));
        deadline_passed(&svc, tile, Purpose::Probe);
        let r = svc.footprint(&[tile], Purpose::Probe).await;
        assert!(r.complete && !r.timed_out);
        assert!(Arc::ptr_eq(&r.tiles[0], &roads));
        assert_eq!(svc.stats_snapshot().probe_timeouts, 0);
    }

    #[tokio::test]
    async fn a_look_at_the_tiles_now_never_waits_and_starts_nothing() {
        let t = install_with(high(None).bytes(), low().bytes());
        let failing = (47, 6);
        write_install(t.path(), failing, &[("High", Some(b"garbage".to_vec()))]);
        let (c, gate) = gated(LONG, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        svc.ensure(TILE, Trigger::DsfOpen);
        assert_eq!(svc.builds_pending(), 1);
        let absent = (10, 10);
        // The gate is shut: a wait for TILE would last LONG.
        let now = tokio::time::timeout(Duration::from_secs(5), svc.tiles_now(&[TILE, absent]))
            .await
            .expect("no wait");
        assert!(
            matches!(now[..], [TileNow::Building, TileNow::Absent]),
            "{now:?}"
        );
        assert_eq!(
            svc.builds_pending(),
            1,
            "no build started for the absent tile"
        );
        assert_eq!(
            svc.stats_snapshot(),
            StatsSnapshot::default(),
            "no wait recorded"
        );

        gate.add_permits(2);
        until_ready(&svc, TILE).await;
        assert!(!svc.footprint(&[failing], Purpose::Generate).await.complete);
        let now = svc.tiles_now(&[TILE, failing, absent]).await;
        assert!(
            matches!(
                now[..],
                [TileNow::Ready(_), TileNow::Failed, TileNow::Absent]
            ),
            "{now:?}"
        );
    }

    #[test]
    fn a_settled_build_is_its_answer_and_a_running_one_a_timeout() {
        let roads = ReadyTile::unread(Arc::new(MergedRoads::empty(TILE)));
        let ready: Pending = futures::future::ready(Some(roads)).boxed().shared();
        assert!(matches!(settled(&ready), TileWait::Ready(_)));
        let failed: Pending = futures::future::ready(None).boxed().shared();
        assert!(matches!(settled(&failed), TileWait::Failed));
        let running: Pending = futures::future::pending().boxed().shared();
        assert!(matches!(settled(&running), TileWait::TimedOut));
    }

    // Instrumentation.

    #[tokio::test]
    async fn a_footprint_past_a_building_tiles_deadline_times_out() {
        let t = install(None);
        let (c, _gate) = gated(30, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
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
        let (c, _gate) = gated(LONG, 30);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        assert!(svc.footprint(&[TILE], Purpose::Generate).await.timed_out);
        let s = svc.stats_snapshot();
        assert_eq!((s.generate_waits, s.generate_timeouts), (1, 1));
        assert_eq!((s.probe_waits, s.probe_timeouts), (0, 0));
    }

    #[tokio::test]
    async fn a_wait_that_ends_ready_is_a_wait_and_no_timeout() {
        let t = install(None);
        let (c, gate) = gated(LONG, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        let (r, ()) = tokio::join!(svc.footprint(&[TILE], Purpose::Probe), async {
            gate.add_permits(1)
        });
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
        let svc = patient(t.path(), None);
        for _ in 0..2 {
            let r = svc.footprint(&[TILE], Purpose::Probe).await;
            assert!(!r.complete && !r.timed_out);
        }
        assert_eq!(svc.stats_snapshot().probe_timeouts, 0);
    }

    #[tokio::test]
    async fn dark_answers_before_roads_are_counted() {
        let t = install(None);
        let svc = patient(t.path(), None);
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
        let svc = patient(t.path(), None);
        let noted = || svc.test.leads_noted.load(Ordering::SeqCst);
        svc.footprint(&[TILE], Purpose::Generate).await;
        assert_eq!(noted(), 0, "not a probe");
        svc.footprint(&[TILE, (10, 10)], Purpose::Probe).await;
        assert_eq!(noted(), 2);
        svc.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(noted(), 2, "once per tile");
    }

    /// Waits until `done` holds, failing the test (naming `what`) if it does
    /// not within a generous bound.
    async fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Waits until `tile` is in the tile cache.
    async fn until_ready(svc: &RoadService, tile: Tile) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !svc.is_ready(tile) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(svc.is_ready(tile), "never ready");
    }

    #[tokio::test]
    async fn a_probe_that_only_starts_a_build_notes_the_lead_once_the_tile_is_ready() {
        // #428: a probe the recipe answers starts its tiles' builds and
        // waits for none; its tiles' lead times are still noted.
        let t = install(None);
        let (c, gate) = gated(LONG, LONG);
        let svc = RoadService::with_config(t.path().into(), vec![], None, LitDiskPermits::new(), c);
        let noted = || svc.test.leads_noted.load(Ordering::SeqCst);
        svc.ensure(TILE, Trigger::Probe);
        assert_eq!(svc.builds_pending(), 1, "the probe started the build");
        assert_eq!(noted(), 0, "not ready yet");
        gate.add_permits(1);
        until_ready(&svc, TILE).await;
        assert_eq!(noted(), 1, "noted when the build made it ready");
        svc.ensure(TILE, Trigger::Probe);
        svc.footprint(&[TILE], Purpose::Probe).await;
        assert_eq!(noted(), 1, "once per tile");

        // A tile ready before its first probe notes it at that probe.
        let other = (10, 10);
        gate.add_permits(1);
        svc.ensure(other, Trigger::DsfOpen);
        until_ready(&svc, other).await;
        assert_eq!(noted(), 1, "a hint is no probe");
        svc.ensure(other, Trigger::Probe);
        assert_eq!(noted(), 2);
    }

    #[tokio::test]
    async fn a_cancelled_first_caller_does_not_restart_the_pack_index() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let _ = tokio::time::timeout(Duration::ZERO, svc.identities(TILE)).await;
        assert_eq!(svc.identities(TILE).await.unwrap().len(), 2);
        assert_eq!(svc.test.index_builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_identity_calls_share_one_stat_pass() {
        let t = install(None);
        let svc = patient(t.path(), None);
        let (a, b) = tokio::join!(svc.identities(TILE), svc.identities(TILE));
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(svc.test.identity_passes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_build_that_ends_without_an_answer_fails_its_tile() {
        let t = install(None);
        let svc = patient(t.path(), None);
        for (answered, tile) in [(true, (1, 1)), (false, (2, 2))] {
            svc.pending.insert(tile, Build::finished(None));
            drop(PendingGuard {
                svc: Arc::clone(&svc),
                tile,
                answered,
            });
            assert!(!svc.pending.contains_key(&tile));
            assert_eq!(svc.failed.contains(&tile), !answered);
        }
    }

    #[tokio::test]
    async fn a_tile_that_fails_while_a_build_is_being_started_starts_none() {
        // The build failed after `start` found the tile not failed, and its
        // guard removed the pending entry: the re-check under the entry's
        // lock must answer failed rather than start a doomed build.
        let t = install(None);
        let svc = patient(t.path(), None);
        svc.test.fail_between_checks.store(true, Ordering::SeqCst);
        assert!(svc.start(TILE, Trigger::Probe).is_none());
        assert!(svc.pending.is_empty(), "no build started");
        let r = svc.footprint(&[TILE], Purpose::Probe).await;
        assert!(!r.complete && !r.timed_out, "failed, at once");
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
        let svc = patient(t.path(), None);
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
        let svc = patient(t.path(), None);
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
        let svc = patient(t.path(), None);
        svc.ensure(TILE, Trigger::DsfOpen);
        assert!(svc.footprint(&[TILE], Purpose::Probe).await.complete);
        assert!(svc.summary_started.load(Ordering::Relaxed));
        let weak = Arc::downgrade(&svc);
        drop(svc);
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_none());
    }
}
