//! Two-tier cache system for DDS tiles.
//!
//! Provides memory and disk caching with LRU eviction, statistics tracking,
//! and provider-specific hierarchical storage.

mod disk;
mod memory;
mod path;
mod stats;
mod system;
mod r#trait;
mod types;

pub use disk::DiskCache;
pub use memory::MemoryCache;
pub use r#trait::{Cache, NoOpCache};
pub use stats::{CacheStatistics, CacheStats};
pub use system::CacheSystem;
pub use types::{CacheConfig, CacheError, CacheKey, DiskCacheConfig, MemoryCacheConfig};

// Re-export path utilities for convenience
pub use path::{cache_path, provider_directory, row_directory};
