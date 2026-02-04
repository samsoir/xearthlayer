//! In-memory LRU index for cache entries.
//!
//! This module provides an in-memory index that tracks cache entries with
//! their access times, enabling efficient LRU-based garbage collection
//! without filesystem scanning.
//!
//! # Memory Efficiency
//!
//! The index stores minimal metadata per entry:
//! - `size_bytes`: 8 bytes
//! - `last_accessed`: 8 bytes
//! - Key overhead: ~30 bytes (DashMap entry)
//!
//! For 2 million entries, this uses approximately 100 MB of RAM.
//!
//! # Lifecycle
//!
//! The index is ephemeral (in-memory only):
//! - Rebuilt from disk on each startup via `populate_from_disk()`
//! - Kept in sync via `record()`, `touch()`, `remove()` during operations
//! - Uses filesystem mtime as initial `last_accessed` during startup

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;

use crate::time::system_time_to_instant;

/// Minimal metadata for cache entry tracking.
///
/// Paths are computed from keys, not stored, to minimize memory usage.
#[derive(Debug, Clone)]
pub struct CacheEntryMetadata {
    /// Size of the cached data in bytes.
    pub size_bytes: u64,
    /// Last access time (updated on get/put).
    pub last_accessed: Instant,
}

/// Statistics from populating the index from disk.
#[derive(Debug, Default)]
pub struct PopulateStats {
    /// Number of files successfully indexed.
    pub files_indexed: u64,
    /// Number of files skipped (not parseable).
    pub skipped_unparseable: u64,
    /// Total size in bytes.
    pub total_bytes: u64,
}

/// A candidate entry for eviction.
#[derive(Debug, Clone)]
pub struct EvictionCandidate {
    /// Cache key.
    pub key: String,
    /// Entry metadata.
    pub metadata: CacheEntryMetadata,
}

/// Thread-safe in-memory LRU index for cache entries.
///
/// Uses `DashMap` for concurrent access and `AtomicU64` for size tracking.
pub struct LruIndex {
    /// Map from cache key to entry metadata.
    entries: DashMap<String, CacheEntryMetadata>,
    /// Total size of all tracked entries.
    total_size: AtomicU64,
    /// Total entry count.
    entry_count: AtomicU64,
    /// Base cache directory for path computation.
    cache_path: PathBuf,
}

impl LruIndex {
    /// Create a new empty LRU index.
    ///
    /// # Arguments
    ///
    /// * `cache_path` - Base directory for the cache files
    pub fn new(cache_path: PathBuf) -> Self {
        Self {
            entries: DashMap::new(),
            total_size: AtomicU64::new(0),
            entry_count: AtomicU64::new(0),
            cache_path,
        }
    }

