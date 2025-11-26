//! Disk cache with async writes and LRU eviction.

use crate::cache::path::cache_path;
use crate::cache::types::{CacheError, CacheKey};
use crate::cache::CacheStats;
use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Disk cache for persistent storage of DDS tiles.
///
/// Provides write-through caching with synchronous writes.
pub struct DiskCache {
    /// Cache directory root
    cache_dir: PathBuf,
    /// Index of cached tiles (key → path)
    index: Arc<Mutex<HashMap<CacheKey, PathBuf>>>,
    /// Maximum size in bytes
    max_size_bytes: usize,
    /// Current size in bytes
    current_size_bytes: Arc<Mutex<usize>>,
    /// Statistics
    stats: Arc<Mutex<CacheStats>>,
}

impl DiskCache {
    /// Create a new disk cache.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Root directory for cache storage
    /// * `max_size_bytes` - Maximum disk space to use (default: 20GB)
    pub fn new(cache_dir: PathBuf, max_size_bytes: usize) -> Result<Self, CacheError> {
        // Create cache directory if it doesn't exist
        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir)?;
        }

        let cache = Self {
            cache_dir: cache_dir.clone(),
            index: Arc::new(Mutex::new(HashMap::new())),
            max_size_bytes,
            current_size_bytes: Arc::new(Mutex::new(0)),
            stats: Arc::new(Mutex::new(CacheStats::new())),
        };

        // Scan existing cache to build index
        cache.scan_cache_dir()?;

        // Evict if over limit on startup (cleanup before simulator requests tiles)
        cache.evict_if_over_limit()?;

        Ok(cache)
    }

    /// Get a cached tile from disk.
    ///
    /// Returns `Some(data)` if found, `None` otherwise.
    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        // Check index first
        let index = self.index.lock().unwrap();
        let path = index.get(key).cloned();
        drop(index);

        if let Some(path) = path {
            // Read from disk
            match fs::read(&path) {
                Ok(data) => {
                    // Update statistics
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.record_disk_hit();
                    }
                    return Some(data);
                }
                Err(_) => {
                    // File not found or read error - remove from index
                    let mut index = self.index.lock().unwrap();
                    index.remove(key);
                }
            }
        }

        // Miss: not in index or file not found
        if let Ok(mut stats) = self.stats.lock() {
            stats.record_disk_miss();
        }
        None
    }

    /// Synchronously write a tile to disk.
    ///
    /// This bypasses the async write queue and writes directly.
    /// Used for testing and when async writes are not needed.
    pub fn put_sync(&self, key: CacheKey, data: Vec<u8>) -> Result<(), CacheError> {
        let path = cache_path(&self.cache_dir, &key.provider, &key.tile, key.format);

        // Create parent directory
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write file
        fs::write(&path, &data)?;

        // Update index
        let mut index = self.index.lock().unwrap();
        index.insert(key, path);

        // Update size
        let mut size = self.current_size_bytes.lock().unwrap();
        *size += data.len();

        // Update statistics
        if let Ok(mut stats) = self.stats.lock() {
            stats.record_disk_write();
            stats.update_disk_size(*size, index.len());
        }

        Ok(())
    }

    /// Check if a key exists in the cache.
    pub fn contains(&self, key: &CacheKey) -> bool {
        let index = self.index.lock().unwrap();
        index.contains_key(key)
    }

    /// Get the number of entries in the cache.
    pub fn entry_count(&self) -> usize {
        let index = self.index.lock().unwrap();
        index.len()
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
    pub fn clear(&self) -> Result<(), CacheError> {
        let mut index = self.index.lock().unwrap();

        // Delete all files
        for path in index.values() {
            let _ = fs::remove_file(path); // Ignore errors
        }

        index.clear();

        let mut size = self.current_size_bytes.lock().unwrap();
        *size = 0;

        if let Ok(mut stats) = self.stats.lock() {
            stats.update_disk_size(0, 0);
        }

        Ok(())
    }

    /// Scan the cache directory to build the index.
    fn scan_cache_dir(&self) -> Result<(), CacheError> {
        if !self.cache_dir.exists() {
            return Ok(());
        }

        let mut total_size = 0;
        let mut index = self.index.lock().unwrap();

        // Recursively scan cache directory
        self.scan_directory(&self.cache_dir, &mut index, &mut total_size)?;

        // Update size
        let mut size = self.current_size_bytes.lock().unwrap();
        *size = total_size;

        // Update statistics
        if let Ok(mut stats) = self.stats.lock() {
            stats.update_disk_size(total_size, index.len());
        }

        Ok(())
    }

    /// Recursively scan a directory for DDS files.
    fn scan_directory(
        &self,
        dir: &Path,
        index: &mut HashMap<CacheKey, PathBuf>,
        total_size: &mut usize,
    ) -> Result<(), CacheError> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                self.scan_directory(&path, index, total_size)?;
            } else if path.extension().and_then(|s| s.to_str()) == Some("dds") {
                // Try to parse the cache key from the path
                if let Some(key) = self.parse_cache_key_from_path(&path) {
                    if let Ok(metadata) = fs::metadata(&path) {
                        *total_size += metadata.len() as usize;
                        index.insert(key, path);
                    }
                }
            }
        }

        Ok(())
    }

    /// Parse a cache key from a file path.
    ///
    /// Expected format: <cache>/<provider>/<format>/<zoom>/<row>/<format>_<zoom>_<row>_<col>.dds
    fn parse_cache_key_from_path(&self, path: &Path) -> Option<CacheKey> {
        // Get filename: BC1_15_12754_5279.dds
        let filename = path.file_stem()?.to_str()?;
        let parts: Vec<&str> = filename.split('_').collect();

        if parts.len() != 4 {
            return None;
        }

        // Parse format
        let format = match parts[0] {
            "BC1" => DdsFormat::BC1,
            "BC3" => DdsFormat::BC3,
            _ => return None,
        };

        // Parse zoom, row, col
        let zoom: u8 = parts[1].parse().ok()?;
        let row: u32 = parts[2].parse().ok()?;
        let col: u32 = parts[3].parse().ok()?;

        // Extract provider from path
        // Path structure: <cache>/<provider>/<format>/...
        let components: Vec<_> = path.components().collect();
        if components.len() < 3 {
            return None;
        }

        // Find provider (it's before format directory)
        let provider = components
            .iter()
            .rev()
            .skip_while(|c| c.as_os_str() != "BC1" && c.as_os_str() != "BC3")
            .nth(1)?
            .as_os_str()
            .to_str()?
            .to_string();

        Some(CacheKey::new(
            provider,
            format,
            TileCoord { row, col, zoom },
        ))
    }

    /// Evict least recently used entries until under size limit.
    pub fn evict_if_over_limit(&self) -> Result<(), CacheError> {
        let current_size = self.size_bytes();
        if current_size <= self.max_size_bytes {
            return Ok(());
        }

        let target_size = (self.max_size_bytes as f64 * 0.9) as usize; // Evict to 90%
        let index = self.index.lock().unwrap();

        // Collect entries with their modification times
        let mut entries: Vec<(CacheKey, PathBuf, SystemTime, u64)> = Vec::new();
        for (key, path) in index.iter() {
            if let Ok(metadata) = fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    entries.push((key.clone(), path.clone(), modified, metadata.len()));
                }
            }
        }

        // Sort by modification time (oldest first)
        entries.sort_by_key(|(_, _, modified, _)| *modified);

        drop(index);

        // Evict entries until under target
        let mut evicted_count = 0;
        let mut freed_bytes = 0;
        let mut current_size = self.current_size_bytes.lock().unwrap();

        for (key, path, _, size) in entries {
            if *current_size <= target_size {
                break;
            }

            // Delete file
            if fs::remove_file(&path).is_ok() {
                // Remove from index
                let mut index = self.index.lock().unwrap();
                index.remove(&key);

                *current_size = current_size.saturating_sub(size as usize);
                freed_bytes += size;
                evicted_count += 1;
            }
        }

        // Update statistics
        if let Ok(mut stats) = self.stats.lock() {
            stats.record_disk_eviction(evicted_count);
            let index = self.index.lock().unwrap();
            stats.update_disk_size(*current_size, index.len());
        }

        tracing::info!(
            "Disk cache eviction: removed {} entries ({} MB), new size: {} MB",
            evicted_count,
            freed_bytes / (1024 * 1024),
            *current_size / (1024 * 1024)
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_temp_cache() -> (DiskCache, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10_000_000).unwrap();
        (cache, temp_dir)
    }

    fn create_test_key(provider: &str, col: u32) -> CacheKey {
        CacheKey::new(
            provider,
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_disk_cache_new() {
        let (cache, _temp) = create_temp_cache();
        assert_eq!(cache.max_size_bytes(), 10_000_000);
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn test_disk_cache_put_and_get() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put_sync(key.clone(), data.clone()).unwrap();

        let retrieved = cache.get(&key);
        assert_eq!(retrieved, Some(data));
    }

    #[test]
    fn test_disk_cache_miss() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);

        let retrieved = cache.get(&key);
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_disk_cache_contains() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);
        let data = vec![1, 2, 3];

        assert!(!cache.contains(&key));
        cache.put_sync(key.clone(), data).unwrap();
        assert!(cache.contains(&key));
    }

    #[test]
    fn test_disk_cache_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        // Create cache and write data
        {
            let cache = DiskCache::new(cache_dir.clone(), 10_000_000).unwrap();
            let key = create_test_key("bing", 1);
            let data = vec![1, 2, 3, 4, 5];
            cache.put_sync(key, data).unwrap();
        }

        // Create new cache instance and verify data persists
        {
            let cache = DiskCache::new(cache_dir, 10_000_000).unwrap();
            assert_eq!(cache.entry_count(), 1);

            let key = create_test_key("bing", 1);
            let retrieved = cache.get(&key);
            assert_eq!(retrieved, Some(vec![1, 2, 3, 4, 5]));
        }
    }

    #[test]
    fn test_disk_cache_size_tracking() {
        let (cache, _temp) = create_temp_cache();
        let key1 = create_test_key("bing", 1);
        let key2 = create_test_key("bing", 2);
        let data1 = vec![0u8; 1000];
        let data2 = vec![0u8; 2000];

        cache.put_sync(key1, data1).unwrap();
        assert_eq!(cache.size_bytes(), 1000);

        cache.put_sync(key2, data2).unwrap();
        assert_eq!(cache.size_bytes(), 3000);
        assert_eq!(cache.entry_count(), 2);
    }

    #[test]
    fn test_disk_cache_clear() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);
        let data = vec![1, 2, 3, 4, 5];

        cache.put_sync(key.clone(), data).unwrap();
        assert_eq!(cache.entry_count(), 1);

        cache.clear().unwrap();
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
        assert!(!cache.contains(&key));
    }

    #[test]
    fn test_disk_cache_provider_separation() {
        let (cache, _temp) = create_temp_cache();
        let bing_key = create_test_key("bing", 1);
        let google_key = create_test_key("google", 1);
        let data = vec![1, 2, 3];

        cache.put_sync(bing_key.clone(), data.clone()).unwrap();
        cache.put_sync(google_key.clone(), data.clone()).unwrap();

        assert!(cache.contains(&bing_key));
        assert!(cache.contains(&google_key));
        assert_eq!(cache.entry_count(), 2);
    }

    #[test]
    fn test_disk_cache_statistics_hits() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);
        let data = vec![1, 2, 3];

        cache.put_sync(key.clone(), data).unwrap();

        cache.get(&key);
        cache.get(&key);

        let stats = cache.stats();
        assert_eq!(stats.disk_hits, 2);
    }

    #[test]
    fn test_disk_cache_statistics_misses() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 1);

        cache.get(&key);
        cache.get(&key);

        let stats = cache.stats();
        assert_eq!(stats.disk_misses, 2);
    }

    #[test]
    fn test_disk_cache_statistics_writes() {
        let (cache, _temp) = create_temp_cache();
        let data = vec![1, 2, 3];

        cache
            .put_sync(create_test_key("bing", 1), data.clone())
            .unwrap();
        cache
            .put_sync(create_test_key("bing", 2), data.clone())
            .unwrap();

        let stats = cache.stats();
        assert_eq!(stats.disk_writes, 2);
    }

    #[test]
    fn test_disk_cache_eviction() {
        let (cache, _temp) = create_temp_cache();
        let data = vec![0u8; 1_000_000]; // 1 MB each

        // Add 15 entries (15 MB total, over 10 MB limit)
        for i in 1..=15 {
            cache
                .put_sync(create_test_key("bing", i), data.clone())
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(cache.size_bytes() > cache.max_size_bytes());

        // Evict until under limit
        cache.evict_if_over_limit().unwrap();

        // Should be under limit now (target is 90% of max)
        assert!(cache.size_bytes() <= cache.max_size_bytes());

        let stats = cache.stats();
        assert!(stats.disk_evictions > 0);
    }

    #[test]
    fn test_parse_cache_key_from_path() {
        let (cache, _temp) = create_temp_cache();
        let key = create_test_key("bing", 5279);

        // Write a file
        cache.put_sync(key.clone(), vec![1, 2, 3]).unwrap();

        // Get the path
        let index = cache.index.lock().unwrap();
        let path = index.get(&key).unwrap();

        // Parse it back
        let parsed = cache.parse_cache_key_from_path(path);
        assert_eq!(parsed, Some(key));
    }
}
