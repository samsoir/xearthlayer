//! Where FUSE gets lit textures.
//!
//! The lit path never touches the ortho executor, coalescer or caches
//! (requirement R8). `BlackLitSource` (phase 1) serves the minimal black DDS
//! for every name; `NightLitSource` (phase 2a) serves full-size black
//! textures where the recipe and the user's art say there is light, and the
//! minimal DDS elsewhere. Phase 2b makes it generate the lit pixels: concurrent
//! reads of one footprint share a single generation, and the number of
//! concurrent generations follows the core count. With the cache layer's lit
//! disk tier, a payload is read from disk before generating and written back
//! after. The levels above the visible ones are black, the top X-Plane
//! uploads included: X-Plane's own objects light the ground there. With a
//! road service attached, roads light footprints too: the probe
//! counts a footprint whose glow reach holds a road as lit, and generation
//! rasters the roads into the texture. A source made shared
//! ([`NightLitSource::shared`]) also pre-generates every footprint its probe
//! commits lit, in the background, behind every read (#433). `LitStats`
//! counts what serving did and logs `Night lighting: summary` every minute.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use tracing::{debug, warn};

use super::admission::{GenerationClass, GenerationPermits, LoadPriority};
use super::disk::TileDsfs;
use super::generator::field::{RecipeField, FIELD_MARGIN_DEG};
use super::generator::{
    CpuLitGenerator, LedModel, LedRegion, LitGenerator, LitRequest, RecipeLookup,
};
use super::layout::minimal_black_dds;
use super::payloads::{LitMemoryBudget, PayloadMemory, Uncached, Wanted};
use super::pregen::{pregen_bound, pregen_workers, PregenQueue, Queued};
use super::retry::{with_retries, READ_RETRIES, READ_RETRY_DELAY};
use super::roads::field::{crosses_footprint, reach_deg, RoadField};
use super::roads::merge::MergedRoads;
use super::roads::service::{
    FootprintRoads, Purpose, ReadFrom, ReadyTile, RoadService, TileNow, Trigger,
};
use super::roads::RoadModel;
use super::stats::{
    DiskMiss, GenerationRecord, LitStats, PayloadLookup, PregenOutcome, ProbeAnswer, ProbeRecord,
    ReadOutcome, WriteBackSkip,
};
use super::{
    check_entry, entry_header, footprint_fingerprint, lit_cache_key, session_fingerprint, ArtDoubt,
    ArtRecord, FileIdentity, Footprint, LaminarArt, LitFingerprint, LitLayout, LitParts,
    PayloadPart, Recipe, RecipeStore, TextureSettingSource,
};
use crate::cache::Cache;
use crate::fuse::DdsFilename;

/// The source of lit DDS bytes for FUSE.
///
/// `size` must equal the length `read` serves for the same name (R7), and
/// neither may fail: every failure is a black texture (R4, R19).
pub trait LitTextureSource: Send + Sync {
    /// The file size for `name`, known before any generation.
    fn size<'a>(&'a self, name: &'a DdsFilename) -> BoxFuture<'a, u64>;

    /// Up to `len` bytes from `offset`; empty at or past the end.
    fn read<'a>(&'a self, name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes>;

    /// X-Plane looked up the DSF of the 1 degree tile `(lat, lon)`: a hint
    /// to get ready for its lit names. Must not block.
    fn dsf_seen(&self, _lat: i32, _lon: i32) {}

    /// Whether `dsf_seen` does anything. The filesystem reads this once and
    /// skips its DSF test on every lookup and open when it is `false`.
    fn wants_dsf_hints(&self) -> bool {
        false
    }

    /// The session is ending: stop background work (pre-generation, #433)
    /// without waiting for it. Reads are still answered. Must not block.
    fn shutdown(&self) {}
}

/// Serves the minimal black DDS for every lit name.
#[derive(Debug, Default, Clone, Copy)]
pub struct BlackLitSource;

impl LitTextureSource for BlackLitSource {
    fn size<'a>(&'a self, _name: &'a DdsFilename) -> BoxFuture<'a, u64> {
        Box::pin(async { minimal_black_dds().len() as u64 })
    }

    fn read<'a>(&'a self, _name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes> {
        Box::pin(async move {
            let dds = minimal_black_dds();
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(dds.len());
            let end = start.saturating_add(len as usize).min(dds.len());
            dds.slice(start..end)
        })
    }
}

/// Concurrent `has_light` probes (recipe and Laminar file reads). The permits
/// sit inside the blocking pool's `BLOCKING_POOL_HEADROOM`.
const LIT_PROBE_PERMITS: usize = 2;

/// Concurrent lit disk tier work: fingerprinting (art walk and recipe stats)
/// with the read of an entry, or the write-back of a generated one, or of one
/// of a road tile's extractions. The permits sit inside the blocking pool's
/// `BLOCKING_POOL_HEADROOM`, counted once: there is one [`LitDiskPermits`]
/// per session.
pub(crate) const LIT_DISK_PERMITS: usize = 2;

/// Most lit write-backs pending at once, waiting for a disk permit or
/// writing (#432). Each holds its payload until written, and reads that
/// miss memory share the disk permits with them; past the bound a
/// write-back is dropped (and counted): the payload stays in memory, and a
/// later session generates it again. A flight's scene loads peaked at 3.
pub(crate) const MAX_PENDING_WRITE_BACKS: u64 = 16;

/// The lit disk tier's [`LIT_DISK_PERMITS`] permits. Made once where the lit
/// source and the road service are built, and handed to both
/// ([`NightLitSource::new`], [`RoadService::new`]), so the tier's work, the
/// road entries' writes included, stays within the permits the night budget
/// counts; neither makes nor replaces them. A clone shares the permits.
#[derive(Clone, Debug)]
pub struct LitDiskPermits(Arc<tokio::sync::Semaphore>);

impl LitDiskPermits {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(LIT_DISK_PERMITS)))
    }

    /// One permit, `None` once the permits are closed (never, today).
    pub(crate) async fn acquire(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.0.acquire().await.ok()
    }

    /// Whether `other` is these permits, not another set.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// `n` permits instead of [`LIT_DISK_PERMITS`], for tests that need
    /// more work in flight than the tier allows.
    #[cfg(test)]
    pub(crate) fn with_permits(n: usize) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(n)))
    }

    /// The permits themselves, for tests that hold or count them.
    #[cfg(test)]
    pub(crate) fn semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.0
    }
}

impl Default for LitDiskPermits {
    fn default() -> Self {
        Self::new()
    }
}

/// Zoom levels a lit footprint is computed for. `parse_dds_filename` accepts
/// any two-digit zoom and `Footprint::from_name` shifts by it, so anything
/// outside this range is dark without computing anything.
const SUPPORTED_LIT_ZOOMS: std::ops::RangeInclusive<u8> = 12..=22;

/// Fewest and most concurrent lit generations (CPU: about a million texels
/// plus a BC1 encode each).
const MIN_LIT_GENERATION_PERMITS: usize = 2;
const MAX_LIT_GENERATION_PERMITS: usize = 4;

// Lit work (probes, generations, disk tier with the road entries' writes,
// road extraction) runs on the blocking pool inside its headroom (R17);
// night may use a third of it so it cannot starve other blocking work. The
// night share is fully used (10 of 10): a new night permit needs a smaller
// one elsewhere or a larger headroom. The disk permits count once because
// the source and the road service share one `LitDiskPermits`.
const _: () = assert!(
    LIT_PROBE_PERMITS
        + MAX_LIT_GENERATION_PERMITS
        + LIT_DISK_PERMITS
        + super::roads::service::ROAD_EXTRACT_PERMITS
        <= crate::executor::BLOCKING_POOL_HEADROOM / 3
);

/// Concurrent lit generations for a machine with `cores` cores: an eighth of
/// them, 2 to 4.
pub fn lit_generation_permits(cores: usize) -> usize {
    (cores / 8).clamp(MIN_LIT_GENERATION_PERMITS, MAX_LIT_GENERATION_PERMITS)
}

/// A recipe that could not be read, even after retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unreadable;

/// Recipe reads for the lit path: retried, and given up on for the session
/// once the retries run out.
///
/// `RecipeStore` caches no I/O error, so each attempt here really rereads.
/// Once a tile's retries are exhausted it is remembered here, and later
/// reads of it fail at once: the retries, their delay and the warning cost
/// once per tile, not once per footprint.
struct SessionRecipes {
    store: Arc<RecipeStore>,
    given_up: DashSet<(i32, i32)>,
}

impl SessionRecipes {
    fn new(store: Arc<RecipeStore>) -> Self {
        Self {
            store,
            given_up: DashSet::new(),
        }
    }

    /// The tile's recipe (`None` for none) and the region of the package it
    /// came from, or `Unreadable`. Blocking I/O.
    fn get(&self, lat: i32, lon: i32) -> Result<(Option<Arc<Recipe>>, LedRegion), Unreadable> {
        if self.given_up.contains(&(lat, lon)) {
            return Err(Unreadable);
        }
        with_retries(READ_RETRIES, READ_RETRY_DELAY, || {
            self.store.get_with_region(lat, lon)
        })
        .map_err(|e| {
            if self.given_up.insert((lat, lon)) {
                warn!(lat, lon, error = %e,
                      "Night lighting: recipe unreadable after retries; tile stays dark this session");
            }
            Unreadable
        })
    }

    /// Identity of the recipe file the tile's recipe was loaded from (`None`
    /// for none) and the region of its package, or `Unreadable` when it
    /// cannot be loaded. Not retried: a failure only means no disk tier for
    /// this payload, and generation retries. Blocking I/O on a cache miss.
    fn identity(
        &self,
        lat: i32,
        lon: i32,
    ) -> Result<(Option<FileIdentity>, LedRegion), Unreadable> {
        if self.given_up.contains(&(lat, lon)) {
            return Err(Unreadable);
        }
        self.store
            .identity_with_region(lat, lon)
            .map_err(|_| Unreadable)
    }

    /// The identity and region of each recipe of `name`'s field tiles (see
    /// [`Self::identity`]), or `Unreadable`. Blocking I/O on a store miss.
    fn identities(&self, name: &DdsFilename) -> Result<Vec<TileRecipeIdentity>, Unreadable> {
        field_tiles(name)
            .into_iter()
            .map(|(lat, lon)| {
                let (identity, region) = self.identity(lat, lon)?;
                Ok(((lat, lon), identity, region))
            })
            .collect()
    }

    /// [`Self::identities`] without I/O, when the store holds every one of
    /// the tiles already (the probe and earlier generations loaded them):
    /// `None` when it does not, and the blocking call is needed. A tile
    /// given up on is `Unreadable` here too.
    fn identities_held(
        &self,
        name: &DdsFilename,
    ) -> Option<Result<Vec<TileRecipeIdentity>, Unreadable>> {
        let mut held = Vec::new();
        for (lat, lon) in field_tiles(name) {
            if self.given_up.contains(&(lat, lon)) {
                return Some(Err(Unreadable));
            }
            let (identity, region) = self.store.loaded_identity_with_region(lat, lon)?;
            held.push(((lat, lon), identity, region));
        }
        Some(Ok(held))
    }

    /// Every recipe the generator can read for `name`, or `Unreadable` when
    /// one of them cannot be read. Blocking I/O.
    fn for_footprint(&self, name: &DdsFilename) -> Result<FootprintRecipes, Unreadable> {
        field_tiles(name)
            .into_iter()
            .map(|(lat, lon)| Ok(((lat, lon), self.get(lat, lon)?)))
            .collect()
    }
}

/// A field tile, the identity of its recipe file (`None` for none) and the
/// region of the package it came from.
type TileRecipeIdentity = ((i32, i32), Option<FileIdentity>, LedRegion);

/// The tiles whose recipes the generator can read for `name`.
fn field_tiles(name: &DdsFilename) -> Vec<(i32, i32)> {
    Footprint::from_name(name)
        .expanded(FIELD_MARGIN_DEG)
        .tiles()
}

/// The tiles whose roads can light `name` at `size` texels: its footprint
/// grown by the glow's reach, never by less than the recipe field's margin.
/// The probe, the generation and the fingerprint all ask for these.
pub(crate) fn road_tiles(name: &DdsFilename, size: u32, model: &RoadModel) -> Vec<(i32, i32)> {
    let margin = FIELD_MARGIN_DEG.max(reach_deg(name, size, model));
    Footprint::from_name(name).expanded(margin).tiles()
}

/// The recipes of the tiles a footprint's field covers, each with the region
/// of the package it came from.
type FootprintRecipes = HashMap<(i32, i32), (Option<Arc<Recipe>>, LedRegion)>;

/// Between two `Night lighting: summary` lines.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

/// Default overall brightness (`night.brightness`).
pub const DEFAULT_NIGHT_BRIGHTNESS: f64 = 1.0;

/// The lit probe's first step (#428): whether `footprint`'s recipes, read
/// through `lookup` (`is_lit` naming the lit terrains), light it. A
/// footprint they light is answered without waiting for its roads.
fn recipe_lights(
    footprint: &Footprint,
    lookup: RecipeLookup<'_>,
    is_lit: impl Fn(&str) -> bool,
) -> bool {
    RecipeField::new(footprint, lookup).has_light(is_lit)
}

/// The lit probe's answer for `footprint`, given its recipe step: lit by
/// its recipes when `recipe_lit`, the roads not consulted (#428); else by
/// a road of `road_tiles` (those ready when the probe's road wait ended)
/// within `reach` degrees of it, across the antimeridian too
/// ([`crosses_footprint`], over the raster's own box); else dark.
fn probe_answer(
    recipe_lit: bool,
    footprint: &Footprint,
    road_tiles: &[Arc<MergedRoads>],
    reach: f64,
) -> ProbeAnswer {
    if recipe_lit {
        ProbeAnswer::Recipe
    } else if road_tiles
        .iter()
        .any(|roads| crosses_footprint(roads, footprint, reach))
    {
        ProbeAnswer::Road
    } else {
        ProbeAnswer::Dark
    }
}

/// The night lit source: full-size, correctly laid out textures where the
/// recipe and the user's art say there is light (the visible levels
/// generated on the first read that reaches them, coalesced across
/// concurrent readers, with permits following the core count, and every
/// level above them black, the top X-Plane uploads included); the minimal
/// black DDS everywhere else.
pub struct NightLitSource {
    recipes: Arc<SessionRecipes>,
    art: Arc<LaminarArt>,
    setting: Arc<dyn TextureSettingSource>,
    /// Session-stable answers. Never evicts: an evicted entry could be
    /// re-probed to a different answer, and a lit file's size must not change
    /// while the kernel may hold the old one (R7). One byte-sized entry per
    /// footprint ever asked about.
    lit: DashMap<(u32, u32, u8), bool>,
    probes: tokio::sync::Semaphore,
    /// Generated visible levels by footprint, the first visible level and
    /// the levels below it apart (#432). In an `Arc` so the summary task can
    /// hold it weakly.
    payloads: Arc<PayloadMemory>,
    /// Admitted by priority (#433): reads of the first visible level, then
    /// reads of the small levels, then pre-generations.
    generations: GenerationPermits,
    /// Footprints committed lit, waiting for pre-generation (#433).
    pregens: PregenQueue,
    /// `true` while ortho demand is in flight (the prefetch lane's pause
    /// state): pre-generation starts nothing while it is (R17). `None`
    /// pre-generates regardless.
    demand: Option<tokio::sync::watch::Receiver<bool>>,
    /// This source, once made shared ([`Self::shared`]): what the
    /// pre-generation workers hold between footprints, so they never keep
    /// the source alive. Dangling (no pre-generation) until then.
    this: Weak<NightLitSource>,
    brightness: f64,
    generator: Arc<dyn LitGenerator>,
    /// Lit payloads on disk, from the cache layer; None when caching is off.
    disk: Option<Arc<dyn Cache>>,
    /// The lit disk tier's permits, given at construction: shared with the
    /// detached write-backs, which hold a permit too, and with the road
    /// service's writes.
    disk_io: LitDiskPermits,
    /// Fingerprint of the session's inputs (generator, brightness, art,
    /// road model), computed on the first payload, after the builders have
    /// run.
    session: tokio::sync::OnceCell<[u8; 32]>,
    /// Road lights; `None` leaves every answer and payload as without them.
    roads: Option<Arc<RoadService>>,
    road_model: RoadModel,
    /// Shared with the payload cache's eviction listener and the
    /// write-backs; the summary task holds it weakly.
    stats: Arc<LitStats>,
    summary_started: AtomicBool,
    /// Road tiles whose changed DSFs have been logged: once each.
    road_sources_logged: DashSet<(i32, i32)>,
}

/// A generated part of a footprint, how complete its roads were, and the
/// first doubt its Laminar art lookups met (an art file given up on after
/// I/O errors, or one the lit fingerprint does not cover).
struct Generated {
    bytes: Bytes,
    roads: RoadsState,
    /// The DSFs each road tile it drew was read from.
    read_from: Vec<Arc<ReadFrom>>,
    art_doubt: Option<ArtDoubt>,
}

/// Whether a generation had every road tile it could draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoadsState {
    Complete,
    /// A tile was still building at the deadline: a later generation may
    /// have it, so the payload is kept nowhere.
    TimedOut,
    /// A tile failed: it has no roads this session, so the payload is kept
    /// in memory, but not on disk (a later session may read the tile).
    Failed,
}

impl RoadsState {
    fn of(roads: &FootprintRoads) -> Self {
        if roads.timed_out {
            Self::TimedOut
        } else if roads.complete {
            Self::Complete
        } else {
            Self::Failed
        }
    }
}

/// Why a payload generated with `roads`, a lit disk tier present, is not
/// written back, if it is not: the first reason that applies, in
/// [`WriteBackSkip`]'s order. Roads still building make it a payload kept
/// nowhere (a later generation may have them). Without a fingerprint there
/// is no safe disk entry. A failed road tile has no roads this session, but
/// a later session may read it. Roads read from DSFs other than those the
/// fingerprint names (`roads_agree` false: a DSF changed while the session
/// ran) would be served under a fingerprint that does not describe them. A
/// doubt among the payload's Laminar art lookups (`art_doubted`: a file
/// given up on after I/O errors, or one the fingerprint does not cover) may
/// make the payload differ from a later session's under the same
/// fingerprint; a file that is missing or does not decode does not, as its
/// dark class is the same every session.
fn write_back_skip(
    roads: RoadsState,
    fingerprinted: bool,
    roads_agree: bool,
    art_doubted: bool,
) -> Option<WriteBackSkip> {
    match roads {
        RoadsState::TimedOut => Some(WriteBackSkip::RoadsLate),
        _ if !fingerprinted => Some(WriteBackSkip::NoFingerprint),
        RoadsState::Failed => Some(WriteBackSkip::RoadFailed),
        RoadsState::Complete if !roads_agree => Some(WriteBackSkip::RoadSourcesChanged),
        RoadsState::Complete if art_doubted => Some(WriteBackSkip::ArtFailed),
        RoadsState::Complete => None,
    }
}

/// The first road tile the fingerprint names (`fingerprinted`, from the
/// session's identity pass) that was not read from the DSFs it names by the
/// build whose roads were drawn (`read_from`), or `None` when every one was:
/// the roads agree with the fingerprint. A tile drawn from no build has not
/// been read from them.
fn road_sources_changed(
    fingerprinted: &[TileDsfs],
    read_from: &[Arc<ReadFrom>],
) -> Option<(i32, i32)> {
    fingerprinted
        .iter()
        .find(|(tile, identities)| {
            !read_from
                .iter()
                .find(|r| r.tile == *tile)
                .is_some_and(|r| r.agrees_with(identities))
        })
        .map(|(tile, _)| *tile)
}

impl NightLitSource {
    /// A source whose lit disk tier work takes `disk_permits`, the
    /// session's one set, which a road service attached with
    /// [`Self::with_roads`] must share.
    pub fn new(
        store: Arc<RecipeStore>,
        art: Arc<LaminarArt>,
        setting: Arc<dyn TextureSettingSource>,
        disk_permits: LitDiskPermits,
    ) -> Self {
        let stats = Arc::new(LitStats::default());
        let generations =
            lit_generation_permits(std::thread::available_parallelism().map_or(1, |n| n.get()));
        Self {
            recipes: Arc::new(SessionRecipes::new(store)),
            art,
            setting,
            lit: DashMap::new(),
            probes: tokio::sync::Semaphore::new(LIT_PROBE_PERMITS),
            payloads: Arc::new(PayloadMemory::new(
                LitMemoryBudget::FLOOR.head,
                LitMemoryBudget::FLOOR.tail,
                Arc::clone(&stats),
            )),
            generations: GenerationPermits::new(generations),
            // One pre-generation in flight per permit but one: a permit is
            // always left to reads, and pre-generations share the lit disk
            // permits with reads no more than that.
            pregens: PregenQueue::new(
                pregen_bound(LitMemoryBudget::FLOOR.tail),
                pregen_workers(generations),
            ),
            demand: None,
            this: Weak::new(),
            brightness: DEFAULT_NIGHT_BRIGHTNESS,
            generator: Arc::new(CpuLitGenerator),
            disk: None,
            disk_io: disk_permits,
            session: tokio::sync::OnceCell::new(),
            roads: None,
            road_model: RoadModel::DEFAULT,
            stats,
            summary_started: AtomicBool::new(false),
            road_sources_logged: DashSet::new(),
        }
    }

