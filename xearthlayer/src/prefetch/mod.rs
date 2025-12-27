//! Predictive tile caching based on X-Plane telemetry.
//!
//! This module provides functionality to pre-fetch satellite imagery tiles
//! around the aircraft's position to reduce FPS drops when the simulator
//! loads new scenery.
//!
//! # Prefetch Strategies
//!
//! ## Radial Prefetcher (Recommended)
//!
//! Simple, cache-aware prefetching that maintains a buffer of tiles around
//! the current position:
//!
//! ```text
//! X-Plane UDP (49003) → TelemetryListener → AircraftState
//!                                               ↓
//!                                        RadialPrefetcher
//!                                          ├─ Check memory cache
//!                                          ├─ Skip cached tiles
//!                                          ├─ Skip recently-attempted
//!                                          └─ Submit missing tiles
//!                                               ↓
//!                                        DdsHandler (existing pipeline)
//! ```
//!
//! ## Heading-Aware Prefetcher (Coming Soon)
//!
//! Direction-aware prefetching with forward cone and turn detection.
//! See [`config::HeadingAwarePrefetchConfig`] for configuration options.
//!
//! ## Legacy Scheduler
//!
//! Complex flight-path prediction (cone + radial). Use RadialPrefetcher instead.
//!
//! # X-Plane Setup
//!
//! Users must enable UDP data output in X-Plane:
//! 1. Settings → Data Output
//! 2. Enable "Network via UDP"
//! 3. Set destination IP to localhost or machine running XEarthLayer
//! 4. Set port (default 49003)
//! 5. Enable data indices: 3 (speeds), 17 (heading), 20 (position)

mod buffer;
mod builder;
mod condition;
pub mod cone;
pub mod config;
pub mod coordinates;
mod error;
mod heading_aware;
pub mod inference;
mod listener;
mod predictor;
mod radial;
mod scenery_index;
mod scheduler;
mod state;
mod strategy;
pub mod types;

pub use condition::{
    AlwaysActiveCondition, MinimumSpeedCondition, NeverActiveCondition, PrefetchCondition,
};
pub use error::PrefetchError;
pub use listener::TelemetryListener;
pub use predictor::{PredictedTile, TilePredictor};
pub use radial::{
    RadialPrefetchConfig, RadialPrefetchStats, RadialPrefetchStatsSnapshot, RadialPrefetcher,
};
pub use scheduler::{PrefetchScheduler, PrefetchStats, PrefetchStatsSnapshot, SchedulerConfig};
pub use state::{
    AircraftSnapshot, AircraftState, DetailedPrefetchStats, GpsStatus, PrefetchMode,
    PrefetchStatusSnapshot, SharedPrefetchStatus,
};
pub use strategy::Prefetcher;

// Heading-aware prefetch exports
pub use buffer::{merge_prefetch_tiles, BufferGenerator};
pub use cone::ConeGenerator;
pub use config::{FuseInferenceConfig, HeadingAwarePrefetchConfig, ZoomLevelPrefetchConfig};
pub use heading_aware::{
    HeadingAwarePrefetchStats, HeadingAwarePrefetchStatsSnapshot, HeadingAwarePrefetcher,
    HeadingAwarePrefetcherConfig,
};
pub use inference::{Direction, FuseRequestAnalyzer, LoadedEnvelope, TileRequestCallback};
pub use types::{InputMode, PrefetchTile, PrefetchZone, TurnDirection, TurnState};

// Builder for prefetcher strategy creation
pub use builder::{PrefetchStrategy, PrefetcherBuilder};

// Scenery-aware prefetch
pub use scenery_index::{SceneryIndex, SceneryIndexConfig, SceneryIndexError, SceneryTile};
