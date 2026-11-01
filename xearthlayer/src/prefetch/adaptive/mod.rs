//! Adaptive tile-based prefetch system.
//!
//! This module implements a prefetch system that adapts its strategy by
//! flight phase (ground/cruise).
//!
//! # Key Features
//!
//! - **Flight Phase Strategies**: Different algorithms for ground (ring) and
//!   cruise (boundary-driven) operations
//! - **Sliding Prefetch Box**: PrefetchBox provides heading-biased region
//!   detection for cruise-phase prefetch (#93)
//! - **Takeoff Transition Throttle**: Grace period + ramp-up after Ground→Cruise
//!   to avoid resource contention during takeoff (#62)
//!
//! # Module Structure
//!
//! ```text
//! adaptive/
//! ├── mod.rs                  # This file - module exports
//! ├── config.rs               # Configuration types
//! ├── calibration.rs          # StrategyMode, PerformanceCalibration
//! ├── strategy.rs             # AdaptivePrefetchStrategy trait
//! ├── boundary_strategy.rs    # Region lifecycle management (#58)
//! ├── phase_detector.rs       # Ground/cruise detection
//! ├── scenery_window.rs       # Window config holder (unused; see design doc)
//! ├── transition_throttle.rs  # Takeoff ramp-up throttle (#62)
//! └── coordinator/            # Central orchestration
//! ```
//!
//! # Design References
//!
//! - Design document: `docs/dev/adaptive-prefetch-design.md`
//! - Research basis: `docs/dev/xplane-scenery-loading-whitepaper.md`

mod boundary_strategy;
mod calibration;
mod config;
mod coordinator;
pub mod extent;
mod phase_detector;
mod prefetch_box;
mod scenery_window;
mod strategy;
mod transition_throttle;

// Re-export public types
pub use boundary_strategy::BoundaryStrategy;
pub use calibration::PerformanceCalibration;
pub use config::AdaptivePrefetchConfig;
pub use coordinator::{AdaptivePrefetchCoordinator, CoordinatorStatus};
pub use extent::compute_extent;
pub use phase_detector::{FlightPhase, PhaseDetector};
pub use prefetch_box::PrefetchBox;
pub use scenery_window::{SceneryWindow, SceneryWindowConfig};
pub use strategy::{AdaptivePrefetchStrategy, PrefetchPlan, PrefetchPlanMetadata, TrackQuadrant};
pub use transition_throttle::TransitionThrottle;
