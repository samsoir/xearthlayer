//! Cache system for the async pipeline.
//!
//! The async pipeline uses:
//! - `MemoryCache` for DDS tiles (LRU eviction, thread-safe)
//! - `ParallelDiskCache` (in `pipeline/disk_cache.rs`) for chunks
//! - `DiskCacheDaemon` for periodic disk cache eviction
//!
//! This module provides the memory cache, disk eviction daemon, and supporting types.

mod disk_eviction;
mod memory;
mod path;
mod stats;
mod types;

pub use disk_eviction::{run_eviction_daemon, EvictionResult};
pub use memory::MemoryCache;
pub use stats::{CacheStatistics, CacheStats};
pub use types::{CacheError, CacheKey, DiskCacheConfig, MemoryCacheConfig};

// Re-export path utilities for convenience
pub use path::{
    cache_path, clear_disk_cache, disk_cache_stats, provider_directory, row_directory,
    ClearCacheResult,
};
