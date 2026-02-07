//! Cache system for the async pipeline.
//!
//! This module provides a flexible, domain-agnostic cache infrastructure:
//!
//! # New Cache Service Architecture
//!
//! The recommended approach uses the generic `Cache` trait and `CacheService`:
//!
//! - [`CacheService`] - Lifecycle wrapper for starting/stopping caches
//! - [`Cache`] trait - Generic key-value interface (string keys, byte values)
//! - [`MemoryCacheProvider`] - In-memory LRU cache using moka
//! - [`DiskCacheProvider`] - On-disk cache with **internal GC daemon**
//!
//! ```ignore
//! use xearthlayer::cache::{CacheService, ServiceCacheConfig};
//!
//! // Start a memory cache service
//! let service = CacheService::start(ServiceCacheConfig::memory(2_000_000_000, None)).await?;
//!
//! // Use the cache
//! let cache = service.cache();
//! cache.set("tile:15:12754:5279", dds_data).await?;
//!
//! // Shutdown gracefully
//! service.shutdown().await;
//! ```
//!
//! # Key Feature: Self-Contained GC
//!
//! The `DiskCacheProvider` owns its GC daemon internally - no external wiring needed.
//! This fixes a critical bug where the GC daemon was never started in TUI mode.
//!
//! # Legacy API
//!
//! The following types are still available for backward compatibility:
//! - [`MemoryCache`] - Original moka wrapper (domain-specific CacheKey)
//!
//! The async pipeline uses:
//! - `MemoryCache` for DDS tiles (LRU eviction, thread-safe)
//! - `DiskCacheProvider` for chunks with `GcSchedulerDaemon` for periodic eviction

// New cache service architecture (Phase 1)
mod config;
pub mod gc_scheduler;
pub mod lru_index;
pub mod migrate;
pub mod providers;
mod service;
mod traits;

// Domain decorators (Phase 2)
pub mod adapters;
pub mod clients;

// Legacy cache implementation
mod memory;
mod path;
mod stats;
mod types;

// New cache service exports
pub use config::{DiskProviderConfig, ProviderConfig, ServiceCacheConfig};
pub use service::CacheService;
pub use traits::{Cache, GcResult, ServiceCacheError};

// Provider exports (for advanced use cases)
pub use gc_scheduler::{
    GcSchedulerDaemon, DEFAULT_CHECK_INTERVAL_SECS, DEFAULT_TARGET_RATIO, DEFAULT_TRIGGER_THRESHOLD,
};
pub use lru_index::{
    key_to_full_path, key_to_region, CacheEntryMetadata, EvictionCandidate, LruIndex, PopulateStats,
};
pub use providers::{DiskCacheProvider, MemoryCacheProvider};

// Domain decorator exports (Phase 2)
pub use adapters::{DiskCacheBridge, MemoryCacheBridge};
pub use clients::{ChunkCacheClient, TileCacheClient};

// Legacy exports (backward compatibility)
pub use memory::MemoryCache;
pub use stats::{CacheStatistics, CacheStats};
pub use types::{CacheError, CacheKey, DiskCacheConfig, MemoryCacheConfig};

// Re-export path utilities for convenience
pub use path::{
    cache_path, clear_disk_cache, disk_cache_stats, provider_directory, row_directory,
    ClearCacheResult,
};
