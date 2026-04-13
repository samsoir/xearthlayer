//! Tile filtering pipeline for prefetch plans.
//!
//! Four sequential filtering stages remove tiles that don't need prefetching:
//! 1. **Memory cache** — tiles already in the volatile memory cache
//! 2. **Patch regions** — tiles in DSF regions owned by scenery patches
//! 3. **Package disk** — tiles present as DDS files in installed Ortho4XP
//!    packages (served via FUSE passthrough, no XEL work needed)
//! 4. **DDS disk cache** — tiles already in XEL's own DDS disk cache from
//!    prior prefetch or FUSE-on-demand generation
//!
//! Each stage returns the filtered list and a count of skipped tiles.
//! Stages are ordered cheapest-first to minimise per-tile filter cost.

use std::collections::HashSet;
use std::sync::Arc;

use crate::coord::TileCoord;
use crate::executor::{DaemonMemoryCache, DdsDiskCacheChecker};
use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::tile_based::DsfTileCoord;

// ─────────────────────────────────────────────────────────────────────────────
// Result type
// ─────────────────────────────────────────────────────────────────────────────

/// Counts from the filtering pipeline.
#[derive(Debug, Default)]
pub(crate) struct FilterCounts {
    /// Tiles skipped because they were in the local tracking set or memory cache.
    pub cache_hits: usize,
    /// Tiles skipped because they are in patch-owned DSF regions.
    pub patch_skipped: usize,
    /// Tiles skipped because a DDS file already exists in an installed package.
    pub disk_skipped: usize,
    /// Tiles skipped because they are already in the XEL DDS disk cache.
    pub dds_disk_hits: usize,
}

