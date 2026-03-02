//! Adaptive tile-based prefetch system.
//!
//! This module implements an adaptive prefetch system that self-calibrates
//! based on measured tile generation throughput, adapts strategies by flight
//! phase (ground/cruise), and uses track-based turn detection for accurate
//! band calculation.
//!
//! # Key Features
//!
//! - **Performance Calibration**: Measures throughput during X-Plane's initial
//!   12° load to determine prefetch capability
//! - **Flight Phase Strategies**: Different algorithms for ground (ring) and
//!   cruise (band) operations
//! - **Track-Based Turn Detection**: Uses ground track (not heading) to detect
//!   turns and pause prefetch until stable
//! - **DSF Boundary-Aware Ordering**: Tiles sorted by DSF column/row urgency
//!   to match X-Plane's scenery loading pattern (#58)
//! - **Takeoff Transition Throttle**: Grace period + ramp-up after Ground→Cruise
//!   to avoid resource contention during takeoff (#62)
//! - **Rolling Recalibration**: Adjusts mode if throughput degrades during flight
//!
//! # Strategy Modes
//!
//! | Throughput | Mode | Trigger |
//! |------------|------|---------|
//! | > 30 tiles/sec | Aggressive | Position-based (0.3° into DSF) |
//! | 10-30 tiles/sec | Opportunistic | Circuit breaker close |
//! | < 10 tiles/sec | Disabled | Skip prefetch |
//!
//! # Module Structure
//!
//! ```text
//! adaptive/
//! ├── mod.rs                  # This file - module exports
//! ├── config.rs               # Configuration types
//! ├── calibration/            # Performance calibration (submodule)
//! │   ├── mod.rs              # Calibration module exports
//! │   ├── types.rs            # StrategyMode, PerformanceCalibration
//! │   ├── observer.rs         # ThroughputObserver trait
//! │   ├── calibrator.rs       # Initial calibration
//! │   └── rolling.rs          # Rolling recalibration
//! ├── strategy.rs             # AdaptivePrefetchStrategy trait
//! ├── band_calculator.rs      # DSF-aligned band geometry
//! ├── boundary_prioritizer.rs # DSF boundary-aware tile ordering (#58)
//! ├── boundary_strategy.rs    # Boundary crossing → DSF region generation (#58)
//! ├── cruise_strategy.rs      # Cruise flight prefetch
//! ├── ground_strategy.rs      # Ground operations prefetch
//! ├── phase_detector.rs       # Ground/cruise detection
//! ├── turn_detector.rs        # Track stability monitoring
//! ├── transition_throttle.rs  # Takeoff ramp-up throttle (#62)
//! └── coordinator.rs          # Central orchestration
//! ```
//!
//! # Example Usage
//!
//! ```ignore
//! use xearthlayer::prefetch::adaptive::{
//!     AdaptivePrefetchConfig, PerformanceCalibrator, ThroughputObserver,
//! };
//!
//! // Create calibrator during service startup
//! let calibrator = Arc::new(PerformanceCalibrator::with_defaults());
//!
//! // Wire to job executor for completion callbacks
//! executor.set_throughput_observer(Arc::clone(&calibrator) as SharedThroughputObserver);
//!
//! // After initial load, get calibration
//! if let Some(cal) = calibrator.get_calibration() {
//!     println!("Throughput: {:.1} tiles/sec", cal.throughput_tiles_per_sec);
//!     println!("Mode: {}", cal.recommended_strategy);
//! }
//! ```
//!
//! # Design References
//!
//! - Design document: `docs/dev/adaptive-prefetch-design.md`
//! - Research basis: `docs/dev/xplane-scenery-loading-whitepaper.md`

mod band_calculator;
mod boundary_monitor;
mod boundary_prioritizer;
mod boundary_strategy;
mod calibration;
mod config;
mod coordinator;
mod cruise_strategy;
mod ground_strategy;
mod phase_detector;
mod scenery_window;
mod strategy;
mod transition_throttle;
mod turn_detector;

// Re-export public types
pub use band_calculator::{BandCalculator, DsfTileCoord};
pub use boundary_monitor::{BoundaryAxis, BoundaryCrossing, BoundaryMonitor};
pub use boundary_prioritizer::prioritize as prioritize_by_boundary;
pub use boundary_strategy::{BoundaryStrategy, TargetRegion};
pub use calibration::{
    create_throughput_observer, create_throughput_observer_with_config, PerformanceCalibration,
    PerformanceCalibrator, RecalibrationResult, RollingCalibrator, SharedThroughputObserver,
    StrategyMode, ThroughputObserver,
};
pub use config::{AdaptivePrefetchConfig, CalibrationConfig, KillswitchMode, PrefetchMode};
pub use coordinator::{AdaptivePrefetchCoordinator, CoordinatorStatus};
pub use cruise_strategy::CruiseStrategy;
pub use ground_strategy::{GroundStrategy, LoadedAreaBounds};
pub use phase_detector::{FlightPhase, PhaseDetector};
pub use strategy::{AdaptivePrefetchStrategy, PrefetchPlan, PrefetchPlanMetadata, TrackQuadrant};
pub use scenery_window::{SceneryWindow, SceneryWindowConfig, WindowState};
pub use transition_throttle::TransitionThrottle;
pub use turn_detector::{TurnDetector, TurnState};
