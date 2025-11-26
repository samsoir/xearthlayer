//! FUSE filesystem for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture reads
//! and generates satellite imagery DDS files on demand.

mod filename;
mod filesystem;
mod passthrough;
mod placeholder;

pub use filename::{parse_dds_filename, DdsFilename, ParseError};
pub use filesystem::XEarthLayerFS;
pub use passthrough::PassthroughFS;
pub use placeholder::{generate_default_placeholder, generate_magenta_placeholder};
