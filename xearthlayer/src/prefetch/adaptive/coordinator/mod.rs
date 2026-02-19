//! Adaptive prefetch coordinator module.
//!
//! This module provides the [`AdaptivePrefetchCoordinator`] which orchestrates
//! all adaptive prefetch components including:
//!
//! - Performance calibration for mode selection
//! - Flight phase detection (ground/cruise)
//! - Turn detection for prefetch pausing
//! - Strategy selection and execution
//!
//! # Module Structure
//!
//! The coordinator is decomposed into focused submodules:
//!
//! - [`constants`] - Timing constants for cycle intervals and staleness detection
//! - [`status`] - Status types for UI and monitoring
//! - [`telemetry`] - Telemetry processing utilities
//! - [`time_budget`] - Time budget validation for prefetch plans
//! - [`core`] - Core coordinator implementation and Prefetcher trait
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::adaptive::coordinator::AdaptivePrefetchCoordinator;
//!
//! let coordinator = AdaptivePrefetchCoordinator::with_defaults()
//!     .with_calibration(calibration)
//!     .with_throttler(circuit_breaker)
//!     .with_dds_client(dds_client);
//!
//! // Run the coordinator loop
//! coordinator.run(shutdown_token).await;
//! ```

mod constants;
mod core;
mod runner;
mod status;
mod telemetry;
mod time_budget;

// Re-export primary types
pub use core::AdaptivePrefetchCoordinator;
#[allow(unused_imports)]
pub use core::{
    BACKPRESSURE_DEFER_THRESHOLD, BACKPRESSURE_REDUCED_FRACTION, BACKPRESSURE_REDUCE_THRESHOLD,
};
pub use status::CoordinatorStatus;

// Re-export constants (allow unused since they're public API)
#[allow(unused_imports)]
pub use constants::{MIN_CYCLE_INTERVAL, STALENESS_CHECK_INTERVAL, TELEMETRY_STALE_THRESHOLD};

// Re-export utility functions for external use (allow unused since they're public API)
#[allow(unused_imports)]
pub use telemetry::{extract_track, is_telemetry_stale, should_run_cycle};
#[allow(unused_imports)]
pub use time_budget::{can_complete_in_time, time_until_trigger};
