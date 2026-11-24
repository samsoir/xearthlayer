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
//! rasters the roads into the texture.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use moka::policy::EvictionPolicy;
use tracing::{debug, warn};

use super::generator::field::{RecipeField, FIELD_MARGIN_DEG};
use super::generator::{CpuLitGenerator, LedModel, LedRegion, LitGenerator, LitRequest};
use super::layout::minimal_black_dds;
use super::roads::field::{reach_deg, RoadField};
use super::roads::service::{FootprintRoads, Purpose, RoadService, Trigger};
use super::roads::RoadModel;
use super::{
    decode_entry, encode_entry, footprint_fingerprint, lit_cache_key, session_fingerprint,
    FileIdentity, Footprint, LaminarArt, LitFingerprint, LitLayout, Recipe, RecipeStore,
    TextureSettingSource,
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
/// with the read of an entry, or the write-back of a generated one. The
/// permits sit inside the blocking pool's `BLOCKING_POOL_HEADROOM`.
const LIT_DISK_PERMITS: usize = 2;

/// Zoom levels a lit footprint is computed for. `parse_dds_filename` accepts
/// any two-digit zoom and `Footprint::from_name` shifts by it, so anything
/// outside this range is dark without computing anything.
const SUPPORTED_LIT_ZOOMS: std::ops::RangeInclusive<u8> = 12..=22;

/// Fewest and most concurrent lit generations (CPU: about a million texels
/// plus a BC1 encode each).
const MIN_LIT_GENERATION_PERMITS: usize = 2;
const MAX_LIT_GENERATION_PERMITS: usize = 4;

// Lit work (probes, generations, disk tier, road extraction) runs on the
// blocking pool inside its headroom (R17); night may use a third of it so it
// cannot starve other blocking work. The night share is fully used (10 of
// 10): a new night permit needs a smaller one elsewhere or a larger headroom.
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

/// Extra attempts at a recipe read that failed with an I/O error before the
/// tile is given up on for the session.
const RECIPE_RETRIES: u32 = 2;
const RECIPE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Calls `attempt` until it succeeds, at most `retries + 1` times.
fn with_retries<T, E>(
    retries: u32,
    delay: std::time::Duration,
    mut attempt: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut left = retries;
    loop {
        match attempt() {
            Err(_) if left > 0 => {
                left -= 1;
                std::thread::sleep(delay);
            }
            result => return result,
        }
    }
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
        with_retries(RECIPE_RETRIES, RECIPE_RETRY_DELAY, || {
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

    /// Every recipe the generator can read for `name`, or `Unreadable` when
    /// one of them cannot be read. Blocking I/O.
    fn for_footprint(&self, name: &DdsFilename) -> Result<FootprintRecipes, Unreadable> {
        field_tiles(name)
            .into_iter()
            .map(|(lat, lon)| Ok(((lat, lon), self.get(lat, lon)?)))
            .collect()
    }
}

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

/// Memory for generated visible levels (about 0.7 MB per urban ZL16 texture).
const LIT_PAYLOAD_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Default overall brightness (`night.brightness`).
pub const DEFAULT_NIGHT_BRIGHTNESS: f64 = 1.0;

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
    /// Generated visible levels by footprint. Evicting is safe: a payload
    /// regenerates to the same bytes (R13), or to none (black) once one of its
    /// recipes has been given up on, and never changes the size. A payload is
    /// only built from a complete set of recipes.
    payloads: moka::future::Cache<(u32, u32, u8), Bytes>,
    generations: tokio::sync::Semaphore,
    brightness: f64,
    generator: Arc<dyn LitGenerator>,
    /// Lit payloads on disk, from the cache layer; None when caching is off.
    disk: Option<Arc<dyn Cache>>,
    /// Shared with the detached write-backs, which hold a permit too.
    disk_io: Arc<tokio::sync::Semaphore>,
    /// Fingerprint of the session's inputs (generator, brightness, art,
    /// road model), computed on the first payload, after the builders have
    /// run.
    session: tokio::sync::OnceCell<[u8; 32]>,
    /// Road lights; `None` leaves every answer and payload as without them.
    roads: Option<Arc<RoadService>>,
    road_model: RoadModel,
}

/// A generated part of a footprint, and how complete its roads were.
struct Generated {
    bytes: Bytes,
    roads: RoadsState,
}

/// Whether a generation had every road tile it could draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoadsState {
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

/// A payload served but not kept in the memory cache, or `None` (black):
/// moka shares it with every caller waiting on the same load and caches
/// nothing.
#[derive(Debug)]
struct Uncached(Option<Bytes>);

impl NightLitSource {
    pub fn new(
        store: Arc<RecipeStore>,
        art: Arc<LaminarArt>,
        setting: Arc<dyn TextureSettingSource>,
    ) -> Self {
        Self {
            recipes: Arc::new(SessionRecipes::new(store)),
            art,
            setting,
            lit: DashMap::new(),
            probes: tokio::sync::Semaphore::new(LIT_PROBE_PERMITS),
            payloads: payload_cache(LIT_PAYLOAD_CACHE_BYTES),
            generations: tokio::sync::Semaphore::new(lit_generation_permits(
                std::thread::available_parallelism().map_or(1, |n| n.get()),
            )),
            brightness: DEFAULT_NIGHT_BRIGHTNESS,
            generator: Arc::new(CpuLitGenerator),
            disk: None,
            disk_io: Arc::new(tokio::sync::Semaphore::new(LIT_DISK_PERMITS)),
            session: tokio::sync::OnceCell::new(),
            roads: None,
            road_model: RoadModel::DEFAULT,
        }
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

    /// Light roads from `roads`: in the probe, in generation and in the
    /// disk tier's fingerprints.
    pub fn with_roads(mut self, roads: Arc<RoadService>) -> Self {
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
                complete: true,
                timed_out: false,
            },
        }
    }

    /// The generated visible levels of a lit footprint, or `None` (black)
    /// when generation fails or a recipe cannot be read. Concurrent callers
    /// for one key share one load or generation. A failure is not cached,
    /// nor a payload whose roads were late: the next read generates again.
    async fn payload(&self, name: &DdsFilename, layout: &LitLayout) -> Option<Bytes> {
        let load = self.load_or_generate(name.clone(), layout.clone());
        shared(&self.payloads, name, load).await
    }

    /// Disk first, then generation; a generated payload is written back
    /// without waiting, unless an art file failed to read this session or a
    /// road tile was missing. `Err` is a payload not to keep in memory:
    /// black, or generated while a road tile was late. Without a disk tier
    /// this is `generate`.
    async fn load_or_generate(
        &self,
        name: DdsFilename,
        layout: LitLayout,
    ) -> Result<Bytes, Uncached> {
        let Some(disk) = self.disk.clone() else {
            return memory_only(self.generate(name, &layout).await);
        };
        let key = lit_cache_key(&name);
        // The disk permit covers the fingerprint and the read, and is released
        // before generating: a lit task never holds two permits at once.
        let Ok(permit) = self.disk_io.acquire().await else {
            return Err(Uncached(None));
        };
        let fp = self.fingerprint(&name).await;
        let stored = match &fp {
            Some(fp) => match disk.get(&key).await {
                Ok(Some(entry)) => decode_entry(entry, fp, layout.payload_len()),
                _ => None,
            },
            None => None,
        };
        drop(permit);
        if let Some(payload) = stored {
            debug!(
                row = name.row,
                col = name.col,
                zoom = name.zoom,
                "Night lighting: lit texture read from disk"
            );
            return Ok(payload);
        }
        let Some(fp) = fp else {
            // No fingerprint, no safe disk entry: generate and keep it in memory only.
            return memory_only(self.generate(name, &layout).await);
        };
        let generated = self.generate(name, &layout).await;
        let Some(Generated {
            bytes: payload,
            roads: RoadsState::Complete,
        }) = generated
        else {
            return memory_only(generated);
        };
        if self.art.had_failure() {
            // An art file failed to read this session: the payload may
            // differ from a later session's under the same fingerprint, so
            // it stays in memory only.
            return Ok(payload);
        }
        let entry = encode_entry(&fp, &payload);
        let disk_io = Arc::clone(&self.disk_io);
        tokio::spawn(async move {
            let Ok(_permit) = disk_io.acquire().await else {
                return;
            };
            if let Err(e) = disk.set(&key, entry).await {
                debug!(error = %e, key, "Night lighting: lit disk write failed");
            }
        });
        Ok(payload)
    }

    /// Fingerprint of every input `name`'s payload is generated from, or
    /// `None` when computing it failed (a recipe that cannot be loaded among
    /// them). Recipe identities are those taken when each recipe was loaded,
    /// so a file replaced mid-session does not lend the payload its identity.
    /// Road DSF identities are taken once per tile per session, in priority
    /// order. The art walk, recipe loads and DSF `stat`s run on the blocking
    /// pool; the caller holds a disk permit and none of them takes another.
    async fn fingerprint(&self, name: &DdsFilename) -> Option<LitFingerprint> {
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
        let (recipes, owned) = (Arc::clone(&self.recipes), name.clone());
        let identities = tokio::task::spawn_blocking(move || {
            field_tiles(&owned)
                .into_iter()
                .map(|(lat, lon)| {
                    let (identity, region) = recipes.identity(lat, lon)?;
                    Ok(((lat, lon), identity, region))
                })
                .collect::<Result<Vec<_>, Unreadable>>()
        })
        .await
        .ok()?
        .ok()?;
        // A tile whose identities could not be taken leaves no fingerprint:
        // the payload is then kept in memory only.
        let roads = match &self.roads {
            Some(roads) => futures::future::join_all(
                self.road_tiles(name, self.layout(name).first_visible)
                    .into_iter()
                    .map(|tile| async move { Some((tile, roads.identities(tile).await?)) }),
            )
            .await
            .into_iter()
            .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        Some(footprint_fingerprint(session, &identities, &roads))
    }

    /// The generated visible levels of the footprint. Waits for its roads
    /// first, holding no permit, then generates under a generation permit;
    /// the road raster is generation work and runs under it.
    async fn generate(&self, name: DdsFilename, layout: &LitLayout) -> Option<Generated> {
        let size = layout.first_visible;
        let found = self.footprint_roads(&name, size, Purpose::Generate).await;
        let roads_state = RoadsState::of(&found);
        let road_tiles = found.tiles;
        let _permit = self.generations.acquire().await.ok()?;
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
            let patterns = |n: &str| art.pattern(n);
            // The tiles' handles are dropped once rastered: the road cache
            // alone keeps merged roads.
            let roads = RoadField::raster(&owned, size, &road_tiles, &road_model);
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
            Some((super::generator::encode_visible(image), roads.stats))
        })
        .await;
        match generated {
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
            Ok(Some((Ok(bytes), road))) => {
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    ms = started.elapsed().as_millis() as u64,
                    road_segments = road.segments,
                    road_raster_us = road.raster_us,
                    road_texels = road.lit_texels,
                    size,
                    roads_complete = roads_state == RoadsState::Complete,
                    roads_timed_out = roads_state == RoadsState::TimedOut,
                    "Night lighting: lit texture generated"
                );
                Some(Generated {
                    bytes,
                    roads: roads_state,
                })
            }
            Ok(Some((Err(e), _))) => {
                warn!(error = %e, "Night lighting: lit encode failed; texture served black");
                None
            }
            Err(_) => {
                warn!("Night lighting: lit generation failed; texture served black");
                None
            }
        }
    }

    async fn has_light(&self, name: &DdsFilename) -> bool {
        if !SUPPORTED_LIT_ZOOMS.contains(&name.zoom) {
            return false;
        }
        let key = (name.row, name.col, name.zoom);
        if let Some(hit) = self.lit.get(&key) {
            return *hit;
        }
        // Roads before the permit: a lit task never waits for roads while
        // holding one.
        let FootprintRoads {
            tiles: road_tiles,
            complete: roads_complete,
            timed_out: roads_timed_out,
        } = self
            .footprint_roads(name, self.layout(name).first_visible, Purpose::Probe)
            .await;
        let started = Instant::now();
        let Ok(_permit) = self.probes.acquire().await else {
            return false;
        };
        if let Some(hit) = self.lit.get(&key) {
            return *hit; // another probe answered while we waited
        }
        let wait_ms = started.elapsed().as_millis() as u64;
        let probe_started = Instant::now();
        let (recipes, art) = (Arc::clone(&self.recipes), Arc::clone(&self.art));
        let footprint = Footprint::from_name(name);
        // How far outside the footprint a road can light it: the raster's
        // reach at the size generation uses, so the two agree.
        let reach = if road_tiles.is_empty() {
            0.0
        } else {
            reach_deg(name, self.layout(name).first_visible, &self.road_model)
        };
        let lit = match tokio::task::spawn_blocking(move || {
            // The field reads every tile the generator could read for this
            // footprint, once each; an unreadable one counts as dark.
            let lookup = |lat: i32, lon: i32| recipes.get(lat, lon).ok().and_then(|(r, _)| r);
            RecipeField::new(&footprint, &lookup).has_light(|n| art.lit_terrain(n).is_some())
                || road_tiles.iter().any(|roads| {
                    roads.crosses(
                        footprint.south - reach,
                        footprint.west - reach,
                        footprint.north + reach,
                        footprint.east + reach,
                    )
                })
        })
        .await
        {
            Ok(answer) => answer,
            Err(_) => {
                // The probe is meant to be panic-free: XEL's panic hook
                // unmounts on any panic. This arm is a last resort, and it
                // remembers the dark answer so size and read keep agreeing.
                warn!("Night lighting: lit probe failed; footprint stays dark this session");
                false
            }
        };
        let (lit, committed) = self.commit(key, lit);
        if committed && !lit && roads_timed_out {
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
            wait_ms,
            probe_ms = probe_started.elapsed().as_millis() as u64,
            roads_complete,
            roads_timed_out,
            "Night lighting: lit probe"
        );
        lit
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
}

