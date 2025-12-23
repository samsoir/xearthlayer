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

mod condition;
mod error;
mod listener;
mod predictor;
mod radial;
mod scheduler;
mod state;
mod strategy;

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
pub use state::{AircraftSnapshot, AircraftState, PrefetchStatusSnapshot, SharedPrefetchStatus};
pub use strategy::Prefetcher;
