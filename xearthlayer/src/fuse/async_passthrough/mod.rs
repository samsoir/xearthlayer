//! Shared types for FUSE filesystem DDS generation.
//!
//! This module provides types used by the fuse3 passthrough filesystem:
//! - [`types`] - Request/response types for DDS generation
//! - [`inode`] - Inode allocation and management
//!
//! Note: The legacy fuser-based AsyncPassthroughFS implementation is retained
//! here for reference but is no longer used. The fuse3 implementation is the
//! active code path.

// Legacy fuser implementation (unused, retained for reference)
#[allow(dead_code)]
mod attributes;
#[allow(dead_code)]
mod filesystem;

// Shared types used by fuse3
pub mod inode;
mod types;

// Re-export shared types for use by fuse3 and public API
pub use types::{DdsHandler, DdsRequest, DdsResponse};
