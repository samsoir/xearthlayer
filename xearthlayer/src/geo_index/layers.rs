//! Layer data types for the GeoIndex.
//!
//! Each layer type defines a schema for one aspect of geospatial data stored
//! in the [`GeoIndex`](super::GeoIndex). Implementors of [`GeoLayer`] can be
//! stored and queried by type, enabling a schemaless design where each use case
//! defines the data it needs.

use std::time::Instant;

/// Marker trait for data types stored in the GeoIndex.
///
/// Each layer type defines a schema for one aspect of geospatial data.
/// Implementors must be `Send + Sync + 'static` for thread-safe storage,
/// and `Clone` for safe retrieval across thread boundaries.
///
/// # Example
///
/// ```
/// use xearthlayer::geo_index::GeoLayer;
///
/// #[derive(Debug, Clone)]
/// struct FlightPath {
///     callsign: String,
/// }
/// impl GeoLayer for FlightPath {}
/// ```
pub trait GeoLayer: Clone + Send + Sync + 'static {}

/// Indicates a region is owned by a scenery patch.
///
/// When present for a region in the GeoIndex, package resources in that region
/// should be hidden from FUSE — only the patch's resources are visible.
///
/// # Usage
///
/// ```
/// use xearthlayer::geo_index::{GeoIndex, DsfRegion, PatchCoverage};
///
/// let index = GeoIndex::new();
/// let region = DsfRegion::new(43, 6);
/// index.insert::<PatchCoverage>(region, PatchCoverage {
///     patch_name: "XEL Patch EU Nice 1".to_string(),
/// });
/// assert!(index.contains::<PatchCoverage>(&region));
/// ```
#[derive(Debug, Clone)]
pub struct PatchCoverage {
    /// Name of the patch owning this region.
    pub patch_name: String,
}

impl GeoLayer for PatchCoverage {}

/// Marks a DSF region as retained by X-Plane (inferred from SceneTracker
/// observations and the sliding window + buffer model).
///
/// Inserted/removed by `SceneryWindow` each coordinator cycle.
#[derive(Debug, Clone, Copy)]
pub struct RetainedRegion;

impl GeoLayer for RetainedRegion {}

/// Tracks the prefetch state of a DSF region in the XEL Window.
///
/// Follows a two-phase commit pattern:
/// - `InProgress`: tiles submitted to executor, awaiting cache confirmation
/// - `Prefetched`: all tiles confirmed in cache
/// - `NoCoverage`: no scenery data exists for this region (skip silently)
///
/// Regions not in the index are considered "absent" (eligible for prefetch).
#[derive(Debug, Clone)]
pub struct PrefetchedRegion {
    state: RegionState,
    since: Instant,
}

/// The state of a prefetched DSF region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionState {
    /// Tiles submitted to executor, awaiting cache confirmation.
    InProgress,
    /// All tiles confirmed in cache.
    Prefetched,
    /// No scenery data exists for this region.
    NoCoverage,
}

impl PrefetchedRegion {
    /// Create a new `InProgress` state (tiles submitted, awaiting confirmation).
    pub fn in_progress() -> Self {
        Self {
            state: RegionState::InProgress,
            since: Instant::now(),
        }
    }

    /// Create a new `Prefetched` state (all tiles confirmed in cache).
    pub fn prefetched() -> Self {
        Self {
            state: RegionState::Prefetched,
            since: Instant::now(),
        }
    }

    /// Create a new `NoCoverage` state (no scenery data for this region).
    pub fn no_coverage() -> Self {
        Self {
            state: RegionState::NoCoverage,
            since: Instant::now(),
        }
    }

    /// Returns `true` if this region is in the `InProgress` state.
    pub fn is_in_progress(&self) -> bool {
        self.state == RegionState::InProgress
    }

    /// Returns `true` if this region is in the `Prefetched` state.
    pub fn is_prefetched(&self) -> bool {
        self.state == RegionState::Prefetched
    }

    /// Returns `true` if this region is in the `NoCoverage` state.
    pub fn is_no_coverage(&self) -> bool {
        self.state == RegionState::NoCoverage
    }

    /// Returns `true` if this `InProgress` region has exceeded the staleness timeout.
    ///
    /// Only `InProgress` regions can be stale; `Prefetched` and `NoCoverage` are
    /// terminal states and never expire.
    pub fn is_stale(&self, timeout: std::time::Duration) -> bool {
        self.state == RegionState::InProgress && self.since.elapsed() >= timeout
    }

    /// Returns `true` if the given region should be prefetched.
    ///
    /// A region should be prefetched only if it is absent from the index
    /// (no entry means never evaluated). Any present state (`InProgress`,
    /// `Prefetched`, or `NoCoverage`) means the region should be skipped.
    pub fn should_prefetch(index: &super::GeoIndex, region: &super::DsfRegion) -> bool {
        !index.contains::<PrefetchedRegion>(region)
    }
}

impl GeoLayer for PrefetchedRegion {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo_index::{DsfRegion, GeoIndex};

    #[test]
    fn test_retained_region_is_geo_layer() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        index.insert::<RetainedRegion>(region, RetainedRegion);
        assert!(index.contains::<RetainedRegion>(&region));
    }

    #[test]
    fn test_retained_region_removal() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        index.insert::<RetainedRegion>(region, RetainedRegion);
        index.remove::<RetainedRegion>(&region);
        assert!(!index.contains::<RetainedRegion>(&region));
    }

    #[test]
    fn test_patch_coverage_clone() {
        let pc = PatchCoverage {
            patch_name: "Test Patch".to_string(),
        };
        let cloned = pc.clone();
        assert_eq!(cloned.patch_name, "Test Patch");
    }

    #[test]
    fn test_patch_coverage_debug() {
        let pc = PatchCoverage {
            patch_name: "Test".to_string(),
        };
        let debug = format!("{:?}", pc);
        assert!(debug.contains("PatchCoverage"));
        assert!(debug.contains("Test"));
    }

    #[test]
    fn test_prefetched_region_in_progress() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        let state = index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_in_progress());
        assert!(!state.is_prefetched());
        assert!(!state.is_no_coverage());
    }

    #[test]
    fn test_prefetched_region_confirmed() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        let state = index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_prefetched());
    }

    #[test]
    fn test_prefetched_region_no_coverage() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());
        let state = index.get::<PrefetchedRegion>(&region).unwrap();
        assert!(state.is_no_coverage());
    }

    #[test]
    fn test_prefetched_region_staleness_timeout() {
        use std::time::Duration;
        let state = PrefetchedRegion::in_progress();
        // Fresh InProgress should not be stale
        assert!(!state.is_stale(Duration::from_secs(120)));
    }

    #[test]
    fn test_prefetched_region_should_prefetch() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(50, 9);

        // Absent → should prefetch
        assert!(PrefetchedRegion::should_prefetch(&index, &region));

        // InProgress → should NOT prefetch
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        assert!(!PrefetchedRegion::should_prefetch(&index, &region));

        // Prefetched → should NOT prefetch
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
        assert!(!PrefetchedRegion::should_prefetch(&index, &region));

        // NoCoverage → should NOT prefetch
        index.insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());
        assert!(!PrefetchedRegion::should_prefetch(&index, &region));
    }

    // Compile-time assertions for trait bounds
    fn _assert_send_sync<T: Send + Sync + 'static>() {}
    fn _assert_geo_layer_bounds() {
        _assert_send_sync::<PatchCoverage>();
        _assert_send_sync::<PrefetchedRegion>();
    }
}
