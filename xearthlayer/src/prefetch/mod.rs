//! Predictive tile caching based on X-Plane telemetry.
//!
//! This module provides functionality to pre-fetch satellite imagery tiles
//! around the aircraft's position to reduce FPS drops when the simulator
//! loads new scenery.
//!
//! # Prefetch Strategies
//!
//! ## Adaptive Prefetcher (Recommended)
//!
//! Self-calibrating prefetch with flight phase detection (ground/cruise):
//!
//! ```text
//! X-Plane Telemetry → TelemetryListener → AircraftState
//!                                              ↓
//!                                    AdaptivePrefetchCoordinator
//!                                      ├─ PhaseDetector (ground/cruise)
//!                                      ├─ GroundStrategy (ring prefetch)
//!                                      ├─ CruiseStrategy (band prefetch)
//!                                      └─ CircuitBreaker (load detection)
//!                                              ↓
//!                                       DdsClient → Executor
//! ```
//!
//! The adaptive system automatically calibrates based on measured throughput
//! and selects the appropriate prefetch mode (aggressive/opportunistic/disabled).
//!
//! # X-Plane Setup
//!
//! Users must enable UDP data output in X-Plane:
//! 1. Settings → Data Output
//! 2. Enable "Network via UDP"
//! 3. Set destination IP to localhost or machine running XEarthLayer
//! 4. Set port (default 49003)
//! 5. Enable data indices: 3 (speeds), 17 (heading), 20 (position)

pub mod adaptive;
mod builder;
mod circuit_breaker;
mod condition;
pub mod config;
pub mod coordinates;
mod error;
pub mod inference;
mod listener;
mod load_monitor;
mod prewarm;
pub mod scenery_cache;
mod scenery_index;
mod state;
mod strategy;
mod throttler;
pub mod tile_based;
pub mod types;

pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitState, RESOURCE_SATURATION_THRESHOLD,
};
pub use condition::{
    AlwaysActiveCondition, MinimumSpeedCondition, NeverActiveCondition, PrefetchCondition,
};
pub use error::PrefetchError;
pub use listener::TelemetryListener;
pub use load_monitor::{FuseLoadMonitor, SharedFuseLoadMonitor};
pub use state::{
    AircraftSnapshot, AircraftState, DetailedPrefetchStats, GpsStatus, PrefetchMode,
    PrefetchStatsSnapshot, PrefetchStatusSnapshot, SharedPrefetchStatus,
};
pub use strategy::Prefetcher;
pub use throttler::{AlwaysThrottle, NeverThrottle, PrefetchThrottler, ThrottleState};

// FUSE inference (still used by FUSE layer for position callbacks)
pub use config::FuseInferenceConfig;
pub use inference::{Direction, FuseRequestAnalyzer, LoadedEnvelope, TileRequestCallback};
pub use types::{InputMode, PrefetchTile, PrefetchZone, TurnDirection, TurnState};

// Legacy strategy migration utilities
pub use builder::{is_legacy_strategy, warn_if_legacy};

// Scenery-aware prefetch
pub use scenery_index::{
    IndexingProgress, SceneryIndex, SceneryIndexConfig, SceneryIndexError, SceneryTile,
};

// Cold-start prewarm
pub use prewarm::{
    generate_dsf_grid, start_prewarm, DsfGridBounds, FileTerrainScanner, PrewarmConfig,
    PrewarmHandle, PrewarmStatus, TerrainScanner,
};

// Scenery index cache
pub use scenery_cache::{
    cache_path as scenery_cache_path, cache_status, load_cache, save_cache, CacheLoadResult,
    CacheStatus,
};

// DSF tile types (used by FUSE layer and adaptive prefetch)
pub use tile_based::{DdsAccessEvent, DsfTileCoord};

// Adaptive prefetch (self-calibrating DSF-aligned)
pub use adaptive::{AdaptivePrefetchConfig, AdaptivePrefetchCoordinator};
