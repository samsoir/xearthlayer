//! Terminal UI for XEarthLayer.
//!
//! Provides a real-time dashboard showing pipeline status, network throughput,
//! cache utilization, and error rates.

pub mod dashboard;
pub mod widgets;

pub use dashboard::{
    Dashboard, DashboardConfig, DashboardEvent, DashboardState, LoadingProgress, PrewarmProgress,
};
