//! Terminal UI for XEarthLayer.
//!
//! Provides a real-time dashboard showing pipeline status, network throughput,
//! cache utilization, and error rates.
//!
//! # Module Structure
//!
//! - `dashboard` - Main TUI dashboard (see submodules for details)
//! - `packages` - The package manager TUI (`xearthlayer packages`)
//! - `widgets` - Reusable UI widget components

pub mod dashboard;
pub mod packages;
pub mod symbols;
pub mod widgets;

pub use dashboard::{
    Dashboard, DashboardConfig, DashboardEvent, DashboardState, LoadingPhase, LoadingProgress,
    PrefetchLaneConfig, PrewarmProgress,
};
pub use widgets::{EncoderStatus, FeatureStatus, XworldProStatus};
