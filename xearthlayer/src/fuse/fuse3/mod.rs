//! Async multi-threaded FUSE filesystem using fuse3.
//!
//! This module provides a fully async FUSE implementation that leverages
//! the Tokio runtime for concurrent filesystem operations. All operations
//! run as async tasks, enabling true parallel processing of X-Plane's DDS
//! requests.
//!
//! # Architecture
//!
//! ```text
//! X-Plane                    Tokio Runtime (multi-threaded)
//!    │                              │
//!    ├── read(file1.dds) ──────────►├── spawn task ──► generate_dds()
//!    ├── read(file2.dds) ──────────►├── spawn task ──► generate_dds()
//!    ├── read(file3.dds) ──────────►├── spawn task ──► generate_dds()
//!    │   [All run concurrently]     │   [All process in parallel]
//!    │◄── responses ────────────────┤
//! ```
//!
//! # Design
//!
//! - **Threading**: multi-threaded via the Tokio runtime
//! - **Async**: native `async`/`await` (no blocking `block_on()`)
//! - **Concurrency**: DDS requests processed in parallel
//! - **Self reference**: `&self` (immutable), enabling shared concurrent access

mod inode;
mod ortho_union_fs;
mod shared;
mod types;

pub use ortho_union_fs::Fuse3OrthoUnionFS;
pub use shared::{chunk_to_tile_coords, DdsRequestor, FileAttrBuilder, VirtualDdsConfig, TTL};
pub use types::{Fuse3Error, Fuse3Result, MountHandle, SpawnedMountHandle};