/// The memory cache of generated payloads, holding up to `max_bytes` of them.
///
/// LRU, not moka's default TinyLFU: once the cache is full, admission by
/// popularity would reject a payload just generated (asked for once) in
/// favour of payloads read more often, and X-Plane's next read of its
/// levels would load or generate it again.
fn payload_cache(max_bytes: u64) -> moka::future::Cache<(u32, u32, u8), Bytes> {
    moka::future::Cache::builder()
        .eviction_policy(EvictionPolicy::lru())
        .max_capacity(max_bytes)
        .weigher(|_, v: &Bytes| v.len().try_into().unwrap_or(u32::MAX))
        .build()
}

/// The cached part of `name` in `cache`, loaded by `load` on a miss, which
/// concurrent callers share; `None` (black) when the load gave nothing.
async fn shared(
    cache: &moka::future::Cache<(u32, u32, u8), Bytes>,
    name: &DdsFilename,
    load: impl std::future::Future<Output = Result<Bytes, Uncached>>,
) -> Option<Bytes> {
    match cache
        .try_get_with((name.row, name.col, name.zoom), load)
        .await
    {
        Ok(part) => Some(part),
        Err(uncached) => uncached.0.clone(),
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

/// A generated part for the memory cache: kept unless its roads were
/// late; `None` (black) is never kept.
fn memory_only(generated: Option<Generated>) -> Result<Bytes, Uncached> {
    match generated {
        Some(Generated {
            bytes,
            roads: RoadsState::Complete | RoadsState::Failed,
        }) => Ok(bytes),
        Some(Generated {
            bytes,
            roads: RoadsState::TimedOut,
        }) => Err(Uncached(Some(bytes))),
        None => Err(Uncached(None)),
    }
}

impl LitTextureSource for NightLitSource {
    fn size<'a>(&'a self, name: &'a DdsFilename) -> BoxFuture<'a, u64> {
        Box::pin(async move {
            if self.has_light(name).await {
                self.layout(name).total_len
            } else {
                minimal_black_dds().len() as u64
            }
        })
    }

    fn read<'a>(&'a self, name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes> {
        Box::pin(async move {
            if self.has_light(name).await {
                let layout = self.layout(name);
                // The header and the levels above the visible ones are
                // black and need no pixels (X-Plane's objects light the
                // ground under the uploaded top level); the visible levels
                // are generated only by a read that reaches them.
                let reached = layout.reaches_payload(offset, len);
                let payload = if reached {
                    self.payload(name, &layout).await
                } else {
                    None
                };
                let part = read_part(reached, payload.is_some());
                let bytes = layout.read_lit(offset, len, payload.as_ref());
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
                    "Night lighting: lit read"
                );
                bytes
            } else {
                BlackLitSource.read(name, offset, len).await
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

    #[tokio::test]
    async fn a_full_payload_cache_keeps_a_new_payload_and_drops_the_least_recently_read() {
        // moka's default TinyLFU admits a new entry into a full cache only
        // when it has been asked for more often than the entry it would
        // evict: a payload just generated has been asked for once, so it lost
        // to payloads read more than once and was generated again on the
        // next read (#387).
        const PAYLOAD_BYTES: usize = 1024;
        const HELD: u32 = 4;
        let cache = payload_cache(PAYLOAD_BYTES as u64 * u64::from(HELD));
        let name = |row: u32| DdsFilename { row, ..lit() };
        let key = |row: u32| (row, lit().col, lit().zoom);
        let read = |row: u32| {
            let cache = &cache;
            async move {
                let load = async { Ok(Bytes::from(vec![0xAB; PAYLOAD_BYTES])) };
                assert!(shared(cache, &name(row), load).await.is_some());
            }
        };

        for row in 0..HELD {
            read(row).await;
        }
        cache.run_pending_tasks().await; // full: moka counts reads from here
        for _ in 0..2 {
            for row in 0..HELD {
                read(row).await;
            }
        }
        cache.run_pending_tasks().await;
        read(HELD).await;
        cache.run_pending_tasks().await;

        assert!(cache.contains_key(&key(HELD)), "the new payload is kept");
        assert!(
            !cache.contains_key(&key(0)),
            "the least recently read payload makes room"
        );
        assert_eq!(cache.entry_count(), u64::from(HELD));
    }

    #[test]
    fn retries_stop_at_the_first_success() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, std::time::Duration::ZERO, || {
            calls += 1;
            if calls < 2 {
                Err("transient")
            } else {
                Ok(7)
            }
        });
        assert_eq!((r, calls), (Ok(7), 2));
    }

    #[test]
    fn retries_give_up_after_the_limit() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, std::time::Duration::ZERO, || {
            calls += 1;
            Err("down")
        });
        assert_eq!((r, calls), (Err("down"), 3));
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
            let pkg = temp.path().join("pkg");
            let art = temp.path().join("art");
            if !art.exists() {
                write_tree(temp);
            }
            NightLitSource::new(
                Arc::new(RecipeStore::new(vec![pkg])),
                Arc::new(LaminarArt::new(art)),
                Arc::new(FixedTextureSetting(4)),
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
            assert_eq!(source.payload(&name, &layout).await, None);
            assert_eq!(source.payloads.entry_count(), 0, "nothing cached");
            source.payloads.run_pending_tasks().await;
            assert!(source
                .payloads
                .get(&(name.row, name.col, name.zoom))
                .await
                .is_none());
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
            source.payloads.run_pending_tasks().await;
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
            assert!(crate::night::decode_entry(entry, &fp, layout.payload_len()).is_some());
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
                if crate::night::decode_entry(entry, &b_fp, LitLayout::new(16, 4).payload_len())
                    .is_some()
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
                if crate::night::decode_entry(entry, &b_fp, layout.payload_len()).is_some() {
                    overwritten = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(overwritten, "the entry carries the new fingerprint");
            let entry = disk.get(&key).await.unwrap().unwrap();
            assert!(crate::night::decode_entry(entry, &a_fp, layout.payload_len()).is_none());
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

            assert!(a.payload(&name, &layout).await.is_some());
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
            b.payload(&name, &layout).await;
            assert_eq!(calls(&second), 1, "regenerated, not decoded");
        }

        #[tokio::test]
        async fn an_art_read_failure_writes_nothing() {
            let temp = TempDir::new().unwrap();
            let disk = disk();
            let name = name_at(37.20, -122.725);
            let source = source_in(&temp).with_disk_cache(disk.clone());
            // The lit texture exists but does not decode.
            std::fs::write(
                temp.path().join("art/textures12/urban_FAR/uw5_FAR_LIT.dds"),
                b"DDS ",
            )
            .unwrap();
            let layout = LitLayout::new(16, 4);
            assert!(
                source.payload(&name, &layout).await.is_some(),
                "still served from memory"
            );
            settle(&disk, &name).await;
            assert!(disk
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
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
            assert_eq!(source.payload(&name, &layout).await, None, "served black");
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
            futures::future::join_all(names.iter().map(|n| source.payload(n, &layout))).await;
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
        /// `generate_ms` for a generation, its builds held by `gate` when
        /// one is given (each `add_permits(1)` lets one build run).
        fn roads(
            xp: std::path::PathBuf,
            probe_ms: u64,
            generate_ms: u64,
            gate: Option<Arc<tokio::sync::Semaphore>>,
        ) -> Arc<RoadService> {
            RoadService::with_config(
                xp,
                vec![],
                None,
                RoadServiceConfig {
                    probe_wait: Duration::from_millis(probe_ms),
                    generate_wait: Duration::from_millis(generate_ms),
                    gate,
                    ..RoadServiceConfig::default()
                },
            )
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
            (source_in(temp).with_roads(roads.clone()), roads)
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
                30,
                LONG,
                Some(gate.clone()),
            );
            let source = source_in(&temp).with_roads(roads.clone());
            // The gate is shut: both probes time out before the build runs.
            // Lit by its recipe: a timeout cannot have changed the answer.
            let lit = lit_name();
            assert_eq!(source.size(&lit).await, source.layout(&lit).total_len);
            assert_eq!(source.size(&name).await, 152, "committed without roads");
            let s = roads.stats_snapshot();
            assert_eq!((s.probe_timeouts, s.answered_before_roads), (2, 1));
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
            assert!(source.payload(&name, &layout).await.is_some(), "served");
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
            assert!(source.payload(&name, &layout).await.is_some());
            assert_eq!(calls(&white), 1, "the failed tile is final: from memory");
        }

        #[tokio::test]
        async fn a_generation_past_its_road_wait_is_kept_nowhere_and_heals() {
            let temp = TempDir::new().unwrap();
            let d = disk();
            let name = lit_name();
            let gate = shut();
            let roads = roads(
                xp_install(&temp, Some(road_across(&name))),
                LONG,
                30,
                Some(gate.clone()),
            );
            let white = Arc::new(White::default());
            let source = source_in(&temp)
                .with_generator(white.clone())
                .with_disk_cache(d.clone())
                .with_roads(roads.clone());
            let layout = LitLayout::new(16, 4);
            // The gate is shut: the generation times out on its roads.
            assert!(source.payload(&name, &layout).await.is_some(), "served");
            assert_eq!(roads.stats_snapshot().generate_timeouts, 1);
            source.payloads.run_pending_tasks().await;
            assert_eq!(source.payloads.entry_count(), 0, "not in memory");
            assert!(source.payload(&name, &layout).await.is_some());
            assert_eq!(calls(&white), 2, "generated again");
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_none());
            // Once the roads are ready the payload is complete and kept.
            gate.add_permits(1);
            until_ready(&roads, TILE).await;
            assert!(source.payload(&name, &layout).await.is_some());
            assert!(source.payload(&name, &layout).await.is_some());
            assert_eq!(calls(&white), 3, "kept once complete");
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_some());
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
            let source = source_in(&temp).with_roads(roads.clone());
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
                30,
                Some(gate.clone()),
            );
            let source = source_in(&temp).with_roads(roads.clone());
            let layout = source.layout(&name);
            let (size, ()) = tokio::join!(source.size(&name), async { gate.add_permits(1) });
            assert_eq!(size, layout.total_len, "lit by the road");
            // The tile is evicted and its rebuild is held: generation times out.
            roads.evict(TILE).await;
            let file = source.read(&name, 0, u32::MAX).await;
            assert_eq!(file.len() as u64, size, "size and read agree");
            let lit = &file[layout.payload_start() as usize..];
            assert!(lit.iter().all(|&b| b == 0), "the payload black");
            assert_eq!(roads.stats_snapshot().generate_timeouts, 1);
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
                .payload(&name, &LitLayout::new(16, 4))
                .await
                .is_some());
            settle(&d, &name).await;
            assert!(d
                .get(&crate::night::lit_cache_key(&name))
                .await
                .unwrap()
                .is_some());
        }

        /// Records whether its request carried road light.
        #[derive(Default)]
        struct SeesRoads(std::sync::atomic::AtomicBool);

        impl crate::night::LitGenerator for SeesRoads {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
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
            let _ = source.payload(&lit_name(), &LitLayout::new(16, 4)).await;
            assert!(seen.0.load(std::sync::atomic::Ordering::SeqCst));
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
                let roads = RoadService::new(temp.path().join("xp"), vec![], None);
                let source = source_in(temp)
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

        #[cfg(unix)]
        #[tokio::test]
        async fn a_road_dsf_that_cannot_be_stated_leaves_no_fingerprint() {
            use std::os::unix::fs::PermissionsExt;
            let temp = TempDir::new().unwrap();
            let name = lit_name();
            let xp = xp_install(&temp, Some(road_across(&name)));
            let roads = RoadService::new(xp.clone(), vec![], None);
            let source = source_in(&temp)
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
            let source = source_in(&temp).with_roads(roads.clone());
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
            let source = source_in(&temp)
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
                futures::future::join_all(waiting.iter().map(|n| source.payload(n, &layout))),
                async {
                    let payload = tokio::time::timeout(
                        Duration::from_secs(5),
                        source.payload(&other, &LitLayout::new(18, 4)),
                    )
                    .await;
                    assert!(payload.is_ok_and(|p| p.is_some()));
                    gate.add_permits(1);
                }
            );
        }
    }
}
