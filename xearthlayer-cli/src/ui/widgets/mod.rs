//! Dashboard widgets for the TUI.
//!
//! This module contains:
//! - **Primitives**: Reusable low-level UI components (sparklines, progress bars, formatters)
//! - **Panel Widgets**: Higher-level widgets that compose primitives
//!
//! ## New Layout Widgets (v0.3.0)
//!
//! - `PrefetchSystemWidget` - Prefetch status (status, mode, loading tiles)
//! - `ScenerySystemWidget` - 2-column tile requests/processing
//! - `InputOutputWidget` - 2-column network/disk I/O
//! - `CacheWidget` - Compact cache display (2 lines)
//! - `CacheWidgetCompact` - Extended compact cache (4 lines with stats)

mod cache;
mod control_plane;
mod errors;
mod input_output;
pub mod network;
mod pipeline;
mod prefetch_system;
pub mod primitives;
mod scenery_system;

// Core widget exports
#[allow(unused_imports)] // Still used externally
pub use cache::CacheWidget;
pub use cache::{CacheConfig, CacheWidgetCompact};
#[allow(unused_imports)] // Still used externally
pub use errors::ErrorsWidget;
pub use network::NetworkHistory;
#[allow(unused_imports)] // Still used externally
pub use network::NetworkWidget;

// New v0.3.0 widgets
pub use input_output::{DiskHistory, InputOutputWidget};
pub use prefetch_system::PrefetchSystemWidget;
pub use scenery_system::{SceneryHistory, ScenerySystemWidget};

// Legacy widgets (deprecated in v0.3.0, kept for compatibility)
#[deprecated(since = "0.3.0", note = "Use ScenerySystemWidget instead")]
#[allow(unused_imports)]
pub use control_plane::ControlPlaneWidget;
#[deprecated(since = "0.3.0", note = "Use ScenerySystemWidget instead")]
#[allow(unused_imports)]
pub use pipeline::{PipelineHistory, PipelineWidget};
