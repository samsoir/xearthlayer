//! Dashboard widgets for the TUI.

mod cache;
mod control_plane;
mod errors;
pub mod network;
mod pipeline;

pub use cache::{CacheConfig, CacheWidget};
pub use control_plane::ControlPlaneWidget;
pub use errors::ErrorsWidget;
pub use network::{NetworkHistory, NetworkWidget};
pub use pipeline::{PipelineHistory, PipelineWidget};
