//! Bridge adapters between cache service and executor traits.
//!
//! These adapters implement the executor's `MemoryCache` and `DiskCache` traits
//! using the new cache service infrastructure. This enables backward compatibility
//! with existing code while using the new self-contained cache services.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────────┐
//! │                    Executor System                     │
//! │                                                        │
//! │  Jobs & Tasks use executor::MemoryCache/DiskCache     │
//! └───────────────────────────┬────────────────────────────┘
//!                             │
//!                             ▼
//! ┌───────────────────────────────────────────────────────┐
//! │                   Bridge Adapters                      │
//! │                                                        │
//! │  MemoryCacheBridge ─────► TileCacheClient             │
//! │  DiskCacheBridge ───────► ChunkCacheClient            │
//! └───────────────────────────┬────────────────────────────┘
//!                             │
//!                             ▼
//! ┌───────────────────────────────────────────────────────┐
//! │                   Cache Service                        │
//! │                                                        │
//! │  CacheService::start() ─► Arc<dyn Cache>              │
//! └───────────────────────────────────────────────────────┘
//! ```

mod dds_disk_bridge;
mod disk_bridge;
mod memory_bridge;

pub use dds_disk_bridge::DdsDiskCacheBridge;
pub use disk_bridge::DiskCacheBridge;
pub use memory_bridge::MemoryCacheBridge;
