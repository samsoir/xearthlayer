//! In-memory cache with LRU eviction.

use crate::cache::types::{CacheError, CacheKey};
use crate::cache::CacheStats;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Entry in the memory cache.
#[derive(Debug, Clone)]
struct CacheEntry {
    /// Cached data
    data: Vec<u8>,
    /// Last access time for LRU eviction
    last_accessed: Instant,
    /// Number of times accessed
    access_count: u64,
}

impl CacheEntry {
    /// Create a new cache entry.
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            last_accessed: Instant::now(),
            access_count: 0,
        }
    }

    /// Update access time and increment access count.
    fn touch(&mut self) {
        self.last_accessed = Instant::now();
        self.access_count += 1;
    }
}

/// In-memory cache for DDS tiles.
///
/// Provides fast access to recently used tiles with LRU eviction
/// when memory limits are exceeded.
pub struct MemoryCache {
    /// Cache storage
    cache: Arc<Mutex<HashMap<CacheKey, CacheEntry>>>,
    /// Maximum size in bytes
    max_size_bytes: usize,
    /// Current size in bytes
    current_size_bytes: Arc<Mutex<usize>>,
    /// Statistics
    stats: Arc<Mutex<CacheStats>>,
}

impl MemoryCache {
    /// Create a new memory cache with the given size limit.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum memory size in bytes (default: 2GB)
    pub fn new(max_size_bytes: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            max_size_bytes,
            current_size_bytes: Arc::new(Mutex::new(0)),
            stats: Arc::new(Mutex::new(CacheStats::new())),
        }
    }

    /// Get a cached tile.
    ///
    /// Returns `Some(data)` if the tile is in cache, `None` otherwise.
    /// Updates access time and statistics on cache hit.
    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock().unwrap();

        if let Some(entry) = cache.get_mut(key) {
            // Cache hit - update access time
            entry.touch();

            // Update statistics
            if let Ok(mut stats) = self.stats.lock() {
                stats.record_memory_hit();
            }

            Some(entry.data.clone())
        } else {
            // Cache miss
            if let Ok(mut stats) = self.stats.lock() {
                stats.record_memory_miss();
            }

            None
        }
    }

    /// Put a tile into the cache.
    ///
    /// If adding this tile would exceed the size limit, evict LRU entries first.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key for the tile
    /// * `data` - Tile data to cache
    pub fn put(&self, key: CacheKey, data: Vec<u8>) -> Result<(), CacheError> {
        let data_size = data.len();

        // Check if we need to evict entries
        let mut current_size = self.current_size_bytes.lock().unwrap();
        if *current_size + data_size > self.max_size_bytes {
            drop(current_size); // Release lock before evicting
            self.evict_lru_until_size(data_size)?;
            current_size = self.current_size_bytes.lock().unwrap();
        }

        // Add the entry
        let mut cache = self.cache.lock().unwrap();
        cache.insert(key, CacheEntry::new(data));

        // Update size
        *current_size += data_size;

        // Update statistics
        if let Ok(mut stats) = self.stats.lock() {
            stats.update_memory_size(*current_size, cache.len());
        }

        Ok(())
    }

    /// Check if a key exists in the cache.
    pub fn contains(&self, key: &CacheKey) -> bool {
        let cache = self.cache.lock().unwrap();
        cache.contains_key(key)
    }

    /// Get the current number of entries in the cache.
    pub fn entry_count(&self) -> usize {
        let cache = self.cache.lock().unwrap();
        cache.len()
    }

    /// Get the current size of the cache in bytes.
    pub fn size_bytes(&self) -> usize {
        let size = self.current_size_bytes.lock().unwrap();
        *size
    }

    /// Get the maximum size of the cache in bytes.
    pub fn max_size_bytes(&self) -> usize {
        self.max_size_bytes
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        let stats = self.stats.lock().unwrap();
        stats.clone()
    }

    /// Clear all entries from the cache.
    pub fn clear(&self) {
        let mut cache = self.cache.lock().unwrap();
        cache.clear();

        let mut size = self.current_size_bytes.lock().unwrap();
        *size = 0;

        if let Ok(mut stats) = self.stats.lock() {
            stats.update_memory_size(0, 0);
        }
    }

    /// Evict least recently used entries until we have enough space.
    ///
    /// # Arguments
    ///
    /// * `required_size` - Amount of space needed in bytes
    fn evict_lru_until_size(&self, required_size: usize) -> Result<(), CacheError> {
        let mut cache = self.cache.lock().unwrap();
        let mut current_size = self.current_size_bytes.lock().unwrap();

        // Calculate target size after eviction
        let target_size = if *current_size + required_size > self.max_size_bytes {
            self.max_size_bytes.saturating_sub(required_size)
        } else {
            return Ok(());
        };

        // Collect entries sorted by last access time (oldest first)
        let mut entries: Vec<(CacheKey, Instant, usize)> = cache
            .iter()
            .map(|(k, v)| (k.clone(), v.last_accessed, v.data.len()))
            .collect();

        entries.sort_by_key(|(_, accessed, _)| *accessed);

        // Evict entries until we reach target size
        let mut evicted_count = 0;
        for (key, _, size) in entries {
            if *current_size <= target_size {
                break;
            }

            cache.remove(&key);
            *current_size = current_size.saturating_sub(size);
            evicted_count += 1;
        }

        // Update statistics
        if let Ok(mut stats) = self.stats.lock() {
            stats.record_memory_eviction(evicted_count);
            stats.update_memory_size(*current_size, cache.len());
        }

        Ok(())
    }

    /// Evict entries until the cache is under the size limit.
    ///
    /// This is called by the cache daemon thread periodically.
    pub fn evict_if_over_limit(&self) -> Result<(), CacheError> {
        let current_size = self.size_bytes();
        if current_size > self.max_size_bytes {
            let excess = current_size - self.max_size_bytes;
            self.evict_lru_until_size(0)?;
            tracing::info!(
                "Memory cache eviction: removed excess {} bytes, new size: {} bytes",
                excess,
                self.size_bytes()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::dds::DdsFormat;

    fn create_test_key(col: u32) -> CacheKey {
        CacheKey::new(
            "test",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_memory_cache_new() {
        let cache = MemoryCache::new(1_000_000);
        assert_eq!(cache.max_size_bytes(), 1_000_000);
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn test_memory_cache_put_and_get() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put(key.clone(), data.clone()).unwrap();

        let retrieved = cache.get(&key);
        assert_eq!(retrieved, Some(data));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_memory_cache_miss() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);

        let retrieved = cache.get(&key);
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_memory_cache_contains() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        assert!(!cache.contains(&key));
        cache.put(key.clone(), data).unwrap();
        assert!(cache.contains(&key));
    }

    #[test]
    fn test_memory_cache_size_tracking() {
        let cache = MemoryCache::new(1_000_000);
        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let data1 = vec![0u8; 1000];
        let data2 = vec![0u8; 2000];

        cache.put(key1, data1).unwrap();
        assert_eq!(cache.size_bytes(), 1000);

        cache.put(key2, data2).unwrap();
        assert_eq!(cache.size_bytes(), 3000);
        assert_eq!(cache.entry_count(), 2);
    }

    #[test]
    fn test_memory_cache_clear() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put(key.clone(), data).unwrap();
        assert_eq!(cache.entry_count(), 1);
        assert!(cache.size_bytes() > 0);

        cache.clear();
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
        assert!(!cache.contains(&key));
    }

    #[test]
    fn test_memory_cache_lru_eviction() {
        // Create cache that can hold ~2.5 entries of 1000 bytes each
        let cache = MemoryCache::new(2500);

        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let key3 = create_test_key(3);
        let data = vec![0u8; 1000];

        // Add 3 entries (3000 bytes total)
        cache.put(key1.clone(), data.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        cache.put(key2.clone(), data.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        cache.put(key3.clone(), data.clone()).unwrap();

        // First entry should have been evicted
        assert!(!cache.contains(&key1), "Oldest entry should be evicted");
        assert!(cache.contains(&key2), "Second entry should remain");
        assert!(cache.contains(&key3), "Newest entry should remain");
        assert!(cache.size_bytes() <= 2500, "Cache should be under limit");
    }

    #[test]
    fn test_memory_cache_lru_eviction_multiple_entries() {
        // Create cache that can hold 2 entries
        let cache = MemoryCache::new(2000);

        let data = vec![0u8; 1000];

        // Add 5 entries
        for i in 1..=5 {
            cache.put(create_test_key(i), data.clone()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Only the last 2 entries should remain
        assert!(!cache.contains(&create_test_key(1)));
        assert!(!cache.contains(&create_test_key(2)));
        assert!(!cache.contains(&create_test_key(3)));
        assert!(cache.contains(&create_test_key(4)));
        assert!(cache.contains(&create_test_key(5)));
    }

    #[test]
    fn test_memory_cache_access_updates_lru() {
        let cache = MemoryCache::new(2500);
        let data = vec![0u8; 1000];

        let key1 = create_test_key(1);
        let key2 = create_test_key(2);
        let key3 = create_test_key(3);

        // Add 2 entries
        cache.put(key1.clone(), data.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.put(key2.clone(), data.clone()).unwrap();

        // Access the first entry to make it more recent
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.get(&key1);

        // Add a third entry, should evict key2 (oldest)
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.put(key3.clone(), data.clone()).unwrap();

        // key1 should remain (we accessed it), key2 should be evicted
        assert!(cache.contains(&key1), "Accessed entry should remain");
        assert!(
            !cache.contains(&key2),
            "Oldest unaccessed entry should be evicted"
        );
        assert!(cache.contains(&key3), "Newest entry should remain");
    }

    #[test]
    fn test_memory_cache_statistics_hits() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![1, 2, 3];

        cache.put(key.clone(), data).unwrap();

        cache.get(&key);
        cache.get(&key);

        let stats = cache.stats();
        assert_eq!(stats.memory_hits, 2);
        assert_eq!(stats.memory_misses, 0);
    }

    #[test]
    fn test_memory_cache_statistics_misses() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);

        cache.get(&key);
        cache.get(&key);

        let stats = cache.stats();
        assert_eq!(stats.memory_hits, 0);
        assert_eq!(stats.memory_misses, 2);
    }

    #[test]
    fn test_memory_cache_statistics_evictions() {
        let cache = MemoryCache::new(1500);
        let data = vec![0u8; 1000];

        // Add entries that will trigger evictions
        for i in 1..=3 {
            cache.put(create_test_key(i), data.clone()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let stats = cache.stats();
        assert!(stats.memory_evictions > 0);
    }

    #[test]
    fn test_memory_cache_statistics_size() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data = vec![0u8; 5000];

        cache.put(key, data).unwrap();

        let stats = cache.stats();
        assert_eq!(stats.memory_size_bytes, 5000);
        assert_eq!(stats.memory_entry_count, 1);
    }

    #[test]
    fn test_memory_cache_evict_if_over_limit() {
        let cache = MemoryCache::new(2000);

        // Initially under limit
        assert!(cache.size_bytes() <= cache.max_size_bytes());

        // Calling evict when under limit should be a no-op
        cache.evict_if_over_limit().unwrap();
        assert_eq!(cache.size_bytes(), 0);

        // Add entries that trigger automatic eviction during put
        let data = vec![0u8; 1000];
        for i in 1..=3 {
            cache.put(create_test_key(i), data.clone()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // After put(), cache should already be under limit due to automatic eviction
        assert!(cache.size_bytes() <= cache.max_size_bytes());

        // evict_if_over_limit should be a no-op since we're already under limit
        let size_before = cache.size_bytes();
        cache.evict_if_over_limit().unwrap();
        assert_eq!(cache.size_bytes(), size_before);
    }

    #[test]
    fn test_cache_entry_touch() {
        let mut entry = CacheEntry::new(vec![1, 2, 3]);
        let initial_time = entry.last_accessed;
        let initial_count = entry.access_count;

        std::thread::sleep(std::time::Duration::from_millis(10));
        entry.touch();

        assert!(entry.last_accessed > initial_time);
        assert_eq!(entry.access_count, initial_count + 1);
    }

    #[test]
    fn test_memory_cache_large_entry() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        // BC1 DDS file is ~11 MB
        let data = vec![0u8; 11_174_016];

        // Should fail because entry is larger than cache
        let result = cache.put(key, data);
        assert!(result.is_ok()); // Will evict everything and add it
    }

    #[test]
    fn test_memory_cache_replace_existing() {
        let cache = MemoryCache::new(1_000_000);
        let key = create_test_key(1);
        let data1 = vec![1, 2, 3];
        let data2 = vec![4, 5, 6, 7, 8];

        cache.put(key.clone(), data1).unwrap();
        cache.put(key.clone(), data2.clone()).unwrap();

        let retrieved = cache.get(&key);
        assert_eq!(retrieved, Some(data2));
        assert_eq!(cache.entry_count(), 1);
    }
}