    /// The source, shared, with pre-generation on (#433): each footprint its
    /// probe commits lit is queued and generated in the background, behind
    /// every read, so the read X-Plane makes later is a memory hit. Build it
    /// with the `with_*` methods first. A source not made shared this way
    /// never pre-generates: the workers must outlive the probe that queued
    /// them, and hold the source only weakly between footprints.
    pub fn shared(self) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            ..self
        })
    }

    /// Hold pre-generation while `demand` says ortho demand is in flight
    /// (`DemandTracker::subscribe`, the pause state the prefetch lane's
    /// admission gate reads): lit work must not delay ortho (R17). Reads
    /// are not held.
    pub fn with_demand_gate(mut self, demand: tokio::sync::watch::Receiver<bool>) -> Self {
        self.demand = Some(demand);
        self
    }

    /// At most `bound` pre-generations queued instead of the budget's
    /// ([`pregen_bound`]).
    #[cfg(test)]
    fn with_pregeneration_bound(mut self, bound: usize) -> Self {
        self.pregens = self.pregens.with_bound(bound);
        self
    }

    /// Overall lit brightness (`night.brightness`, 0.5 to 1.5 in config).
    pub fn with_brightness(mut self, brightness: f64) -> Self {
        self.brightness = brightness;
        self
    }

    /// Use `generator` for lit pixels (tests, or another implementation).
    pub fn with_generator(mut self, generator: Arc<dyn LitGenerator>) -> Self {
        self.generator = generator;
        self
    }

    /// Read lit payloads from `disk` before generating, and write generated
    /// ones back to it.
    pub fn with_disk_cache(mut self, disk: Arc<dyn Cache>) -> Self {
        self.disk = Some(disk);
        self
    }

    /// Keep `budget` of payloads in memory instead of
    /// [`LitMemoryBudget::FLOOR`] (the session's share of
    /// `cache.memory_size`, `service::memory_budgets`), and queue at most
    /// as many pre-generations as its tail memory allows ([`pregen_bound`]).
    pub fn with_memory_budget(mut self, budget: LitMemoryBudget) -> Self {
        self.payloads = Arc::new(PayloadMemory::new(
            budget.head,
            budget.tail,
            Arc::clone(&self.stats),
        ));
        self.pregens = self.pregens.with_bound(pregen_bound(budget.tail));
        self
    }

    /// Keep up to `head_bytes` of first visible levels and `tail_bytes` of
    /// the levels below them in memory, instead of the session budgets.
    #[cfg(test)]
    fn with_payload_budgets(mut self, head_bytes: u64, tail_bytes: u64) -> Self {
        self.payloads = Arc::new(PayloadMemory::new(
            head_bytes,
            tail_bytes,
            Arc::clone(&self.stats),
        ));
        self
    }

    /// Light roads from `roads`: in the probe, in generation and in the
    /// disk tier's fingerprints. `roads` must have been built with this
    /// source's [`LitDiskPermits`], which the writes of its road entries
    /// take, so the lit disk tier's work stays within them.
    pub fn with_roads(mut self, roads: Arc<RoadService>) -> Self {
        debug_assert!(
            roads.disk_permits().same(&self.disk_io),
            "a road service attached to a lit source shares its lit disk permits"
        );
        self.roads = Some(roads);
        self
    }

    /// The tiles whose roads can light `name` drawn at `size` texels (see
    /// [`road_tiles`]).
    fn road_tiles(&self, name: &DdsFilename, size: u32) -> Vec<(i32, i32)> {
        road_tiles(name, size, &self.road_model)
    }

    /// The ready roads of the tiles that can light `name` drawn at `size`
    /// texels, waited for as `purpose` allows; complete and empty without
    /// road lights. The caller holds no permit.
    async fn footprint_roads(
        &self,
        name: &DdsFilename,
        size: u32,
        purpose: Purpose,
    ) -> FootprintRoads {
        match &self.roads {
            Some(roads) => roads.footprint(&self.road_tiles(name, size), purpose).await,
            None => FootprintRoads {
                tiles: Vec::new(),
                read_from: Vec::new(),
                complete: true,
                timed_out: false,
            },
        }
    }

    /// The `wanted` parts of a lit footprint's generated visible levels,
    /// none (black) when generation fails or a recipe cannot be read, and
    /// where they came from: memory, or the footprint's load (disk first,
    /// then generation), which concurrent reads share whatever parts they
    /// want. A failure is not kept, nor a payload whose roads were late: the
    /// next read generates again. See [`PayloadMemory`] for which parts a
    /// load keeps. A read that reaches the first visible level waits for a
    /// generation permit ahead of one that does not (#433).
    async fn served_parts(
        &self,
        name: &DdsFilename,
        layout: &LitLayout,
        wanted: Wanted,
    ) -> (LitParts, ReadOutcome) {
        let from_disk = AtomicBool::new(false);
        let class = GenerationClass::of_read(wanted.contains(PayloadPart::Head));
        let load =
            |priority| self.load_or_generate(name.clone(), layout.clone(), &from_disk, priority);
        let served = self
            .payloads
            .serve((name.row, name.col, name.zoom), layout, wanted, class, load)
            .await;
        for part in PayloadPart::ALL {
            if let Some(lookup) = served.lookups[part as usize] {
                self.stats.record_payload_lookup(part, lookup);
            }
        }
        let outcome = match (
            served.parts.is_empty(),
            served.lookup(),
            from_disk.load(Ordering::Relaxed),
        ) {
            (true, _, _) => ReadOutcome::Black,
            (false, PayloadLookup::Hit, _) => ReadOutcome::MemoryHit,
            (false, PayloadLookup::Joined, _) => ReadOutcome::Joined,
            (false, PayloadLookup::Loaded, true) => ReadOutcome::DiskHit,
            (false, PayloadLookup::Loaded, false) => ReadOutcome::Generated,
        };
        (served.parts, outcome)
    }

    /// The whole payload as [`Self::served_parts`] serves it, both parts
    /// wanted, in one buffer, or `None` (black).
    #[cfg(test)]
    async fn served_payload(
        &self,
        name: &DdsFilename,
        layout: &LitLayout,
    ) -> (Option<Bytes>, ReadOutcome) {
        let (parts, outcome) = self.served_parts(name, layout, Wanted::whole()).await;
        let payload = match (parts.get(PayloadPart::Head), parts.get(PayloadPart::Tail)) {
            (Some(head), Some(tail)) => Some(Bytes::from([&head[..], &tail[..]].concat())),
            _ => None,
        };
        (payload, outcome)
    }

    /// Disk first, then generation; a generated payload is written back
    /// without waiting, unless [`write_back_skip`] gives a reason not to (a
    /// road tile missing, no fingerprint, a doubt among its art lookups), or
    /// [`MAX_PENDING_WRITE_BACKS`] are pending already (dropped, counted).
    /// When the reason is the art, a debug line names the file and why; a
    /// payload skipped for an earlier reason logs none. When the reason is a
    /// changed road DSF, a debug line names the road tile, once per tile for
    /// the session. `Err` is a payload not to keep in memory: black, or
    /// generated while a road tile was late. Without a disk tier this is
    /// `generate`. Sets `from_disk` when the payload is read from the disk
    /// tier. `priority` is the load's, for its generation permit.
    async fn load_or_generate(
        &self,
        name: DdsFilename,
        layout: LitLayout,
        from_disk: &AtomicBool,
        priority: Arc<LoadPriority>,
    ) -> Result<Bytes, Uncached> {
        let Some(disk) = self.disk.clone() else {
            return memory_only(self.generate(name, &layout, &priority).await);
        };
        let key = lit_cache_key(&name);
        // The disk permit covers the fingerprint and the read, and is released
        // before generating: a lit task never holds two permits at once.
        let asked = Instant::now();
        let Some(permit) = self.disk_io.acquire().await else {
            return Err(Uncached(None));
        };
        let permit_wait = asked.elapsed();
        let (fp, road_dsfs) = match self.fingerprint_with_roads(&name).await {
            Some((fp, road_dsfs)) => (Some(fp), road_dsfs),
            None => (None, Vec::new()),
        };
        let stored = match &fp {
            Some(fp) => match disk.get(&key).await {
                Ok(Some(entry)) => check_entry(entry, fp, layout.payload_len()).map_err(Into::into),
                Ok(None) => Err(DiskMiss::Absent),
                Err(_) => Err(DiskMiss::ReadError),
            },
            None => Err(DiskMiss::NoFingerprint),
        };
        drop(permit);
        self.stats
            .record_disk_lookup(permit_wait, stored.as_ref().err().copied());
        if let Ok(payload) = stored {
            debug!(
                row = name.row,
                col = name.col,
                zoom = name.zoom,
                "Night lighting: lit texture read from disk"
            );
            from_disk.store(true, Ordering::Relaxed);
            return Ok(payload);
        }
        let (row, col, zoom) = (name.row, name.col, name.zoom);
        let generated = self.generate(name, &layout, &priority).await;
        let changed = generated
            .as_ref()
            .and_then(|g| road_sources_changed(&road_dsfs, &g.read_from));
        let skip = generated.as_ref().and_then(|g| {
            write_back_skip(
                g.roads,
                fp.is_some(),
                changed.is_none(),
                g.art_doubt.is_some(),
            )
        });
        if let Some(skip) = skip {
            self.stats.record_write_back_skipped(skip);
        }
        if let (Some(WriteBackSkip::RoadSourcesChanged), Some(tile)) = (skip, changed) {
            // Every footprint over the tile skips its write-back for the
            // rest of the session (the counter says how many): logged once.
            if self.road_sources_logged.insert(tile) {
                debug!(
                    row,
                    col,
                    zoom,
                    lat = tile.0,
                    lon = tile.1,
                    "Night lighting: lit textures over a road tile kept off disk; its DSFs changed since its roads were read"
                );
            }
        }
        if let (
            Some(WriteBackSkip::ArtFailed),
            Some(Generated {
                art_doubt: Some(doubt),
                ..
            }),
        ) = (skip, &generated)
        {
            debug!(
                row,
                col,
                zoom,
                file = %doubt.file.display(),
                reason = doubt.kind.reason(),
                "Night lighting: lit texture kept off disk; an art file it uses may differ next session"
            );
        }
        let (Some(fp), None, Some(Generated { bytes: payload, .. })) = (&fp, skip, &generated)
        else {
            return memory_only(generated);
        };
        // Pending from here, so a write-back spawned but not yet run counts.
        let Some(pending) = self.stats.write_back(MAX_PENDING_WRITE_BACKS) else {
            return Ok(payload.clone());
        };
        // The header and the payload are written as two slices: the payload
        // is not copied into the entry.
        let entry = vec![entry_header(fp, payload.len()), payload.clone()];
        let payload = payload.clone();
        let disk_io = self.disk_io.clone();
        tokio::spawn(async move {
            let Some(_permit) = disk_io.acquire().await else {
                return;
            };
            let written = disk.set_parts(&key, entry).await;
            if let Err(e) = &written {
                debug!(error = %e, key, "Night lighting: lit disk write failed");
            }
            pending.finished(written.is_ok());
        });
        Ok(payload)
    }

    /// Fingerprint of every input `name`'s payload is generated from (see
    /// [`Self::fingerprint_with_roads`]).
    #[cfg(test)]
    async fn fingerprint(&self, name: &DdsFilename) -> Option<LitFingerprint> {
        self.fingerprint_with_roads(name).await.map(|(fp, _)| fp)
    }

    /// Fingerprint of every input `name`'s payload is generated from, and
    /// the road DSF identities in it, or `None` when computing it failed (a
    /// recipe that cannot be loaded among them). Recipe identities are those
    /// taken when each recipe was loaded, so a file replaced mid-session does
    /// not lend the payload its identity; when the store holds them all, they
    /// are read without a blocking hop. Road DSF identities are taken once
    /// per tile per session, in priority order; a payload is written under
    /// them only when its roads were read from those DSFs (#430). The art
    /// walk, recipe loads (on a store miss) and DSF `stat`s run on the
    /// blocking pool; the
    /// caller holds a disk permit and none of them takes another.
    async fn fingerprint_with_roads(
        &self,
        name: &DdsFilename,
    ) -> Option<(LitFingerprint, Vec<TileDsfs>)> {
        let session = *self
            .session
            .get_or_try_init(|| {
                let (art, brightness) = (Arc::clone(&self.art), self.brightness);
                let roads = match self.roads {
                    Some(_) => super::roads::roads_digest(&self.road_model),
                    None => [0; 32],
                };
                async move {
                    tokio::task::spawn_blocking(move || {
                        session_fingerprint(brightness, art.fingerprint(), roads)
                    })
                    .await
                }
            })
            .await
            .ok()?;
        // The store usually holds the recipes already (the probe read them):
        // the blocking hop only on a store miss.
        let identities = match self.recipes.identities_held(name) {
            Some(held) => held.ok()?,
            None => {
                let (recipes, owned) = (Arc::clone(&self.recipes), name.clone());
                tokio::task::spawn_blocking(move || recipes.identities(&owned))
                    .await
                    .ok()?
                    .ok()?
            }
        };
        // A tile whose identities could not be taken leaves no fingerprint:
        // the payload is then kept in memory only.
        let layout = self.layout(name);
        let roads = match &self.roads {
            Some(roads) => futures::future::join_all(
                self.road_tiles(name, layout.first_visible)
                    .into_iter()
                    .map(|tile| async move { Some((tile, roads.identities(tile).await?)) }),
            )
            .await
            .into_iter()
            .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        Some((
            footprint_fingerprint(session, &identities, &roads, layout.road_supersample()),
            roads,
        ))
    }

    /// The generated visible levels of the footprint. Waits for its roads
    /// first, holding no permit, then generates under a generation permit;
    /// the road raster is generation work and runs under it. Holding the
    /// permit, it looks once more, without waiting, at the road tiles its
    /// wait left out, so roads that came while it queued are drawn (#427).
    /// The permit is admitted by `priority`'s class (#433); `None` (black,
    /// nothing generated) when it is refused.
    async fn generate(
        &self,
        name: DdsFilename,
        layout: &LitLayout,
        priority: &LoadPriority,
    ) -> Option<Generated> {
        let (size, road_supersample) = (layout.first_visible, layout.road_supersample());
        let found = self.footprint_roads(&name, size, Purpose::Generate).await;
        let asked_roads = RoadsState::of(&found);
        let asked = Instant::now();
        let permit = {
            let _waiting = self.stats.generations_waiting.enter();
            self.generations.acquire(priority).await?
        };
        let (class, permit_wait) = (permit.class(), asked.elapsed());
        let found = match &self.roads {
            Some(roads) if asked_roads != RoadsState::Complete => {
                let tiles = self.road_tiles(&name, size);
                let now = roads.tiles_now(&tiles).await;
                recheck(&tiles, found, now)
            }
            _ => found,
        };
        let roads_state = RoadsState::of(&found);
        let roads_ready_at_permit =
            asked_roads == RoadsState::TimedOut && roads_state == RoadsState::Complete;
        let (road_tiles, read_from) = (found.tiles, found.read_from);
        let started = Instant::now();
        let (recipes, art, generator, owned, size, brightness, road_model) = (
            Arc::clone(&self.recipes),
            Arc::clone(&self.art),
            Arc::clone(&self.generator),
            name.clone(),
            size,
            self.brightness,
            self.road_model,
        );
        let generated = tokio::task::spawn_blocking(move || {
            // All recipes first: a payload missing one would be cached and
            // differ from its regeneration (R13).
            let Ok(fetched) = recipes.for_footprint(&owned) else {
                return None;
            };
            let lookup = |lat: i32, lon: i32| fetched.get(&(lat, lon)).and_then(|(r, _)| r.clone());
            let regions = |lat: i32, lon: i32| {
                fetched
                    .get(&(lat, lon))
                    .map_or(LedRegion::Unknown, |(_, g)| *g)
            };
            // The doubts this payload's art lookups meet decide whether it
            // may be kept on disk.
            let art_record = ArtRecord::default();
            let patterns = |n: &str| art.pattern_recorded(n, &art_record);
            // The tiles' handles are dropped once rastered: the road cache
            // alone keeps merged roads. A texture capped below its density
            // has its roads rastered at the density and averaged down (#431).
            let roads = RoadField::raster_supersampled(
                &owned,
                size,
                road_supersample,
                &road_tiles,
                &road_model,
            );
            drop(road_tiles);
            let image = generator.generate(&LitRequest {
                name: &owned,
                size,
                brightness,
                recipes: &lookup,
                patterns: &patterns,
                regions: &regions,
                led: &LedModel::DEFAULT,
                roads: &roads,
                road_model: &road_model,
            });
            // Only its stats are wanted now: the field (4 MiB at 1024 px
            // when a road is lit) is not held through the encode (#440).
            let road_stats = roads.stats;
            drop(roads);
            Some((
                super::generator::encode_visible(image),
                road_stats,
                art_record.into_doubt(),
            ))
        })
        .await;
        let result = match generated {
            Ok(None) => {
                // Warned once, when the tile was given up on.
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    "Night lighting: a recipe is unreadable; texture served black"
                );
                None
            }
            Ok(Some((Ok(bytes), road, art_doubt))) => {
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    ms = started.elapsed().as_millis() as u64,
                    permit_wait_ms = permit_wait.as_millis() as u64,
                    class = class.label(),
                    road_segments = road.segments,
                    road_raster_us = road.raster_us,
                    road_texels = road.lit_texels,
                    size,
                    roads_complete = roads_state == RoadsState::Complete,
                    roads_timed_out = roads_state == RoadsState::TimedOut,
                    roads_ready_at_permit,
                    "Night lighting: lit texture generated"
                );
                Some(Generated {
                    bytes,
                    roads: roads_state,
                    read_from,
                    art_doubt,
                })
            }
            Ok(Some((Err(e), _, _))) => {
                warn!(error = %e, "Night lighting: lit encode failed; texture served black");
                None
            }
            Err(_) => {
                warn!("Night lighting: lit generation failed; texture served black");
                None
            }
        };
        drop(permit);
        self.stats.record_generation(&GenerationRecord {
            class,
            permit_wait,
            generation: started.elapsed(),
            roads: asked_roads,
            roads_ready_at_permit,
            payload: result.is_some(),
        });
        result
    }

    /// Whether `name` is lit, answered once per footprint for the session.
    /// The recipe is read first, under a probe permit; only a footprint it
    /// leaves dark waits for its road tiles (holding no permit, #428), whose
    /// builds were started before the recipe read so they start as early as
    /// they would for a wait.
    async fn has_light(&self, name: &DdsFilename) -> bool {
        if !SUPPORTED_LIT_ZOOMS.contains(&name.zoom) {
            return false;
        }
        let key = (name.row, name.col, name.zoom);
        if let Some(hit) = self.lit.get(&key) {
            return *hit;
        }
        let size = self.layout(name).first_visible;
        let road_tiles = self.roads.as_ref().map(|roads| {
            let tiles = self.road_tiles(name, size);
            for &tile in &tiles {
                roads.ensure(tile, Trigger::Probe);
            }
            (roads, tiles)
        });
        let started = Instant::now();
        let Ok(permit) = self.probes.acquire().await else {
            return false;
        };
        let permit_wait = started.elapsed();
        if let Some(hit) = self.lit.get(&key).map(|hit| *hit) {
            // Another probe answered while we waited: its wait counts too.
            self.stats.record_probe(&ProbeRecord {
                road_wait: None,
                permit_wait,
                committed: None,
            });
            return hit;
        }
        let probe_started = Instant::now();
        let (recipes, art) = (Arc::clone(&self.recipes), Arc::clone(&self.art));
        let footprint = Footprint::from_name(name);
        let recipe_lit = tokio::task::spawn_blocking(move || {
            // The field reads every tile the generator could read for this
            // footprint, once each; an unreadable one counts as dark.
            let lookup = |lat: i32, lon: i32| recipes.get(lat, lon).ok().and_then(|(r, _)| r);
            // `lit_terrain` records no doubt, which is safe here: the probe
            // decides a size, and a size is never persisted.
            recipe_lights(&footprint, &lookup, |n| art.lit_terrain(n).is_some())
        })
        .await;
        let probe_time = probe_started.elapsed();
        drop(permit);
        let mut roads_seen = None;
        let mut road_wait = None;
        let answer = match recipe_lit {
            Ok(recipe_lit) => {
                let mut ready = Vec::new();
                let mut reach = 0.0;
                if let (false, Some((roads, tiles))) = (recipe_lit, road_tiles) {
                    // Roads after the permit, and only for a footprint its
                    // recipe leaves dark: a lit task never waits for roads
                    // while holding one.
                    let road_started = Instant::now();
                    let found = roads.footprint(&tiles, Purpose::Probe).await;
                    road_wait = Some(road_started.elapsed());
                    roads_seen = Some((found.complete, found.timed_out));
                    // How far outside the footprint a road can light it:
                    // the raster's reach at the size generation uses, so the
                    // two agree. About 20 ns a tile: no blocking hop.
                    reach = reach_deg(name, size, &self.road_model);
                    ready = found.tiles;
                }
                probe_answer(recipe_lit, &footprint, &ready, reach)
            }
            Err(_) => {
                // The probe is meant to be panic-free. This arm is a last
                // resort, and it remembers the dark answer so size and read
                // keep agreeing.
                warn!("Night lighting: lit probe failed; footprint stays dark this session");
                ProbeAnswer::Dark
            }
        };
        let (lit, committed) = self.commit(key, answer.is_lit());
        if committed && lit {
            self.queue_pregeneration(name);
        }
        self.stats.record_probe(&ProbeRecord {
            road_wait,
            permit_wait,
            committed: committed.then_some(answer),
        });
        let roads_timed_out = roads_seen.map(|(_, timed_out)| timed_out);
        if committed && !lit && roads_timed_out == Some(true) {
            // A road still building could have lit it; the answer stands
            // for the session (R7).
            if let Some(roads) = &self.roads {
                roads.note_dark_answer_before_roads();
            }
        }
        debug!(
            row = name.row,
            col = name.col,
            zoom = name.zoom,
            lit,
            answer = answer.label(),
            wait_ms = permit_wait.as_millis() as u64,
            road_wait_ms = road_wait.map(|w| w.as_millis() as u64),
            probe_ms = probe_time.as_millis() as u64,
            roads_complete = roads_seen.map(|(complete, _)| complete),
            roads_timed_out,
            "Night lighting: lit probe"
        );
        lit
    }

    /// Queues `name`, just committed lit, for pre-generation, starting a
    /// worker when fewer than allowed run; dropped (counted) when the queue
    /// is full. Nothing on a source not made shared, or outside a runtime.
    fn queue_pregeneration(&self, name: &DdsFilename) {
        if self.this.strong_count() == 0 {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Queued::Yes { start_worker: true } = self.pregens.push(name.clone(), &self.stats) {
            runtime.spawn(pregenerate_queued(self.this.clone()));
        }
    }

    /// Pre-generates `name`'s payload as a read of its small levels loads
    /// it, waiting for its generation permit behind every read: through the
    /// footprint's one load, so a read arriving meanwhile joins it (and
    /// raises it to its own class) and never generates it again, and a
    /// footprint whose small levels are in memory loads nothing. A payload
    /// on the lit disk tier is loaded, not generated: a disk lookup is
    /// cheap next to a generation, and the later read is then a memory hit,
    /// not a disk lookup on the read's path. What is kept follows the read's
    /// rules: the small levels in memory, the first visible level not
    /// (no read reached it), the write-back as for any generation.
    async fn pregenerate(&self, name: &DdsFilename) -> PregenOutcome {
        let layout = self.layout(name);
        let (from_disk, kept) = (AtomicBool::new(false), AtomicBool::new(false));
        let load = |priority| async {
            let loaded = self
                .load_or_generate(name.clone(), layout.clone(), &from_disk, priority)
                .await;
            kept.store(loaded.is_ok(), Ordering::Relaxed);
            loaded
        };
        let served = self
            .payloads
            .serve(
                (name.row, name.col, name.zoom),
                &layout,
                Wanted::only(PayloadPart::Tail),
                GenerationClass::Pregeneration,
                load,
            )
            .await;
        let outcome = pregen_outcome(
            served.lookup(),
            !served.parts.is_empty(),
            from_disk.load(Ordering::Relaxed),
            kept.load(Ordering::Relaxed),
        );
        debug!(
            row = name.row,
            col = name.col,
            zoom = name.zoom,
            outcome = outcome.label(),
            "Night lighting: lit pre-generation"
        );
        outcome
    }

    /// The committed answer of [`Self::commit`] alone.
    #[cfg(test)]
    fn remember(&self, key: (u32, u32, u8), lit: bool) -> bool {
        self.commit(key, lit).0
    }

    /// Commits a probe answer and returns the committed one, and whether it
    /// is this call's: the first answer for a key wins, so two probes that
    /// finish with different answers cannot give `size` and `read` different
    /// lengths (R7).
    fn commit(&self, key: (u32, u32, u8), lit: bool) -> (bool, bool) {
        match self.lit.entry(key) {
            dashmap::Entry::Occupied(e) => (*e.get(), false),
            dashmap::Entry::Vacant(e) => (*e.insert(lit), true),
        }
    }

    fn layout(&self, name: &DdsFilename) -> LitLayout {
        LitLayout::new(name.zoom, self.setting.texture_setting())
    }

    /// Starts the minute summary once, on the first call inside a Tokio
    /// runtime. The task holds the statistics and the payload cache only
    /// weakly, so it ends at the first tick after the source is dropped.
    fn start_summary(&self) {
        if self.summary_started.load(Ordering::Relaxed) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.summary_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let (stats, payloads) = (Arc::downgrade(&self.stats), Arc::downgrade(&self.payloads));
        runtime.spawn(log_summaries(stats, payloads));
    }
}

