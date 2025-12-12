//! FUSE filesystem for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture reads
//! and generates satellite imagery DDS files on demand.
//!
//! # Implementations
//!
//! - [`AsyncPassthroughFS`] - Async passthrough with pipeline integration (primary)
//! - [`XEarthLayerFS`] - Standalone virtual-only filesystem

mod async_passthrough;
mod filename;
mod filesystem;
mod placeholder;

pub use async_passthrough::{AsyncPassthroughFS, DdsHandler, DdsRequest, DdsResponse};
pub use filename::{parse_dds_filename, DdsFilename, ParseError};
pub use filesystem::XEarthLayerFS;
pub use placeholder::{generate_default_placeholder, generate_magenta_placeholder};
