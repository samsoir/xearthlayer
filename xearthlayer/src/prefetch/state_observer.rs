//! Detects divergence between prefetch's region-state claims and reality.
//!
//! When FUSE has to generate a tile on demand in a region prefetch marked
//! `Prefetched` or `NoCoverage`, the claim was wrong. Three causes produce
//! this — a gap in the scenery index, a premature promotion, or eviction of
//! the tile after promotion — and the response is the same for all three:
//! clear the state so the region is re-prefetched. See #176.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::coord::TileCoord;
use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion};
use crate::metrics::MetricsClient;

/// Minimum interval between demotions of the same region.
///
/// Mirrors the default `prefetch.stale_region_timeout`. It is a constant
/// rather than plumbed config because FUSE has no reason to depend on the
/// prefetch config, and this bounds a diagnostic response, not a correctness
/// decision. Eviction inside the retained window can recur, and an unbounded
/// demote → re-prefetch → evict loop would churn on a long flight.
const DEFAULT_DEMOTION_INTERVAL: Duration = Duration::from_secs(120);

pub struct PrefetchStateObserver {
    geo_index: Arc<GeoIndex>,
    metrics_client: Option<MetricsClient>,
    /// Last demotion per region. Bounded by the number of regions that have
    /// ever diverged, which is small enough not to need eviction.
    last_demotion: Mutex<HashMap<DsfRegion, Instant>>,
    demotion_interval: Duration,
    divergences: AtomicU64,
    demotions: AtomicU64,
}

impl PrefetchStateObserver {
    pub fn new(geo_index: Arc<GeoIndex>) -> Self {
        Self {
            geo_index,
            metrics_client: None,
            last_demotion: Mutex::new(HashMap::new()),
            demotion_interval: DEFAULT_DEMOTION_INTERVAL,
            divergences: AtomicU64::new(0),
            demotions: AtomicU64::new(0),
        }
    }

    pub fn with_metrics_client(mut self, client: MetricsClient) -> Self {
        self.metrics_client = Some(client);
        self
    }

    /// Override the demotion rate-limit window. Test seam.
    pub fn with_demotion_interval(mut self, interval: Duration) -> Self {
        self.demotion_interval = interval;
        self
    }

    /// Total divergences observed since this observer was created.
    ///
    /// Mirrors the `PrefetchStateDiverged` metric emitted on the same path.
    /// It exists because `MetricsClient` is fire-and-forget over an
    /// unbounded channel — a test cannot synchronously assert that a metric
    /// was emitted, so this counter gives the rate-limiting behaviour
    /// something observable to check.
    pub fn divergences(&self) -> u64 {
        self.divergences.load(Ordering::Relaxed)
    }

    /// Total demotions performed since this observer was created.
    ///
    /// Mirrors the `PrefetchRegionDemoted` metric emitted on the same path.
    /// It exists for the same reason as [`Self::divergences`]: the metrics
    /// path cannot be observed synchronously, so tests need a direct way to
    /// confirm the rate limit actually suppressed a demotion.
    pub fn demotions(&self) -> u64 {
        self.demotions.load(Ordering::Relaxed)
    }

    /// Evaluate a completed DDS response.
    ///
    /// Cheap and non-blocking on the common path: a cache hit returns before
    /// any lookup, and a region with no prefetch state returns after one.
    pub fn observe(&self, tile: TileCoord, cache_hit: bool) {
        if cache_hit {
            return;
        }

        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);

        let Some(state) = self.geo_index.get::<PrefetchedRegion>(&region) else {
            return;
        };
        // InProgress regions are expected to miss — prefetch has not finished.
        if !(state.is_prefetched() || state.is_no_coverage()) {
            return;
        }

        self.divergences.fetch_add(1, Ordering::Relaxed);
        if let Some(ref metrics) = self.metrics_client {
            metrics.prefetch_state_diverged();
        }

        if !self.claim_demotion(region) {
            return;
        }

