//! X-Plane Request Model (Scene Tracker)
//!
//! This module maintains an empirical model of what scenery X-Plane has requested,
//! detecting loading patterns and providing data for position inference and prefetch
//! prediction.
//!
//! # Design Philosophy
//!
//! **Empirical data is the foundation. Inference is a calculation on top.**
//!
//! - **Store**: What X-Plane actually requested (DDS tile coordinates)
//! - **Derive**: 1°×1° regions, position, bounds via calculation
//! - **Never assume**: Mapping between DDS tiles and geographic regions
//!
//! The Scene Tracker does NOT interpret or infer meaning from the data. It maintains
//! a factual model of what X-Plane has requested. Interpretation (e.g., "the aircraft
//! is probably here") is the responsibility of consumers like the APT module.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::scene_tracker::{SceneTracker, DefaultSceneTracker};
//!
//! // Create scene tracker with event channel
//! let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
//! let tracker = DefaultSceneTracker::new(rx);
//!
//! // Query loaded regions (derived from empirical tile data)
//! let regions = tracker.loaded_regions();
//! let bounds = tracker.loaded_bounds();
//! ```

mod coords;
mod model;
mod tracker;

pub use coords::{tile_to_lat_lon, TileCoordConversion};
pub use model::{DdsTileCoord, FuseAccessEvent, GeoBounds, GeoRegion, SceneLoadingState};
pub use tracker::{DefaultSceneTracker, SceneTracker, SceneTrackerConfig};
