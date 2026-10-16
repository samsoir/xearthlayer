//! Terminal UI for XEarthLayer.
//!
//! Provides a real-time dashboard showing pipeline status, network throughput,
//! cache utilization, and error rates.
//!
//! # Module Structure
//!
//! - `dashboard` - Main TUI dashboard (see submodules for details)
//! - `widgets` - Reusable UI widget components

pub mod dashboard;
pub mod widgets;

pub use dashboard::{
    Dashboard, DashboardConfig, DashboardEvent, DashboardState, LoadingPhase, LoadingProgress,
    PrefetchLaneConfig, PrewarmProgress,
};
