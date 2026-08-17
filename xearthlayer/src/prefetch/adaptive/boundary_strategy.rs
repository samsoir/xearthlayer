//! Region lifecycle management for the prefetch system.
//!
//! Provides region lifecycle management:
//! - [`BoundaryStrategy::promote_completed_regions`] — promotes `InProgress`
//!   regions to `Prefetched` once all their tiles are confirmed in cache.

use std::sync::Arc;

use crate::coord::TileCoord;
use crate::executor::DdsDiskCacheChecker;
use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion, RetainedRegion};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::SceneryIndex;

/// Region lifecycle management for the prefetch system.
///
/// Handles region state transitions (InProgress → Prefetched) and
/// retention-based eviction. Staleness is handled by the coordinator's
/// `evaluate_stale_regions`, which dispatches on [`RegionDiskState`]:
/// [`RegionDiskState::Complete`] promotes, [`RegionDiskState::Incomplete`]
/// either retries immediately (coverage advanced) or defers on an escalating
/// ladder (coverage stuck), and [`RegionDiskState::NoTiles`] — the only path
/// to `NoCoverage` — retires the region on the first look, since the scenery
/// index is fully built before the coordinator receives it. When
/// [`RegionDiskState::Unknown`] reports no authoritative source to consult,
/// the region is left untouched and no strike is recorded.
///
/// An `Incomplete` region is never retired, however long it takes: see #226,
/// where timing-based strikes retired regions that had indexed coverage.
pub struct BoundaryStrategy;

/// What the authoritative DDS disk cache can tell us about a region.
///
/// Deliberately not a `bool`: the answer feeds a decision whose terminal
/// outcome is permanent exclusion (`NoCoverage`), so "we cannot tell" must
/// be distinguishable from "we checked and it is absent".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionDiskState {
    /// Every tile the scenery index attributes to this region is present.
    Complete,
    /// At least one attributed tile is absent. See [`TileCoverage`] for why
    /// the counts are not unconditionally present.
    Incomplete { coverage: TileCoverage },
    /// The scenery index attributes no tiles to this region.
    NoTiles,
    /// No authoritative source to consult — no checker and/or no index.
    Unknown,
}

/// How much an `Incomplete` answer knows about the region's tile counts.
///
/// This is an enum rather than `Option<(usize, usize)>` or a pair of zeroes
/// because "not counted" must be *unrepresentable as a plausible count*.
/// `Incomplete { covered: 0, total: 0 }` from a short-circuiting scan would
/// read as a real observation ("nothing has arrived, and the region is
/// empty") and would drive `evaluate_stale_regions` straight into a strike —
/// exactly the class of silently-wrong number #226 exists to remove.
/// [`TileCoverage::NotCounted`] carries no payload, so there is no number to
/// misread, and [`TileCoverage::counts`] forces every reader to acknowledge
/// its absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileCoverage {
    /// Every tile was checked. `covered < total` by construction.
    Exact { covered: usize, total: usize },
    /// The scan stopped at the first missing tile, so no counts exist.
    /// Produced only for [`CoverageDetail::FirstMissOnly`] callers, which by
    /// definition discard the counts.
    NotCounted,
}

impl TileCoverage {
    /// The `(covered, total)` pair, or `None` when the scan short-circuited.
    pub fn counts(&self) -> Option<(usize, usize)> {
        match self {
            Self::Exact { covered, total } => Some((*covered, *total)),
            Self::NotCounted => None,
        }
    }
}

/// How thoroughly [`BoundaryStrategy::region_disk_state`] should scan a
/// region's tile set.
///
/// One function, one behaviour, parameterised — not two functions that can
/// drift apart, which is the defect #176 was filed to remove.
///
/// The parameter exists because the per-tile check is not free. When the XEL
/// DDS cache misses, `tile_is_covered` falls through to
/// `OrthoUnionIndex::dds_tile_exists`, which tries 4 filename prefixes and —
/// because `textures/` is in `LAZY_DIRECTORIES` and is deliberately never
/// indexed — resolves each by `stat()`ing every installed source. An absent
/// tile therefore costs ~4 × N_sources syscalls, and
/// [`BoundaryStrategy::promote_completed_regions`] runs over every
/// `InProgress` region on a 2-second cycle. Counting tiles it then discards
/// would steal CPU from the executor under exactly the cold-cache backlog
/// this code exists to relieve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageDetail {
    /// Stop at the first missing tile. For callers that only need the
    /// `Complete` / not-`Complete` distinction.
    FirstMissOnly,
    /// Check every tile and report exact counts. For callers that must tell
    /// an *advancing* region from a *stuck* one.
    ExactCounts,
}

