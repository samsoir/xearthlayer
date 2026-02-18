//! Geospatial reference database for XEarthLayer.
//!
//! The [`GeoIndex`] provides an in-memory, thread-safe, type-keyed spatial store
//! indexed by 1°×1° [`DsfRegion`] coordinates. Each "layer" (implementing
//! [`GeoLayer`]) is an independent collection of typed data — enabling a
//! schemaless design where each use case defines its own data schema.
//!
//! # Architecture
//!
//! ```text
//! GeoIndex
//! ├── Layer: PatchCoverage
//! │   ├── (+43+006) → PatchCoverage { patch_name: "XEL Nice" }
//! │   └── (+43+007) → PatchCoverage { patch_name: "XEL Nice" }
//! └── Layer: (future types...)
//! ```
//!
//! # Thread Safety
//!
//! - Layer-level access: `RwLock` (rare writes for layer creation)
//! - Region-level access: `DashMap` (concurrent reads, per-shard write locks)
//!
//! # Usage
//!
//! ```
//! use xearthlayer::geo_index::{GeoIndex, DsfRegion, PatchCoverage};
//!
//! let index = GeoIndex::new();
//!
//! // Populate patch coverage
//! index.populate(vec![
//!     (DsfRegion::new(43, 6), PatchCoverage { patch_name: "Nice".to_string() }),
//!     (DsfRegion::new(43, 7), PatchCoverage { patch_name: "Nice".to_string() }),
//! ]);
//!
//! // Query
//! assert!(index.contains::<PatchCoverage>(&DsfRegion::new(43, 6)));
//! assert!(!index.contains::<PatchCoverage>(&DsfRegion::new(50, 10)));
//! ```

mod index;
mod layers;
mod region;

pub use index::GeoIndex;
pub use layers::{GeoLayer, PatchCoverage};
pub use region::DsfRegion;
