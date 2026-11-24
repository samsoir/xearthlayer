//! Task implementations for the executor framework.
//!
//! This module provides task implementations for the DDS generation pipeline
//! and tile prefetching, containing the core business logic for tile processing.
//!
//! # DDS Pipeline Tasks
//!
//! The DDS pipeline uses two sequential tasks:
//!
//! - [`DownloadChunksTask`] - Downloads 256 satellite imagery chunks (Network)
//! - [`BuildAndCacheDdsTask`] - Assembles, encodes, and caches the DDS tile (CPU)
//!
//! # Data Flow
//!
//! ## DDS Pipeline (optimized 2-task flow)
//!
//! ```text
//! DownloadChunks     → "chunks": ChunkResults
//! BuildAndCacheDds   → (reads "chunks", writes to memory cache)
//! ```
//!
//! # Resource Types
//!
//! Each task declares its resource type for the executor's resource pools:
//! - `DownloadChunksTask`: `ResourceType::Network`
//! - `BuildAndCacheDdsTask`: `ResourceType::CPU`

mod build_and_cache_dds;
mod cache_gc_batch;
mod cached_tile_probe;
pub(crate) mod chunk_image;
mod download_chunks;

// Primary task types
pub use build_and_cache_dds::BuildAndCacheDdsTask;
pub use cache_gc_batch::{
    get_deleted_count_from_output, get_freed_bytes_from_output, CacheGcBatchTask,
    OUTPUT_KEY_DELETED_COUNT, OUTPUT_KEY_FREED_BYTES,
};
pub use cached_tile_probe::{
    tile_already_cached, CachedTileProbeTask, CACHED_TILE_PROBE_TASK, TILE_CACHED_KEY,
};
pub use download_chunks::DownloadChunksTask;