        self.geo_index.remove::<PrefetchedRegion>(&region);
        self.demotions.fetch_add(1, Ordering::Relaxed);
        if let Some(ref metrics) = self.metrics_client {
            metrics.prefetch_region_demoted();
        }

        tracing::warn!(
            lat = region.lat,
            lon = region.lon,
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            was_no_coverage = state.is_no_coverage(),
            "Prefetch state contradicted by on-demand generation — region demoted"
        );
    }

    /// Returns true if this region may be demoted now, recording the attempt.
    ///
    /// The lock is only reached on the divergence path, after both early
    /// returns, so contention is not a concern.
    fn claim_demotion(&self, region: DsfRegion) -> bool {
        let now = Instant::now();
        let mut log = self.last_demotion.lock().unwrap();
        match log.get(&region) {
            Some(last) if now.duration_since(*last) < self.demotion_interval => false,
            _ => {
                log.insert(region, now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo_index::PrefetchedRegion;

    /// A tile whose centre lies inside DsfRegion { lat: 33, lon: -119 }.
    fn tile_in_33_119() -> TileCoord {
        crate::coord::to_tile_coords(33.5, -118.5, 12).unwrap()
    }

    fn region_of(tile: TileCoord) -> DsfRegion {
        let (lat, lon) = tile.to_lat_lon();
        DsfRegion::from_lat_lon(lat, lon)
    }

    #[test]
    fn test_diverged_prefetched_region_is_demoted() {
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));
        observer.observe(tile, false);

        assert!(
            geo_index.get::<PrefetchedRegion>(&region).is_none(),
            "a Prefetched region contradicted by an on-demand generation must be demoted"
        );
    }

    #[test]
    fn test_diverged_nocoverage_region_is_cleared() {
        // NoCoverage is the harsher claim: it excludes the region for the
        // session. A FUSE generation there disproves it outright.
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));
        observer.observe(tile, false);

        assert!(geo_index.get::<PrefetchedRegion>(&region).is_none());
    }

    #[test]
    fn test_cache_hit_is_not_divergence() {
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));
        observer.observe(tile, true);

        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .unwrap()
                .is_prefetched(),
            "serving a prefetched tile from cache is the system working"
        );
    }

    #[test]
    fn test_in_progress_region_is_not_divergence() {
        // A region still being prefetched is *expected* to miss.
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));
        observer.observe(tile, false);

        assert!(geo_index
            .get::<PrefetchedRegion>(&region)
            .unwrap()
            .is_in_progress());
    }

    #[test]
    fn test_untracked_region_is_not_divergence() {
        let geo_index = Arc::new(GeoIndex::new());
        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));
        observer.observe(tile_in_33_119(), false);
        assert_eq!(geo_index.count::<PrefetchedRegion>(), 0);
    }

    #[test]
    fn test_demotion_is_rate_limited_but_divergence_is_always_counted() {
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index));

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        observer.observe(tile, false);
        assert_eq!(observer.divergences(), 1);
        assert_eq!(observer.demotions(), 1);

        // Region re-promoted, then diverges again inside the window.
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        observer.observe(tile, false);

        assert_eq!(observer.divergences(), 2, "every divergence is counted");
        assert_eq!(
            observer.demotions(),
            1,
            "the second demotion is rate limited"
        );
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .unwrap()
                .is_prefetched(),
            "rate-limited divergence must leave the state alone"
        );
    }

    #[test]
    fn test_demotion_allowed_again_after_the_window() {
        let geo_index = Arc::new(GeoIndex::new());
        let tile = tile_in_33_119();
        let region = region_of(tile);

        let observer = PrefetchStateObserver::new(Arc::clone(&geo_index))
            .with_demotion_interval(Duration::ZERO);

        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        observer.observe(tile, false);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        observer.observe(tile, false);

        assert_eq!(
            observer.demotions(),
            2,
            "a zero window permits every demotion"
        );
    }
}
