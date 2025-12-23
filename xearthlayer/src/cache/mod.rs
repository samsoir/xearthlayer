//! Cache system for the async pipeline.
//!
//! The async pipeline uses:
//! - `MemoryCache` for DDS tiles (LRU eviction, thread-safe)
//! - `ParallelDiskCache` (in `pipeline/disk_cache.rs`) for chunks
//!
//! This module provides the memory cache and supporting types.

mod memory;
mod path;
mod stats;
mod types;

pub use memory::MemoryCache;
pub use stats::{CacheStatistics, CacheStats};
pub use types::{CacheError, CacheKey, DiskCacheConfig, MemoryCacheConfig};

// Re-export path utilities for convenience
pub use path::{
    cache_path, clear_disk_cache, disk_cache_stats, provider_directory, row_directory,
    ClearCacheResult,
};
