//! Dashboard widgets for the TUI.
//!
//! This module contains:
//! - **Primitives**: Reusable low-level UI components (sparklines, progress bars, formatters)
//! - **Panel Widgets**: Higher-level widgets that compose primitives
//!
//! ## Layout Widgets (v0.3.1)
//!
//! - `PrefetchSystemWidget` - Prefetch state and lane capacity meter
//! - `ScenerySystemWidget` - 3-column: REQUESTS | QUEUE | PROCESSING, plus FUSE latency
//! - `FuseLatencyWidget` - FUSE generation latency line (inside Scenery System)
//! - `InputOutputWidget` - 2-column network/disk I/O
//! - `CacheWidgetCompact` - Cache display with hit rates and sizes
//! - `FeatureStatusWidget` - GPU encoding, night lighting and X-World Pro state

mod aircraft_position;
mod cache;
mod control_plane;
mod feature_status;
mod fuse_latency;
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
pub use feature_status::{EncoderStatus, FeatureStatus, FeatureStatusWidget, XworldProStatus};
pub use fuse_latency::FuseLatencyWidget;
pub use input_output::{DiskHistory, InputOutputWidget};
pub use prefetch_system::{LaneView, PrefetchSystemWidget};
pub use scenery_system::{SceneryHistory, ScenerySystemWidget};

// Legacy widgets (deprecated in v0.3.0, still used in render_sections.rs)
#[deprecated(since = "0.3.0", note = "Use ScenerySystemWidget instead")]
pub use control_plane::ControlPlaneWidget;
