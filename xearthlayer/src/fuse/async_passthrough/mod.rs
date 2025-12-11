//! Async passthrough FUSE filesystem with pipeline integration.
//!
//! This module provides a FUSE filesystem that overlays an existing scenery
//! pack directory and generates DDS textures on-demand via an async pipeline.
//!
//! # Module Structure
//!
//! - [`types`] - Request/response types for DDS generation
//! - [`inode`] - Inode allocation and management
//! - [`attributes`] - File attribute conversion utilities
//!
//! # Architecture
//!
//! ```text
//! FUSE Handler Thread          Tokio Runtime
//! ┌─────────────────┐          ┌─────────────────┐
//! │  read() called  │          │                 │
//! │       │         │          │  Pipeline       │
//! │       ▼         │          │  Processor      │
//! │ Create oneshot  │──req───►│       │         │
//! │       │         │          │       ▼         │
//! │   Block on rx   │◄──res───│  DDS Data       │
//! │       │         │          │                 │
//! │       ▼         │          └─────────────────┘
//! │  reply.data()   │
//! └─────────────────┘
//! ```

mod attributes;
mod filesystem;
mod inode;
mod types;

// Re-export public API
pub use filesystem::AsyncPassthroughFS;
pub use types::{DdsHandler, DdsRequest, DdsResponse};