    /// Record a new cache entry or update an existing one.
    ///
    /// If the key already exists, updates size and access time.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key (e.g., "tile:15:12754:5279")
    /// * `size` - Size of the cached data in bytes
    pub fn record(&self, key: &str, size: u64) {
        let metadata = CacheEntryMetadata {
            size_bytes: size,
            last_accessed: Instant::now(),
        };

        if let Some(old) = self.entries.insert(key.to_string(), metadata) {
            // Updating existing entry - adjust size delta
            let old_size = old.size_bytes;
            if size > old_size {
                self.total_size
                    .fetch_add(size - old_size, Ordering::Relaxed);
            } else {
                self.total_size
                    .fetch_sub(old_size - size, Ordering::Relaxed);
            }
        } else {
            // New entry
            self.total_size.fetch_add(size, Ordering::Relaxed);
            self.entry_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Update the access time for an existing entry.
    ///
    /// Does nothing if the key doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key to touch
    pub fn touch(&self, key: &str) {
        if let Some(mut entry) = self.entries.get_mut(key) {
            entry.last_accessed = Instant::now();
        }
    }

    /// Remove an entry from the index.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key to remove
    ///
    /// # Returns
    ///
    /// The removed metadata, or `None` if the key didn't exist.
    pub fn remove(&self, key: &str) -> Option<CacheEntryMetadata> {
        if let Some((_, metadata)) = self.entries.remove(key) {
            self.total_size
                .fetch_sub(metadata.size_bytes, Ordering::Relaxed);
            self.entry_count.fetch_sub(1, Ordering::Relaxed);
            Some(metadata)
        } else {
            None
        }
    }

    /// Check if a key exists in the index.
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Get eviction candidates sorted by last access time (oldest first).
    ///
    /// Returns entries older than `threshold` up to `limit` count.
    ///
    /// # Arguments
    ///
    /// * `min_age` - Minimum time since last access (entries accessed more recently are skipped)
    /// * `limit` - Maximum number of candidates to return
    ///
    /// # Returns
    ///
    /// A vector of eviction candidates, sorted oldest first.
    pub fn get_eviction_candidates(
        &self,
        min_age: std::time::Duration,
        limit: usize,
    ) -> Vec<EvictionCandidate> {
        let threshold = Instant::now() - min_age;

        // Collect candidates older than threshold
        let mut candidates: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.value().last_accessed < threshold)
            .map(|entry| EvictionCandidate {
                key: entry.key().clone(),
                metadata: entry.value().clone(),
            })
            .collect();

        // Sort by last_accessed (oldest first)
        candidates.sort_by_key(|c| c.metadata.last_accessed);

        // Return up to limit
        candidates.truncate(limit);
        candidates
    }

    /// Compute the file path for a cache key.
    ///
    /// Uses a safe filename encoding that is reversible.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key
    ///
    /// # Returns
    ///
    /// Full path to the cache file.
    pub fn key_to_path(&self, key: &str) -> PathBuf {
        self.cache_path.join(Self::key_to_filename(key))
    }

    /// Convert a cache key to a safe filename.
    ///
    /// Replaces ':' with '_' to create a filesystem-safe name.
    /// The encoding is reversible via `filename_to_key`.
    ///
    /// # Examples
    ///
    /// - `"tile:15:12754:5279"` -> `"tile_15_12754_5279.cache"`
    pub fn key_to_filename(key: &str) -> String {
        format!("{}.cache", key.replace(':', "_"))
    }

    /// Parse a filename back to a cache key.
    ///
    /// Reverses the encoding from `key_to_filename`.
    ///
    /// # Returns
    ///
    /// `Some(key)` if the filename is valid, `None` otherwise.
    pub fn filename_to_key(filename: &str) -> Option<String> {
        let name = filename.strip_suffix(".cache")?;
        Some(name.replace('_', ":"))
    }

    /// Get the total size of all cached entries in bytes.
    pub fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Relaxed)
    }

    /// Get the number of entries in the index.
    pub fn entry_count(&self) -> u64 {
        self.entry_count.load(Ordering::Relaxed)
    }

    /// Populate the index from existing cache files on disk.
    ///
    /// Scans the cache directory and adds entries for each valid cache file.
    /// Uses file mtime as initial `last_accessed` approximation.
    ///
    /// This should be called once at startup.
    ///
    /// # Returns
    ///
    /// Statistics about the population process.
    pub async fn populate_from_disk(&self) -> std::io::Result<PopulateStats> {
        let mut stats = PopulateStats::default();

        // Check if directory exists
        if !self.cache_path.exists() {
            return Ok(stats);
        }

        let mut dir = tokio::fs::read_dir(&self.cache_path).await?;

        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();

            // Skip non-files
            let metadata = match tokio::fs::metadata(&path).await {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            // Parse filename to get key
            let filename = match path.file_name().and_then(|n| n.to_str()) {
                Some(f) => f,
                None => {
                    stats.skipped_unparseable += 1;
                    continue;
                }
            };

            let key = match Self::filename_to_key(filename) {
                Some(k) => k,
                None => {
                    stats.skipped_unparseable += 1;
                    continue;
                }
            };

            // Use file mtime as initial last_accessed approximation
            let last_accessed = metadata
                .modified()
                .ok()
                .and_then(system_time_to_instant)
                .unwrap_or_else(Instant::now);

            let size = metadata.len();

            self.entries.insert(
                key,
                CacheEntryMetadata {
                    size_bytes: size,
                    last_accessed,
                },
            );

            self.total_size.fetch_add(size, Ordering::Relaxed);
            self.entry_count.fetch_add(1, Ordering::Relaxed);
            stats.files_indexed += 1;
            stats.total_bytes += size;

            // Yield periodically to avoid blocking (every 100 files)
            if stats.files_indexed % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }

        // Calculate average file size for diagnostics
        let avg_size_kb = if stats.files_indexed > 0 {
            (stats.total_bytes / stats.files_indexed) / 1_000
        } else {
            0
        };

        tracing::debug!(
            files = stats.files_indexed,
            skipped = stats.skipped_unparseable,
            total_size_mb = stats.total_bytes / 1_000_000,
            avg_size_kb = avg_size_kb,
            "LRU index populated from disk"
        );

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    // ─────────────────────────────────────────────────────────────────────────
    // Basic operations
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn lru_index_record_updates_total_size() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        assert_eq!(index.total_size(), 0);
        assert_eq!(index.entry_count(), 0);

        index.record("tile:15:100:200", 1000);

        assert_eq!(index.total_size(), 1000);
        assert_eq!(index.entry_count(), 1);

        index.record("tile:15:100:201", 2000);

        assert_eq!(index.total_size(), 3000);
        assert_eq!(index.entry_count(), 2);
    }

    #[test]
    fn lru_index_record_updates_existing_entry() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        index.record("tile:15:100:200", 1000);
        assert_eq!(index.total_size(), 1000);
        assert_eq!(index.entry_count(), 1);

        // Update with larger size
        index.record("tile:15:100:200", 1500);
        assert_eq!(index.total_size(), 1500);
        assert_eq!(index.entry_count(), 1); // Still 1 entry

        // Update with smaller size
        index.record("tile:15:100:200", 500);
        assert_eq!(index.total_size(), 500);
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn lru_index_touch_updates_last_accessed() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        index.record("tile:15:100:200", 1000);

        let before = index.entries.get("tile:15:100:200").unwrap().last_accessed;

        // Wait a bit to ensure time difference
        std::thread::sleep(Duration::from_millis(10));

        index.touch("tile:15:100:200");

        let after = index.entries.get("tile:15:100:200").unwrap().last_accessed;

        assert!(after > before, "touch() should update last_accessed");
    }

    #[test]
    fn lru_index_touch_nonexistent_is_noop() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        // Should not panic or error
        index.touch("nonexistent:key");

        assert_eq!(index.entry_count(), 0);
    }

    #[test]
    fn lru_index_remove_decrements_total_size() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        index.record("tile:15:100:200", 1000);
        index.record("tile:15:100:201", 2000);

        assert_eq!(index.total_size(), 3000);
        assert_eq!(index.entry_count(), 2);

        let removed = index.remove("tile:15:100:200");

        assert!(removed.is_some());
        assert_eq!(removed.unwrap().size_bytes, 1000);
        assert_eq!(index.total_size(), 2000);
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn lru_index_remove_nonexistent_returns_none() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        let removed = index.remove("nonexistent:key");

        assert!(removed.is_none());
        assert_eq!(index.total_size(), 0);
        assert_eq!(index.entry_count(), 0);
    }

    #[test]
    fn lru_index_contains() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        assert!(!index.contains("tile:15:100:200"));

        index.record("tile:15:100:200", 1000);

        assert!(index.contains("tile:15:100:200"));
        assert!(!index.contains("tile:15:100:201"));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Eviction candidates
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn lru_index_eviction_candidates_returns_oldest_first() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        // Add entries with different ages
        index.record("old", 100);
        std::thread::sleep(Duration::from_millis(20));

        index.record("medium", 200);
        std::thread::sleep(Duration::from_millis(20));

        index.record("new", 300);

        // Get all candidates (min_age = 0 means all entries)
        let candidates = index.get_eviction_candidates(Duration::ZERO, 10);

        assert_eq!(candidates.len(), 3);
        // Oldest should be first
        assert_eq!(candidates[0].key, "old");
        assert_eq!(candidates[1].key, "medium");
        assert_eq!(candidates[2].key, "new");
    }

    #[test]
    fn lru_index_eviction_candidates_respects_limit() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        for i in 0..10 {
            index.record(&format!("key:{}", i), 100);
        }

        let candidates = index.get_eviction_candidates(Duration::ZERO, 3);

        assert_eq!(candidates.len(), 3);
    }

    #[test]
    fn lru_index_eviction_candidates_respects_min_age() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        index.record("old", 100);
        std::thread::sleep(Duration::from_millis(50));

        index.record("new", 200);

        // Only entries older than 30ms
        let candidates = index.get_eviction_candidates(Duration::from_millis(30), 10);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, "old");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Filename encoding
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn key_to_filename_encodes_correctly() {
        assert_eq!(
            LruIndex::key_to_filename("tile:15:12754:5279"),
            "tile_15_12754_5279.cache"
        );
    }

    #[test]
    fn filename_to_key_decodes_correctly() {
        assert_eq!(
            LruIndex::filename_to_key("tile_15_12754_5279.cache"),
            Some("tile:15:12754:5279".to_string())
        );
    }

    #[test]
    fn filename_to_key_returns_none_for_invalid() {
        assert_eq!(LruIndex::filename_to_key("not_a_cache_file.txt"), None);
        assert_eq!(LruIndex::filename_to_key(""), None);
    }

    #[test]
    fn key_to_filename_roundtrip() {
        let key = "tile:15:12754:5279";
        let filename = LruIndex::key_to_filename(key);
        let decoded = LruIndex::filename_to_key(&filename);

        assert_eq!(decoded, Some(key.to_string()));
    }

    #[test]
    fn key_to_path_uses_cache_directory() {
        let temp_dir = TempDir::new().unwrap();
        let index = LruIndex::new(temp_dir.path().to_path_buf());

        let path = index.key_to_path("tile:15:100:200");

        assert_eq!(path.parent().unwrap(), temp_dir.path());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "tile_15_100_200.cache"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Disk population
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn populate_from_disk_indexes_all_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create some cache files
        std::fs::write(
            temp_dir.path().join("tile_15_100_200.cache"),
            vec![0u8; 1000],
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("tile_15_100_201.cache"),
            vec![0u8; 2000],
        )
        .unwrap();

        let index = LruIndex::new(temp_dir.path().to_path_buf());
        let stats = index.populate_from_disk().await.unwrap();

        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.total_bytes, 3000);
        assert_eq!(index.entry_count(), 2);
        assert_eq!(index.total_size(), 3000);

        assert!(index.contains("tile:15:100:200"));
        assert!(index.contains("tile:15:100:201"));
    }

    #[tokio::test]
    async fn populate_from_disk_skips_non_cache_files() {
        let temp_dir = TempDir::new().unwrap();

        // Valid cache file
        std::fs::write(
            temp_dir.path().join("tile_15_100_200.cache"),
            vec![0u8; 1000],
        )
        .unwrap();

        // Non-cache files
        std::fs::write(temp_dir.path().join("readme.txt"), "hello").unwrap();
        std::fs::write(temp_dir.path().join("data.json"), "{}").unwrap();

        let index = LruIndex::new(temp_dir.path().to_path_buf());
        let stats = index.populate_from_disk().await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.skipped_unparseable, 2);
        assert_eq!(index.entry_count(), 1);
    }

    #[tokio::test]
    async fn populate_from_disk_handles_empty_directory() {
        let temp_dir = TempDir::new().unwrap();

        let index = LruIndex::new(temp_dir.path().to_path_buf());
        let stats = index.populate_from_disk().await.unwrap();

        assert_eq!(stats.files_indexed, 0);
        assert_eq!(index.entry_count(), 0);
    }

    #[tokio::test]
    async fn populate_from_disk_handles_nonexistent_directory() {
        let index = LruIndex::new(PathBuf::from("/nonexistent/path/that/doesnt/exist"));
        let stats = index.populate_from_disk().await.unwrap();

        assert_eq!(stats.files_indexed, 0);
        assert_eq!(index.entry_count(), 0);
    }
}
