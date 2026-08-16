//! Core traits for the generic cache service.
//!
//! The `Cache` trait provides a domain-agnostic key-value interface for caching.
//! All cache providers implement this trait, allowing callers to use any backend
//! through a consistent interface.
//!
//! # Design Principles
//!
//! - **String keys**: Human-readable for debugging, flexible for any domain
//! - **Vec<u8> values**: Raw bytes, no serialization opinions imposed
//! - **Minimal interface**: Only essential operations, no domain-specific concerns
//! - **Self-contained GC**: Providers manage their own garbage collection
//! - **Dyn-compatible**: Uses `Pin<Box<dyn Future>>` for trait object support
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::cache::{Cache, CacheService, ProviderConfig};
//!
//! // Start a memory cache service
//! let service = CacheService::start(ServiceCacheConfig {
//!     max_size_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
//!     provider: ProviderConfig::Memory { ttl: None },
//! }).await?;
//!
//! // Use the cache
//! let cache = service.cache();
//! cache.set("key", vec![1, 2, 3]).await?;
//! let value = cache.get("key").await?;
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use thiserror::Error;

/// Result of a garbage collection operation.
#[derive(Debug, Clone, Default)]
pub struct GcResult {
    /// Number of entries removed during GC.
    pub entries_removed: usize,
    /// Total bytes freed during GC.
    pub bytes_freed: u64,
    /// Duration of the GC operation in milliseconds.
    pub duration_ms: u64,
}

impl fmt::Display for GcResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GC: removed {} entries, freed {} bytes in {}ms",
            self.entries_removed, self.bytes_freed, self.duration_ms
        )
    }
}

/// Errors that can occur during cache operations.
#[derive(Debug, Error)]
pub enum ServiceCacheError {
    /// I/O error during cache operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The cache service is shutting down.
    #[error("Cache is shutting down")]
    ShuttingDown,

    /// Key exceeds maximum allowed size.
    #[error("Key too large: {size} bytes (max: {max})")]
    KeyTooLarge { size: usize, max: usize },

    /// Value exceeds maximum allowed size.
    #[error("Value too large: {size} bytes (max: {max})")]
    ValueTooLarge { size: usize, max: usize },

    /// Failed to spawn background task.
    #[error("Failed to spawn task: {0}")]
    SpawnError(String),

    /// Provider-specific error.
    #[error("Provider error: {0}")]
    Provider(String),
}

/// Boxed future type for dyn-compatible async methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Generic cache interface for key-value storage.
///
/// Providers implement this trait to offer caching capabilities. The interface
/// is intentionally minimal and domain-agnostic - domain concepts like tile
/// coordinates are handled by decorator layers.
///
/// # String Keys
///
/// Keys are strings for several reasons:
/// - **Debuggability**: Keys are human-readable in logs and debugging tools
/// - **Flexibility**: Any domain can be mapped to strings via decorators
/// - **Consistency**: Same key format works across memory, disk, and network caches
///
/// # Garbage Collection
///
/// Each provider manages its own GC strategy:
/// - Memory providers use automatic LRU eviction (moka)
/// - Disk providers run periodic background eviction daemons
/// - The `gc()` method allows manual triggering when needed
///
/// # Thread Safety
///
/// All implementations must be `Send + Sync` for use across async tasks.
///
/// # Dyn Compatibility
///
/// This trait uses `Pin<Box<dyn Future>>` for async methods to support
/// trait objects (`Arc<dyn Cache>`). This allows domain decorators to
/// wrap any cache implementation polymorphically.
pub trait Cache: Send + Sync {
    /// Store a value with the given key.
    ///
    /// If the key already exists, the value is replaced.
    /// Eviction may occur if the cache exceeds its size limit.
    ///
    /// # Arguments
    ///
    /// * `key` - The cache key (should be reasonably short)
    /// * `value` - The value to store
    ///
    /// # Errors
    ///
    /// Returns `ServiceCacheError` if:
    /// - I/O fails (disk caches)
    /// - Key or value exceeds size limits
    /// - Cache is shutting down
    fn set(&self, key: &str, value: Vec<u8>) -> BoxFuture<'_, Result<(), ServiceCacheError>>;

    /// Retrieve a value by key.
    ///
    /// # Arguments
    ///
    /// * `key` - The cache key to look up
    ///
    /// # Returns
    ///
    /// - `Ok(Some(data))` if the key exists
    /// - `Ok(None)` if the key is not found
    /// - `Err(_)` if an error occurs
    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, ServiceCacheError>>;

    /// Delete a value by key.
    ///
    /// # Arguments
    ///
    /// * `key` - The cache key to delete
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if the key existed and was deleted
    /// - `Ok(false)` if the key did not exist
    /// - `Err(_)` if an error occurs
    fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>>;

    /// Check if a key exists without retrieving the value.
    ///
    /// More efficient than `get()` when you only need to know existence.
    ///
    /// # Arguments
    ///
    /// * `key` - The cache key to check
    fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>>;

    /// Check if a key exists, synchronously.
    ///
    /// Both providers already answer this from an in-memory map — moka's
    /// `contains_key` and `LruIndex::contains` are each sync — so the
    /// `BoxFuture` returned by `contains()` is pure overhead for a caller
    /// that is itself sync. Prefetch's region-completeness scan calls this
    /// several hundred times per region per cycle; going through the async
    /// wrapper there cost a `block_in_place` + `block_on` + `Box::pin`
    /// allocation per tile. Resolves TODO(#175).
    fn contains_sync(&self, key: &str) -> bool;

    /// Get the current size of the cache in bytes.
    ///
    /// For memory caches, this is the weighted size of all entries.
    /// For disk caches, this is the total size of all cached files.
    fn size_bytes(&self) -> u64;

    /// Get the current number of entries in the cache.
    fn entry_count(&self) -> u64;

    /// Get the maximum configured size in bytes.
    fn max_size_bytes(&self) -> u64;

    /// Update the maximum cache size.
    ///
    /// The provider handles eviction if the new limit is smaller than
    /// the current size. This may trigger immediate garbage collection.
    ///
    /// # Arguments
    ///
    /// * `size_bytes` - The new maximum size in bytes
    fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>>;

    /// Trigger garbage collection manually.
    ///
    /// For providers with automatic GC (like moka), this may be a no-op
    /// or simply run pending maintenance tasks. For disk providers, this
    /// forces an immediate eviction cycle.
    ///
    /// # Returns
    ///
    /// Statistics about the GC operation.
    fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_result_default() {
        let result = GcResult::default();
        assert_eq!(result.entries_removed, 0);
        assert_eq!(result.bytes_freed, 0);
        assert_eq!(result.duration_ms, 0);
    }

    #[test]
    fn test_gc_result_display() {
        let result = GcResult {
            entries_removed: 10,
            bytes_freed: 1024,
            duration_ms: 50,
        };
        let display = format!("{}", result);
        assert!(display.contains("10"));
        assert!(display.contains("1024"));
        assert!(display.contains("50ms"));
    }

    #[test]
    fn test_cache_error_display() {
        let err = ServiceCacheError::ShuttingDown;
        assert_eq!(format!("{}", err), "Cache is shutting down");

        let err = ServiceCacheError::KeyTooLarge { size: 100, max: 50 };
        assert!(format!("{}", err).contains("100"));
        assert!(format!("{}", err).contains("50"));
    }

    #[test]
    fn test_cache_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let cache_err: ServiceCacheError = io_err.into();
        assert!(matches!(cache_err, ServiceCacheError::Io(_)));
    }
}