impl BoundaryStrategy {
    /// Creates a new `BoundaryStrategy`.
    pub fn new() -> Self {
        Self
    }

    /// The single definition of "is this region fully covered?".
    ///
    /// Consumed by both the fast path (`promote_completed_regions`) and the
    /// rescue path (`AdaptivePrefetchCoordinator::evaluate_stale_regions`).
    /// Prior to #223's review these were two copies that already disagreed
    /// on the empty and unanswerable cases — the same copy-drift shape #176
    /// was filed to remove.
    pub fn region_disk_state(
        region: DsfRegion,
        scenery_index: Option<&Arc<SceneryIndex>>,
        dds_disk_checker: Option<&Arc<dyn DdsDiskCacheChecker>>,
        ortho_union_index: Option<&Arc<OrthoUnionIndex>>,
        detail: CoverageDetail,
    ) -> RegionDiskState {
        let (Some(index), Some(checker)) = (scenery_index, dds_disk_checker) else {
            return RegionDiskState::Unknown;
        };
        let tiles = index.tiles_in_region(region);
        if tiles.is_empty() {
            return RegionDiskState::NoTiles;
        }

        match detail {
            // Cheap path. `all` stops at the first missing tile, which is all
            // `promote_completed_regions` needs — see [`CoverageDetail`] for
            // why the difference is not academic.
            CoverageDetail::FirstMissOnly => {
                if tiles
                    .iter()
                    .all(|t| Self::tile_is_covered(t, checker, ortho_union_index))
                {
                    RegionDiskState::Complete
                } else {
                    RegionDiskState::Incomplete {
                        coverage: TileCoverage::NotCounted,
                    }
                }
            }
            // Full scan: `evaluate_stale_regions` needs `covered` to tell a
            // slow region from a stuck one. It runs only over regions stale
            // for longer than `stale_region_timeout` (120s), so the cost is
            // bounded in a way the every-cycle path is not.
            CoverageDetail::ExactCounts => {
                let total = tiles.len();
                let covered = tiles
                    .iter()
                    .filter(|t| Self::tile_is_covered(t, checker, ortho_union_index))
                    .count();

                if covered == total {
                    RegionDiskState::Complete
                } else {
                    RegionDiskState::Incomplete {
                        coverage: TileCoverage::Exact { covered, total },
                    }
                }
            }
        }
    }

    /// A tile counts as covered if EITHER source has it.
    ///
    /// This is the same disjunction the submit-side filter pipeline applies:
    /// stage 3 (`filter_disk_tiles`) drops tiles that already exist as real
    /// `.dds` files in an installed package, so prefetch never writes them to
    /// the XEL DDS cache. Promotion must use the same definition or regions
    /// covered by package-shipped imagery can never be confirmed.
    fn tile_is_covered(
        tile: &TileCoord,
        checker: &Arc<dyn DdsDiskCacheChecker>,
        ortho_union_index: Option<&Arc<OrthoUnionIndex>>,
    ) -> bool {
        // XEL DDS disk cache: keyed by TILE coords.
        if checker.tile_exists_blocking(tile.row, tile.col, tile.zoom) {
            return true;
        }
        // OrthoUnionIndex: keyed by CHUNK coords. Mixing these up silently
        // returns false for every tile — see the #172 keying bug.
        match ortho_union_index {
            Some(ortho) => {
                let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
                ortho.dds_tile_exists(chunk_row, chunk_col, chunk_zoom)
            }
            None => false,
        }
    }

