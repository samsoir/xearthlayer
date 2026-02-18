//! Layer data types for the GeoIndex.
//!
//! Each layer type defines a schema for one aspect of geospatial data stored
//! in the [`GeoIndex`](super::GeoIndex). Implementors of [`GeoLayer`] can be
//! stored and queried by type, enabling a schemaless design where each use case
//! defines the data it needs.

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

#[cfg(test)]
mod tests {
    use super::*;

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

    // Compile-time assertions for trait bounds
    fn _assert_send_sync<T: Send + Sync + 'static>() {}
    fn _assert_geo_layer_bounds() {
        _assert_send_sync::<PatchCoverage>();
    }
}