/// A pre-generation worker: pre-generates queued footprints, one at a
/// time, until none is left, the queue is closed or the source is gone. It
/// holds the source only while it runs one. Before taking each, it waits
/// while ortho demand is in flight (R17), holding only the demand
/// receiver; a demand job beginning after it took one does not stop that
/// one, which waits for its permit behind every read. A receiver whose
/// sender is gone (the runtime stopping) holds nothing back.
async fn pregenerate_queued(this: Weak<NightLitSource>) {
    loop {
        let Some(demand) = this.upgrade().map(|source| source.demand.clone()) else {
            return;
        };
        if let Some(mut demand) = demand {
            let _ = demand.wait_for(|in_flight| !*in_flight).await;
        }
        let Some(source) = this.upgrade() else {
            return;
        };
        let Some(name) = source.pregens.next(&source.stats) else {
            return;
        };
        let outcome = source.pregenerate(&name).await;
        source.stats.record_pregen(outcome);
    }
}

/// What a pre-generation did, one outcome each: from how its tail was
/// found (`lookup`), whether it has a payload, whether it was read from
/// the disk tier, and whether its load's payload may be kept (`kept`; only
/// read for a load it ran). A payload generated but not to keep (roads
/// late) is no use to the later read, which generates again.
fn pregen_outcome(
    lookup: PayloadLookup,
    payload: bool,
    from_disk: bool,
    kept: bool,
) -> PregenOutcome {
    match (lookup, payload, kept, from_disk) {
        (PayloadLookup::Hit, ..) => PregenOutcome::InMemory,
        (PayloadLookup::Joined, ..) => PregenOutcome::Joined,
        (PayloadLookup::Loaded, false, ..) => PregenOutcome::Black,
        (PayloadLookup::Loaded, true, false, _) => PregenOutcome::Uncached,
        (PayloadLookup::Loaded, true, true, true) => PregenOutcome::OnDisk,
        (PayloadLookup::Loaded, true, true, false) => PregenOutcome::Generated,
    }
}

/// Logs `Night lighting: summary` every [`SUMMARY_INTERVAL`] until the
/// source is dropped.
async fn log_summaries(stats: Weak<LitStats>, payloads: Weak<PayloadMemory>) {
    let mut tick = tokio::time::interval(SUMMARY_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        let (Some(stats), Some(payloads)) = (stats.upgrade(), payloads.upgrade()) else {
            break;
        };
        stats.log_summary(payloads.occupancy());
    }
}

/// The `part` field of a lit read's debug event: whether the read reached
/// the visible levels, and whether they came back (`black` when not).
fn read_part(reached: bool, payload: bool) -> &'static str {
    match (reached, payload) {
        (false, _) => "blank",
        (true, true) => "lit",
        (true, false) => "black",
    }
}

/// The roads of `asked` (a footprint's road tiles, in order) once more:
/// each tile `held` from the wait, else as it is `now`. A tile still
/// building or absent (evicted since) counts as late, a failed one as
/// failed, as in the wait.
fn recheck(asked: &[(i32, i32)], held: FootprintRoads, now: Vec<TileNow>) -> FootprintRoads {
    let mut out = FootprintRoads {
        tiles: Vec::with_capacity(asked.len()),
        read_from: Vec::with_capacity(asked.len()),
        complete: true,
        timed_out: false,
    };
    for (&tile, now) in asked.iter().zip(now) {
        let held = held
            .tiles
            .iter()
            .position(|r| r.tile == tile)
            .map(|i| (Arc::clone(&held.tiles[i]), Arc::clone(&held.read_from[i])));
        match (held, now) {
            (Some((roads, read_from)), _)
            | (None, TileNow::Ready(ReadyTile { roads, read_from })) => {
                out.tiles.push(roads);
                out.read_from.push(read_from);
            }
            (None, TileNow::Failed) => out.complete = false,
            (None, TileNow::Building | TileNow::Absent) => {
                out.complete = false;
                out.timed_out = true;
            }
        }
    }
    out
}

/// A generated part for the memory cache: kept unless its roads were
/// late; `None` (black) is never kept.
fn memory_only(generated: Option<Generated>) -> Result<Bytes, Uncached> {
    match generated {
        Some(Generated {
            bytes,
            roads: RoadsState::Complete | RoadsState::Failed,
            ..
        }) => Ok(bytes),
        Some(Generated {
            bytes,
            roads: RoadsState::TimedOut,
            ..
        }) => Err(Uncached(Some(bytes))),
        None => Err(Uncached(None)),
    }
}

impl LitTextureSource for NightLitSource {
    fn size<'a>(&'a self, name: &'a DdsFilename) -> BoxFuture<'a, u64> {
        Box::pin(async move {
            self.start_summary();
            if self.has_light(name).await {
                self.layout(name).total_len
            } else {
                minimal_black_dds().len() as u64
            }
        })
    }