    /// Mark a region as having no scenery coverage.
    pub fn mark_no_coverage(&self, region: &DsfRegion, geo_index: &GeoIndex) {
        geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::no_coverage());
        tracing::debug!(
            lat = region.lat,
            lon = region.lon,
            "boundary: marked NoCoverage"
        );
    }

    /// Mark a region as having prefetch in progress.
    pub fn mark_in_progress(&self, region: &DsfRegion, geo_index: &GeoIndex) {
        geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::in_progress());
        tracing::debug!(
            lat = region.lat,
            lon = region.lon,
            "boundary: marked InProgress"
        );
    }

    /// Check InProgress regions and promote to Prefetched if all tiles are
    /// present in the authoritative DDS disk cache.
    ///
    /// For each InProgress region, looks up its tile set via
    /// `SceneryIndex::tiles_in_region` — the single definition of "what
    /// tiles belong to this region" shared with the submit and rescue
    /// paths (#176) — and queries the `DdsDiskCacheChecker` for each tile.
    /// A region is promoted to `Prefetched` only when **every** one of its
    /// tiles is covered.
    ///
    /// Scans with [`CoverageDetail::FirstMissOnly`]: this runs on every
    /// coordinator cycle (2s) over every `InProgress` region, and the
    /// `covered`/`total` counts are discarded here anyway. Only the rescue
    /// path pays for exact counts. See [`CoverageDetail`] for the syscall
    /// arithmetic that makes the difference matter.
    ///
    /// If either `dds_disk_checker` or `scenery_index` is `None`, promotion
    /// is skipped — there is no authoritative source to consult, or no way
    /// to know which tiles the region should contain. The rescue path
    /// (`evaluate_stale_regions`) will still fire for stale regions.
    ///
    /// See #172 Part 3: the prior version consulted a local `HashSet`
    /// shadow of "tiles we believe are cached" that failed to track ~94%
    /// of actually-cached tiles during production flights (4 normal vs.
    /// 61 stale-rescue promotions in a 9-hour log). Querying the
    /// authoritative cache directly collapses one source of truth; the
    /// shadow itself was deleted in #176 once confirmed write-only.
    pub fn promote_completed_regions(
        geo_index: &GeoIndex,
        dds_disk_checker: Option<&Arc<dyn DdsDiskCacheChecker>>,
        scenery_index: Option<&Arc<SceneryIndex>>,
        ortho_union_index: Option<&Arc<OrthoUnionIndex>>,
    ) -> Vec<DsfRegion> {
        let in_progress: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_in_progress())
            .map(|(dsf, _)| dsf)
            .collect();

        let mut promoted: Vec<DsfRegion> = Vec::new();
        for region in &in_progress {
            match Self::region_disk_state(
                *region,
                scenery_index,
                dds_disk_checker,
                ortho_union_index,
                CoverageDetail::FirstMissOnly,
            ) {
                RegionDiskState::Complete => {
                    geo_index.insert::<PrefetchedRegion>(*region, PrefetchedRegion::prefetched());
                    promoted.push(*region);
                }
                RegionDiskState::Incomplete { .. }
                | RegionDiskState::NoTiles
                | RegionDiskState::Unknown => {
                    continue;
                }
            }
        }

        if !promoted.is_empty() {
            tracing::debug!(
                promoted = promoted.len(),
                "Promoted InProgress regions to Prefetched"
            );
        }
        promoted
    }

    /// Evict `PrefetchedRegion` entries for regions no longer in the retained window.
    ///
    /// Removes `Prefetched`, `Deferred`, and `NoCoverage` entries whose DSF
    /// region is not present in the `RetainedRegion` layer. `InProgress`
    /// entries are preserved because they represent actively running
    /// prefetch jobs.
    ///
    /// Returns an empty `Vec` (no-op) when the `RetainedRegion` layer is
    /// empty, indicating retention tracking is not yet active.
    ///
    /// Returns the evicted regions so the caller can prune any per-region
    /// bookkeeping (e.g. the coordinator's `region_retry` map) it holds
    /// alongside the index — this function is a static taking only
    /// `&GeoIndex` and cannot reach that bookkeeping itself.
    pub fn evict_non_retained(geo_index: &GeoIndex) -> Vec<DsfRegion> {
        let retained = geo_index.regions::<RetainedRegion>();
        if retained.is_empty() {
            return Vec::new(); // Retention not active yet
        }

        let retained_set: std::collections::HashSet<DsfRegion> = retained.into_iter().collect();

        let to_evict: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(dsf, region)| !region.is_in_progress() && !retained_set.contains(dsf))
            .map(|(dsf, _)| dsf)
            .collect();

        for region in &to_evict {
            geo_index.remove::<PrefetchedRegion>(region);
        }

        if !to_evict.is_empty() {
            tracing::debug!(
                evicted = to_evict.len(),
                "Evicted non-retained PrefetchedRegion entries"
            );
        }
        to_evict
    }
}

