//! Caching layer for package library fetching.
//!
//! Provides a time-based cache to avoid repeated network requests for
//! library indexes that haven't changed.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::package::PackageLibrary;

use super::traits::LibraryClient;
use super::ManagerResult;

/// Default cache TTL (5 minutes).
const DEFAULT_TTL_SECS: u64 = 300;

/// A cached library entry with timestamp.
#[derive(Clone)]
struct CacheEntry {
    library: PackageLibrary,
    fetched_at: Instant,
}

impl CacheEntry {
    fn new(library: PackageLibrary) -> Self {
        Self {
            library,
            fetched_at: Instant::now(),
        }
    }

    fn is_expired(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() > ttl
    }
}

/// Caching wrapper for a [`LibraryClient`].
///
/// Caches library fetches for a configurable TTL to reduce network requests.
/// Thread-safe for concurrent access.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::manager::{HttpLibraryClient, CachedLibraryClient};
/// use std::time::Duration;
///
/// let http_client = HttpLibraryClient::new();
/// let cached = CachedLibraryClient::new(http_client)
///     .with_ttl(Duration::from_secs(600)); // 10 minute cache
///
/// // First call fetches from network
/// let lib1 = cached.fetch_library("https://example.com/library.txt")?;
///
/// // Second call returns cached result (if within TTL)
/// let lib2 = cached.fetch_library("https://example.com/library.txt")?;
/// ```
pub struct CachedLibraryClient<C: LibraryClient> {
    inner: C,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
    ttl: Duration,
}

impl<C: LibraryClient> CachedLibraryClient<C> {
    /// Create a new cached client wrapping the given client.
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(DEFAULT_TTL_SECS),
        }
    }

    /// Set the cache TTL (time-to-live).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Get the current TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Force a fresh fetch, bypassing the cache.
    ///
    /// The result is still cached for future requests.
    pub fn fetch_fresh(&self, url: &str) -> ManagerResult<PackageLibrary> {
        let library = self.inner.fetch_library(url)?;

        // Update cache
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(url.to_string(), CacheEntry::new(library.clone()));
        }

        Ok(library)
    }

    /// Clear all cached entries.
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Clear a specific cached entry.
    pub fn invalidate(&self, url: &str) {
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(url);
        }
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> CacheStats {
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());

        let mut stats = CacheStats {
            entries: cache.len(),
            valid_entries: 0,
            expired_entries: 0,
        };

        for entry in cache.values() {
            if entry.fetched_at.elapsed() > self.ttl {
                stats.expired_entries += 1;
            } else {
                stats.valid_entries += 1;
            }
        }

        stats
    }
}

impl<C: LibraryClient> LibraryClient for CachedLibraryClient<C> {
    fn fetch_library(&self, url: &str) -> ManagerResult<PackageLibrary> {
        // Check cache first
        if let Ok(cache) = self.cache.read() {
            if let Some(entry) = cache.get(url) {
                if !entry.is_expired(self.ttl) {
                    tracing::debug!("Cache hit for library: {}", url);
                    return Ok(entry.library.clone());
                }
                tracing::debug!("Cache expired for library: {}", url);
            }
        }

        // Cache miss or expired - fetch fresh
        tracing::debug!("Cache miss for library: {}", url);
        self.fetch_fresh(url)
    }

    fn fetch_metadata(&self, url: &str) -> ManagerResult<crate::package::PackageMetadata> {
        // Metadata is not cached - always fetch fresh
        // (metadata files change more frequently than library indexes)
        self.inner.fetch_metadata(url)
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStats {
    /// Total number of cached entries.
    pub entries: usize,
    /// Number of valid (non-expired) entries.
    pub valid_entries: usize,
    /// Number of expired entries (still in cache).
    pub expired_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ManagerError;
    use crate::package::PackageMetadata;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock client that counts fetches.
    struct CountingClient {
        fetch_count: Arc<AtomicUsize>,
        library: PackageLibrary,
    }

    impl CountingClient {
        fn new() -> Self {
            Self {
                fetch_count: Arc::new(AtomicUsize::new(0)),
                library: PackageLibrary::new(),
            }
        }

        fn fetch_count(&self) -> usize {
            self.fetch_count.load(Ordering::SeqCst)
        }
    }

    impl LibraryClient for CountingClient {
        fn fetch_library(&self, _url: &str) -> ManagerResult<PackageLibrary> {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.library.clone())
        }

        fn fetch_metadata(&self, _url: &str) -> ManagerResult<PackageMetadata> {
            Err(ManagerError::MetadataFetchFailed {
                url: "mock".to_string(),
                reason: "not implemented".to_string(),
            })
        }
    }

    #[test]
    fn test_cache_hit() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);

        // First fetch - cache miss
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 1);

        // Second fetch - cache hit
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 1); // Still 1

        // Different URL - cache miss
        let _ = cached
            .fetch_library("http://example.com/other.txt")
            .unwrap();
        assert_eq!(cached.inner.fetch_count(), 2);
    }

    #[test]
    fn test_cache_expiration() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner).with_ttl(Duration::from_millis(10)); // Very short TTL

        // First fetch
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 1);

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(20));

        // Should fetch again due to expiration
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 2);
    }

    #[test]
    fn test_force_refresh() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);

        // First fetch
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 1);

        // Force refresh bypasses cache
        let _ = cached.fetch_fresh("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 2);

        // Normal fetch should hit updated cache
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 2); // Still 2
    }

    #[test]
    fn test_clear_cache() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);

        // Populate cache
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 1);

        // Clear cache
        cached.clear_cache();

        // Should fetch again
        let _ = cached.fetch_library("http://example.com/lib.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 2);
    }

    #[test]
    fn test_invalidate() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);

        // Populate cache with two entries
        let _ = cached.fetch_library("http://example.com/lib1.txt").unwrap();
        let _ = cached.fetch_library("http://example.com/lib2.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 2);

        // Invalidate only one
        cached.invalidate("http://example.com/lib1.txt");

        // lib1 should fetch again, lib2 should hit cache
        let _ = cached.fetch_library("http://example.com/lib1.txt").unwrap();
        let _ = cached.fetch_library("http://example.com/lib2.txt").unwrap();
        assert_eq!(cached.inner.fetch_count(), 3); // Only lib1 fetched again
    }

    #[test]
    fn test_cache_stats() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner).with_ttl(Duration::from_secs(1));

        let stats = cached.cache_stats();
        assert_eq!(stats.entries, 0);

        // Add some entries
        let _ = cached.fetch_library("http://example.com/lib1.txt").unwrap();
        let _ = cached.fetch_library("http://example.com/lib2.txt").unwrap();

        let stats = cached.cache_stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.valid_entries, 2);
        assert_eq!(stats.expired_entries, 0);
    }

    #[test]
    fn test_default_ttl() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);
        assert_eq!(cached.ttl(), Duration::from_secs(DEFAULT_TTL_SECS));
    }

    #[test]
    fn test_custom_ttl() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner).with_ttl(Duration::from_secs(600));
        assert_eq!(cached.ttl(), Duration::from_secs(600));
    }

    #[test]
    fn test_metadata_not_cached() {
        let inner = CountingClient::new();
        let cached = CachedLibraryClient::new(inner);

        // Metadata calls should pass through (and fail in our mock)
        let result = cached.fetch_metadata("http://example.com/meta.txt");
        assert!(result.is_err());
    }
}
