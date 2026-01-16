//! Reusable UI primitive components.
//!
//! This module contains low-level visual components that can be composed
//! into higher-level widgets. Following the Single Responsibility Principle,
//! these primitives handle only rendering concerns.
//!
//! ## Components
//!
//! - **Sparkline**: Compact time-series chart using Unicode blocks
//! - **ProgressBar**: Horizontal bar showing progress toward a goal
//! - **Format**: Human-readable formatting for bytes, throughput, etc.

mod format;
mod progress_bar;
mod sparkline;

// Re-export commonly used formatters (others available via primitives::format::*)
pub use format::{format_bytes, format_bytes_usize, format_throughput};
pub use progress_bar::{ProgressBar, ProgressBarStyle};
pub use sparkline::{Sparkline, SparklineHistory};
