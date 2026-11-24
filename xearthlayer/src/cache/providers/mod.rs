//! Cache provider implementations.
//!
//! Each provider implements the `Cache` trait and manages its own lifecycle,
//! including garbage collection. Providers are created via `CacheService::start()`.
//!
//! # Available Providers
//!
//! - [`MemoryCacheProvider`]: In-memory cache using moka, TinyLFU eviction (#405)
//! - [`DiskCacheProvider`]: On-disk cache with background GC daemon
//!
//! # Creating Providers
//!
//! Providers should not be created directly. Use `CacheService::start()` instead:
//!
//! ```ignore
//! use xearthlayer::cache::{CacheService, ServiceCacheConfig, ProviderConfig};
//!
//! let service = CacheService::start(ServiceCacheConfig {
//!     max_size_bytes: 2_000_000_000,
//!     provider: ProviderConfig::Memory { ttl: None },
//! }).await?;
//! ```

mod disk;
mod memory;

pub use disk::DiskCacheProvider;
pub use memory::MemoryCacheProvider;
