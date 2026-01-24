//! Dashboard widgets for the TUI.
//!
//! This module contains:
//! - **Primitives**: Reusable low-level UI components (sparklines, progress bars, formatters)
//! - **Panel Widgets**: Higher-level widgets that compose primitives
//!
//! ## Layout Widgets (v0.3.0)
//!
//! - `PrefetchSystemWidget` - Prefetch status (status, mode, loading tiles)
//! - `ScenerySystemWidget` - 2-column tile requests/processing
//! - `InputOutputWidget` - 2-column network/disk I/O
//! - `CacheWidgetCompact` - Cache display with hit rates and sizes

mod aircraft_position;
mod cache;
mod control_plane;
mod input_output;
pub mod network;
mod prefetch_system;
pub mod primitives;
mod scenery_system;

// Core widget exports
pub use cache::{CacheConfig, CacheWidgetCompact};
pub use network::NetworkHistory;

// v0.3.0 widgets
pub use aircraft_position::AircraftPositionWidget;
pub use input_output::{DiskHistory, InputOutputWidget};
pub use prefetch_system::PrefetchSystemWidget;
pub use scenery_system::{SceneryHistory, ScenerySystemWidget};

// Legacy widgets (deprecated in v0.3.0, still used in render_sections.rs)
#[deprecated(since = "0.3.0", note = "Use ScenerySystemWidget instead")]
pub use control_plane::ControlPlaneWidget;
