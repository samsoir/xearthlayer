//! Cache stage - stores generated tiles in cache.
//!
//! This stage writes the completed DDS tile to the memory cache for fast
//! repeated access. Disk cache writes happen earlier (during download stage)
//! at the chunk level.

use crate::coord::TileCoord;
use crate::pipeline::{JobId, MemoryCache};
use std::sync::Arc;
use tracing::{debug, instrument, warn};

/// Stores a generated tile in the memory cache.
///
/// This stage:
/// 1. Writes the complete DDS tile to memory cache
/// 2. Logs any cache write failures but doesn't propagate them
///    (caching is an optimization, not a requirement)
///
/// # Arguments
///
/// * `job_id` - For logging correlation
/// * `tile` - The tile coordinates
/// * `dds_data` - The encoded DDS data
/// * `memory_cache` - The memory cache to write to
///
/// # Note
///
/// This function never fails - cache write errors are logged but ignored
/// because caching is purely an optimization.
#[instrument(skip(dds_data, memory_cache), fields(job_id = %job_id))]
pub async fn cache_stage<M>(
    job_id: JobId,
    tile: TileCoord,
    dds_data: &[u8],
    memory_cache: Arc<M>,
) where
    M: MemoryCache,
{
    let size = dds_data.len();

    // Memory cache is synchronous (fast in-memory operation)
    memory_cache.put(tile.row, tile.col, tile.zoom, dds_data.to_vec());

    debug!(
        job_id = %job_id,
        tile_row = tile.row,
        tile_col = tile.col,
        zoom = tile.zoom,
        size_bytes = size,
        "Cache stage complete"
    );
}

/// Checks if a tile is in the memory cache.
///
/// This is called early in the pipeline to short-circuit if we have a cache hit.
///
/// # Arguments
///
/// * `tile` - The tile coordinates to check
/// * `memory_cache` - The memory cache to check
///
/// # Returns
///
/// The cached DDS data if present.
pub fn check_memory_cache<M>(tile: TileCoord, memory_cache: &M) -> Option<Vec<u8>>
where
    M: MemoryCache,
{
    memory_cache.get(tile.row, tile.col, tile.zoom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock memory cache for testing
    struct MockMemoryCache {
        data: Mutex<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockMemoryCache {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    impl MemoryCache for MockMemoryCache {
        fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(&(row, col, zoom)).cloned()
        }

        fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
            self.data.lock().unwrap().insert((row, col, zoom), data);
        }

        fn size_bytes(&self) -> usize {
            self.data
                .lock()
                .unwrap()
                .values()
                .map(|v| v.len())
                .sum()
        }

        fn entry_count(&self) -> usize {
            self.data.lock().unwrap().len()
        }
    }

    #[tokio::test]
    async fn test_cache_stage_writes() {
        let cache = Arc::new(MockMemoryCache::new());
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let dds_data = vec![0xDD, 0x53, 0x00, 0x00];

        cache_stage(JobId::new(), tile, &dds_data, Arc::clone(&cache)).await;

        // Verify data was cached
        let cached = cache.get(100, 200, 16);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap(), dds_data);
    }

    #[test]
    fn test_check_memory_cache_hit() {
        let cache = MockMemoryCache::new();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };
        let dds_data = vec![0xDD, 0x53, 0x00, 0x00];

        cache.put(100, 200, 16, dds_data.clone());

        let result = check_memory_cache(tile, &cache);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dds_data);
    }

    #[test]
    fn test_check_memory_cache_miss() {
        let cache = MockMemoryCache::new();
        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 16,
        };

        let result = check_memory_cache(tile, &cache);
        assert!(result.is_none());
    }

    #[test]
    fn test_cache_size_tracking() {
        let cache = MockMemoryCache::new();

        cache.put(1, 1, 1, vec![0u8; 100]);
        cache.put(2, 2, 2, vec![0u8; 200]);

        assert_eq!(cache.entry_count(), 2);
        assert_eq!(cache.size_bytes(), 300);
    }
}
