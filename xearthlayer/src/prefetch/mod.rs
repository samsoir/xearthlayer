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
mod circuit_breaker;
mod condition;
pub mod cone;
pub mod config;
pub mod coordinates;
mod error;
mod heading_aware;
pub mod inference;
pub mod intersection;
mod listener;
mod load_monitor;
mod predictor;
mod prewarm;
mod radial;
pub mod scenery_cache;
mod scenery_index;
mod scheduler;
mod state;
mod strategy;
mod throttler;
pub mod tile_based;
pub mod types;

pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use condition::{
    AlwaysActiveCondition, MinimumSpeedCondition, NeverActiveCondition, PrefetchCondition,
};
pub use error::PrefetchError;
pub use listener::TelemetryListener;
pub use load_monitor::{FuseLoadMonitor, SharedFuseLoadMonitor};
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
pub use throttler::{AlwaysThrottle, NeverThrottle, PrefetchThrottler, ThrottleState};

// Heading-aware prefetch exports
pub use buffer::{merge_prefetch_tiles, BufferGenerator};
pub use cone::ConeGenerator;
pub use config::{FuseInferenceConfig, HeadingAwarePrefetchConfig};
pub use heading_aware::{
    HeadingAwarePrefetchStats, HeadingAwarePrefetchStatsSnapshot, HeadingAwarePrefetcher,
    HeadingAwarePrefetcherConfig,
};
pub use inference::{Direction, FuseRequestAnalyzer, LoadedEnvelope, TileRequestCallback};
pub use types::{InputMode, PrefetchTile, PrefetchZone, TurnDirection, TurnState};

// Builder for prefetcher strategy creation
pub use builder::{PrefetchStrategy, PrefetcherBuilder};

// Scenery-aware prefetch
pub use scenery_index::{
    IndexingProgress, SceneryIndex, SceneryIndexConfig, SceneryIndexError, SceneryTile,
};

// Cold-start prewarm
pub use prewarm::{PrewarmConfig, PrewarmPrefetcher, PrewarmProgress};

// Scenery index cache
pub use scenery_cache::{
    cache_path as scenery_cache_path, cache_status, load_cache, save_cache, CacheLoadResult,
    CacheStatus,
};

// Tile-based prefetch (DSF-aligned)
pub use tile_based::{
    DdsAccessEvent, DsfTileCoord, TileBasedConfig, TileBasedPrefetcher, TileBurstTracker,
    TilePredictor as DsfTilePredictor,
};