    fn read<'a>(&'a self, name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes> {
        Box::pin(async move {
            let asked = Instant::now();
            self.start_summary();
            if self.has_light(name).await {
                let layout = self.layout(name);
                // The header and the levels above the visible ones are
                // black and need no pixels (X-Plane's objects light the
                // ground under the uploaded top level); the visible levels
                // are generated only by a read that reaches them.
                // Each part of them is served from memory or loaded apart
                // (#432): most reads want the small levels alone.
                let wanted = Wanted::reached(&layout, offset, len);
                let reached = !wanted.is_empty();
                let (parts, outcome) = if reached {
                    self.served_parts(name, &layout, wanted).await
                } else {
                    (LitParts::default(), ReadOutcome::Blank)
                };
                let part = read_part(reached, !parts.is_empty());
                let bytes = layout.read_lit(offset, len, &parts);
                let elapsed = asked.elapsed();
                self.stats.record_read(outcome, elapsed);
                // The order X-Plane reads a lit file in shows whether it
                // loads the small levels (at the end) before the top.
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    offset,
                    len,
                    payload_start = layout.payload_start(),
                    total = layout.total_len,
                    part,
                    from = outcome.label(),
                    us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
                    "Night lighting: lit read"
                );
                bytes
            } else {
                let bytes = BlackLitSource.read(name, offset, len).await;
                self.stats.record_read(ReadOutcome::Dark, asked.elapsed());
                bytes
            }
        })
    }

    fn dsf_seen(&self, lat: i32, lon: i32) {
        if let Some(roads) = &self.roads {
            roads.ensure((lat, lon), Trigger::DsfOpen);
        }
    }

    fn wants_dsf_hints(&self) -> bool {
        self.roads.is_some()
    }

    /// Closes the pre-generation queue (what it held never runs) and
    /// refuses pre-generations waiting for a permit; one already generating
    /// finishes (at most one per permit, as a read's would) and is kept. A
    /// read joining a refused pre-generation's load reads black that once
    /// (see [`GenerationPermits::close`] for why that is acceptable).
    fn shutdown(&self) {
        self.pregens.close(&self.stats);
        self.generations.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{DdsFilename, TextureKind};

    #[test]
    fn every_outcome_of_a_read_has_its_own_part_value() {
        assert_eq!(read_part(false, false), "blank");
        assert_eq!(read_part(true, true), "lit");
        assert_eq!(read_part(true, false), "black");
    }

    fn lit() -> DdsFilename {
        DdsFilename {
            row: 25712,
            col: 12960,
            zoom: 16,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    #[test]
    fn night_permits_fit_a_third_of_the_headroom() {
        const {
            assert!(
                LIT_PROBE_PERMITS
                    + MAX_LIT_GENERATION_PERMITS
                    + LIT_DISK_PERMITS
                    + crate::night::roads::service::ROAD_EXTRACT_PERMITS
                    <= crate::executor::BLOCKING_POOL_HEADROOM / 3
            )
        };
    }

    #[test]
    fn generation_permits_follow_the_core_count() {
        assert_eq!(lit_generation_permits(4), 2);
        assert_eq!(lit_generation_permits(24), 3);
        assert_eq!(lit_generation_permits(64), 4);
    }

    #[tokio::test]
    async fn black_source_reports_the_minimal_size() {
        assert_eq!(BlackLitSource.size(&lit()).await, 152);
    }

    #[tokio::test]
    async fn black_source_serves_ranges_of_the_minimal_dds() {
        let whole = BlackLitSource.read(&lit(), 0, 4096).await;
        assert_eq!(whole, minimal_black_dds());
        assert_eq!(BlackLitSource.read(&lit(), 128, 8).await.len(), 8);
        assert!(
            BlackLitSource.read(&lit(), 152, 8).await.is_empty(),
            "at EOF"
        );
        assert!(
            BlackLitSource.read(&lit(), 9999, 8).await.is_empty(),
            "past EOF"
        );
    }

    mod night_source {
        use super::super::*;
        use crate::fuse::{DdsFilename, TextureKind};
        use crate::night::laminar::art::FlakyArtFiles;
        use crate::night::roads::dsf::test_dsf::{install, Builder};
        use crate::night::roads::service::RoadServiceConfig;
        use crate::night::{FixedTextureSetting, LaminarArt, RecipeStore};
        use std::sync::Arc;
        use std::time::Duration;
        use tempfile::TempDir;

        const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");
        const EMPTY: &[u8] = include_bytes!("../../tests/fixtures/night/empty.oxn");

        /// A package with urban.oxn for +37-123 and an installation where
        /// urban_tmp_sdry3 is lit.
        fn fixture() -> (TempDir, NightLitSource) {
            let temp = TempDir::new().unwrap();
            let source = source_in(&temp);
            (temp, source)
        }

        /// A fresh source over the fixture tree in `temp`, writing the tree
        /// first when it is absent, so two sessions can share one tree.
        fn source_in(temp: &TempDir) -> NightLitSource {
            source_with_permits(temp, LitDiskPermits::new())
        }

        /// `source_in` with the lit disk permits of `roads`, as the mount
        /// builds them, ready for `with_roads(roads)`.
        fn source_sharing(temp: &TempDir, roads: &RoadService) -> NightLitSource {
            source_with_permits(temp, roads.disk_permits().clone())
        }

        fn source_with_permits(temp: &TempDir, disk_permits: LitDiskPermits) -> NightLitSource {
            let pkg = temp.path().join("pkg");
            let art = temp.path().join("art");
            if !art.exists() {
                write_tree(temp);
            }
            NightLitSource::new(
                Arc::new(RecipeStore::new(vec![pkg])),
                Arc::new(LaminarArt::new(art)),
                Arc::new(FixedTextureSetting(4)),
                disk_permits,
            )
        }

        fn write_tree(temp: &TempDir) {
            let pkg = temp.path().join("pkg");
            let recipe = crate::night::recipe_path(&pkg, 37, -123);
            std::fs::create_dir_all(recipe.parent().unwrap()).unwrap();
            std::fs::write(&recipe, URBAN).unwrap();

            let art = temp.path().join("art");
            std::fs::create_dir_all(art.join("terrain12")).unwrap();
            std::fs::create_dir_all(art.join("textures12/urban_FAR")).unwrap();
            std::fs::write(
                art.join("library.txt"),
                "A\n1200\nLIBRARY\nEXPORT lib/g10/terrain10/urban_tmp_sdry3.ter terrain12/u.ter\n",
            )
            .unwrap();
            std::fs::write(
                art.join("terrain12/u.ter"),
                "TEXTURE_LIT ../textures12/urban_FAR/uw5_FAR_LIT.dds\n",
            )
            .unwrap();
            // A 4 x 4 solid red DXT1 lit texture.
            let mut dds =
                crate::dds::DdsHeader::new(4, 4, 1, crate::dds::DdsFormat::BC1).to_bytes();
            dds.extend_from_slice(&[0x00, 0xF8, 0, 0, 0, 0, 0, 0]);
            std::fs::write(art.join("textures12/urban_FAR/uw5_FAR_LIT.dds"), dds).unwrap();
        }

        /// The ZL16 texture name whose footprint contains (lat, lon).
        fn name_at(lat: f64, lon: f64) -> DdsFilename {
            name_at_zoom(lat, lon, 16)
        }

        fn name_at_zoom(lat: f64, lon: f64, zoom: u8) -> DdsFilename {
            let n = f64::from(1u32 << zoom);
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let lat_r = lat.to_radians();
            let y = ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0
                * n) as u32;
            DdsFilename {
                row: y / 16 * 16,
                col: x / 16 * 16,
                zoom,
                map_type: "BI".into(),
                kind: TextureKind::Lit,
            }
        }

        /// Makes the fixture's recipe unreadable; false when running as root,
        /// where permissions do not apply.
        #[cfg(unix)]
        fn set_recipe_readable(temp: &TempDir, readable: bool) -> bool {
            use std::os::unix::fs::PermissionsExt;
            let path = crate::night::recipe_path(&temp.path().join("pkg"), 37, -123);
            let mode = if readable { 0o644 } else { 0o000 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            readable || std::fs::File::open(&path).is_err()
        }

        #[test]
        fn the_first_committed_probe_answer_wins() {
            // Two probes of one footprint can finish with different answers
            // (one recipe read failing after its retries); whichever commits
            // first is the answer for everyone, so size and read agree (R7).
            let (_t, source) = fixture();
            let key = (1, 2, 16);
            assert!(source.remember(key, true));
            assert!(source.remember(key, false), "the later answer is dropped");
            assert!(source.lit.get(&key).is_some_and(|v| *v));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn a_recipe_read_failure_during_generation_is_not_a_payload() {
            // A payload built without one of its recipes would be cached and
            // differ from a regeneration (R13): generation gives up instead,
            // and the read serves full-size black.
            let (temp, source) = fixture();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            if !set_recipe_readable(&temp, false) {
                return;
            }
            assert_eq!(source.served_payload(&name, &layout).await.0, None);
            assert_eq!(source.payloads.entry_count(), 0, "nothing cached");
        }

        #[test]
        fn recipe_identities_the_store_holds_are_taken_without_a_load() {
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let pkg = temp.path().join("pkg");
            let recipes = SessionRecipes::new(Arc::new(RecipeStore::new(vec![pkg.clone()])));
            let name = lit_name();
            assert_eq!(recipes.identities_held(&name), None, "nothing loaded yet");
            let loaded = recipes.identities(&name).unwrap();
            assert!(loaded.iter().any(|(_, identity, _)| identity.is_some()));
            // Held from here: the file is not read again.
            std::fs::remove_file(crate::night::recipe_path(&pkg, 37, -123)).unwrap();
            assert_eq!(recipes.identities_held(&name), Some(Ok(loaded)));
            let (tile, _, _) = recipes.identities(&name).unwrap()[0];
            recipes.given_up.insert(tile);
            assert_eq!(recipes.identities_held(&name), Some(Err(Unreadable)));
        }

        #[tokio::test]
        async fn a_fingerprint_from_held_identities_is_the_one_from_a_load() {
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let cold = source_in(&temp).fingerprint(&name).await;
            let warm = source_in(&temp);
            warm.recipes.identities(&name).unwrap();
            assert!(warm.recipes.identities_held(&name).is_some());
            assert_eq!(warm.fingerprint(&name).await, cold);
            assert!(cold.is_some());
        }

        /// `source_in` at texture setting `t`.
        fn source_at_setting(temp: &TempDir, t: u8) -> NightLitSource {
            let source = source_in(temp);
            NightLitSource {
                setting: Arc::new(FixedTextureSetting(t)),
                ..source
            }
        }

        #[tokio::test]
        async fn a_capped_footprint_rastered_at_another_road_density_has_its_own_fingerprint() {
            // The lit disk key names no setting: at ZL14, t = 2 and t = 4
            // both serve a 2048 px first visible level, the second with its
            // roads rastered at twice that (#431). Same length, other pixels.
            let temp = TempDir::new().unwrap();
            let name = name_at_zoom(37.20, -122.725, 14);
            let (low, full) = (LitLayout::new(14, 2), LitLayout::new(14, 4));
            assert_eq!(low.payload_len(), full.payload_len());
            assert_ne!(low.road_supersample(), full.road_supersample());
            let mut at = Vec::new();
            for t in 2..=4 {
                at.push(
                    source_at_setting(&temp, t)
                        .fingerprint(&name)
                        .await
                        .unwrap(),
                );
            }
            assert_ne!(at[0], at[2], "t = 2 and t = 4");
            assert_eq!(at[1], at[2], "t = 3 rasters as t = 4");
        }

        #[tokio::test]
        async fn an_uncapped_footprint_has_one_fingerprint_at_every_setting() {
            let temp = TempDir::new().unwrap();
            let name = name_at(37.20, -122.725);
            let mut fingerprints = Vec::new();
            for t in 0..=4 {
                fingerprints.push(
                    source_at_setting(&temp, t)
                        .fingerprint(&name)
                        .await
                        .unwrap(),
                );
            }
            assert!(fingerprints.windows(2).all(|w| w[0] == w[1]));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn an_unreadable_tile_is_remembered_for_the_session() {
            // After its retries a tile is given up on for the session, so the
            // retries, their delay and the warning cost once per tile, not
            // once per footprint.
            let (temp, source) = fixture();
            if !set_recipe_readable(&temp, false) {
                return;
            }
            assert_eq!(source.size(&name_at(37.20, -122.725)).await, 152);
            assert!(set_recipe_readable(&temp, true));
            let other = name_at_zoom(37.17, -122.74, 17);
            assert_eq!(source.size(&other).await, 152, "the tile stays given up");
            assert_eq!(source.read(&other, 0, u32::MAX).await, minimal_black_dds());
        }

        #[tokio::test]
        async fn a_lit_footprint_is_full_size() {
            let (_t, source) = fixture();
            let name = name_at(37.20, -122.725); // inside the golden urban block over radiance
            assert_eq!(source.size(&name).await, 2_796_344);
            let head = source.read(&name, 0, 128).await;
            assert_eq!(&head[0..4], b"DDS ");
            assert_eq!(source.read(&name, 1_000_000, 4096).await.len(), 4096);
        }

        #[tokio::test]
        async fn a_dark_footprint_is_the_minimal_black_dds() {
            let (_t, source) = fixture();
            let name = name_at(37.60, -122.20); // countryside in the golden tile
            assert_eq!(source.size(&name).await, 152);
            assert_eq!(source.read(&name, 0, 4096).await, minimal_black_dds());
        }

        #[tokio::test]
        async fn a_lit_footprint_has_light_in_its_visible_levels() {
            let (_t, source) = fixture();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert_eq!(
                visible.len() as u64,
                layout.payload_len(),
                "short read at EOF"
            );
            assert!(visible.iter().any(|&b| b != 0), "generated light");
        }

        #[tokio::test]
        async fn zero_brightness_keeps_it_black() {
            let (_t, source) = fixture();
            let source = source.with_brightness(0.0);
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert!(visible.iter().all(|&b| b == 0));
            assert_eq!(source.size(&name).await, layout.total_len, "size unchanged");
        }

        #[tokio::test]
        async fn size_and_read_agree() {
            let (_t, source) = fixture();
            for name in [name_at(37.20, -122.725), name_at(37.60, -122.20)] {
                let size = source.size(&name).await;
                assert_eq!(source.read(&name, 0, u32::MAX).await.len() as u64, size);
            }
        }

        #[tokio::test]
        async fn a_size_answer_is_remembered_for_the_session() {
            let (temp, source) = fixture();
            let name = name_at(37.20, -122.725);
            assert_eq!(source.size(&name).await, 2_796_344);
            std::fs::remove_file(crate::night::recipe_path(
                &temp.path().join("pkg"),
                37,
                -123,
            ))
            .unwrap();
            assert_eq!(source.size(&name).await, 2_796_344, "memoised");
            assert_eq!(
                source.read(&name, 0, u32::MAX).await.len() as u64,
                2_796_344,
                "read agrees with the memoised size"
            );
        }

        /// A generator that lights every texel white, counting its calls.
        #[derive(Default)]
        struct White(std::sync::atomic::AtomicUsize);

        impl crate::night::generator::LitGenerator for White {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        /// Records the region and LED model the generator is handed.
        #[derive(Default)]
        struct Recording(std::sync::Mutex<Option<(LedRegion, bool)>>);

        impl crate::night::generator::LitGenerator for Recording {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                *self.0.lock().unwrap() =
                    Some(((r.regions)(37, -123), *r.led == LedModel::DEFAULT));
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn the_generator_gets_the_package_region_and_the_default_model() {
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let recording = Arc::new(Recording::default());
            let source = NightLitSource::new(
                Arc::new(RecipeStore::with_regions(vec![(
                    temp.path().join("pkg"),
                    LedRegion::Na,
                )])),
                Arc::new(LaminarArt::new(temp.path().join("art"))),
                Arc::new(FixedTextureSetting(4)),
                LitDiskPermits::new(),
            )
            .with_generator(recording.clone());
            let name = name_at(37.20, -122.725);
            source
                .read(&name, LitLayout::new(16, 4).payload_start(), 1 << 20)
                .await;
            assert_eq!(*recording.0.lock().unwrap(), Some((LedRegion::Na, true)));
        }

        #[tokio::test]
        async fn the_injected_generator_makes_the_payload() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert!(visible.iter().any(|&b| b != 0));
            assert_eq!(white.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        /// Lights every texel white and records the size of every request.
        #[derive(Default)]
        struct Sizes(std::sync::Mutex<Vec<u32>>);

        impl Sizes {
            fn sizes(&self) -> Vec<u32> {
                let mut sizes = self.0.lock().unwrap().clone();
                sizes.sort_unstable();
                sizes
            }
        }

        impl crate::night::generator::LitGenerator for Sizes {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.lock().unwrap().push(r.size);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn the_uploaded_top_level_reads_black_and_generates_nothing() {
            // X-Plane shows the top level under its 3D scenery, for the
            // textures nearest the aircraft; there its objects light the
            // ground, and generated street lights were too coarse.
            let (_t, source) = fixture();
            let sizes = Arc::new(Sizes::default());
            let source = source.with_generator(sizes.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let top = source
                .read(&name, layout.payload_start() - 4096, 4096)
                .await;
            assert_eq!(top.len(), 4096);
            assert!(top.iter().all(|&b| b == 0), "black");
            assert!(sizes.sizes().is_empty(), "nothing generated");
        }

        /// Draws nothing: an empty image, which the encoder refuses.
        #[derive(Default)]
        struct Unencodable(std::sync::atomic::AtomicUsize);

        impl crate::night::generator::LitGenerator for Unencodable {
            fn generate(&self, _: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                image::RgbaImage::new(0, 0)
            }
        }

        #[tokio::test]
        async fn a_payload_whose_encode_fails_is_black_and_not_kept() {
            let (_t, source) = fixture();
            let failing = Arc::new(Unencodable::default());
            let source = source.with_generator(failing.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let payload = source.read(&name, layout.payload_start(), 4096).await;
            assert!(payload.iter().all(|&b| b == 0), "black");
            assert_eq!(source.payloads.entry_count(), 0, "not kept");
            source.read(&name, layout.payload_start(), 4096).await;
            assert_eq!(failing.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn a_read_across_the_top_and_the_payload_writes_only_the_payload_to_disk() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            source.read(&name, layout.payload_start() - 8, 16).await;
            settle(&d, &name).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let key = crate::night::lit_cache_key(&name);
            let entry = d.get(&key).await.unwrap().expect("the payload is written");
            let fp = source.fingerprint(&name).await.unwrap();
            assert!(crate::night::check_entry(entry, &fp, layout.payload_len()).is_ok());
            // The memory provider's count is eventually consistent (moka
            // applies a write on its next housekeeping pass, at most 300 ms
            // after an operation): keep operating until it has caught up.
            for _ in 0..100 {
                if d.entry_count() >= 1 {
                    break;
                }
                let _ = d.get(&key).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(d.entry_count(), 1, "and nothing else");
        }

        #[tokio::test]
        async fn a_read_across_the_top_and_the_payload_lights_only_the_payload() {
            let (_t, source) = fixture();
            let sizes = Arc::new(Sizes::default());
            let source = source.with_generator(sizes.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let bytes = source.read(&name, layout.payload_start() - 8, 16).await;
            assert!(bytes[..8].iter().all(|&b| b == 0), "the top is black");
            assert!(bytes[8..].iter().any(|&b| b != 0), "the payload is lit");
            assert_eq!(sizes.sizes(), [1024]);
        }

        /// Like `White`, but slow enough that concurrent reads overlap.
        #[derive(Default)]
        struct Slow(std::sync::atomic::AtomicUsize);

        impl crate::night::generator::LitGenerator for Slow {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(100));
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn concurrent_reads_share_one_generation() {
            let (_t, source) = fixture();
            let white = Arc::new(Slow::default());
            let source = source.with_generator(white.clone());
            let name = name_at(37.20, -122.725);
            let start = LitLayout::new(16, 4).payload_start();
            let reads = (0..8).map(|_| source.read(&name, start, 4096));
            let results = futures::future::join_all(reads).await;
            assert!(results.iter().all(|r| r.iter().any(|&b| b != 0)));
            assert_eq!(white.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        /// A memory cache standing in for the lit disk tier.
        fn disk() -> Arc<dyn crate::cache::Cache> {
            Arc::new(crate::cache::MemoryCacheProvider::new(64 << 20, None))
        }

        /// Waits for the fire-and-forget disk write of `name` to land.
        async fn settle(disk: &Arc<dyn crate::cache::Cache>, name: &DdsFilename) {
            for _ in 0..50 {
                if let Ok(Some(_)) = disk.get(&crate::night::lit_cache_key(name)).await {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        fn calls(generator: &White) -> usize {
            generator.0.load(std::sync::atomic::Ordering::SeqCst)
        }

        #[tokio::test]
        async fn a_second_session_reads_the_payload_from_disk() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let start = LitLayout::new(16, 4).payload_start();
            let first = Arc::new(White::default());
            let a = source_in(&temp)
                .with_generator(first.clone())
                .with_disk_cache(disk.clone());
            let bytes = a.read(&name, start, 4096).await;
            assert!(bytes.iter().any(|&b| b != 0));
            settle(&disk, &name).await;

            let second = Arc::new(White::default());
            let b = source_in(&temp)
                .with_generator(second.clone())
                .with_disk_cache(disk.clone());
            assert_eq!(b.read(&name, start, 4096).await, bytes);
            assert_eq!(calls(&second), 0, "served from disk");
        }

        /// Like `source_in`, with the package tagged as in `region`.
        fn source_in_region(temp: &TempDir, region: LedRegion) -> NightLitSource {
            let pkg = temp.path().join("pkg");
            let art = temp.path().join("art");
            if !art.exists() {
                write_tree(temp);
            }
            NightLitSource::new(
                Arc::new(RecipeStore::with_regions(vec![(pkg, region)])),
                Arc::new(LaminarArt::new(art)),
                Arc::new(FixedTextureSetting(4)),
                LitDiskPermits::new(),
            )
        }

        #[tokio::test]
        async fn the_package_region_invalidates_lit_disk_entries() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let start = LitLayout::new(16, 4).payload_start();

            let na = Arc::new(White::default());
            let a = source_in_region(&temp, LedRegion::Na)
                .with_generator(na.clone())
                .with_disk_cache(disk.clone());
            a.read(&name, start, 4096).await;
            assert_eq!(calls(&na), 1);
            settle(&disk, &name).await;

            let eu = Arc::new(White::default());
            let b = source_in_region(&temp, LedRegion::Eu)
                .with_generator(eu.clone())
                .with_disk_cache(disk.clone());
            b.read(&name, start, 4096).await;
            assert_eq!(calls(&eu), 1, "another region regenerates");
            // Wait until the Eu entry has replaced the Na one.
            let b_fp = b.fingerprint(&name).await.unwrap();
            let key = crate::night::lit_cache_key(&name);
            for _ in 0..50 {
                let entry = disk.get(&key).await.unwrap().unwrap();
                if crate::night::check_entry(entry, &b_fp, LitLayout::new(16, 4).payload_len())
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            // The Eu entry now sits on disk: a source over Eu is served from
            // it, so the generator count above cannot be a missing cache.
            let again_eu = Arc::new(White::default());
            let d = source_in_region(&temp, LedRegion::Eu)
                .with_generator(again_eu.clone())
                .with_disk_cache(disk.clone());
            d.read(&name, start, 4096).await;
            assert_eq!(calls(&again_eu), 0, "the same region is served from disk");
        }

        #[tokio::test]
        async fn another_brightness_regenerates_and_overwrites() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let a = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            a.read(&name, layout.payload_start(), 4096).await;
            settle(&disk, &name).await;
            let a_fp = a.fingerprint(&name).await.unwrap();

            let second = Arc::new(White::default());
            let b = source_in(&temp)
                .with_brightness(1.25)
                .with_generator(second.clone())
                .with_disk_cache(disk.clone());
            b.read(&name, layout.payload_start(), 4096).await;
            assert_eq!(calls(&second), 1, "regenerated");
            let b_fp = b.fingerprint(&name).await.unwrap();
            assert_ne!(a_fp, b_fp);

            let key = crate::night::lit_cache_key(&name);
            let mut overwritten = false;
            for _ in 0..50 {
                let entry = disk.get(&key).await.unwrap().unwrap();
                if crate::night::check_entry(entry, &b_fp, layout.payload_len()).is_ok() {
                    overwritten = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(overwritten, "the entry carries the new fingerprint");
            let entry = disk.get(&key).await.unwrap().unwrap();
            assert!(crate::night::check_entry(entry, &a_fp, layout.payload_len()).is_err());
        }

        #[tokio::test]
        async fn a_changed_recipe_regenerates() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let start = LitLayout::new(16, 4).payload_start();
            let a = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            a.read(&name, start, 4096).await;
            settle(&disk, &name).await;

            // The same bytes, rewritten 10 s later.
            let path = crate::night::recipe_path(&temp.path().join("pkg"), 37, -123);
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            std::fs::write(&path, URBAN).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime + std::time::Duration::from_secs(10))
                .unwrap();

            let second = Arc::new(White::default());
            let b = source_in(&temp)
                .with_generator(second.clone())
                .with_disk_cache(disk.clone());
            b.read(&name, start, 4096).await;
            assert_eq!(calls(&second), 1, "regenerated");
        }

        #[tokio::test]
        async fn a_recipe_replaced_mid_session_is_not_trusted_next_session() {
            // The first session loads urban.oxn, then the file is replaced
            // while it runs. Its payload is built from urban.oxn, so its
            // entry must not match the replacement in the next session.
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let a = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            assert_eq!(a.size(&name).await, layout.total_len, "recipe loaded");

            let path = crate::night::recipe_path(&temp.path().join("pkg"), 37, -123);
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            std::fs::write(&path, EMPTY).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime + std::time::Duration::from_secs(10))
                .unwrap();

            assert!(a.served_payload(&name, &layout).await.0.is_some());
            settle(&disk, &name).await;
            assert!(disk
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_some());

            let second = Arc::new(White::default());
            let b = source_in(&temp)
                .with_generator(second.clone())
                .with_disk_cache(disk.clone());
            b.served_payload(&name, &layout).await;
            assert_eq!(calls(&second), 1, "regenerated, not decoded");
        }

        /// The fixture's lit texture as the art resolves it (normalized),
        /// which is the path its reads are made with.
        fn fixture_lit_file(temp: &TempDir) -> std::path::PathBuf {
            temp.path().join("art/textures12/urban_FAR/uw5_FAR_LIT.dds")
        }

        /// A session over the fixture tree in `temp` (written when absent)
        /// whose art files are read through `files`.
        fn source_reading_art(
            temp: &TempDir,
            files: Arc<dyn crate::night::ArtFiles>,
        ) -> NightLitSource {
            if !temp.path().join("art").exists() {
                write_tree(temp);
            }
            NightLitSource::new(
                Arc::new(RecipeStore::new(vec![temp.path().join("pkg")])),
                Arc::new(LaminarArt::with_files(temp.path().join("art"), files)),
                Arc::new(FixedTextureSetting(4)),
                LitDiskPermits::new(),
            )
        }

        async fn on_disk(disk: &Arc<dyn crate::cache::Cache>, name: &DdsFilename) -> bool {
            disk.get(&crate::night::lit_cache_key(name))
                .await
                .unwrap()
                .is_some()
        }

        #[tokio::test]
        async fn an_undecodable_lit_texture_keeps_every_payload_on_disk() {
            // A lit texture that reads but does not decode darkens its class
            // the same way every session (the fingerprint covers the file),
            // so payloads drawn with the dark class are written too.
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            std::fs::write(fixture_lit_file(&temp), b"DDS ").unwrap();
            let disk = disk();
            let files = Arc::new(FlakyArtFiles::new(fixture_lit_file(&temp), 0));
            let source = source_reading_art(&temp, files.clone()).with_disk_cache(disk.clone());
            let layout = LitLayout::new(16, 4);
            let (other, uses) = (dark_name(), lit_name());
            let other_bytes = source.served_payload(&other, &layout).await.0.unwrap();
            assert_eq!(files.reads(), 0, "the other footprint does not use it");
            let uses_bytes = source.served_payload(&uses, &layout).await.0.unwrap();
            assert_eq!(files.reads(), 1, "this one does");
            let snap = counted(&source, |s| s.disk_writes == 2).await;
            assert_eq!(snap.write_backs_skipped, [0; 5]);
            assert!(on_disk(&disk, &other).await);
            assert!(on_disk(&disk, &uses).await);

            // The next session reads both back.
            let next_generator = Arc::new(White::default());
            let next = source_in(&temp)
                .with_generator(next_generator.clone())
                .with_disk_cache(disk.clone());
            assert_eq!(
                next.served_payload(&other, &layout).await.0,
                Some(other_bytes)
            );
            assert_eq!(
                next.served_payload(&uses, &layout).await.0,
                Some(uses_bytes)
            );
            assert_eq!(calls(&next_generator), 0, "both from disk");
        }

        #[tokio::test]
        async fn a_lit_texture_whose_first_read_fails_is_lit_and_written() {
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let expected = source_in(&temp).served_payload(&name, &layout).await.0;
            assert!(
                expected.as_ref().unwrap().iter().any(|&b| b != 0),
                "the class lights this footprint"
            );
            let disk = disk();
            let files = Arc::new(FlakyArtFiles::new(fixture_lit_file(&temp), 1));
            let source = source_reading_art(&temp, files.clone()).with_disk_cache(disk.clone());
            let payload = source.served_payload(&name, &layout).await.0;
            assert_eq!(files.reads(), 2, "read again after the failure");
            assert_eq!(payload, expected, "lit as with a clean read");
            counted(&source, |s| s.disk_writes == 1).await;
            assert!(on_disk(&disk, &name).await);
        }

        #[tokio::test]
        async fn a_lit_texture_unreadable_on_every_attempt_keeps_only_its_payloads_off_disk() {
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let disk = disk();
            let files = Arc::new(FlakyArtFiles::new(fixture_lit_file(&temp), usize::MAX));
            let source = source_reading_art(&temp, files.clone()).with_disk_cache(disk.clone());
            let layout = LitLayout::new(16, 4);
            let (other, uses) = (dark_name(), lit_name());
            let log = crate::night::test_log::LogCapture::start();
            assert!(source.served_payload(&uses, &layout).await.0.is_some());
            assert!(source.served_payload(&other, &layout).await.0.is_some());
            counted(&source, |s| s.disk_writes == 1).await;
            let log = log.text();
            assert!(!on_disk(&disk, &uses).await, "may light next session");
            assert!(on_disk(&disk, &other).await, "does not use the file");
            let kept_off: Vec<_> = log
                .lines()
                .filter(|l| l.contains("lit texture kept off disk"))
                .collect();
            assert_eq!(kept_off.len(), 1, "{log}");
            assert!(kept_off[0].contains("uw5_FAR_LIT.dds"), "{log}");
            assert_eq!(
                files.reads(),
                1 + crate::night::retry::READ_RETRIES as usize
            );
            // Kept in memory for the session.
            assert_eq!(
                source.served_payload(&uses, &layout).await.1,
                ReadOutcome::MemoryHit
            );
        }

        #[tokio::test]
        async fn a_lit_texture_outside_the_fingerprint_keeps_its_payloads_off_disk() {
            // Lit, but from a file the fingerprint walk does not cover: it
            // may change without changing the fingerprint.
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let outside = temp.path().join("outside/uw5_FAR_LIT.dds");
            std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
            std::fs::rename(fixture_lit_file(&temp), &outside).unwrap();
            std::fs::write(
                temp.path().join("art/terrain12/u.ter"),
                "TEXTURE_LIT ../../outside/uw5_FAR_LIT.dds\n",
            )
            .unwrap();
            let disk = disk();
            let source = source_in(&temp).with_disk_cache(disk.clone());
            let (other, uses) = (dark_name(), lit_name());
            let log = crate::night::test_log::LogCapture::start();
            let payload = source.served_payload(&uses, &LitLayout::new(16, 4)).await.0;
            assert!(payload.unwrap().iter().any(|&b| b != 0), "lit");
            source.served_payload(&other, &LitLayout::new(16, 4)).await;
            let snap = counted(&source, |s| s.disk_writes == 1).await;
            assert_eq!(snap.write_backs_skipped(WriteBackSkip::ArtFailed), 1);
            assert!(!on_disk(&disk, &uses).await);
            assert!(on_disk(&disk, &other).await);
            let log = log.text();
            assert!(
                log.lines().any(|l| l.contains("kept off disk")
                    && l.contains("not covered by the lit fingerprint")
                    && l.contains("outside/uw5_FAR_LIT.dds")),
                "{log}"
            );
        }

        #[tokio::test]
        async fn a_corrupt_entry_regenerates() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            disk.set(
                &crate::night::lit_cache_key(&name),
                Bytes::from_static(b"XELLgarbage"),
            )
            .await
            .unwrap();
            let white = Arc::new(White::default());
            let source = source_in(&temp)
                .with_generator(white.clone())
                .with_disk_cache(disk.clone());
            let start = LitLayout::new(16, 4).payload_start();
            let bytes = source.read(&name, start, 4096).await;
            assert_eq!(calls(&white), 1, "regenerated");
            assert!(bytes.iter().any(|&b| b != 0));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn an_unreadable_recipe_writes_nothing() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            if !set_recipe_readable(&temp, false) {
                return;
            }
            // Straight to the payload: a read would answer dark at the probe
            // and never reach generation.
            let layout = LitLayout::new(16, 4);
            assert_eq!(
                source.served_payload(&name, &layout).await.0,
                None,
                "served black"
            );
            settle(&disk, &name).await;
            assert!(disk
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
        }

        /// A disk tier that records the most calls it ever had in flight at
        /// once, gets and sets together.
        #[derive(Default)]
        struct Counting {
            inner: Option<crate::cache::MemoryCacheProvider>,
            in_flight: std::sync::atomic::AtomicUsize,
            most: std::sync::atomic::AtomicUsize,
        }

        impl Counting {
            fn new() -> Self {
                Self {
                    inner: Some(crate::cache::MemoryCacheProvider::new(64 << 20, None)),
                    ..Self::default()
                }
            }

            fn inner(&self) -> &crate::cache::MemoryCacheProvider {
                self.inner.as_ref().unwrap()
            }

            /// Runs `call` counted as in flight, held long enough to overlap.
            async fn counted<T>(&self, call: impl std::future::Future<Output = T>) -> T {
                use std::sync::atomic::Ordering::SeqCst;
                let now = self.in_flight.fetch_add(1, SeqCst) + 1;
                self.most.fetch_max(now, SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let out = call.await;
                self.in_flight.fetch_sub(1, SeqCst);
                out
            }
        }

        impl crate::cache::Cache for Counting {
            fn set(
                &self,
                key: &str,
                value: Bytes,
            ) -> futures::future::BoxFuture<'_, Result<(), crate::cache::ServiceCacheError>>
            {
                let key = key.to_owned();
                Box::pin(async move { self.counted(self.inner().set(&key, value)).await })
            }

            fn get(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<
                '_,
                Result<Option<Bytes>, crate::cache::ServiceCacheError>,
            > {
                let key = key.to_owned();
                Box::pin(async move { self.counted(self.inner().get(&key)).await })
            }

            fn delete(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<'_, Result<bool, crate::cache::ServiceCacheError>>
            {
                self.inner().delete(key)
            }

            fn contains(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<'_, Result<bool, crate::cache::ServiceCacheError>>
            {
                self.inner().contains(key)
            }

            fn contains_sync(&self, key: &str) -> bool {
                self.inner().contains_sync(key)
            }

            fn size_bytes(&self) -> u64 {
                self.inner().size_bytes()
            }

            fn entry_count(&self) -> u64 {
                self.inner().entry_count()
            }

            fn max_size_bytes(&self) -> u64 {
                self.inner().max_size_bytes()
            }

            fn set_max_size(
                &self,
                size_bytes: u64,
            ) -> futures::future::BoxFuture<'_, Result<(), crate::cache::ServiceCacheError>>
            {
                self.inner().set_max_size(size_bytes)
            }

            fn gc(
                &self,
            ) -> futures::future::BoxFuture<
                '_,
                Result<crate::cache::GcResult, crate::cache::ServiceCacheError>,
            > {
                self.inner().gc()
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn lit_disk_work_stays_within_its_permits() {
            // Disk reads and write-backs run on the blocking pool; they must
            // stay inside the lit share of its headroom (R17) however many
            // footprints miss memory at once.
            let temp = TempDir::new().unwrap();
            let counting = Arc::new(Counting::new());
            let disk: Arc<dyn crate::cache::Cache> = counting.clone();
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            let layout = LitLayout::new(16, 4);
            let names: Vec<_> = (0..8)
                .map(|i| name_at(37.20, -122.725 + f64::from(i) * 0.02))
                .collect();
            futures::future::join_all(names.iter().map(|n| source.served_payload(n, &layout)))
                .await;
            // Wait for the write-backs through the uncounted inner cache.
            for name in &names {
                let key = crate::night::lit_cache_key(name);
                for _ in 0..50 {
                    if counting.inner().contains_sync(&key) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                assert!(counting.inner().contains_sync(&key), "written back");
            }
            let most = counting.most.load(std::sync::atomic::Ordering::SeqCst);
            assert!(
                most <= LIT_DISK_PERMITS,
                "{most} disk calls in flight at once"
            );
        }

        #[tokio::test]
        async fn a_footprint_beside_lit_ground_is_lit() {
            // The golden urban block's east edge is lon -122.70. A footprint
            // whose west edge is a little east of it holds no lit cell, but
            // the soft edge reaches into it, so it must not be answered dark.
            // ZL22 texture 1629616_667600: west edge -122.69943.
            let (_t, source) = fixture();
            let name = DdsFilename {
                row: 1_629_616,
                col: 667_600,
                zoom: 22,
                map_type: "BI".into(),
                kind: TextureKind::Lit,
            };
            let f = crate::night::Footprint::from_name(&name);
            assert!(f.west > -122.70 && f.west - -122.70 < 1.0 / 1200.0);
            assert_eq!(source.size(&name).await, LitLayout::new(22, 4).total_len);
        }

        #[tokio::test]
        async fn an_unsupported_zoom_is_dark_without_panicking() {
            let (_t, source) = fixture();
            let mut name = name_at(37.20, -122.725);
            name.zoom = 40;
            assert_eq!(source.size(&name).await, 152);
            assert_eq!(source.read(&name, 0, 4096).await, minimal_black_dds());
        }

        // Road lights.

        /// The fixture's tile.
        const TILE: (i32, i32) = (37, -123);

        /// The dark fixture footprint (countryside at 37.60, -122.20).
        fn dark_name() -> DdsFilename {
            name_at(37.60, -122.20)
        }

        /// The lit fixture footprint (urban block at 37.20, -122.725).
        fn lit_name() -> DdsFilename {
            name_at(37.20, -122.725)
        }

        /// An overlay DSF for the fixture tile with one motorway (subtype
        /// 100) north to south at `lon`, past `name`'s footprint at both ends.
        fn road_at(name: &DdsFilename, lon: f64) -> Vec<u8> {
            let f = Footprint::from_name(name);
            let mut b = Builder::new(TILE.0, TILE.1);
            b.props.push(("sim/overlay".into(), "1".into()));
            b.road(
                TILE.0,
                TILE.1,
                100,
                &[(lon, f.north + 0.005), (lon, f.south - 0.005)],
            );
            b.bytes()
        }

        /// A motorway through the middle of `name`'s footprint.
        fn road_across(name: &DdsFilename) -> Vec<u8> {
            let f = Footprint::from_name(name);
            road_at(name, (f.west + f.east) / 2.0)
        }

        /// An X-Plane install under `temp/xp` with one pack `Roads` holding
        /// `dsf` for the fixture tile (`None` writes an unreadable DSF).
        fn xp_install(temp: &TempDir, dsf: Option<Vec<u8>>) -> std::path::PathBuf {
            let root = temp.path().join("xp");
            install(&root, TILE, &[("Roads", dsf)]);
            root
        }

        /// A wait no build outlasts: a test that expects the roads returns
        /// as soon as they are ready.
        const LONG: u64 = 10_000;

        /// A road service over `xp` that waits `probe_ms` for a probe and
        /// `generate_ms` for a generation from the first ask (caps equal to
        /// the waits), its builds held by `gate` when one is given (each
        /// `add_permits(1)` lets one build run).
        fn roads(
            xp: std::path::PathBuf,
            probe_ms: u64,
            generate_ms: u64,
            gate: Option<Arc<tokio::sync::Semaphore>>,
        ) -> Arc<RoadService> {
            let (probe, generate) = (
                Duration::from_millis(probe_ms),
                Duration::from_millis(generate_ms),
            );
            RoadService::with_config(
                xp,
                vec![],
                None,
                LitDiskPermits::new(),
                RoadServiceConfig {
                    probe_wait: probe,
                    probe_cap: probe,
                    generate_wait: generate,
                    generate_cap: generate,
                    gate,
                    ..RoadServiceConfig::default()
                },
            )
        }

        /// A road service over `xp` with one extract permit, whose builds
        /// each take a permit of `hold` once they hold it, and whose probes
        /// wait `probe_ms` from their build's permit, `cap_ms` at most.
        fn held_roads(
            xp: std::path::PathBuf,
            probe_ms: u64,
            cap_ms: u64,
            hold: Arc<tokio::sync::Semaphore>,
        ) -> Arc<RoadService> {
            RoadService::with_config(
                xp,
                vec![],
                None,
                LitDiskPermits::new(),
                RoadServiceConfig {
                    probe_wait: Duration::from_millis(probe_ms),
                    probe_cap: Duration::from_millis(cap_ms),
                    extract_permits: 1,
                    hold: Some(hold),
                    ..RoadServiceConfig::default()
                },
            )
        }

        /// The tile north of the fixture's, which no fixture footprint asks
        /// for: its build is started and holds the only extract permit.
        async fn hold_the_permit(roads: &Arc<RoadService>) {
            let other = (TILE.0 + 1, TILE.1);
            assert!(!road_tiles(&dark_name(), 1024, &RoadModel::DEFAULT).contains(&other));
            roads.ensure(other, Trigger::DsfOpen);
            until("the other build gets its permit", || {
                roads.extract_permits_free() == 0
            })
            .await;
        }

        /// Waits until `done` holds, failing the test (naming `what`) if it
        /// does not within a generous bound.
        async fn until(what: &str, done: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "never: {what}");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        /// The road tiles of `name` as `source` draws them.
        fn tiles_of(source: &NightLitSource, name: &DdsFilename) -> Vec<(i32, i32)> {
            road_tiles(name, source.layout(name).first_visible, &RoadModel::DEFAULT)
        }

        /// Starts the builds of `tiles` and makes their deadlines for
        /// `purpose` already past, as if a caller had asked a whole cap ago.
        fn deadlines_passed(roads: &Arc<RoadService>, tiles: &[(i32, i32)], purpose: Purpose) {
            for &tile in tiles {
                roads.ensure(tile, Trigger::DsfOpen);
                roads.deadline_passed(tile, purpose);
            }
        }

        /// A shut build gate.
        fn shut() -> Arc<tokio::sync::Semaphore> {
            Arc::new(tokio::sync::Semaphore::new(0))
        }

        /// `source_in` with roads from `dsf`, waited for as long as needed.
        fn source_with_roads(
            temp: &TempDir,
            dsf: Option<Vec<u8>>,
        ) -> (NightLitSource, Arc<RoadService>) {
            let roads = roads(xp_install(temp, dsf), LONG, LONG, None);
            (
                source_sharing(temp, &roads).with_roads(roads.clone()),
                roads,
            )
        }

        #[cfg(debug_assertions)]
        #[test]
        #[should_panic(expected = "shares its lit disk permits")]
        fn a_road_service_with_its_own_disk_permits_is_refused() {
            // Two sets of lit disk permits would exceed the night budget.
            let temp = TempDir::new().unwrap();
            let roads =
                RoadService::new(temp.path().join("xp"), vec![], None, LitDiskPermits::new());
            let _ = source_in(&temp).with_roads(roads);
        }

        async fn until_ready(roads: &Arc<RoadService>, tile: (i32, i32)) {
            for _ in 0..1000 {
                if roads.is_ready(tile) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the tile was never extracted");
        }

        #[tokio::test]
        async fn a_dark_footprint_crossed_by_a_road_is_lit() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let (source, _) = source_with_roads(&temp, Some(road_across(&name)));
            let layout = source.layout(&name);
            assert_eq!(source.size(&name).await, layout.total_len);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert!(visible.iter().any(|&b| b != 0), "the road is drawn");
        }

        #[tokio::test]
        async fn without_a_road_the_dark_footprint_stays_minimal() {
            let temp = TempDir::new().unwrap();
            let mut empty = Builder::new(TILE.0, TILE.1);
            empty.nets.push("lib/g10/roads.net".into());
            let (source, roads) = source_with_roads(&temp, Some(empty.bytes()));
            assert_eq!(source.size(&dark_name()).await, 152);
            assert!(roads.is_ready(TILE), "answered with the roads, not before");
            assert_eq!(
                source.size(&lit_name()).await,
                source.layout(&lit_name()).total_len
            );
        }

        #[tokio::test]
        async fn a_road_within_the_glow_reach_lights_a_footprint_and_one_beyond_does_not() {
            // The dark footprint's east edge lies just below a cell boundary
            // of the road map, so a road a quarter of the reach east of it is
            // in the next cell: a test of the bare footprint would miss it.
            let name = dark_name();
            let f = Footprint::from_name(&name);
            let size = LitLayout::new(name.zoom, 4).first_visible;
            let reach = reach_deg(&name, size, &RoadModel::DEFAULT);

            let temp = TempDir::new().unwrap();
            let (near, roads) =
                source_with_roads(&temp, Some(road_at(&name, f.east + reach / 4.0)));
            let tiles = roads.footprint(&[TILE], Purpose::Generate).await.tiles;
            assert!(
                !tiles[0].crosses(f.south, f.west, f.north, f.east),
                "the road is outside the footprint's own cells"
            );
            assert_eq!(near.size(&name).await, near.layout(&name).total_len);
            let visible = near
                .read(&name, near.layout(&name).payload_start(), 1 << 20)
                .await;
            assert!(
                visible.iter().any(|&b| b != 0),
                "its glow reaches the footprint"
            );

            let temp = TempDir::new().unwrap();
            let (far, _) =
                source_with_roads(&temp, Some(road_at(&name, f.east + reach + 2.0 / 256.0)));
            assert_eq!(far.size(&name).await, 152);
        }

        #[tokio::test]
        async fn a_probe_that_times_out_answers_from_the_recipe_and_counts_dark_answers() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            // The gate is shut. Lit by its recipe, one footprint never waits
            // (#428); the dark one asks after its build's deadline passed.
            let lit = lit_name();
            assert_eq!(source.size(&lit).await, source.layout(&lit).total_len);
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Probe);
            let size = tokio::time::timeout(Duration::from_secs(5), source.size(&name)).await;
            assert_eq!(size.ok(), Some(152), "committed without roads");
            let s = roads.stats_snapshot();
            assert_eq!((s.probe_timeouts, s.answered_before_roads), (1, 1));
            // The committed answer holds once the roads are ready (R7).
            gate.add_permits(1);
            until_ready(&roads, TILE).await;
            assert_eq!(source.size(&name).await, 152);
            assert_eq!(source.read(&name, 0, u32::MAX).await, minimal_black_dds());
            assert_eq!(roads.stats_snapshot().answered_before_roads, 1);
        }

        #[tokio::test]
        async fn a_dark_answer_for_a_failed_tile_is_not_counted() {
            let temp = TempDir::new().unwrap();
            let (source, roads) = source_with_roads(&temp, None);
            assert_eq!(source.size(&dark_name()).await, 152);
            let s = roads.stats_snapshot();
            assert_eq!((s.probe_timeouts, s.answered_before_roads), (0, 0));
        }

        #[tokio::test]
        async fn a_generation_with_a_failed_road_tile_is_kept_in_memory_not_on_disk() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let (source, _) = source_with_roads(&temp, None); // unreadable DSF
            let white = Arc::new(White::default());
            let source = source
                .with_generator(white.clone())
                .with_disk_cache(d.clone());
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            assert!(
                source.served_payload(&name, &layout).await.0.is_some(),
                "served"
            );
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
            assert!(source.served_payload(&name, &layout).await.0.is_some());
            assert_eq!(calls(&white), 1, "the failed tile is final: from memory");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_generation_takes_the_roads_that_came_while_it_waited_for_its_permit() {
            // #427: the generation times out on its roads, which are ready
            // by the time it holds its permit: it draws them and is kept.
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let seen = Arc::new(SeesRoads::default());
            let source = source_sharing(&temp, &roads)
                .with_generator(seen.clone())
                .with_disk_cache(d.clone())
                .with_roads(roads.clone());
            // Its roads are still building past their deadline: the
            // generation times out on them at once.
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Generate);
            let held = source.generations.hold_all().await;
            let layout = LitLayout::new(16, 4);
            let ((payload, outcome), ()) =
                tokio::join!(source.served_payload(&name, &layout), async {
                    // Past its road wait: it now waits for a permit.
                    counted(&source, |_| source.stats.generations_waiting.now() == 1).await;
                    assert_eq!(roads.stats_snapshot().generate_timeouts, 1);
                    gate.add_permits(tiles.len());
                    for tile in &tiles {
                        until_ready(&roads, *tile).await;
                    }
                    drop(held);
                });
            assert!(payload.is_some());
            assert_eq!(outcome, ReadOutcome::Generated);
            assert!(seen.0.load(Ordering::SeqCst), "the road field was drawn");
            assert_eq!(seen.1.load(Ordering::SeqCst), 1, "generated once");
            assert!(
                source
                    .payloads
                    .contains(PayloadPart::Tail, (name.row, name.col, name.zoom)),
                "kept in memory"
            );
            let snap = counted(&source, |s| s.disk_writes == 1).await;
            assert!(on_disk(&d, &name).await, "written to the disk tier");
            assert_eq!(snap.write_backs_skipped, [0; 5]);
            assert_eq!(snap.generation_roads(RoadsState::TimedOut), 1, "at its ask");
            assert_eq!(snap.roads_ready_at_permit, 1);
            assert_eq!(
                source.served_payload(&name, &layout).await.1,
                ReadOutcome::MemoryHit
            );
            assert_eq!(seen.1.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn a_recheck_keeps_what_the_wait_had_and_adds_what_came_since() {
            let (a, b, c, d) = ((1, 1), (1, 2), (1, 3), (1, 4));
            let merged = |t| Arc::new(crate::night::roads::merge::MergedRoads::empty(t));
            let ready = |t| TileNow::Ready(ReadyTile::unread(merged(t)));
            let held_a = merged(a);
            let held = FootprintRoads {
                read_from: vec![ReadyTile::unread(Arc::clone(&held_a)).read_from],
                tiles: vec![Arc::clone(&held_a)],
                complete: false,
                timed_out: true,
            };
            let late = recheck(
                &[a, b, c, d],
                held.clone(),
                vec![
                    TileNow::Absent, // evicted since: the wait's copy is kept
                    ready(b),
                    TileNow::Building,
                    TileNow::Failed,
                ],
            );
            let tiles: Vec<_> = late.tiles.iter().map(|r| r.tile).collect();
            assert_eq!(tiles, [a, b], "in the order asked");
            let read: Vec<_> = late.read_from.iter().map(|r| r.tile).collect();
            assert_eq!(read, tiles, "each with what it was read from");
            assert!(Arc::ptr_eq(&late.tiles[0], &held_a));
            assert_eq!(RoadsState::of(&late), RoadsState::TimedOut);

            let failed = recheck(
                &[a, d],
                held.clone(),
                vec![TileNow::Absent, TileNow::Failed],
            );
            assert_eq!(RoadsState::of(&failed), RoadsState::Failed);

            let healed = recheck(&[a, b], held, vec![TileNow::Absent, ready(b)]);
            assert_eq!(RoadsState::of(&healed), RoadsState::Complete);
            assert_eq!(healed.tiles.len(), 2);
        }

        #[tokio::test]
        async fn a_generation_past_its_road_wait_is_kept_nowhere_and_heals() {
            // #427 keeps the rule (its optional hold of such payloads is not
            // taken): a payload whose roads are still building when it holds
            // its generation permit is served and kept nowhere.
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let white = Arc::new(White::default());
            let source = source_sharing(&temp, &roads)
                .with_generator(white.clone())
                .with_disk_cache(d.clone())
                .with_roads(roads.clone());
            let layout = LitLayout::new(16, 4);
            // The gate is shut and the roads' generate deadline has passed:
            // each generation times out on its roads, without a wait.
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Generate);
            assert!(
                source.served_payload(&name, &layout).await.0.is_some(),
                "served"
            );
            assert_eq!(roads.waits_entered(), 0, "no wait past the deadline");
            assert_eq!(roads.stats_snapshot().generate_timeouts, 1);
            assert_eq!(source.payloads.entry_count(), 0, "not in memory");
            assert!(source.served_payload(&name, &layout).await.0.is_some());
            assert_eq!(calls(&white), 2, "generated again");
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
            // Once the roads are ready the payload is complete and kept.
            gate.add_permits(tiles.len());
            for &tile in &tiles {
                until_ready(&roads, tile).await;
            }
            assert!(source.served_payload(&name, &layout).await.0.is_some());
            assert!(source.served_payload(&name, &layout).await.0.is_some());
            assert_eq!(calls(&white), 3, "kept once complete");
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_some());
        }

        #[tokio::test]
        async fn a_recipe_lit_footprint_answers_without_waiting_for_its_roads() {
            // #428: the recipe alone lights it, so the probe does not wait
            // for the build the gate holds, which it has started.
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                FOREVER,
                FOREVER,
                Some(shut()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            assert_eq!(road_tiles(&name, 1024, &RoadModel::DEFAULT), [TILE]);
            // A road wait would last FOREVER: the bound is far below it.
            let size = tokio::time::timeout(Duration::from_secs(5), source.size(&name)).await;
            assert_eq!(size.ok(), Some(source.layout(&name).total_len));
            assert_eq!(roads.builds_pending(), 1, "the build was started");
            let s = roads.stats_snapshot();
            assert_eq!((s.probe_waits, s.probe_timeouts), (0, 0));
            assert_eq!(source.stats.snapshot().probes(ProbeAnswer::Recipe), 1);
        }

        #[tokio::test]
        async fn a_recipe_dark_footprint_waits_for_its_roads() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            let tiles = tiles_of(&source, &name);
            // The gate opens once the probe has asked for its roads, long
            // before its deadline.
            let (size, ()) = tokio::join!(source.size(&name), async {
                until("the probe asks for its roads", || {
                    tiles.iter().all(|&t| roads.is_asked(t, Purpose::Probe))
                })
                .await;
                gate.add_permits(tiles.len());
            });
            assert_eq!(size, source.layout(&name).total_len, "lit by the road");
            let s = roads.stats_snapshot();
            assert_eq!(
                (s.probe_waits, s.probe_timeouts, s.answered_before_roads),
                (1, 0, 0)
            );
            assert_eq!(source.stats.snapshot().probes(ProbeAnswer::Road), 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_probe_waits_from_its_builds_permit_not_from_its_queue() {
            // #429: the probe's tile queues for the extract permit longer
            // than the probe wait; its wait starts when it gets the permit.
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let hold = shut();
            let wait = Duration::from_millis(LONG);
            let roads = held_roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                FOREVER,
                hold.clone(),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            hold_the_permit(&roads).await;
            // The probe's builds, queued behind the held one, were first
            // asked for a wait and a half ago: past the wait, within the cap.
            let tiles = tiles_of(&source, &name);
            for &tile in &tiles {
                roads.ensure(tile, Trigger::DsfOpen);
                roads.asked_ago(tile, Purpose::Probe, wait * 3 / 2);
            }
            let (size, ()) = tokio::join!(source.size(&name), async {
                until("the probe waits for its roads", || {
                    roads.waits_entered() == tiles.len()
                })
                .await;
                // The held build, then the probe's.
                hold.add_permits(1 + tiles.len());
            });
            assert_eq!(size, source.layout(&name).total_len, "lit by the road");
            let s = roads.stats_snapshot();
            assert_eq!(
                (s.probe_waits, s.probe_timeouts, s.answered_before_roads),
                (1, 0, 0)
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_probe_queued_past_its_cap_answers_dark_and_counts_it() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let hold = shut();
            let roads = held_roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                2 * LONG,
                hold.clone(),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            hold_the_permit(&roads).await;
            // The probe's builds, queued behind the held one, were first
            // asked for a whole cap ago.
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Probe);
            // A wait from the permit would outlast the bound: the cap ends it.
            let size = tokio::time::timeout(Duration::from_secs(5), source.size(&name)).await;
            assert_eq!(size.ok(), Some(152), "committed without roads");
            assert_eq!(roads.waits_entered(), 0, "past its cap: no wait");
            let s = roads.stats_snapshot();
            assert_eq!((s.probe_timeouts, s.answered_before_roads), (1, 1));
            hold.add_permits(1 + tiles.len());
        }

        #[tokio::test]
        async fn a_cached_answer_returns_before_any_road_wait() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(shut()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            source.remember((name.row, name.col, name.zoom), false);
            // A road wait would last LONG: the bound is far below it.
            let size = tokio::time::timeout(Duration::from_secs(5), source.size(&name)).await;
            assert_eq!(size.ok(), Some(152));
            assert_eq!(roads.builds_pending(), 0, "no build started");
        }

        #[tokio::test]
        async fn lit_by_a_road_at_the_probe_and_missing_at_generation_is_full_size_black() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            let layout = source.layout(&name);
            let tiles = tiles_of(&source, &name);
            let (size, ()) =
                tokio::join!(source.size(&name), async { gate.add_permits(tiles.len()) });
            assert_eq!(size, layout.total_len, "lit by the road");
            // The tiles are evicted and their rebuilds held, past their
            // generate deadline: generation times out.
            for &tile in &tiles {
                roads.evict(tile).await;
            }
            deadlines_passed(&roads, &tiles, Purpose::Generate);
            let waits = roads.waits_entered();
            let file = source.read(&name, 0, u32::MAX).await;
            assert_eq!(roads.waits_entered(), waits, "the generation did not wait");
            assert_eq!(file.len() as u64, size, "size and read agree");
            let lit = &file[layout.payload_start() as usize..];
            assert!(lit.iter().all(|&b| b == 0), "the payload black");
            assert_eq!(roads.stats_snapshot().generate_timeouts, 1);
        }

        #[test]
        fn a_footprint_on_the_antimeridian_asks_for_the_tile_across_it() {
            // The ZL16 texture whose east edge is the line, at Taveuni.
            let mut name = name_at(-16.8, 179.95);
            name.col = (1 << 16) - 16;
            assert_eq!(Footprint::from_name(&name).east, 180.0);
            let size = LitLayout::new(name.zoom, 4).first_visible;
            let fields = field_tiles(&name);
            let roads = road_tiles(&name, size, &RoadModel::DEFAULT);
            for tiles in [&fields, &roads] {
                assert!(tiles.contains(&(-17, 179)), "{tiles:?}");
                assert!(tiles.contains(&(-17, -180)), "{tiles:?}");
                assert!(
                    tiles.iter().all(|t| (-180..180).contains(&t.1)),
                    "{tiles:?}"
                );
            }
            // And the westmost texture asks for tile 179.
            name.col = 0;
            assert!(field_tiles(&name).contains(&(-17, 179)));
            assert!(road_tiles(&name, size, &RoadModel::DEFAULT).contains(&(-17, 179)));
        }

        /// The ZL16 textures by the line at Taveuni, east edge on the line
        /// and west edge on it, with their footprints and the probe's reach.
        fn textures_at_the_antimeridian() -> [(Footprint, f64); 2] {
            let mut name = name_at(-16.8, 179.95);
            [(1 << 16) - 16, 0].map(|col| {
                name.col = col;
                let size = LitLayout::new(name.zoom, 4).first_visible;
                let reach = reach_deg(&name, size, &RoadModel::DEFAULT);
                (Footprint::from_name(&name), reach)
            })
        }

        /// A north-south road at `lon` in `tile`, past `fp`'s north and south.
        fn road_past(tile: (i32, i32), fp: &Footprint, lon: f64) -> Arc<MergedRoads> {
            crate::night::roads::field::tests::merged(
                tile,
                &[(100, vec![(lon, fp.north + 0.01), (lon, fp.south - 0.01)])],
            )
        }

        #[test]
        fn given_a_road_just_across_the_antimeridian_the_probe_answers_lit() {
            // No recipe anywhere: only the road, 30 m across the line, can
            // light the texture.
            let [(east, east_reach), (west, west_reach)] = textures_at_the_antimeridian();
            assert_eq!((east.east, west.west), (180.0, -180.0));
            let no_recipe = |_: i32, _: i32| None;
            let across_east = [road_past((-17, -180), &east, -179.999_7)];
            assert_eq!(
                probe_answer(
                    recipe_lights(&east, &no_recipe, |_| true),
                    &east,
                    &across_east,
                    east_reach
                ),
                ProbeAnswer::Road
            );
            let across_west = [road_past((-17, 179), &west, 179.999_7)];
            assert_eq!(
                probe_answer(
                    recipe_lights(&west, &no_recipe, |_| true),
                    &west,
                    &across_west,
                    west_reach
                ),
                ProbeAnswer::Road
            );
        }

        #[test]
        fn given_a_recipe_that_lights_the_footprint_the_roads_are_not_consulted() {
            // #428: lit by its recipe, with a road across it, with a road
            // that alone would not light it, or with no roads waited for.
            let [(east, east_reach), _] = textures_at_the_antimeridian();
            let across = road_past((-17, -180), &east, -179.999_7);
            let beyond = road_past((-17, -180), &east, -179.5);
            for road_tiles in [vec![across], vec![beyond], vec![]] {
                assert_eq!(
                    probe_answer(true, &east, &road_tiles, east_reach),
                    ProbeAnswer::Recipe
                );
            }
        }

        #[test]
        fn given_no_road_across_the_antimeridian_the_probe_answers_dark() {
            let [(east, east_reach), (west, west_reach)] = textures_at_the_antimeridian();
            let no_recipe = |_: i32, _: i32| None;
            // The tiles across the line, with no road in them, or with one
            // well beyond the reach.
            for across in [
                Arc::new(crate::night::roads::merge::MergedRoads::empty((-17, -180))),
                road_past((-17, -180), &east, -179.5),
            ] {
                assert_eq!(
                    probe_answer(
                        recipe_lights(&east, &no_recipe, |_| true),
                        &east,
                        &[across],
                        east_reach
                    ),
                    ProbeAnswer::Dark
                );
            }
            for across in [
                Arc::new(crate::night::roads::merge::MergedRoads::empty((-17, 179))),
                road_past((-17, 179), &west, 179.5),
            ] {
                assert_eq!(
                    probe_answer(
                        recipe_lights(&west, &no_recipe, |_| true),
                        &west,
                        &[across],
                        west_reach
                    ),
                    ProbeAnswer::Dark
                );
            }
        }

        #[test]
        fn road_tiles_follow_the_glow_reach_past_the_field_margin() {
            // A wide glow: its reach is far beyond the recipe field's margin.
            let mut model = RoadModel::DEFAULT;
            model.classes[0].sigma_m = 3_000.0;
            // A footprint whose east edge is 0.03 to 0.07 degrees from the
            // next tile, walking east from (37.5, -122.5).
            let mut name = name_at(37.5, -122.5);
            while !(0.03..0.07).contains(
                &(Footprint::from_name(&name).east.ceil() - Footprint::from_name(&name).east),
            ) {
                name.col += 16;
            }
            let size = LitLayout::new(name.zoom, 4).first_visible;
            let reach = reach_deg(&name, size, &model);
            assert!(reach > 0.07, "{reach}");
            let fields = field_tiles(&name);
            let roads = road_tiles(&name, size, &model);
            assert_eq!(fields.len(), 1);
            assert_eq!(roads.len(), 2, "the tile east is within reach");
            assert!(roads.contains(&fields[0]));
            // A coarse texture at a low setting reaches past the margin with
            // the default model too.
            let coarse = name_at_zoom(70.0, 20.0, 12);
            assert!(reach_deg(&coarse, 256, &RoadModel::DEFAULT) > FIELD_MARGIN_DEG);
        }

        #[tokio::test]
        async fn a_generation_with_its_roads_is_written_to_disk() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let (source, _) = source_with_roads(&temp, Some(road_across(&name)));
            let source = source
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone());
            assert!(source
                .served_payload(&name, &LitLayout::new(16, 4))
                .await
                .0
                .is_some());
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_some());
        }

        /// Records whether its request carried road light, and counts its
        /// calls.
        #[derive(Default)]
        struct SeesRoads(
            std::sync::atomic::AtomicBool,
            std::sync::atomic::AtomicUsize,
        );

        impl crate::night::LitGenerator for SeesRoads {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.0
                    .store(!r.roads.is_empty(), std::sync::atomic::Ordering::SeqCst);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn the_generator_gets_the_road_field() {
            let temp = TempDir::new().unwrap();
            let (source, _) = source_with_roads(&temp, Some(road_across(&lit_name())));
            let seen = Arc::new(SeesRoads::default());
            let source = source.with_generator(seen.clone());
            let _ = source
                .served_payload(&lit_name(), &LitLayout::new(16, 4))
                .await
                .0;
            assert!(seen.0.load(std::sync::atomic::Ordering::SeqCst));
        }

        /// Records the size and road field digest of every request it is
        /// handed, and draws nothing (no encode: the field is the point).
        #[derive(Default)]
        struct RoadDigests(std::sync::Mutex<Vec<(u32, u32, u64)>>);

        impl crate::night::LitGenerator for RoadDigests {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                let digest = crate::night::roads::field::tests::field_digest(r.roads);
                self.0
                    .lock()
                    .unwrap()
                    .push((r.size, r.roads.size(), digest));
                image::RgbaImage::new(0, 0)
            }
        }

        #[tokio::test]
        async fn a_capped_texture_gets_its_roads_rastered_at_twice_its_size() {
            // ZL14 at the full setting: 2048 px, its roads at 4096 (#431).
            let temp = TempDir::new().unwrap();
            let name = name_at_zoom(37.20, -122.725, 14);
            let (source, roads) = source_with_roads(&temp, Some(road_across(&name)));
            let seen = Arc::new(RoadDigests::default());
            let source = source.with_generator(seen.clone());
            let layout = source.layout(&name);
            assert_eq!((layout.first_visible, layout.road_supersample()), (2048, 2));
            let _ = source.served_payload(&name, &layout).await;
            let tiles = roads
                .footprint(&tiles_of(&source, &name), Purpose::Generate)
                .await
                .tiles;
            let field = |s| {
                let f = RoadField::raster_supersampled(&name, 2048, s, &tiles, &RoadModel::DEFAULT);
                assert!(!f.is_empty());
                crate::night::roads::field::tests::field_digest(&f)
            };
            assert_ne!(field(2), field(1));
            assert_eq!(*seen.0.lock().unwrap(), [(2048, 2048, field(2))]);
        }

        #[tokio::test]
        async fn a_dsf_lookup_starts_extraction() {
            let temp = TempDir::new().unwrap();
            let (source, roads) = source_with_roads(&temp, Some(road_across(&dark_name())));
            source.dsf_seen(TILE.0, TILE.1);
            until_ready(&roads, TILE).await;
            // The trait default does nothing and must not block.
            BlackLitSource.dsf_seen(TILE.0, TILE.1);
        }

        #[tokio::test]
        async fn road_dsfs_and_road_lights_key_the_disk_entries() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let start = LitLayout::new(16, 4).payload_start();
            let session = |temp: &TempDir| {
                let white = Arc::new(White::default());
                let roads =
                    RoadService::new(temp.path().join("xp"), vec![], None, LitDiskPermits::new());
                let source = source_sharing(temp, &roads)
                    .with_generator(white.clone())
                    .with_disk_cache(d.clone())
                    .with_roads(roads);
                (source, white)
            };
            xp_install(&temp, Some(road_across(&name)));
            let (a, _) = session(&temp);
            a.read(&name, start, 4096).await;
            settle(&d, &name).await;
            let a_fp = a.fingerprint(&name).await.unwrap();

            let (b, white) = session(&temp);
            b.read(&name, start, 4096).await;
            assert_eq!(calls(&white), 0, "same DSFs: served from disk");

            // The same DSF rewritten 10 s later.
            let dsf = temp
                .path()
                .join("xp/Custom Scenery/Roads/Earth nav data/+30-130/+37-123.dsf");
            let mtime = std::fs::metadata(&dsf).unwrap().modified().unwrap();
            std::fs::File::options()
                .write(true)
                .open(&dsf)
                .unwrap()
                .set_modified(mtime + Duration::from_secs(10))
                .unwrap();
            let (c, white) = session(&temp);
            c.read(&name, start, 4096).await;
            assert_eq!(calls(&white), 1, "a changed DSF regenerates");
            assert_ne!(c.fingerprint(&name).await.unwrap(), a_fp);

            let without = source_in(&temp).with_disk_cache(d.clone());
            assert_ne!(
                without.fingerprint(&name).await.unwrap(),
                a_fp,
                "road lights on and off are different inputs"
            );
        }

        /// Records the road segments of every request it is handed.
        #[derive(Default)]
        struct RoadSegments(std::sync::Mutex<Vec<usize>>);

        impl RoadSegments {
            fn seen(&self) -> Vec<usize> {
                self.0.lock().unwrap().clone()
            }
        }

        impl crate::night::LitGenerator for RoadSegments {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.lock().unwrap().push(r.roads.stats.segments);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        /// The fixture tile's road DSF under `temp/xp`.
        fn road_dsf(temp: &TempDir) -> std::path::PathBuf {
            temp.path()
                .join("xp/Custom Scenery/Roads/Earth nav data/+30-130/+37-123.dsf")
        }

        /// Rewrites the road DSF with a road of two segments across `name`
        /// (the first version has one), another length and a later mtime.
        fn rewrite_road_dsf(temp: &TempDir, name: &DdsFilename) {
            let path = road_dsf(temp);
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let f = Footprint::from_name(name);
            let lon = (f.west + f.east) / 2.0;
            let mut b = Builder::new(TILE.0, TILE.1);
            b.props.push(("sim/overlay".into(), "1".into()));
            b.road(
                TILE.0,
                TILE.1,
                100,
                &[
                    (lon, f.north + 0.005),
                    (lon + 0.001, (f.north + f.south) / 2.0),
                    (lon, f.south - 0.005),
                ],
            );
            let bytes = b.bytes();
            assert_ne!(bytes.len() as u64, std::fs::metadata(&path).unwrap().len());
            std::fs::write(&path, bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime + Duration::from_secs(10))
                .unwrap();
        }

        /// A session over the tree in `temp` with roads, the lit disk tier
        /// `d` and a generator recording road segments.
        fn road_session(
            temp: &TempDir,
            d: &Arc<dyn crate::cache::Cache>,
        ) -> (NightLitSource, Arc<RoadService>, Arc<RoadSegments>) {
            let roads =
                RoadService::new(temp.path().join("xp"), vec![], None, LitDiskPermits::new());
            let seen = Arc::new(RoadSegments::default());
            let source = source_sharing(temp, &roads)
                .with_generator(seen.clone())
                .with_disk_cache(d.clone())
                .with_roads(roads.clone());
            (source, roads, seen)
        }

        /// No write-back was ever spawned by `source` (a skipped payload's is
        /// decided before its read returns): none pending at any point of
        /// the window, none finished.
        fn assert_no_write_back(
            source: &NightLitSource,
            snap: &crate::night::stats::LitStatsSnapshot,
        ) {
            assert_eq!((snap.disk_writes, snap.disk_write_failures), (0, 0));
            let window = source.stats.take_window().unwrap();
            assert_eq!(window.write_backs_pending_peak, 0, "none ever pending");
        }

        #[tokio::test]
        async fn roads_read_from_a_dsf_replaced_since_are_not_written_under_its_identity() {
            // #430 case 1: the tile is built from the first DSF, which is
            // replaced before the session's identity pass. The payload has
            // the first DSF's roads, the fingerprint the second's.
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            xp_install(&temp, Some(road_across(&name)));
            let (a, roads, seen) = road_session(&temp, &d);
            roads.ensure(TILE, Trigger::DsfOpen);
            until_ready(&roads, TILE).await;
            rewrite_road_dsf(&temp, &name);
            let (payload, outcome) = a.served_payload(&name, &layout).await;
            assert!(payload.is_some(), "served");
            assert_eq!(outcome, ReadOutcome::Generated);
            assert_eq!(seen.seen(), [1], "the first DSF's road");
            let snap = a.stats.snapshot();
            assert_eq!(
                snap.write_backs_skipped(WriteBackSkip::RoadSourcesChanged),
                1
            );
            assert_eq!(snap.write_backs_skipped, [0, 0, 0, 1, 0], "and no other");
            assert_no_write_back(&a, &snap);
            assert!(!on_disk(&d, &name).await, "no entry written");
            assert_eq!(
                a.served_payload(&name, &layout).await.1,
                ReadOutcome::MemoryHit,
                "kept in memory"
            );
            // Another footprint over the tile is kept off disk too; the tile
            // is noted for its log line once.
            assert_eq!(a.road_sources_logged.len(), 1);
            let other = dark_name();
            assert_eq!(tiles_of(&a, &other), tiles_of(&a, &name));
            assert!(a.served_payload(&other, &layout).await.0.is_some());
            assert_eq!(
                a.stats
                    .snapshot()
                    .write_backs_skipped(WriteBackSkip::RoadSourcesChanged),
                2
            );
            assert_eq!(a.road_sources_logged.len(), 1, "logged once per tile");
            assert!(a.road_sources_logged.contains(&TILE));

            // The next session reads the second DSF and generates with it.
            let (b, _, seen) = road_session(&temp, &d);
            assert_eq!(
                b.served_payload(&name, &layout).await.1,
                ReadOutcome::Generated
            );
            assert_eq!(seen.seen(), [2], "the second DSF's road");
            counted(&b, |s| s.disk_writes == 1).await;
        }

        #[tokio::test]
        async fn a_tile_rebuilt_from_a_replaced_dsf_is_not_written_under_the_old_identity() {
            // #430 case 2: the identity pass sees the first DSF; the file is
            // replaced, the tile evicted and rebuilt from the second.
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            xp_install(&temp, Some(road_across(&name)));
            let (a, roads, seen) = road_session(&temp, &d);
            roads.ensure(TILE, Trigger::DsfOpen);
            until_ready(&roads, TILE).await;
            let first = a.fingerprint(&name).await.unwrap();
            rewrite_road_dsf(&temp, &name);
            roads.evict(TILE).await;
            assert!(a.served_payload(&name, &layout).await.0.is_some());
            assert_eq!(seen.seen(), [2], "drawn from the rebuild");
            assert_eq!(a.fingerprint(&name).await.unwrap(), first, "the old one");
            let snap = a.stats.snapshot();
            assert_eq!(
                snap.write_backs_skipped(WriteBackSkip::RoadSourcesChanged),
                1
            );
            assert_no_write_back(&a, &snap);
            assert!(!on_disk(&d, &name).await, "no entry written");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn a_road_dsf_that_cannot_be_stated_leaves_no_fingerprint() {
            use std::os::unix::fs::PermissionsExt;
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let xp = xp_install(&temp, Some(road_across(&name)));
            let roads = RoadService::new(xp.clone(), vec![], None, LitDiskPermits::new());
            let source = source_sharing(&temp, &roads)
                .with_disk_cache(disk())
                .with_roads(roads.clone());
            assert!(roads.identities((10, 10)).await.is_some(), "index built");
            let block = xp.join("Custom Scenery/Roads/Earth nav data/+30-130");
            let mode = |m| std::fs::set_permissions(&block, std::fs::Permissions::from_mode(m));
            mode(0o000).unwrap();
            if std::fs::read_dir(&block).is_ok() {
                mode(0o755).unwrap();
                return; // root: permissions do not apply
            }
            let fp = source.fingerprint(&name).await;
            mode(0o755).unwrap();
            assert_eq!(fp, None, "not the fingerprint of a tile without DSFs");
            assert!(source.fingerprint(&name).await.is_some());
        }

        /// A wait no test outlasts: a caller stuck behind it fails its
        /// test's own bound instead of finishing late.
        const FOREVER: u64 = 600_000;

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn probes_wait_for_roads_without_a_probe_permit() {
            // Probes in the fixture tile wait for roads whose build the gate
            // holds. If they held the probe permits meanwhile, a probe of
            // another tile (no recipe, no DSF, roads ready: dark at once)
            // would queue behind them until the gate opens.
            let temp = TempDir::new().unwrap();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&dark_name()))),
                FOREVER,
                FOREVER,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads).with_roads(roads.clone());
            let other = name_at(38.5, -122.5);
            let (ready, ()) = tokio::join!(roads.footprint(&[(38, -123)], Purpose::Probe), async {
                gate.add_permits(1)
            });
            assert!(ready.complete);
            let waiting: Vec<_> = (0..2 * LIT_PROBE_PERMITS)
                .map(|i| name_at(37.60, -122.20 - 0.1 * i as f64))
                .collect();
            let (_, ()) = tokio::join!(
                futures::future::join_all(waiting.iter().map(|n| source.size(n))),
                async {
                    // The bound is far below the waiters' wait: only a probe
                    // queued behind them could reach it.
                    let size =
                        tokio::time::timeout(Duration::from_secs(5), source.size(&other)).await;
                    assert_eq!(size.ok(), Some(152));
                    gate.add_permits(1);
                }
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn generations_wait_for_roads_without_a_generation_permit() {
            let temp = TempDir::new().unwrap();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&lit_name()))),
                FOREVER,
                FOREVER,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads)
                .with_generator(Arc::new(White::default()))
                .with_roads(roads.clone());
            let layout = LitLayout::new(16, 4);
            // ZL18: a small texture, so its own generation is quick.
            let other = name_at_zoom(38.5, -122.5, 18);
            let (ready, ()) =
                tokio::join!(roads.footprint(&[(38, -123)], Purpose::Generate), async {
                    gate.add_permits(1)
                });
            assert!(ready.complete);
            let waiting: Vec<_> = (0..2 * MAX_LIT_GENERATION_PERMITS)
                .map(|i| name_at(37.20, -122.725 + 0.05 * i as f64))
                .collect();
            let (_, ()) = tokio::join!(
                futures::future::join_all(
                    waiting.iter().map(|n| source.served_payload(n, &layout))
                ),
                async {
                    let payload = tokio::time::timeout(
                        Duration::from_secs(5),
                        source.served_payload(&other, &LitLayout::new(18, 4)),
                    )
                    .await;
                    assert!(payload.is_ok_and(|p| p.0.is_some()));
                    gate.add_permits(1);
                }
            );
        }

        // Statistics (#426).

        /// Waits until `done` holds for the source's counters.
        async fn counted(
            source: &NightLitSource,
            done: impl Fn(&crate::night::stats::LitStatsSnapshot) -> bool,
        ) -> crate::night::stats::LitStatsSnapshot {
            for _ in 0..200 {
                let snap = source.stats.snapshot();
                if done(&snap) {
                    return snap;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("never counted: {:?}", source.stats.snapshot());
        }

        #[tokio::test]
        async fn a_second_read_of_a_payload_counts_a_memory_hit() {
            let (_t, source) = fixture();
            let source = source.with_generator(Arc::new(White::default()));
            let name = lit_name();
            let start = LitLayout::new(16, 4).payload_start();
            source.read(&name, start, 4096).await;
            source.read(&name, start, 4096).await;
            source.read(&name, 0, 128).await;
            source.read(&dark_name(), 0, 128).await;
            let snap = source.stats.snapshot();
            assert_eq!(snap.reads(ReadOutcome::Generated), 1);
            assert_eq!(snap.reads(ReadOutcome::MemoryHit), 1);
            assert_eq!(snap.reads(ReadOutcome::Blank), 1);
            assert_eq!(snap.reads(ReadOutcome::Dark), 1);
            // The reads at the start of the payload want its head alone.
            assert_eq!((snap.payload_hits, snap.payload_misses), ([1, 0], [1, 0]));
            assert_eq!(snap.generations(), 1);
            assert_eq!(snap.generation_roads(RoadsState::Complete), 1);
            let w = source.stats.take_window().unwrap();
            assert_eq!(w.read(ReadOutcome::MemoryHit).unwrap().count, 1);
            assert_eq!(w.generation.unwrap().count, 1);
        }

        #[tokio::test]
        async fn a_read_that_joins_another_reads_load_counts_as_joined() {
            let (_t, source) = fixture();
            let source = source.with_generator(Arc::new(White::default()));
            let name = lit_name();
            let start = LitLayout::new(16, 4).payload_start();
            // The probe answered first, so each read goes straight to the
            // payload in its first poll.
            assert_eq!(source.size(&name).await, source.layout(&name).total_len);
            let held = source.generations.hold_all().await;
            // The first read runs the load and waits for a generation permit;
            // the second, polled next, finds the load in flight and joins it.
            let (first, second, ()) = tokio::join!(
                source.read(&name, start, 4096),
                source.read(&name, start, 4096),
                async {
                    counted(&source, |_| source.stats.generations_waiting.now() == 1).await;
                    drop(held);
                }
            );
            assert_eq!(first, second);
            let snap = source.stats.snapshot();
            assert_eq!(snap.reads(ReadOutcome::Generated), 1);
            assert_eq!(snap.reads(ReadOutcome::Joined), 1);
            assert_eq!(snap.reads(ReadOutcome::MemoryHit), 0);
            assert_eq!(
                (snap.payload_hits, snap.payload_joins, snap.payload_misses),
                ([0, 0], [1, 0], [1, 0])
            );
            // Now cached: a true hit.
            source.read(&name, start, 4096).await;
            assert_eq!(source.stats.snapshot().payload_hits, [1, 0]);
        }

        // Payload parts in memory (#432).

        /// A lit disk tier that counts its gets and sets; with a gate, each
        /// set first waits for one of its permits (the writes blocked until
        /// the test adds them).
        struct Recorded {
            inner: crate::cache::MemoryCacheProvider,
            gets: std::sync::atomic::AtomicUsize,
            sets: std::sync::atomic::AtomicUsize,
            gate: Option<Arc<tokio::sync::Semaphore>>,
            /// The parts of every value written in parts, in order.
            written_parts: std::sync::Mutex<Vec<Vec<Bytes>>>,
        }

        impl Recorded {
            fn new(gate: Option<Arc<tokio::sync::Semaphore>>) -> Arc<Self> {
                Arc::new(Self {
                    inner: crate::cache::MemoryCacheProvider::new(64 << 20, None),
                    gets: Default::default(),
                    sets: Default::default(),
                    gate,
                    written_parts: Default::default(),
                })
            }

            fn gets(&self) -> usize {
                self.gets.load(std::sync::atomic::Ordering::SeqCst)
            }
        }

        impl crate::cache::Cache for Recorded {
            fn set(
                &self,
                key: &str,
                value: Bytes,
            ) -> futures::future::BoxFuture<'_, Result<(), crate::cache::ServiceCacheError>>
            {
                let key = key.to_owned();
                Box::pin(async move {
                    self.sets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _open = match &self.gate {
                        Some(gate) => Some(gate.acquire().await.unwrap()),
                        None => None,
                    };
                    self.inner.set(&key, value).await
                })
            }

            fn set_parts(
                &self,
                key: &str,
                parts: Vec<Bytes>,
            ) -> futures::future::BoxFuture<'_, Result<(), crate::cache::ServiceCacheError>>
            {
                self.written_parts.lock().unwrap().push(parts.clone());
                let joined = Bytes::from(parts.concat());
                self.set(key, joined)
            }

            fn get(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<
                '_,
                Result<Option<Bytes>, crate::cache::ServiceCacheError>,
            > {
                self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.get(key)
            }

            fn delete(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<'_, Result<bool, crate::cache::ServiceCacheError>>
            {
                self.inner.delete(key)
            }

            fn contains(
                &self,
                key: &str,
            ) -> futures::future::BoxFuture<'_, Result<bool, crate::cache::ServiceCacheError>>
            {
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

            fn set_max_size(
                &self,
                size_bytes: u64,
            ) -> futures::future::BoxFuture<'_, Result<(), crate::cache::ServiceCacheError>>
            {
                self.inner.set_max_size(size_bytes)
            }

            fn gc(
                &self,
            ) -> futures::future::BoxFuture<
                '_,
                Result<crate::cache::GcResult, crate::cache::ServiceCacheError>,
            > {
                self.inner.gc()
            }
        }

        /// The `i`th ZL18 footprint east of the lit fixture's: small
        /// payloads (a 256 px first visible level), cheap to generate.
        fn small_name(i: u32) -> DdsFilename {
            let base = name_at_zoom(37.20, -122.725, 18);
            DdsFilename {
                col: base.col + 16 * i,
                ..base
            }
        }

        /// What a read of the levels below the first visible one wants.
        fn small_levels(layout: &LitLayout) -> Wanted {
            let tail = layout.part_range(PayloadPart::Tail);
            Wanted::reached(layout, tail.start, (tail.end - tail.start) as u32)
        }

        /// What a read of the first visible level wants.
        fn first_visible(layout: &LitLayout) -> Wanted {
            let head = layout.part_range(PayloadPart::Head);
            Wanted::reached(layout, head.start, (head.end - head.start) as u32)
        }

        #[tokio::test]
        async fn the_small_levels_of_as_many_footprints_as_their_budget_holds_stay_in_memory() {
            // A budget for N tails, less than one whole ZL18 payload: before
            // the split it held none. N + 1 footprints read through their
            // small levels evict the least recently read tail alone.
            const N: u32 = 3;
            let temp = TempDir::new().unwrap();
            let (white, disk) = (Arc::new(White::default()), Recorded::new(None));
            let layout = LitLayout::new(18, 4);
            let (head_len, tail_len) = (
                layout.part_len(PayloadPart::Head),
                layout.part_len(PayloadPart::Tail),
            );
            assert!(u64::from(N) * tail_len < layout.payload_len());
            let source = source_in(&temp)
                .with_generator(white.clone())
                .with_disk_cache(disk.clone())
                .with_payload_budgets(head_len, u64::from(N) * tail_len);
            for i in 0..=N {
                let (parts, outcome) = source
                    .served_parts(&small_name(i), &layout, small_levels(&layout))
                    .await;
                assert_eq!(outcome, ReadOutcome::Generated, "{i}");
                assert!(parts.get(PayloadPart::Tail).is_some());
                source.payloads.run_pending_tasks();
            }
            let (gets, made) = (disk.gets(), calls(&white));
            for i in 1..=N {
                let (_, outcome) = source
                    .served_parts(&small_name(i), &layout, small_levels(&layout))
                    .await;
                assert_eq!(outcome, ReadOutcome::MemoryHit, "{i}");
            }
            assert_eq!(disk.gets(), gets, "the disk tier was not asked");
            assert_eq!(calls(&white), made, "nothing generated");
            let (_, first) = source
                .served_parts(&small_name(0), &layout, small_levels(&layout))
                .await;
            assert_ne!(first, ReadOutcome::MemoryHit, "the first made room");
        }

        #[tokio::test]
        async fn a_first_visible_level_read_after_a_small_level_read_is_the_whole_payloads() {
            // The small levels' load kept the tail alone; the first visible
            // level comes from the disk tier, as one whole-payload read did
            // before the split, and is kept from then on.
            let temp = TempDir::new().unwrap();
            let (white, disk) = (Arc::new(White::default()), Recorded::new(None));
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let whole = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .served_payload(&name, &layout)
                .await
                .0
                .unwrap();
            let expected = layout.read_lit(0, u32::MAX, &layout.parts_of(&whole));
            let source = source_in(&temp)
                .with_generator(white.clone())
                .with_disk_cache(disk.clone());
            let (_, outcome) = source
                .served_parts(&name, &layout, small_levels(&layout))
                .await;
            assert_eq!(outcome, ReadOutcome::Generated);
            let key = (name.row, name.col, name.zoom);
            assert!(!source.payloads.contains(PayloadPart::Head, key));
            counted(&source, |s| s.disk_writes == 1).await;
            let (parts, outcome) = source
                .served_parts(&name, &layout, first_visible(&layout))
                .await;
            assert_eq!(outcome, ReadOutcome::DiskHit);
            assert_eq!(calls(&white), 1, "not generated again");
            let head = layout.part_range(PayloadPart::Head);
            assert_eq!(
                &layout.read_lit(head.start, (head.end - head.start) as u32, &parts)[..],
                &expected[head.start as usize..head.end as usize]
            );
            assert!(source.payloads.contains(PayloadPart::Head, key), "kept");
            let (_, again) = source
                .served_parts(&name, &layout, first_visible(&layout))
                .await;
            assert_eq!(again, ReadOutcome::MemoryHit);
            let snap = source.stats.snapshot();
            assert_eq!(snap.payload_misses, [1, 1]);
            assert_eq!(snap.payload_hits, [1, 0]);
        }

        #[tokio::test]
        async fn without_a_disk_tier_a_first_visible_level_read_generates_it_again() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone());
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let (small, _) = source
                .served_parts(&name, &layout, small_levels(&layout))
                .await;
            let (parts, outcome) = source
                .served_parts(&name, &layout, first_visible(&layout))
                .await;
            assert_eq!(outcome, ReadOutcome::Generated);
            assert_eq!(calls(&white), 2);
            let (head, tail) = (PayloadPart::Head, PayloadPart::Tail);
            assert_eq!(parts.get(tail), small.get(tail), "the same payload");
            assert!(parts.get(head).is_some_and(|h| h.iter().any(|&b| b != 0)));
        }

        #[tokio::test]
        async fn reads_split_across_the_part_boundary_give_one_reads_bytes() {
            let (_t, source) = fixture();
            let source = source.with_generator(Arc::new(White::default()));
            let name = small_name(0);
            assert_eq!(source.size(&name).await, source.layout(&name).total_len);
            let layout = source.layout(&name);
            let start = layout.payload_start();
            let boundary = layout.part_range(PayloadPart::Tail).start;
            let one = source
                .read(&name, start, (layout.total_len - start) as u32)
                .await;
            for cut in [boundary - 8, boundary, boundary + 8] {
                // A fresh source each time, so the pieces load as they come.
                let (_t, fresh) = fixture();
                let fresh = fresh.with_generator(Arc::new(White::default()));
                let tail = fresh
                    .read(&name, cut, (layout.total_len - cut) as u32)
                    .await;
                let head = fresh.read(&name, start, (cut - start) as u32).await;
                assert_eq!([&head[..], &tail[..]].concat(), &one[..], "cut at {cut}");
            }
        }

        #[tokio::test]
        async fn a_first_visible_level_read_joining_a_small_level_reads_load_generates_once() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone());
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let held = source.generations.hold_all().await;
            // The small levels' read runs the load and waits for a permit;
            // the first visible level's, polled next, joins it.
            let ((_, small), (head, joined), ()) = tokio::join!(
                source.served_parts(&name, &layout, small_levels(&layout)),
                source.served_parts(&name, &layout, first_visible(&layout)),
                async {
                    counted(&source, |_| source.stats.generations_waiting.now() == 1).await;
                    drop(held);
                }
            );
            assert_eq!(
                (small, joined),
                (ReadOutcome::Generated, ReadOutcome::Joined)
            );
            assert_eq!(calls(&white), 1, "one generation");
            assert!(head.get(PayloadPart::Head).is_some());
            let key = (name.row, name.col, name.zoom);
            assert!(source.payloads.contains(PayloadPart::Head, key));
            assert!(source.payloads.contains(PayloadPart::Tail, key));
        }

        // Generation permits by priority (#433).

        /// Lights every texel white and records the column of every
        /// footprint it generates, in order.
        #[derive(Default)]
        struct Order(std::sync::Mutex<Vec<u32>>);

        impl crate::night::generator::LitGenerator for Order {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.lock().unwrap().push(r.name.col);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn a_free_permit_goes_to_a_first_visible_level_read_before_a_small_level_read() {
            let (_t, source) = fixture();
            let order = Arc::new(Order::default());
            let source = source.with_generator(order.clone());
            let layout = LitLayout::new(18, 4);
            let (small, near) = (small_name(0), small_name(1));
            let mut held = source.generations.hold_all().await;
            // The small levels' read queues first.
            let ((_, small_read), (_, near_read), ()) = tokio::join!(
                source.served_parts(&small, &layout, small_levels(&layout)),
                source.served_parts(&near, &layout, first_visible(&layout)),
                async {
                    counted(&source, |_| source.stats.generations_waiting.now() == 2).await;
                    held.pop();
                }
            );
            assert_eq!(
                (small_read, near_read),
                (ReadOutcome::Generated, ReadOutcome::Generated)
            );
            assert_eq!(*order.0.lock().unwrap(), [near.col, small.col]);
            let snap = source.stats.snapshot();
            assert_eq!(snap.generation_class(GenerationClass::HeadRead), 1);
            assert_eq!(snap.generation_class(GenerationClass::TailRead), 1);
        }

        #[tokio::test]
        async fn a_first_visible_level_read_joining_a_small_level_reads_load_raises_it() {
            let (_t, source) = fixture();
            let order = Arc::new(Order::default());
            let source = source.with_generator(order.clone());
            let layout = LitLayout::new(18, 4);
            let (shared, other) = (small_name(0), small_name(1));
            let mut held = source.generations.hold_all().await;
            // `shared`'s small levels queue first, then `other`'s; the read
            // of `shared`'s first visible level joins the first load and
            // takes it ahead of `other`'s.
            let ((_, first), (_, second), (_, joined), ()) = tokio::join!(
                source.served_parts(&shared, &layout, small_levels(&layout)),
                source.served_parts(&other, &layout, small_levels(&layout)),
                source.served_parts(&shared, &layout, first_visible(&layout)),
                async {
                    counted(&source, |_| source.stats.generations_waiting.now() == 2).await;
                    held.pop();
                }
            );
            assert_eq!(
                (first, second, joined),
                (
                    ReadOutcome::Generated,
                    ReadOutcome::Generated,
                    ReadOutcome::Joined
                )
            );
            assert_eq!(*order.0.lock().unwrap(), [shared.col, other.col]);
            assert_eq!(
                source
                    .stats
                    .snapshot()
                    .generation_class(GenerationClass::HeadRead),
                1,
                "admitted as the read that joined it"
            );
        }

        // Pre-generation on probe (#433).

        fn key_of(name: &DdsFilename) -> (u32, u32, u8) {
            (name.row, name.col, name.zoom)
        }

        /// Waits until `done` holds for the source's counters: background
        /// work, a generation among it, with a generous guard (a debug
        /// build's encode under a loaded test run takes seconds).
        async fn settled(
            source: &NightLitSource,
            done: impl Fn(&crate::night::stats::LitStatsSnapshot) -> bool,
        ) -> crate::night::stats::LitStatsSnapshot {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let snap = source.stats.snapshot();
                if done(&snap) {
                    return snap;
                }
                assert!(Instant::now() < deadline, "never counted: {snap:?}");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        #[test]
        fn a_pregenerations_outcome_follows_how_its_small_levels_were_found() {
            use PayloadLookup::{Hit, Joined, Loaded};
            use PregenOutcome as P;
            let cases = [
                ((Hit, true, false, false), P::InMemory),
                ((Joined, true, false, false), P::Joined),
                ((Joined, false, false, false), P::Joined),
                ((Loaded, false, false, false), P::Black),
                ((Loaded, true, false, false), P::Uncached),
                ((Loaded, true, false, true), P::Generated),
                ((Loaded, true, true, true), P::OnDisk),
            ];
            for ((lookup, payload, from_disk, kept), want) in cases {
                assert_eq!(
                    pregen_outcome(lookup, payload, from_disk, kept),
                    want,
                    "{lookup:?} payload {payload} disk {from_disk} kept {kept}"
                );
            }
        }

        #[tokio::test]
        async fn a_footprint_probed_lit_is_pregenerated_and_read_from_memory() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone()).shared();
            let name = lit_name();
            let layout = source.layout(&name);
            assert_eq!(source.size(&name).await, layout.total_len);
            let snap = settled(&source, |s| s.pregenerations(PregenOutcome::Generated) == 1).await;
            assert_eq!((snap.pregen_queued, snap.pregen_started), (1, 1));
            assert_eq!(snap.generation_class(GenerationClass::Pregeneration), 1);
            assert_eq!(snap.reads, [0; 7], "no read yet");
            let key = key_of(&name);
            assert!(source.payloads.contains(PayloadPart::Tail, key));
            assert!(
                !source.payloads.contains(PayloadPart::Head, key),
                "no read reached the first visible level"
            );
            let tail = layout.part_range(PayloadPart::Tail);
            let bytes = source.read(&name, tail.start, 4096).await;
            assert!(bytes.iter().any(|&b| b != 0));
            let snap = source.stats.snapshot();
            assert_eq!(snap.reads(ReadOutcome::MemoryHit), 1);
            assert_eq!(calls(&white), 1, "generated once, before the read");
        }

        #[tokio::test]
        async fn a_source_not_shared_does_not_pregenerate() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone());
            let name = lit_name();
            assert_eq!(source.size(&name).await, source.layout(&name).total_len);
            tokio::task::yield_now().await;
            assert_eq!(source.stats.snapshot().pregen_queued, 0);
            assert_eq!(calls(&white), 0);
        }

        #[tokio::test]
        async fn every_permit_held_and_a_pregeneration_queued_the_next_permit_goes_to_a_read() {
            let (_t, source) = fixture();
            let order = Arc::new(Order::default());
            let source = source.with_generator(order.clone()).shared();
            let layout = LitLayout::new(16, 4);
            let probed = lit_name();
            let read = DdsFilename {
                col: probed.col + 16,
                ..probed.clone()
            };
            let mut held = source.generations.hold_all().await;
            assert_eq!(source.size(&probed).await, layout.total_len);
            settled(&source, |_| {
                source.generations.waiting(GenerationClass::Pregeneration) == 1
            })
            .await;
            let ((_, outcome), ()) = tokio::join!(
                source.served_parts(&read, &layout, small_levels(&layout)),
                async {
                    settled(&source, |_| source.stats.generations_waiting.now() == 2).await;
                    held.pop();
                }
            );
            assert_eq!(outcome, ReadOutcome::Generated);
            // The pre-generation takes the read's permit once it is done.
            drop(held);
            settled(&source, |s| s.pregenerations(PregenOutcome::Generated) == 1).await;
            assert_eq!(
                *order.0.lock().unwrap(),
                [read.col, probed.col],
                "the read first"
            );
        }

        #[tokio::test]
        async fn a_read_arriving_during_a_pregeneration_of_its_footprint_generates_nothing() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone()).shared();
            let name = lit_name();
            let layout = source.layout(&name);
            let held = source.generations.hold_all().await;
            assert_eq!(source.size(&name).await, layout.total_len);
            settled(&source, |_| {
                source.generations.waiting(GenerationClass::Pregeneration) == 1
            })
            .await;
            let (bytes, ()) =
                tokio::join!(source.read(&name, layout.payload_start(), 4096), async {
                    // The read joined the pre-generation's load and raised it.
                    settled(&source, |_| {
                        source.generations.waiting(GenerationClass::HeadRead) == 1
                    })
                    .await;
                    drop(held);
                });
            assert!(bytes.iter().any(|&b| b != 0));
            let snap = settled(&source, |s| s.pregenerations(PregenOutcome::Generated) == 1).await;
            assert_eq!(snap.reads(ReadOutcome::Joined), 1);
            assert_eq!(calls(&white), 1, "one generation");
            assert_eq!(
                snap.generation_class(GenerationClass::HeadRead),
                1,
                "admitted as the read"
            );
            let key = key_of(&name);
            assert!(
                source.payloads.contains(PayloadPart::Head, key),
                "the read's"
            );
            assert!(source.payloads.contains(PayloadPart::Tail, key));
        }

        #[tokio::test]
        async fn a_pregeneration_of_a_footprint_in_memory_loads_nothing() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone()).shared();
            let name = lit_name();
            let layout = source.layout(&name);
            source
                .served_parts(&name, &layout, small_levels(&layout))
                .await;
            assert_eq!(source.size(&name).await, layout.total_len);
            let snap = settled(&source, |s| s.pregenerations(PregenOutcome::InMemory) == 1).await;
            assert_eq!(snap.generations(), 1, "the read's alone");
            assert_eq!(calls(&white), 1);
        }

        #[tokio::test]
        async fn a_pregeneration_of_a_footprint_on_disk_loads_it_and_generates_nothing() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let first = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone());
            first.served_payload(&name, &layout).await;
            settled(&first, |s| s.disk_writes == 1).await;

            let second = Arc::new(White::default());
            let source = source_in(&temp)
                .with_generator(second.clone())
                .with_disk_cache(d.clone())
                .shared();
            assert_eq!(source.size(&name).await, layout.total_len);
            let snap = settled(&source, |s| s.pregenerations(PregenOutcome::OnDisk) == 1).await;
            assert_eq!((snap.disk_hits, snap.generations()), (1, 0));
            assert_eq!(calls(&second), 0);
            let tail = layout.part_range(PayloadPart::Tail);
            source.read(&name, tail.start, 4096).await;
            assert_eq!(source.stats.snapshot().reads(ReadOutcome::MemoryHit), 1);
        }

        #[tokio::test]
        async fn a_pregenerated_payload_is_written_to_disk() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone())
                .shared();
            let name = lit_name();
            source.size(&name).await;
            settled(&source, |s| s.disk_writes == 1).await;
            let fp = source.fingerprint(&name).await.unwrap();
            let entry = d.get(&crate::night::lit_cache_key(&name)).await.unwrap();
            let layout = source.layout(&name);
            assert!(crate::night::check_entry(entry.unwrap(), &fp, layout.payload_len()).is_ok());
        }

        #[test]
        fn a_source_holds_the_floor_until_given_a_budget() {
            let (_t, source) = fixture();
            assert_eq!(source.payloads.budget(), LitMemoryBudget::FLOOR);
            assert_eq!(
                source.pregens.bound(),
                crate::night::pregen::MAX_QUEUED_PREGENERATIONS
            );
        }

        #[test]
        fn a_memory_budget_sizes_the_payload_memory_and_the_pregeneration_bound() {
            let tail = crate::night::layout::tail_len(crate::night::layout::FIRST_VISIBLE_ZL16);
            let budget = LitMemoryBudget {
                tail: 600 * tail,
                head: 7 * 524_288,
            };
            let (_t, source) = fixture();
            let source = source.with_memory_budget(budget);
            assert_eq!(source.payloads.budget(), budget);
            assert_eq!(source.pregens.bound(), 300, "half the tails it holds");
            let large = LitMemoryBudget {
                tail: 1 << 32,
                head: 1 << 33,
            };
            let (_t, source) = fixture();
            let source = source.with_memory_budget(large);
            assert_eq!(source.payloads.budget(), large);
            assert_eq!(
                source.pregens.bound(),
                crate::night::pregen::MAX_QUEUED_PREGENERATIONS,
                "at most the cap"
            );
        }

        #[tokio::test]
        async fn pregenerations_past_the_bound_are_dropped_and_counted() {
            // On this one-thread runtime no worker runs before the test
            // awaits: the queue holds all it can, and the rest are dropped.
            const BOUND: usize = 3;
            const PAST: usize = 2;
            let (_t, source) = fixture();
            let source = source
                .with_generator(Arc::new(White::default()))
                .with_pregeneration_bound(BOUND)
                .shared();
            for i in 0..(BOUND + PAST) as u32 {
                source.queue_pregeneration(&small_name(i));
            }
            let snap = source.stats.snapshot();
            assert_eq!(
                (snap.pregen_queued, snap.pregen_dropped),
                (BOUND as u64, PAST as u64)
            );
            assert_eq!(source.pregens.len(), BOUND);
            let snap = settled(&source, |s| {
                s.pregenerations(PregenOutcome::Generated) == BOUND as u64
            })
            .await;
            assert_eq!(snap.pregen_started, BOUND as u64);
            assert_eq!(
                source.stats.take_window().unwrap().pregen_queue_peak,
                BOUND as u64
            );
        }

        /// Lights every texel white; a footprint other than `free_col`'s
        /// first waits until the gate opens, counted as entered.
        struct Gated {
            free_col: u32,
            open: std::sync::Mutex<bool>,
            opened: std::sync::Condvar,
            entered: std::sync::atomic::AtomicUsize,
        }

        impl Gated {
            fn new(free_col: u32) -> Self {
                Self {
                    free_col,
                    open: std::sync::Mutex::new(false),
                    opened: std::sync::Condvar::new(),
                    entered: std::sync::atomic::AtomicUsize::new(0),
                }
            }

            fn entered(&self) -> usize {
                self.entered.load(std::sync::atomic::Ordering::SeqCst)
            }

            fn open(&self) {
                *self.open.lock().unwrap() = true;
                self.opened.notify_all();
            }
        }

        impl crate::night::generator::LitGenerator for Gated {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                if r.name.col != self.free_col {
                    self.entered
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let open = self.open.lock().unwrap();
                    drop(self.opened.wait_while(open, |open| !*open).unwrap());
                }
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn with_every_worker_pregenerating_a_read_takes_the_free_permit_at_once() {
            let (_t, source) = fixture();
            let read = small_name(99);
            let gated = Arc::new(Gated::new(read.col));
            let source = source.with_generator(gated.clone()).shared();
            let permits = source.generations.free();
            let workers = pregen_workers(permits);
            for i in 0..(workers + 2) as u32 {
                source.queue_pregeneration(&small_name(i));
            }
            settled(&source, |_| gated.entered() == workers).await;
            let free = source.generations.free();
            let layout = LitLayout::new(18, 4);
            let served = tokio::time::timeout(
                Duration::from_secs(30),
                source.served_parts(&read, &layout, small_levels(&layout)),
            )
            .await;
            let still_generating = gated.entered();
            gated.open();
            assert_eq!(free, 1, "one permit no pre-generation holds");
            let (_, outcome) = served.expect("the read never waited for a pre-generation");
            assert_eq!(outcome, ReadOutcome::Generated);
            assert_eq!(
                still_generating, workers,
                "the pre-generations were still running"
            );
            settled(&source, |s| {
                s.pregenerations(PregenOutcome::Generated) == (workers + 2) as u64
            })
            .await;
        }

        #[tokio::test]
        async fn no_pregeneration_starts_while_ortho_demand_is_in_flight() {
            let (_t, source) = fixture();
            let (demand, in_flight) = tokio::sync::watch::channel(true);
            let source = source
                .with_generator(Arc::new(White::default()))
                .with_demand_gate(in_flight)
                .shared();
            source.queue_pregeneration(&small_name(0));
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            assert_eq!(source.stats.snapshot().pregen_started, 0, "held");
            assert_eq!(source.pregens.len(), 1);
            // Reads are not held.
            let layout = LitLayout::new(18, 4);
            let (_, outcome) = source
                .served_parts(&small_name(1), &layout, small_levels(&layout))
                .await;
            assert_eq!(outcome, ReadOutcome::Generated);
            demand.send(false).unwrap();
            settled(&source, |s| s.pregenerations(PregenOutcome::Generated) == 1).await;
            // In flight again: the next one waits once more.
            demand.send(true).unwrap();
            source.queue_pregeneration(&small_name(2));
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            assert_eq!(source.stats.snapshot().pregen_started, 1);
            demand.send(false).unwrap();
            settled(&source, |s| s.pregenerations(PregenOutcome::Generated) == 2).await;
        }

        #[tokio::test]
        async fn a_pregeneration_whose_roads_were_late_counts_as_not_kept() {
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads)
                .with_generator(Arc::new(White::default()))
                .with_roads(roads.clone());
            // Each generation times out on its roads, without a wait.
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Generate);
            let source = source.shared();
            assert_eq!(source.size(&name).await, source.layout(&name).total_len);
            let snap = settled(&source, |s| s.pregenerations.iter().sum::<u64>() == 1).await;
            assert_eq!(snap.pregenerations(PregenOutcome::Uncached), 1);
            assert_eq!(snap.pregen_started, 1, "one outcome per pre-generation");
            assert_eq!(snap.generations(), 1);
            assert_eq!(source.payloads.entry_count(), 0, "kept nowhere");
            gate.add_permits(tiles.len());
        }

        #[tokio::test]
        async fn shutdown_with_pregenerations_queued_runs_none_of_them() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone()).shared();
            let held = source.generations.hold_all().await;
            let workers = pregen_workers(held.len());
            let queued = workers + 3;
            for i in 0..queued as u32 {
                source.queue_pregeneration(&small_name(i));
            }
            // Every worker has taken one and waits for a permit.
            settled(&source, |_| {
                source.generations.waiting(GenerationClass::Pregeneration) == workers
            })
            .await;
            source.shutdown();
            let snap = settled(&source, |s| {
                s.pregenerations(PregenOutcome::Black) == workers as u64
            })
            .await;
            assert_eq!(snap.pregen_cancelled, (queued - workers) as u64);
            assert_eq!(snap.generations(), 0);
            assert_eq!(calls(&white), 0, "none ran");
            settled(&source, |_| source.pregens.running() == 0).await;
            source.queue_pregeneration(&small_name(99));
            assert_eq!(source.pregens.len(), 0, "nothing is queued any more");
            // Reads are still served, with the permits back.
            drop(held);
            let layout = LitLayout::new(18, 4);
            let (_, outcome) = source
                .served_parts(&small_name(0), &layout, small_levels(&layout))
                .await;
            assert_eq!(outcome, ReadOutcome::Generated);
        }

        #[tokio::test]
        async fn a_write_back_writes_the_header_and_the_generated_payload_as_two_slices() {
            let temp = TempDir::new().unwrap();
            let disk = Recorded::new(None);
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            let name = small_name(0);
            let layout = LitLayout::new(18, 4);
            let (parts, _) = source.served_parts(&name, &layout, Wanted::whole()).await;
            counted(&source, |s| s.disk_writes == 1).await;
            let written = disk.written_parts.lock().unwrap().clone();
            assert_eq!(written.len(), 1);
            let [header, payload] = &written[0][..] else {
                panic!("{} parts", written[0].len());
            };
            assert_eq!(header.len(), 44);
            assert_eq!(payload.len() as u64, layout.payload_len());
            let served_head = parts.get(PayloadPart::Head).unwrap();
            assert_eq!(
                payload.as_ptr(),
                served_head.as_ptr(),
                "the generated buffer itself, not a copy"
            );
            let fp = source.fingerprint(&name).await.unwrap();
            let entry = disk.get(&crate::night::lit_cache_key(&name)).await.unwrap();
            assert_eq!(
                crate::night::check_entry(entry.unwrap(), &fp, layout.payload_len()),
                Ok(payload.clone())
            );
        }

        #[tokio::test]
        async fn write_backs_past_the_bound_are_dropped_and_counted() {
            // Writes blocked (the disk double's sets wait), with disk permits
            // to spare for the reads: the write-backs pending stop at the
            // bound, and each one past it is dropped.
            const PAST: u64 = 3;
            let temp = TempDir::new().unwrap();
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let disk = Recorded::new(Some(gate.clone()));
            let permits = 2 * MAX_PENDING_WRITE_BACKS as usize;
            let source = source_with_permits(&temp, LitDiskPermits::with_permits(permits))
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk.clone());
            let layout = LitLayout::new(18, 4);
            for i in 0..(MAX_PENDING_WRITE_BACKS + PAST) as u32 {
                let (_, outcome) = source
                    .served_parts(&small_name(i), &layout, small_levels(&layout))
                    .await;
                assert_eq!(outcome, ReadOutcome::Generated, "{i}");
            }
            let snap = source.stats.snapshot();
            assert_eq!(snap.write_backs_dropped, PAST);
            assert_eq!(snap.disk_writes, 0, "every write is blocked");
            let w = source.stats.take_window().unwrap();
            assert_eq!(w.write_backs_pending_peak, MAX_PENDING_WRITE_BACKS);
            gate.add_permits(permits);
            let snap = counted(&source, |s| s.disk_writes == MAX_PENDING_WRITE_BACKS).await;
            assert_eq!(snap.disk_write_failures, 0);
        }

        #[test]
        fn road_sources_changed_names_the_first_tile_not_read_from_its_dsfs() {
            use crate::night::roads::service::DsfIdentity;
            let id = |path: &str| DsfIdentity {
                path: path.into(),
                len: 1,
                mtime_nanos: 1,
            };
            let (a, b) = ((37, -123), (38, -123));
            let read = |tile, path: &str| {
                Arc::new(ReadFrom {
                    tile,
                    dsfs: vec![id(path)],
                    whole: true,
                })
            };
            let fingerprinted: Vec<TileDsfs> = vec![
                (a, Arc::from(vec![id("/a.dsf")])),
                (b, Arc::from(vec![id("/b.dsf")])),
            ];
            let both = [read(a, "/a.dsf"), read(b, "/b.dsf")];
            assert_eq!(road_sources_changed(&fingerprinted, &both), None, "agree");
            assert_eq!(
                road_sources_changed(&fingerprinted, &[both[1].clone(), both[0].clone()]),
                None,
                "matched by tile, not by position"
            );
            assert_eq!(
                road_sources_changed(&fingerprinted, &both[..1]),
                Some(b),
                "a fingerprinted tile missing from what was read"
            );
            assert_eq!(
                road_sources_changed(
                    &fingerprinted,
                    &[read(a, "/old.dsf"), read(b, "/other.dsf")]
                ),
                Some(a),
                "tiles read from other DSFs: the first"
            );
            assert_eq!(
                road_sources_changed(&[], &both),
                None,
                "no road tile in the fingerprint"
            );
        }

        #[test]
        fn a_write_back_is_skipped_for_the_first_reason_that_applies() {
            use RoadsState::{Complete, Failed, TimedOut};
            use WriteBackSkip::{
                ArtFailed, NoFingerprint, RoadFailed, RoadSourcesChanged, RoadsLate,
            };
            for art in [false, true] {
                for agree in [false, true] {
                    let skip = |roads, fp| write_back_skip(roads, fp, agree, art);
                    assert_eq!(skip(TimedOut, false), Some(RoadsLate));
                    assert_eq!(skip(Failed, false), Some(NoFingerprint));
                    assert_eq!(skip(Complete, false), Some(NoFingerprint));
                    assert_eq!(skip(Failed, true), Some(RoadFailed));
                }
                assert_eq!(
                    write_back_skip(Complete, true, false, art),
                    Some(RoadSourcesChanged)
                );
            }
            assert_eq!(write_back_skip(Complete, true, true, true), Some(ArtFailed));
            assert_eq!(write_back_skip(Complete, true, true, false), None);
        }

        #[tokio::test]
        async fn a_probe_without_road_lights_records_no_road_wait() {
            let (_t, source) = fixture();
            source.size(&dark_name()).await;
            let w = source.stats.take_window().unwrap();
            assert_eq!(w.probe_road_wait, None);
            assert_eq!(w.probe_permit_wait.unwrap().count, 1);
        }

        #[tokio::test]
        async fn a_disk_entry_with_another_fingerprint_counts_a_fingerprint_miss() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let layout = LitLayout::new(16, 4);
            let stale = vec![0u8; layout.payload_len() as usize];
            d.set(
                &crate::night::lit_cache_key(&name),
                crate::night::encode_entry(&crate::night::LitFingerprint([9; 32]), &stale),
            )
            .await
            .unwrap();
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone());
            source.read(&name, layout.payload_start(), 4096).await;
            let snap = counted(&source, |s| s.disk_writes == 1).await;
            assert_eq!(snap.disk_misses(DiskMiss::Fingerprint), 1);
            assert_eq!(snap.disk_misses, [0, 0, 0, 1, 0, 0], "and no other");
            assert_eq!(snap.disk_hits, 0);
            assert_eq!(snap.reads(ReadOutcome::Generated), 1);
            let w = source.stats.take_window().unwrap();
            assert_eq!(w.disk_permit_wait.unwrap().count, 1);
            assert_eq!(w.write_backs_pending_peak, 1);

            // The next session reads the rewritten entry.
            let next = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(d.clone());
            next.read(&name, layout.payload_start(), 4096).await;
            let snap = next.stats.snapshot();
            assert_eq!(snap.disk_hits, 1);
            assert_eq!(snap.reads(ReadOutcome::DiskHit), 1);
            assert_eq!((snap.generations(), snap.disk_writes), (0, 0));
        }

        #[tokio::test]
        async fn a_disk_entry_of_a_capped_zooms_old_length_is_generated_again() {
            // Before the cap below ZL16 (#431) a ZL14 payload at t = 4 was
            // the 4096 px chain. Its entry carries today's fingerprint (no
            // version was bumped) and fails the length check alone.
            const OLD_ZL14_PAYLOAD_LEN: u64 = 11_184_824;
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = name_at_zoom(37.20, -122.725, 14);
            let layout = LitLayout::new(14, 4);
            assert_ne!(layout.payload_len(), OLD_ZL14_PAYLOAD_LEN);
            // A generator that draws nothing: what matters is that it is
            // called, and encoding a 2048 px chain takes seconds unoptimised.
            let generator = Arc::new(Unencodable::default());
            let source = source_in(&temp)
                .with_generator(generator.clone())
                .with_disk_cache(d.clone());
            let fp = source.fingerprint(&name).await.expect("a fingerprint");
            let old = vec![0u8; OLD_ZL14_PAYLOAD_LEN as usize];
            d.set(
                &crate::night::lit_cache_key(&name),
                crate::night::encode_entry(&fp, &old),
            )
            .await
            .unwrap();
            let (payload, _) = source.served_payload(&name, &layout).await;
            assert!(payload.is_none(), "the stub's empty image is not served");
            assert_eq!(generator.0.load(std::sync::atomic::Ordering::SeqCst), 1);
            let snap = source.stats.snapshot();
            assert_eq!(snap.disk_misses(DiskMiss::Length), 1);
            assert_eq!(snap.disk_misses, [0, 0, 0, 0, 1, 0], "and no other");
            assert_eq!(snap.disk_hits, 0);
        }

        #[tokio::test]
        async fn an_absent_disk_entry_counts_an_absent_miss() {
            let temp = TempDir::new().unwrap();
            let source = source_in(&temp)
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk());
            let layout = LitLayout::new(16, 4);
            source.served_payload(&lit_name(), &layout).await;
            assert_eq!(source.stats.snapshot().disk_misses(DiskMiss::Absent), 1);
        }

        #[tokio::test]
        async fn an_art_file_given_up_on_counts_a_skipped_write_back() {
            let temp = TempDir::new().unwrap();
            write_tree(&temp);
            let files = Arc::new(FlakyArtFiles::new(fixture_lit_file(&temp), usize::MAX));
            let source = source_reading_art(&temp, files).with_disk_cache(disk());
            assert!(source
                .served_payload(&lit_name(), &LitLayout::new(16, 4))
                .await
                .0
                .is_some());
            let snap = source.stats.snapshot();
            assert_eq!(snap.write_backs_skipped(WriteBackSkip::ArtFailed), 1);
            assert_eq!(snap.write_backs_skipped, [0, 0, 0, 0, 1], "and no other");
        }

        #[tokio::test]
        async fn a_failed_road_tile_counts_a_memory_only_payload() {
            let temp = TempDir::new().unwrap();
            let (source, _) = source_with_roads(&temp, None); // unreadable DSF
            let source = source
                .with_generator(Arc::new(White::default()))
                .with_disk_cache(disk());
            assert!(source
                .served_payload(&lit_name(), &LitLayout::new(16, 4))
                .await
                .0
                .is_some());
            let snap = source.stats.snapshot();
            assert_eq!(snap.write_backs_skipped(WriteBackSkip::RoadFailed), 1);
            assert_eq!(snap.generation_roads(RoadsState::Failed), 1);
        }

        #[tokio::test]
        async fn a_black_generation_is_counted_and_not_a_skipped_write_back() {
            let (_t, source) = fixture();
            let source = source
                .with_generator(Arc::new(Unencodable::default()))
                .with_disk_cache(disk());
            let layout = LitLayout::new(16, 4);
            source.read(&lit_name(), layout.payload_start(), 4096).await;
            let snap = source.stats.snapshot();
            assert_eq!((snap.generations(), snap.generations_black), (1, 1));
            assert_eq!(snap.reads(ReadOutcome::Black), 1);
            assert_eq!(snap.write_backs_skipped, [0; 5]);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_generation_waits_for_its_permit_as_long_as_the_permits_are_held() {
            const HELD: Duration = Duration::from_millis(200);
            let (_t, source) = fixture();
            let source = source.with_generator(Arc::new(White::default()));
            let held = source.generations.hold_all().await;
            let (name, layout) = (lit_name(), LitLayout::new(16, 4));
            let ((payload, _), ()) =
                tokio::join!(source.served_payload(&name, &layout), async move {
                    tokio::time::sleep(HELD).await;
                    drop(held);
                });
            assert!(payload.is_some());
            let w = source.stats.take_window().unwrap();
            let wait = w.generation_permit_wait.unwrap();
            assert!(wait.max_us >= HELD.as_micros() as u64, "{wait:?}");
            assert_eq!(w.generations_waiting_peak, 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn roads_ready_while_a_generation_waits_for_its_permit_are_counted() {
            // #427: the generation timed out on its roads, which were ready by
            // the time it held its permit.
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                LONG,
                Some(gate.clone()),
            );
            let source = source_sharing(&temp, &roads)
                .with_generator(Arc::new(White::default()))
                .with_roads(roads.clone());
            // The roads are still building past their generate deadline.
            let tiles = tiles_of(&source, &name);
            deadlines_passed(&roads, &tiles, Purpose::Generate);
            let held = source.generations.hold_all().await;
            let layout = LitLayout::new(16, 4);
            let ((payload, _), ()) = tokio::join!(source.served_payload(&name, &layout), async {
                // Past the generation's road wait: it now waits for a permit.
                counted(&source, |_| source.stats.generations_waiting.now() == 1).await;
                gate.add_permits(tiles.len());
                for tile in &tiles {
                    until_ready(&roads, *tile).await;
                }
                drop(held);
            });
            assert!(payload.is_some());
            let snap = source.stats.snapshot();
            assert_eq!(snap.generation_roads(RoadsState::TimedOut), 1);
            assert_eq!(snap.roads_ready_at_permit, 1);
        }

        #[tokio::test]
        async fn a_footprint_lit_only_by_a_road_counts_a_road_answer() {
            let temp = TempDir::new().unwrap();
            let name = dark_name();
            let (source, _) = source_with_roads(&temp, Some(road_across(&name)));
            assert_eq!(source.size(&name).await, source.layout(&name).total_len);
            assert_eq!(
                source.size(&lit_name()).await,
                source.layout(&lit_name()).total_len
            );
            let snap = source.stats.snapshot();
            assert_eq!(snap.probes(ProbeAnswer::Road), 1);
            assert_eq!(snap.probes(ProbeAnswer::Recipe), 1, "the recipe first");
            assert_eq!(snap.probes(ProbeAnswer::Dark), 0);
            let w = source.stats.take_window().unwrap();
            assert_eq!(
                w.probe_road_wait.unwrap().count,
                1,
                "the recipe-lit probe waits for no roads"
            );
            assert_eq!(w.probe_permit_wait.unwrap().count, 2);
        }

        #[tokio::test]
        async fn a_dark_footprint_counts_a_dark_answer_once() {
            let (_t, source) = fixture();
            assert_eq!(source.size(&dark_name()).await, 152);
            assert_eq!(source.size(&dark_name()).await, 152, "remembered");
            assert_eq!(source.stats.snapshot().probes, [0, 0, 1]);
        }

        #[tokio::test]
        async fn the_summary_task_does_not_keep_the_source_alive() {
            let (_t, source) = fixture();
            source.size(&dark_name()).await;
            assert!(source.summary_started.load(Ordering::Relaxed));
            let stats = Arc::downgrade(&source.stats);
            let payloads = Arc::downgrade(&source.payloads);
            drop(source);
            tokio::task::yield_now().await;
            assert!(stats.upgrade().is_none());
            assert!(payloads.upgrade().is_none());
        }

        #[test]
        fn outside_a_runtime_no_summary_is_started() {
            let (_t, source) = fixture();
            source.start_summary();
            assert!(!source.summary_started.load(Ordering::Relaxed));
        }
    }
}
