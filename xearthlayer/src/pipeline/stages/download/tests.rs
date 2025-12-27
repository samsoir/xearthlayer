//! Tests for download stage functionality.

#![allow(clippy::type_complexity)]

use super::*;
use crate::pipeline::ChunkDownloadError;
use std::collections::HashMap;
use std::sync::Mutex;

/// Mock provider that returns predictable data.
struct MockProvider {
    /// Chunks that should fail
    failures: Mutex<HashMap<(u32, u32, u8), ChunkDownloadError>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn with_failure(self, row: u32, col: u32, zoom: u8, err: ChunkDownloadError) -> Self {
        self.failures.lock().unwrap().insert((row, col, zoom), err);
        self
    }
}

impl ChunkProvider for MockProvider {
    async fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> Result<Vec<u8>, ChunkDownloadError> {
        if let Some(err) = self.failures.lock().unwrap().get(&(row, col, zoom)) {
            return Err(err.clone());
        }
        // Return predictable test data
        Ok(vec![row as u8, col as u8, zoom])
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// Mock disk cache that stores nothing.
struct NullDiskCache;

impl DiskCache for NullDiskCache {
    async fn get(
        &self,
        _tile_row: u32,
        _tile_col: u32,
        _zoom: u8,
        _chunk_row: u8,
        _chunk_col: u8,
    ) -> Option<Vec<u8>> {
        None
    }

    async fn put(
        &self,
        _tile_row: u32,
        _tile_col: u32,
        _zoom: u8,
        _chunk_row: u8,
        _chunk_col: u8,
        _data: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Mock disk cache that returns cached data.
struct MockDiskCache {
    cached: Mutex<HashMap<(u32, u32, u8, u8, u8), Vec<u8>>>,
}

impl MockDiskCache {
    fn new() -> Self {
        Self {
            cached: Mutex::new(HashMap::new()),
        }
    }

    fn with_cached(
        self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> Self {
        self.cached
            .lock()
            .unwrap()
            .insert((tile_row, tile_col, zoom, chunk_row, chunk_col), data);
        self
    }
}

impl DiskCache for MockDiskCache {
    async fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> Option<Vec<u8>> {
        self.cached
            .lock()
            .unwrap()
            .get(&(tile_row, tile_col, zoom, chunk_row, chunk_col))
            .cloned()
    }

    async fn put(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        self.cached
            .lock()
            .unwrap()
            .insert((tile_row, tile_col, zoom, chunk_row, chunk_col), data);
        Ok(())
    }
}

#[tokio::test]
async fn test_download_stage_all_success() {
    let provider = Arc::new(MockProvider::new());
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

    assert_eq!(results.success_count(), 256);
    assert_eq!(results.failure_count(), 0);
    assert!(results.is_complete());
}

#[tokio::test]
async fn test_download_stage_with_failures() {
    // Make one chunk fail permanently
    let provider = Arc::new(MockProvider::new().with_failure(
        100 * 16 + 5,  // chunk (5, 0)
        200 * 16 + 10, // at tile (100, 200)
        20,            // zoom 16 + 4
        ChunkDownloadError::permanent("not found"),
    ));
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig {
        max_retries: 1, // Quick test
        ..Default::default()
    };
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

    assert_eq!(results.success_count(), 255);
    assert_eq!(results.failure_count(), 1);
    assert!(!results.is_complete());
}

#[tokio::test]
async fn test_download_stage_uses_cache() {
    let provider = Arc::new(MockProvider::new());
    let cache =
        Arc::new(MockDiskCache::new().with_cached(100, 200, 16, 0, 0, vec![0xCA, 0xCE, 0xD]));
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    let results = download_stage(JobId::new(), tile, provider, cache, &config, None).await;

    assert_eq!(results.success_count(), 256);

    // Check that chunk (0, 0) has cached data
    let cached_chunk = results.get(0, 0).unwrap();
    assert_eq!(cached_chunk, &[0xCA, 0xCE, 0xD]);
}

#[tokio::test]
async fn test_download_stage_with_limiter() {
    let provider = Arc::new(MockProvider::new());
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    // Create a limiter with only 10 concurrent requests
    let limiter = Arc::new(HttpConcurrencyLimiter::new(10));

    let results = download_stage_with_limiter(
        JobId::new(),
        tile,
        provider,
        cache,
        &config,
        None,
        Some(Arc::clone(&limiter)),
    )
    .await;

    // All 256 chunks should still succeed
    assert_eq!(results.success_count(), 256);
    assert_eq!(results.failure_count(), 0);
    assert!(results.is_complete());

    // Limiter should have tracked peak usage (should be <= 10)
    assert!(limiter.peak_in_flight() <= 10);
    // After completion, no requests should be in flight
    assert_eq!(limiter.in_flight(), 0);
}

#[tokio::test]
async fn test_download_stage_limiter_constrains_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Track maximum concurrent downloads observed
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let current_concurrent = Arc::new(AtomicUsize::new(0));

    /// Mock provider that tracks concurrency
    struct ConcurrencyTrackingProvider {
        current: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
    }

    impl ChunkProvider for ConcurrencyTrackingProvider {
        async fn download_chunk(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            // Increment current count
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;

            // Update max if this is a new peak
            let mut max = self.max.load(Ordering::SeqCst);
            while current > max {
                match self.max.compare_exchange_weak(
                    max,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(m) => max = m,
                }
            }

            // Simulate some async work to allow other tasks to run
            tokio::time::sleep(std::time::Duration::from_micros(100)).await;

            // Decrement current count
            self.current.fetch_sub(1, Ordering::SeqCst);

            Ok(vec![row as u8, col as u8, zoom])
        }

        fn name(&self) -> &str {
            "concurrency-tracker"
        }
    }

    let provider = Arc::new(ConcurrencyTrackingProvider {
        current: Arc::clone(&current_concurrent),
        max: Arc::clone(&max_concurrent),
    });
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    // Limit to 16 concurrent HTTP requests
    let limiter = Arc::new(HttpConcurrencyLimiter::new(16));

    let results = download_stage_with_limiter(
        JobId::new(),
        tile,
        provider,
        cache,
        &config,
        None,
        Some(limiter),
    )
    .await;

    assert_eq!(results.success_count(), 256);

    // Maximum observed concurrency should not exceed limiter's cap
    let observed_max = max_concurrent.load(Ordering::SeqCst);
    assert!(
        observed_max <= 16,
        "Expected max concurrent <= 16, got {}",
        observed_max
    );
}

#[tokio::test]
async fn test_download_stage_bounded_basic() {
    let provider = Arc::new(MockProvider::new());
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    let limiter = Arc::new(HttpConcurrencyLimiter::new(32));
    let token = CancellationToken::new();

    let results = download_stage_bounded(
        JobId::new(),
        tile,
        provider,
        cache,
        &config,
        None,
        Arc::clone(&limiter),
        token,
    )
    .await;

    assert_eq!(results.success_count(), 256);
    assert_eq!(results.failure_count(), 0);
    // After completion, no requests should be in flight
    assert_eq!(limiter.in_flight(), 0);
    // Peak should not exceed limit
    assert!(limiter.peak_in_flight() <= 32);
}

#[tokio::test]
async fn test_download_stage_bounded_in_flight_matches_actual_connections() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Track how many times in_flight matched observed concurrent downloads
    let samples = Arc::new(AtomicUsize::new(0));
    let mismatches = Arc::new(AtomicUsize::new(0));

    /// Mock provider that verifies in_flight count during download
    struct VerifyingProvider {
        limiter: Arc<HttpConcurrencyLimiter>,
        samples: Arc<AtomicUsize>,
        mismatches: Arc<AtomicUsize>,
    }

    impl ChunkProvider for VerifyingProvider {
        async fn download_chunk(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            self.samples.fetch_add(1, Ordering::SeqCst);

            // The in_flight count should be > 0 since we're actively downloading
            let in_flight = self.limiter.in_flight();
            if in_flight == 0 {
                self.mismatches.fetch_add(1, Ordering::SeqCst);
            }

            // Simulate some work
            tokio::time::sleep(std::time::Duration::from_micros(50)).await;

            Ok(vec![row as u8, col as u8, zoom])
        }

        fn name(&self) -> &str {
            "verifying"
        }
    }

    let limiter = Arc::new(HttpConcurrencyLimiter::new(16));
    let provider = Arc::new(VerifyingProvider {
        limiter: Arc::clone(&limiter),
        samples: Arc::clone(&samples),
        mismatches: Arc::clone(&mismatches),
    });
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };
    let token = CancellationToken::new();

    let results = download_stage_bounded(
        JobId::new(),
        tile,
        provider,
        cache,
        &config,
        None,
        Arc::clone(&limiter),
        token,
    )
    .await;

    assert_eq!(results.success_count(), 256);
    assert_eq!(limiter.in_flight(), 0);

    // All downloads should have seen in_flight > 0
    assert_eq!(
        mismatches.load(Ordering::SeqCst),
        0,
        "Some downloads saw in_flight=0 when it should have been > 0"
    );
}

#[tokio::test]
async fn test_download_stage_bounded_cache_hits_bypass_http_permits() {
    // Pre-populate cache with half the chunks (128 of 256)
    let mut cache = MockDiskCache::new();
    for row in 0..8u8 {
        for col in 0..16u8 {
            cache = cache.with_cached(100, 200, 16, row, col, vec![0xCA, 0xCE]);
        }
    }
    // 128 chunks cached (rows 0-7), 128 need download (rows 8-15)
    let cache = Arc::new(cache);

    let provider = Arc::new(MockProvider::new());
    let config = PipelineConfig::default();
    let tile = TileCoord {
        row: 100,
        col: 200,
        zoom: 16,
    };

    // Use a small HTTP limit - if cache hits counted against it, we'd be slow
    let limiter = Arc::new(HttpConcurrencyLimiter::new(16));
    let token = CancellationToken::new();

    let results = download_stage_bounded(
        JobId::new(),
        tile,
        provider,
        cache,
        &config,
        None,
        Arc::clone(&limiter),
        token,
    )
    .await;

    assert_eq!(results.success_count(), 256);
    assert_eq!(results.failure_count(), 0);
    // Peak should be ≤16 since only 128 chunks needed HTTP permits
    assert!(
        limiter.peak_in_flight() <= 16,
        "Peak {} exceeded limit 16",
        limiter.peak_in_flight()
    );
}

#[tokio::test]
async fn test_download_stage_bounded_multiple_tiles_share_limit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let current_concurrent = Arc::new(AtomicUsize::new(0));

    /// Mock provider that tracks concurrency across tiles
    struct MultiTileProvider {
        current: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
    }

    impl ChunkProvider for MultiTileProvider {
        async fn download_chunk(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;

            let mut max = self.max.load(Ordering::SeqCst);
            while current > max {
                match self.max.compare_exchange_weak(
                    max,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(m) => max = m,
                }
            }

            tokio::time::sleep(std::time::Duration::from_micros(100)).await;

            self.current.fetch_sub(1, Ordering::SeqCst);

            Ok(vec![row as u8, col as u8, zoom])
        }

        fn name(&self) -> &str {
            "multi-tile"
        }
    }

    let provider = Arc::new(MultiTileProvider {
        current: Arc::clone(&current_concurrent),
        max: Arc::clone(&max_concurrent),
    });
    let cache = Arc::new(NullDiskCache);
    let config = PipelineConfig::default();
    let limiter = Arc::new(HttpConcurrencyLimiter::new(32));

    // Spawn 4 tiles concurrently, each with 256 chunks = 1024 total downloads
    // But should never exceed 32 concurrent
    let mut handles = Vec::new();
    for i in 0..4 {
        let provider = Arc::clone(&provider);
        let cache = Arc::clone(&cache);
        let config = config.clone();
        let limiter = Arc::clone(&limiter);

        handles.push(tokio::spawn(async move {
            let tile = TileCoord {
                row: 100 + i,
                col: 200,
                zoom: 16,
            };
            let token = CancellationToken::new();

            download_stage_bounded(
                JobId::new(),
                tile,
                provider,
                cache,
                &config,
                None,
                limiter,
                token,
            )
            .await
        }));
    }

    // Wait for all to complete
    let mut total_success = 0;
    for handle in handles {
        let results = handle.await.unwrap();
        total_success += results.success_count();
    }

    assert_eq!(total_success, 256 * 4);

    // Maximum observed concurrency should not exceed limiter's cap
    let observed_max = max_concurrent.load(Ordering::SeqCst);
    assert!(
        observed_max <= 32,
        "Expected max concurrent <= 32, got {} (4 tiles × 256 chunks = 1024 potential)",
        observed_max
    );

    // After completion, no requests should be in flight
    assert_eq!(limiter.in_flight(), 0);
}