impl Default for BoundaryStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_mark_no_coverage() {
        let strategy = BoundaryStrategy::new();
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        strategy.mark_no_coverage(&region, &geo_index);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_no_coverage());
    }

    #[test]
    fn test_mark_in_progress() {
        let strategy = BoundaryStrategy::new();
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        strategy.mark_in_progress(&region, &geo_index);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    // =========================================================================
    // Region promotion
    // =========================================================================

    /// Mock [`DdsDiskCacheChecker`] backed by an in-memory set of tile
    /// coordinates. Stores by tile coords (not chunk) to match how the
    /// DDS disk cache is actually keyed.
    struct MockDiskChecker {
        tiles: std::sync::Mutex<HashSet<(u32, u32, u8)>>,
    }

    impl MockDiskChecker {
        fn new() -> Self {
            Self {
                tiles: std::sync::Mutex::new(HashSet::new()),
            }
        }

        /// Populate from a set of [`TileCoord`]s.
        fn with_tile_coords(iter: impl IntoIterator<Item = TileCoord>) -> Arc<Self> {
            let me = Self::new();
            {
                let mut set = me.tiles.lock().unwrap();
                for tile in iter {
                    set.insert((tile.row, tile.col, tile.zoom));
                }
            }
            Arc::new(me)
        }
    }

    impl DdsDiskCacheChecker for MockDiskChecker {
        fn tile_exists(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            let present = self.tiles.lock().unwrap().contains(&(row, col, zoom));
            Box::pin(async move { present })
        }

        fn tile_exists_blocking(&self, row: u32, col: u32, zoom: u8) -> bool {
            self.tiles.lock().unwrap().contains(&(row, col, zoom))
        }
    }

    #[test]
    fn test_promote_completed_regions() {
        // #176: promotion has no geometric fallback — it needs a
        // SceneryIndex to know the region's tile set, so wire a minimal
        // single-tile index.
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 50.5,
            lon: 9.5,
            is_sea: false,
        });
        let tiles = index.tiles_in_region(region);
        assert_eq!(tiles.len(), 1);

        // Populate the mock disk checker with every tile's chunk coords.
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());

        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );
        assert_eq!(promoted.len(), 1);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_promote_skips_incomplete_regions() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 25000,
            col: 10000,
            chunk_zoom: 16,
            lat: 50.2,
            lon: 9.2,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 26000,
            col: 11000,
            chunk_zoom: 16,
            lat: 50.8,
            lon: 9.8,
            is_sea: false,
        });
        let tiles = index.tiles_in_region(region);
        assert_eq!(tiles.len(), 2);

        // Mock checker knows only one tile → region is incomplete
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(std::iter::once(tiles[0]));

        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );
        assert_eq!(promoted.len(), 0);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    #[test]
    fn test_promote_skipped_when_no_checker_configured() {
        // Without a `DdsDiskCacheChecker` there is no authoritative source
        // to consult. `promote_completed_regions` should skip and return 0
        // rather than promote based on nothing.
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let promoted = BoundaryStrategy::promote_completed_regions(&geo_index, None, None, None);
        assert_eq!(promoted.len(), 0);

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(
            state.is_in_progress(),
            "Region must stay InProgress when no checker is configured"
        );
    }

    // =========================================================================
    // promote_completed_regions + SceneryIndex zoom integration
    // =========================================================================

    /// Create a SceneryIndex populated with tiles for a specific DSF region.
    fn make_scenery_index_for_region(lat: i32, lon: i32, chunk_zoom: u8) -> Arc<SceneryIndex> {
        use crate::coord::{to_tile_coords, CHUNKS_PER_TILE_SIDE, CHUNK_ZOOM_OFFSET};
        use crate::prefetch::scenery_index::{SceneryIndexConfig, SceneryTile};

        let index = SceneryIndex::new(SceneryIndexConfig::default());

        // Sample a 4x4 grid within the 1deg DSF region and add tiles
        for lat_step in 0..4u32 {
            for lon_step in 0..4u32 {
                let sample_lat = lat as f64 + (lat_step as f64 * 0.25) + 0.125;
                let sample_lon = lon as f64 + (lon_step as f64 * 0.25) + 0.125;
                let tile_zoom = chunk_zoom - CHUNK_ZOOM_OFFSET;
                if let Ok(coord) = to_tile_coords(sample_lat, sample_lon, tile_zoom) {
                    index.add_tile(SceneryTile {
                        row: coord.row * CHUNKS_PER_TILE_SIDE,
                        col: coord.col * CHUNKS_PER_TILE_SIDE,
                        chunk_zoom,
                        lat: sample_lat as f32,
                        lon: sample_lon as f32,
                        is_sea: false,
                    });
                }
            }
        }

        Arc::new(index)
    }

    #[test]
    fn test_promote_completed_regions_with_scenery_index() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // SceneryIndex at chunk_zoom 16 -> tile zoom 12
        let index = make_scenery_index_for_region(50, 9, 16);

        // Get tiles via SceneryIndex — these are zoom 12 tiles
        let tiles = index.tiles_in_region(region);
        assert!(!tiles.is_empty());
        assert!(tiles.iter().all(|t| t.zoom == 12));

        // Populate the mock disk checker with every tile's chunk coords.
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());

        // Promote should work with SceneryIndex (not hardcoded zoom 14)
        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );
        assert_eq!(
            promoted.len(),
            1,
            "Should promote when all scenery index tiles are on disk"
        );

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_promote_fails_with_wrong_zoom_cached() {
        let geo_index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // SceneryIndex at chunk_zoom 16 -> tile zoom 12
        let index = make_scenery_index_for_region(50, 9, 16);

        // Cache the region's real tiles but at zoom 14 (the OLD wrong
        // behavior) instead of zoom 12. Row and col are taken from the
        // region's actual tile set so zoom is the only differing
        // component — otherwise a zoom-insensitive presence check could
        // pass this test for the wrong reason (row/col mismatch alone).
        let region_tiles = index.tiles_in_region(region);
        assert!(
            !region_tiles.is_empty(),
            "Precondition: index covers region (50, 9)"
        );
        let wrong_tiles: Vec<TileCoord> = region_tiles
            .iter()
            .map(|t| TileCoord { zoom: 14, ..*t })
            .collect();
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(wrong_tiles.into_iter());

        // Promote should NOT succeed — cached zoom 14, but index expects zoom 12
        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );
        assert_eq!(
            promoted.len(),
            0,
            "Should not promote when cached tiles are at wrong zoom level"
        );

        let state = geo_index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
    }

    // =========================================================================
    // Region tile-set SSOT regression tests (#176)
    // =========================================================================
    //
    // Note: the `row`/`col` values in the `SceneryTile` fixtures below are
    // arbitrary and not geographically consistent with their `lat`/`lon` —
    // only `lat`/`lon` drive region membership in `tiles_in_region`. `row`/`col`
    // just need to be distinct per tile so dedup/coverage assertions are
    // meaningful.

    use crate::prefetch::scenery_index::SceneryTile;

    /// Build a [`DdsDiskCacheChecker`] that reports only the listed tiles as
    /// present on disk.
    fn test_checker(tiles: &[TileCoord]) -> Arc<dyn DdsDiskCacheChecker> {
        MockDiskChecker::with_tile_coords(tiles.iter().copied())
    }

    #[test]
    fn test_promotion_not_blocked_by_missing_tile_in_adjacent_region() {
        // Regression for #176 defect 1. The old implementation queried a 45nm
        // radius around the region centre, which reaches ~15nm into the regions
        // north and south. Promotion then demanded tiles that were never
        // submitted for this region, so under a moving prefetch box the fast
        // path could effectively never fire.
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(33, -119);

        // One tile inside the region, cached.
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        // One tile in the region immediately north, NOT cached. Well within
        // 45nm of the +33-119 centre, so the old code demanded it.
        index.add_tile(SceneryTile {
            row: 3000,
            col: 4000,
            chunk_zoom: 16,
            lat: 34.2,
            lon: -118.5,
            is_sea: false,
        });

        let geo_index = GeoIndex::new();
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Checker knows only the in-region tile.
        let checker = test_checker(&[TileCoord {
            row: 1000 / 16,
            col: 2000 / 16,
            zoom: 12,
        }]);

        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );

        assert_eq!(
            promoted.len(),
            1,
            "the adjacent region's missing tile must not block promotion"
        );
        assert!(geo_index
            .get::<PrefetchedRegion>(&region)
            .unwrap()
            .is_prefetched());
    }

    #[test]
    fn test_promotion_blocked_by_missing_tile_inside_region() {
        // The other half of the contract: a genuinely incomplete region must
        // not be promoted.
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(33, -119);

        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 5000,
            col: 6000,
            chunk_zoom: 16,
            lat: 33.7,
            lon: -118.2,
            is_sea: false,
        });

        let geo_index = GeoIndex::new();
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let checker = test_checker(&[TileCoord {
            row: 1000 / 16,
            col: 2000 / 16,
            zoom: 12,
        }]);

        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );

        assert_eq!(
            promoted.len(),
            0,
            "a missing in-region tile must block promotion"
        );
        assert!(geo_index
            .get::<PrefetchedRegion>(&region)
            .unwrap()
            .is_in_progress());
    }

    // =========================================================================
    // region_disk_state tests (#176 Task 1 — single completeness predicate)
    // =========================================================================

    #[test]
    fn test_region_disk_state_unknown_without_checker() {
        let index = make_scenery_index_for_region(50, 9, 16);
        let region = DsfRegion { lat: 50, lon: 9 };
        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                None,
                None,
                CoverageDetail::ExactCounts
            ),
            RegionDiskState::Unknown,
            "no checker means we cannot tell — must not be reported as Incomplete"
        );
    }

    #[test]
    fn test_region_disk_state_no_tiles_for_unindexed_region() {
        let index = make_scenery_index_for_region(50, 9, 16);
        let checker = test_checker(&[]);
        // A region the index knows nothing about.
        let region = DsfRegion { lat: -40, lon: 170 };
        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&checker),
                None,
                CoverageDetail::ExactCounts
            ),
            RegionDiskState::NoTiles
        );
    }

    #[test]
    fn test_region_disk_state_reports_covered_and_total() {
        // Index with 3 tiles in the region; mark 2 of them present on disk.
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(47, 8);
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 47.1,
            lon: 8.1,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 3000,
            col: 4000,
            chunk_zoom: 16,
            lat: 47.4,
            lon: 8.4,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 5000,
            col: 6000,
            chunk_zoom: 16,
            lat: 47.7,
            lon: 8.7,
            is_sea: false,
        });
        let tiles = index.tiles_in_region(region);
        assert_eq!(tiles.len(), 3, "fixture precondition: exactly 3 tiles");

        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(vec![tiles[0], tiles[1]]);

        let state = BoundaryStrategy::region_disk_state(
            region,
            Some(&index),
            Some(&checker),
            None,
            CoverageDetail::ExactCounts,
        );

        assert_eq!(
            state,
            RegionDiskState::Incomplete {
                coverage: TileCoverage::Exact {
                    covered: 2,
                    total: 3
                }
            },
            "must report how many of the region's tiles are present, not just that it is incomplete"
        );
    }

    #[test]
    fn test_region_disk_state_complete_and_incomplete() {
        let index = make_scenery_index_for_region(50, 9, 16);
        let region = DsfRegion { lat: 50, lon: 9 };
        let tiles = index.tiles_in_region(region);
        assert!(
            tiles.len() >= 2,
            "fixture must expose >=2 tiles for this test to discriminate"
        );

        let all: Vec<TileCoord> = tiles.clone();
        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&test_checker(&all)),
                None,
                CoverageDetail::ExactCounts
            ),
            RegionDiskState::Complete
        );

        let all_but_one: Vec<TileCoord> = tiles.iter().skip(1).cloned().collect();
        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&test_checker(&all_but_one)),
                None,
                CoverageDetail::ExactCounts
            ),
            RegionDiskState::Incomplete {
                coverage: TileCoverage::Exact {
                    covered: tiles.len() - 1,
                    total: tiles.len()
                }
            }
        );
    }

    // =========================================================================
    // CoverageDetail short-circuit (#226 review C-1)
    // =========================================================================

    /// A checker that reports every tile absent and counts how many times it
    /// was asked. Counting is the only way to observe short-circuiting: both
    /// details agree on the *verdict*, and it is the syscall count that the
    /// 2-second promotion cycle cannot afford.
    struct CountingDiskChecker {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingDiskChecker {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl DdsDiskCacheChecker for CountingDiskChecker {
        fn tile_exists(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { false })
        }

        fn tile_exists_blocking(&self, _row: u32, _col: u32, _zoom: u8) -> bool {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        }
    }

    /// Build a scenery index holding exactly three tiles in (47, 8), none of
    /// which any checker will report as present.
    fn three_tile_index() -> (Arc<SceneryIndex>, DsfRegion) {
        let index = Arc::new(SceneryIndex::with_defaults());
        let region = DsfRegion::new(47, 8);
        for (row, col, lat, lon) in [
            (1000u32, 2000u32, 47.1f32, 8.1f32),
            (3000, 4000, 47.4, 8.4),
            (5000, 6000, 47.7, 8.7),
        ] {
            index.add_tile(SceneryTile {
                row,
                col,
                chunk_zoom: 16,
                lat,
                lon,
                is_sea: false,
            });
        }
        assert_eq!(
            index.tiles_in_region(region).len(),
            3,
            "fixture precondition: exactly 3 tiles"
        );
        (index, region)
    }

    #[test]
    fn test_first_miss_only_stops_at_the_first_absent_tile() {
        // The hot path: `promote_completed_regions` runs every 2s over every
        // InProgress region, and each absent tile costs ~4 x N_sources stats
        // once it falls through to the OrthoUnionIndex. Counting tiles it then
        // discards is the amplification C-1 removes.
        let (index, region) = three_tile_index();
        let counting = CountingDiskChecker::new();
        let checker: Arc<dyn DdsDiskCacheChecker> = counting.clone();

        let state = BoundaryStrategy::region_disk_state(
            region,
            Some(&index),
            Some(&checker),
            None,
            CoverageDetail::FirstMissOnly,
        );

        assert_eq!(
            state,
            RegionDiskState::Incomplete {
                coverage: TileCoverage::NotCounted
            },
            "a short-circuited scan must not report counts it never computed"
        );
        assert_eq!(
            counting.calls(),
            1,
            "must stop at the first missing tile, not scan all 3"
        );
    }

    #[test]
    fn test_exact_counts_scans_every_tile() {
        // The contrast case: the rescue path genuinely needs `covered`, and
        // pays a full scan for it.
        let (index, region) = three_tile_index();
        let counting = CountingDiskChecker::new();
        let checker: Arc<dyn DdsDiskCacheChecker> = counting.clone();

        let state = BoundaryStrategy::region_disk_state(
            region,
            Some(&index),
            Some(&checker),
            None,
            CoverageDetail::ExactCounts,
        );

        assert_eq!(
            state,
            RegionDiskState::Incomplete {
                coverage: TileCoverage::Exact {
                    covered: 0,
                    total: 3
                }
            }
        );
        assert_eq!(
            counting.calls(),
            3,
            "exact counts require checking every tile"
        );
    }

    #[test]
    fn test_first_miss_only_still_confirms_a_complete_region() {
        // Short-circuiting must not weaken the promotion verdict: a complete
        // region has no first miss, so every tile is checked and the answer is
        // identical to the exact-count scan.
        let index = make_scenery_index_for_region(50, 9, 16);
        let region = DsfRegion { lat: 50, lon: 9 };
        let tiles = index.tiles_in_region(region);
        let checker = test_checker(&tiles);

        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&checker),
                None,
                CoverageDetail::FirstMissOnly
            ),
            RegionDiskState::Complete
        );
    }

    #[test]
    fn test_not_counted_carries_no_readable_counts() {
        // The mechanism itself: "not counted" must be impossible to misread as
        // an observation. `counts()` is the only accessor and it returns None.
        assert_eq!(TileCoverage::NotCounted.counts(), None);
        assert_eq!(
            TileCoverage::Exact {
                covered: 2,
                total: 5
            }
            .counts(),
            Some((2, 5))
        );
    }

    #[test]
    fn test_promote_completed_regions_uses_first_miss_only() {
        // Regression guard for #226 final review: the final review round
        // mutated `promote_completed_regions`'s call site from
        // `CoverageDetail::FirstMissOnly` back to `ExactCounts` and the full
        // suite passed green — nothing observed that the cheap scan was
        // actually requested at the call site the C-1 fix depends on. This
        // test drives `promote_completed_regions` itself (not
        // `region_disk_state` directly) so a regression at that call site
        // fails it.
        let geo_index = GeoIndex::new();
        let (index, region) = three_tile_index();
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let counting = CountingDiskChecker::new();
        let checker: Arc<dyn DdsDiskCacheChecker> = counting.clone();

        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            Some(&checker),
            Some(&index),
            None,
        );

        assert!(
            promoted.is_empty(),
            "region has no tiles on disk, must not promote"
        );
        assert_eq!(
            counting.calls(),
            1,
            "promote_completed_regions must scan with FirstMissOnly and stop \
             at the first absent tile, not ExactCounts over all 3"
        );
    }

    // =========================================================================
    // region_disk_state + OrthoUnionIndex coverage (#176 Task 2 / #223 F2)
    // =========================================================================
    //
    // Prefetch deliberately never downloads tiles that already ship as real
    // `.dds` files in installed scenery packages (stage-3 `filter_disk_tiles`
    // in the submit-side filter pipeline). Promotion must recognise those
    // tiles as covered too, or the region can never be confirmed and
    // ratchets to `NoCoverage` over land — a false alarm.

    /// Build an [`OrthoUnionIndex`] backed by real `.dds` files on disk,
    /// named at the given CHUNK coordinates (row, col, zoom) — matching how
    /// `dds_tile_exists` resolves `textures/{row}_{col}_{prefix}{zoom}.dds`.
    ///
    /// Takes the backing `TempDir` by reference — same pattern as
    /// `create_package_with_dds` in `ortho_union/index.rs` — so the caller
    /// owns the guard and its files stay on disk for exactly the scope of
    /// the calling test function. `dds_tile_exists` stats the real
    /// filesystem on every call (lazy resolution), so the `TempDir` must
    /// outlive every lookup made against the returned index, but no longer:
    /// no leaking required.
    fn make_ortho_index_with_chunk_tiles(
        temp: &tempfile::TempDir,
        chunk_tiles: Vec<(u32, u32, u8)>,
    ) -> Arc<crate::ortho_union::OrthoUnionIndex> {
        std::fs::create_dir_all(temp.path().join("textures")).unwrap();
        for (row, col, zoom) in chunk_tiles {
            let filename = format!("{row}_{col}_ZL{zoom}.dds");
            std::fs::write(temp.path().join("textures").join(filename), b"dds content").unwrap();
        }
        let source = crate::ortho_union::OrthoSource::new_package("test", temp.path());
        Arc::new(crate::ortho_union::OrthoUnionIndex::with_sources(vec![
            source,
        ]))
    }

    #[test]
    fn test_region_complete_when_tiles_ship_in_an_installed_package() {
        // A region whose tiles are NOT in the XEL DDS cache but ARE present as
        // real .dds files in an installed package. Prefetch correctly never
        // submitted them (stage-3 disk filter), so promotion must still be able
        // to confirm the region — otherwise it ratchets to NoCoverage over land.
        let index = make_scenery_index_for_region(50, 9, 16);
        let region = DsfRegion { lat: 50, lon: 9 };
        let tiles = index.tiles_in_region(region);
        assert!(!tiles.is_empty(), "fixture precondition");

        let empty_xel_cache = test_checker(&[]);

        // Seed the ortho index at CHUNK coordinates — dds_tile_exists takes
        // chunk coords while tile_exists_blocking takes tile coords. If the
        // implementation passes tile coords here the lookup silently misses and
        // this test fails, which is the point.
        let temp = tempfile::TempDir::new().unwrap();
        let ortho = make_ortho_index_with_chunk_tiles(
            &temp,
            tiles.iter().map(|t| t.chunk_origin()).collect::<Vec<_>>(),
        );

        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&empty_xel_cache),
                Some(&ortho),
                CoverageDetail::ExactCounts,
            ),
            RegionDiskState::Complete
        );
    }

    #[test]
    fn test_region_incomplete_when_neither_source_has_the_tile() {
        // Guards the disjunction against becoming a tautology.
        let index = make_scenery_index_for_region(50, 9, 16);
        let region = DsfRegion { lat: 50, lon: 9 };
        let tiles = index.tiles_in_region(region);
        assert!(!tiles.is_empty(), "fixture precondition");
        let temp = tempfile::TempDir::new().unwrap();
        let ortho = make_ortho_index_with_chunk_tiles(&temp, vec![]);
        assert_eq!(
            BoundaryStrategy::region_disk_state(
                region,
                Some(&index),
                Some(&test_checker(&[])),
                Some(&ortho),
                CoverageDetail::ExactCounts
            ),
            RegionDiskState::Incomplete {
                coverage: TileCoverage::Exact {
                    covered: 0,
                    total: tiles.len()
                }
            }
        );
    }

    // -------------------------------------------------------------------------
    // evict_non_retained tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_evict_non_retained_removes_prefetched_outside_retained() {
        let geo_index = GeoIndex::new();

        // Retained window covers (50,7) and (51,7)
        geo_index.insert::<RetainedRegion>(DsfRegion::new(50, 7), RetainedRegion);
        geo_index.insert::<RetainedRegion>(DsfRegion::new(51, 7), RetainedRegion);

        // Prefetched entries: two inside, two outside
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(48, 7), PrefetchedRegion::prefetched());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(52, 5), PrefetchedRegion::no_coverage());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        assert_eq!(evicted.len(), 2);
        assert!(evicted.contains(&DsfRegion::new(48, 7)));
        assert!(evicted.contains(&DsfRegion::new(52, 5)));
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(48, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(52, 5)));
    }

    #[test]
    fn test_evict_non_retained_preserves_in_progress() {
        let geo_index = GeoIndex::new();

        // Retained window only covers (52, 7) — both (50,7) and (51,7) are outside
        geo_index.insert::<RetainedRegion>(DsfRegion::new(52, 7), RetainedRegion);

        // InProgress at (50,7) is outside retained, but should be preserved
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::in_progress());
        // Prefetched at (51,7) is outside retained, should be evicted
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        assert_eq!(evicted, vec![DsfRegion::new(51, 7)]); // Only Prefetched, not InProgress
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(!geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
    }

    #[test]
    fn test_evict_non_retained_noop_when_no_retained_regions() {
        let geo_index = GeoIndex::new();

        // RetainedRegion layer is empty — retention not yet active
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(50, 7), PrefetchedRegion::prefetched());
        geo_index.insert::<PrefetchedRegion>(DsfRegion::new(51, 7), PrefetchedRegion::prefetched());

        let evicted = BoundaryStrategy::evict_non_retained(&geo_index);

        // Should not evict anything — retention not active means we can't determine
        // what's outside the window
        assert!(evicted.is_empty());
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(50, 7)));
        assert!(geo_index.contains::<PrefetchedRegion>(&DsfRegion::new(51, 7)));
    }
}