impl FilterCounts {
    /// Total tiles filtered across all stages.
    pub fn total(&self) -> usize {
        self.cache_hits + self.patch_skipped + self.disk_skipped + self.dds_disk_hits
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pipeline stages
// ─────────────────────────────────────────────────────────────────────────────

/// Filter tiles already present in the memory cache.
///
/// Returns the surviving tiles and the number of cache hits.
///
/// Consults the memory cache as the authoritative source on every call.
/// The `cached_tiles` parameter is retained for compatibility with the
/// retention machinery (`evict_cached_tiles_outside_retained`) but is no
/// longer read as a fast-path — the memory cache is intentionally small
/// (request absorber, not working-set holder), so a HashSet shadow of
/// prior hits goes stale within seconds, causing the filter to produce
/// false cache-hit reports and starving the prefetcher of real work.
/// See #172 Part 5 post-flight finding: the shadow fast-path was the
/// primary cause of continued FUSE misses on boundary crossings even
/// after Parts 1-4 were shipped.
#[allow(clippy::ptr_arg)]
pub(crate) async fn filter_memory_cache(
    tiles: Vec<TileCoord>,
    cache: &dyn DaemonMemoryCache,
    _cached_tiles: &mut HashSet<TileCoord>,
) -> (Vec<TileCoord>, usize) {
    let mut filtered = Vec::with_capacity(tiles.len());
    let mut hits = 0usize;

    for tile in &tiles {
        // Always query the authoritative memory cache — the HashSet
        // shadow is no longer trusted (see doc comment).
        if cache.contains(tile.row, tile.col, tile.zoom).await {
            hits += 1;
            continue;
        }

        filtered.push(*tile);
    }

    if hits > 0 {
        tracing::debug!(
            cache_hits = hits,
            remaining = filtered.len(),
            "Filtered cached tiles from prefetch plan"
        );
    }

    (filtered, hits)
}

/// Filter tiles in DSF regions owned by scenery patches.
///
/// Returns the surviving tiles and the number of patch-filtered tiles.
pub(crate) fn filter_patched_regions(
    tiles: Vec<TileCoord>,
    geo_index: &GeoIndex,
) -> (Vec<TileCoord>, usize) {
    let before = tiles.len();
    let filtered: Vec<TileCoord> = tiles
        .into_iter()
        .filter(|tile| {
            let (lat, lon) = tile.to_lat_lon();
            let dsf = DsfTileCoord::from_lat_lon(lat, lon);
            !geo_index.contains::<PatchCoverage>(&DsfRegion::new(dsf.lat, dsf.lon))
        })
        .collect();
    let skipped = before - filtered.len();

    if skipped > 0 {
        tracing::debug!(
            patch_skipped = skipped,
            remaining = filtered.len(),
            "Filtered tiles in patched regions"
        );
    }

    (filtered, skipped)
}

/// Filter tiles that already exist as DDS files on disk.
///
/// Returns the surviving tiles and the number of disk-filtered tiles.
pub(crate) fn filter_disk_tiles(
    tiles: Vec<TileCoord>,
    ortho_index: &OrthoUnionIndex,
) -> (Vec<TileCoord>, usize) {
    let before = tiles.len();
    let filtered: Vec<TileCoord> = tiles
        .into_iter()
        .filter(|tile| {
            let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
            !ortho_index.dds_tile_exists(chunk_row, chunk_col, chunk_zoom)
        })
        .collect();
    let skipped = before - filtered.len();

    if skipped > 0 {
        tracing::debug!(
            skipped,
            remaining = filtered.len(),
            "Filtered tiles already on disk"
        );
    }

    (filtered, skipped)
}

/// Filter tiles already present in XEL's DDS disk cache.
///
/// Without this stage, previously-prefetched tiles that have been evicted
/// from memory cache but still live on DDS disk would be re-submitted to
/// the executor, which then walks cache tiers and reads the ~10MB DDS
/// payload to return a hit. At 100+ tiles/cycle of such redundant reads,
/// this produces constant disk activity AND starves actual new work
/// behind a long queue of redundant cache-hit submissions. See #172
/// post-flight finding: the pending drain path was the primary source
/// of "constant disk reads never stopping" in the TUI.
pub(crate) fn filter_dds_disk_cache(
    tiles: Vec<TileCoord>,
    checker: &Arc<dyn DdsDiskCacheChecker>,
) -> (Vec<TileCoord>, usize) {
    let before = tiles.len();
    let filtered: Vec<TileCoord> = tiles
        .into_iter()
        .filter(|tile| {
            let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
            !checker.tile_exists_blocking(chunk_row, chunk_col, chunk_zoom)
        })
        .collect();
    let hits = before - filtered.len();

    if hits > 0 {
        tracing::debug!(
            dds_disk_hits = hits,
            remaining = filtered.len(),
            "Filtered tiles already in XEL DDS disk cache"
        );
    }

    (filtered, hits)
}

/// Run all filtering stages in sequence.
///
/// Stages run cheapest-first:
/// 1. Memory cache (moka in-memory lookup)
/// 2. Patch regions (GeoIndex DashMap lookup)
/// 3. Installed packages (OrthoUnionIndex in-memory lookup)
/// 4. DDS disk cache (LRU index lookup + stat — most expensive, runs last)
///
/// Returns the surviving tiles and aggregate filter counts.
pub(crate) async fn run_filter_pipeline(
    mut tiles: Vec<TileCoord>,
    memory_cache: Option<&dyn DaemonMemoryCache>,
    cached_tiles: &mut HashSet<TileCoord>,
    geo_index: Option<&Arc<GeoIndex>>,
    ortho_union_index: Option<&Arc<OrthoUnionIndex>>,
    dds_disk_checker: Option<&Arc<dyn DdsDiskCacheChecker>>,
) -> (Vec<TileCoord>, FilterCounts) {
    let mut counts = FilterCounts::default();

    // Stage 1: Memory cache filter
    if let Some(cache) = memory_cache {
        let (filtered, hits) = filter_memory_cache(tiles, cache, cached_tiles).await;
        counts.cache_hits = hits;
        tiles = filtered;
    }

    // Stage 2: Patch region filter
    if let Some(gi) = geo_index {
        let (filtered, skipped) = filter_patched_regions(tiles, gi);
        counts.patch_skipped = skipped;
        tiles = filtered;
    }

    // Stage 3: Installed package filter
    if let Some(index) = ortho_union_index {
        let (filtered, skipped) = filter_disk_tiles(tiles, index);
        counts.disk_skipped = skipped;
        tiles = filtered;
    }

    // Stage 4: DDS disk cache filter
    if let Some(checker) = dds_disk_checker {
        let (filtered, hits) = filter_dds_disk_cache(tiles, checker);
        counts.dds_disk_hits = hits;
        tiles = filtered;
    }

    (tiles, counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn test_tiles(count: usize) -> Vec<TileCoord> {
        (0..count)
            .map(|i| TileCoord {
                row: 100 + i as u32,
                col: 200,
                zoom: 14,
            })
            .collect()
    }

    #[test]
    fn test_filter_patched_regions_empty_input() {
        let geo_index = GeoIndex::new();
        let (result, skipped) = filter_patched_regions(vec![], &geo_index);
        assert!(result.is_empty());
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_filter_patched_regions_no_patches() {
        let geo_index = GeoIndex::new();
        let tiles = test_tiles(5);
        let (result, skipped) = filter_patched_regions(tiles, &geo_index);
        assert_eq!(result.len(), 5);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_filter_patched_regions_all_patched() {
        let geo_index = GeoIndex::new();
        let tiles = test_tiles(3);

        // Patch every possible DSF region these tiles fall in
        for tile in &tiles {
            let (lat, lon) = tile.to_lat_lon();
            let dsf = DsfTileCoord::from_lat_lon(lat, lon);
            geo_index.insert::<PatchCoverage>(
                DsfRegion::new(dsf.lat, dsf.lon),
                PatchCoverage {
                    patch_name: "test".to_string(),
                },
            );
        }

        let (result, skipped) = filter_patched_regions(tiles, &geo_index);
        assert!(result.is_empty());
        assert_eq!(skipped, 3);
    }

    #[test]
    fn test_filter_patched_regions_preserves_order() {
        let geo_index = GeoIndex::new();
        let tiles = test_tiles(5);

        // Only patch the middle tile's region
        let mid = &tiles[2];
        let (lat, lon) = mid.to_lat_lon();
        let dsf = DsfTileCoord::from_lat_lon(lat, lon);
        geo_index.insert::<PatchCoverage>(
            DsfRegion::new(dsf.lat, dsf.lon),
            PatchCoverage {
                patch_name: "test".to_string(),
            },
        );

        let (result, skipped) = filter_patched_regions(tiles.clone(), &geo_index);
        // Order of non-patched tiles should be preserved
        assert!(skipped >= 1);
        for (i, tile) in result.iter().enumerate() {
            if i > 0 {
                // Rows should be monotonically increasing (preserved order)
                assert!(tile.row >= result[i - 1].row);
            }
        }
    }

    #[test]
    fn test_filter_disk_tiles_empty_index() {
        let index = OrthoUnionIndex::new();
        let tiles = test_tiles(5);
        let (result, skipped) = filter_disk_tiles(tiles, &index);
        assert_eq!(result.len(), 5);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_filter_counts_total() {
        let counts = FilterCounts {
            cache_hits: 3,
            patch_skipped: 2,
            disk_skipped: 1,
            dds_disk_hits: 4,
        };
        assert_eq!(counts.total(), 10);
    }

    #[tokio::test]
    async fn test_run_filter_pipeline_no_filters() {
        let tiles = test_tiles(5);
        let mut tracked = HashSet::new();

        let (result, counts) =
            run_filter_pipeline(tiles, None, &mut tracked, None, None, None).await;
        assert_eq!(result.len(), 5);
        assert_eq!(counts.total(), 0);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Regression guard: #172 post-flight finding — shadow staleness bug
    //
    // The `cached_tiles` HashSet shadow is no longer consulted by
    // `filter_memory_cache`. It went stale in production (memory cache
    // evicts tiles under its small LRU budget, but the shadow keeps saying
    // "cached"), causing the filter to falsely report 100% cache hits and
    // starving the prefetcher of work for minutes at a time.
    //
    // The fix: always query the authoritative memory cache. The shadow is
    // retained as a function parameter for compatibility with retention
    // bookkeeping, but is neither read nor written by the filter.
    //
    // These tests prove the filter does NOT trust the shadow.
    // ─────────────────────────────────────────────────────────────────────────

    use crate::executor::DaemonMemoryCache;

    /// A DaemonMemoryCache that ALWAYS reports "miss" — simulates a cache
    /// that has evicted every tile it once held.
    struct AlwaysMissCache;

    impl DaemonMemoryCache for AlwaysMissCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>
        {
            Box::pin(async { None })
        }

        fn put(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
            _data: Vec<u8>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn test_filter_does_not_trust_stale_shadow_over_authoritative_cache() {
        // Scenario: the caller populated `cached_tiles` (perhaps from a
        // previous cycle's hits), but the memory cache has since evicted
        // every one of those tiles. The filter must NOT claim cache hits
        // based on the shadow — it must query the real cache.
        let cache = AlwaysMissCache;
        let tiles: Vec<TileCoord> = (0..10)
            .map(|i| TileCoord {
                row: 100 + i,
                col: 200,
                zoom: 14,
            })
            .collect();

        // Pre-populate the shadow with every tile — simulating the stale
        // state seen in production where shadow insertions accumulated
        // faster than retention eviction could clear them.
        let mut stale_shadow: HashSet<TileCoord> = tiles.iter().copied().collect();

        let (filtered, hits) = filter_memory_cache(tiles.clone(), &cache, &mut stale_shadow).await;

        assert_eq!(
            hits, 0,
            "Stale shadow must not produce fake cache hits (real cache always misses)"
        );
        assert_eq!(
            filtered, tiles,
            "All uncached tiles must survive the filter regardless of shadow contents"
        );
    }

    #[tokio::test]
    async fn test_filter_does_not_write_to_shadow() {
        // The shadow is now write-through inert — the filter should not
        // add entries even when tiles ARE truly cached. This decouples
        // the filter from the shadow's lifecycle (retention cleanup no
        // longer races with filter population).
        struct AlwaysHitCache;
        impl DaemonMemoryCache for AlwaysHitCache {
            fn get(
                &self,
                _row: u32,
                _col: u32,
                _zoom: u8,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>
            {
                Box::pin(async { Some(vec![0u8; 16]) })
            }
            fn put(
                &self,
                _row: u32,
                _col: u32,
                _zoom: u8,
                _data: Vec<u8>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
                Box::pin(async {})
            }
        }

        let cache = AlwaysHitCache;
        let tiles = test_tiles(5);
        let mut shadow: HashSet<TileCoord> = HashSet::new();

        let (filtered, hits) = filter_memory_cache(tiles, &cache, &mut shadow).await;

        assert_eq!(hits, 5, "Real hits should still be counted");
        assert!(filtered.is_empty(), "All hits should be filtered out");
        assert!(
            shadow.is_empty(),
            "Filter must not write to the shadow — avoid accumulating stale entries"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // DDS disk cache filter stage (#172 post-flight, "constant disk reads")
    //
    // Tiles in XEL's DDS disk cache must not be re-submitted to the executor.
    // Without this stage, pending-drain cycles re-submit cached tiles, the
    // executor walks cache tiers, reads the DDS payload off disk to return
    // as a hit, and produces constant disk activity while starving genuine
    // work. These tests guard the filter stage.
    // ─────────────────────────────────────────────────────────────────────────

    struct ChunkSetDiskChecker(HashSet<(u32, u32, u8)>);

    impl crate::executor::DdsDiskCacheChecker for ChunkSetDiskChecker {
        fn tile_exists(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            let present = self.0.contains(&(row, col, zoom));
            Box::pin(async move { present })
        }

        fn tile_exists_blocking(&self, row: u32, col: u32, zoom: u8) -> bool {
            self.0.contains(&(row, col, zoom))
        }
    }

    #[test]
    fn test_filter_dds_disk_cache_removes_tiles_in_cache() {
        // Populate checker with 5 of 10 tiles' chunk-origin coords.
        // Filter should strip exactly those 5.
        let tiles = test_tiles(10);
        let mut in_cache = HashSet::new();
        for tile in tiles.iter().take(5) {
            let (r, c, z) = tile.chunk_origin();
            in_cache.insert((r, c, z));
        }
        let checker: Arc<dyn crate::executor::DdsDiskCacheChecker> =
            Arc::new(ChunkSetDiskChecker(in_cache));

        let (filtered, hits) = filter_dds_disk_cache(tiles.clone(), &checker);
        assert_eq!(hits, 5);
        assert_eq!(filtered.len(), 5);
        // Survivors should be the last 5 tiles (which weren't in the cache)
        assert_eq!(filtered, tiles[5..]);
    }

    #[test]
    fn test_filter_dds_disk_cache_empty_checker_passes_all() {
        let tiles = test_tiles(10);
        let checker: Arc<dyn crate::executor::DdsDiskCacheChecker> =
            Arc::new(ChunkSetDiskChecker(HashSet::new()));

        let (filtered, hits) = filter_dds_disk_cache(tiles.clone(), &checker);
        assert_eq!(hits, 0);
        assert_eq!(filtered, tiles);
    }

    #[tokio::test]
    async fn test_run_filter_pipeline_includes_dds_disk_stage() {
        // End-to-end: pipeline with all filters OFF except DDS disk.
        // Tiles in the DDS disk mock should be stripped; the rest survive.
        let tiles = test_tiles(8);
        let mut tracked = HashSet::new();
        let mut in_cache = HashSet::new();
        for tile in tiles.iter().take(3) {
            let (r, c, z) = tile.chunk_origin();
            in_cache.insert((r, c, z));
        }
        let checker: Arc<dyn crate::executor::DdsDiskCacheChecker> =
            Arc::new(ChunkSetDiskChecker(in_cache));

        let (result, counts) = run_filter_pipeline(
            tiles.clone(),
            None,
            &mut tracked,
            None,
            None,
            Some(&checker),
        )
        .await;

        assert_eq!(counts.dds_disk_hits, 3);
        assert_eq!(counts.cache_hits, 0);
        assert_eq!(counts.patch_skipped, 0);
        assert_eq!(counts.disk_skipped, 0);
        assert_eq!(result.len(), 5);
        assert_eq!(result, tiles[3..]);
    }

    #[tokio::test]
    async fn test_filter_correctly_splits_partial_real_cache() {
        // Authoritative check: given a cache that hits on even rows and
        // misses on odd rows, the filter should split accordingly. This
        // confirms the filter actually queries the real cache rather than
        // rubber-stamping via shadow.
        struct EvenRowsCache;
        impl DaemonMemoryCache for EvenRowsCache {
            fn get(
                &self,
                row: u32,
                _col: u32,
                _zoom: u8,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>
            {
                let hit = row % 2 == 0;
                Box::pin(async move {
                    if hit {
                        Some(vec![0u8; 16])
                    } else {
                        None
                    }
                })
            }
            fn put(
                &self,
                _row: u32,
                _col: u32,
                _zoom: u8,
                _data: Vec<u8>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
                Box::pin(async {})
            }
        }

        let cache = EvenRowsCache;
        let tiles = test_tiles(10); // rows 100..109 — evens at 100,102,104,106,108
        let mut shadow = HashSet::new();

        let (filtered, hits) = filter_memory_cache(tiles, &cache, &mut shadow).await;

        assert_eq!(hits, 5, "5 even-row tiles should hit");
        assert_eq!(filtered.len(), 5, "5 odd-row tiles should survive");
        for tile in &filtered {
            assert_eq!(tile.row % 2, 1, "Only odd-row tiles should remain");
        }
    }
}
