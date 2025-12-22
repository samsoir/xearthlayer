//! Predictive tile caching based on X-Plane telemetry.
//!
//! This module provides functionality to pre-fetch satellite imagery tiles
//! ahead of the aircraft's position to reduce FPS drops when the simulator
//! loads new scenery.
//!
//! # Architecture
//!
//! ```text
//! X-Plane UDP (49003) → TelemetryListener → AircraftState
//!                                               ↓
//!                                        TilePredictor (cone + radial)
//!                                               ↓
//!                                        PrefetchScheduler
//!                                               ↓
//!                                        DdsHandler (existing pipeline)
//! ```
//!
//! # X-Plane Setup
//!
//! Users must enable UDP data output in X-Plane:
//! 1. Settings → Data Output
//! 2. Enable "Network via UDP"
//! 3. Set destination IP to localhost or machine running XEarthLayer
//! 4. Set port (default 49003)
//! 5. Enable data indices: 3 (speeds), 17 (heading), 20 (position)

mod error;
mod listener;
mod predictor;
mod scheduler;
mod state;

pub use error::PrefetchError;
pub use listener::TelemetryListener;
pub use predictor::{PredictedTile, TilePredictor};
pub use scheduler::{PrefetchScheduler, PrefetchStats, PrefetchStatsSnapshot, SchedulerConfig};
pub use state::{AircraftSnapshot, AircraftState, PrefetchStatusSnapshot, SharedPrefetchStatus};
