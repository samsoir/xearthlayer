//! FUSE filesystem for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture reads
//! and generates satellite imagery DDS files on demand.
//!
//! # Implementation
//!
//! Uses [`Fuse3PassthroughFS`] - an async multi-threaded passthrough filesystem
//! that overlays existing scenery directories while generating DDS textures on-demand.

// Internal modules for shared types (used by fuse3)
pub(crate) mod async_passthrough;

mod coalesce;
mod filename;
pub mod fuse3;
mod placeholder;

// Re-export types for public API
pub use async_passthrough::{DdsHandler, DdsRequest, DdsResponse};
pub use coalesce::{CoalesceResult, CoalescedResult, CoalescerStats, RequestCoalescer};
pub use filename::{parse_dds_filename, DdsFilename, ParseError};
pub use fuse3::{Fuse3Error, Fuse3PassthroughFS, Fuse3Result, MountHandle, SpawnedMountHandle};
pub use placeholder::{
    generate_default_placeholder, generate_magenta_placeholder, get_default_placeholder,
    init_placeholder_cache, validate_dds_or_placeholder, EXPECTED_DDS_SIZE,
};
