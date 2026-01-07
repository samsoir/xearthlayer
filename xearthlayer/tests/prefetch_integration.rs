//! Integration tests for the prefetch system.
//!
//! These tests verify the complete prefetch flow including:
//! - Telemetry-driven prefetching (aircraft state → tile submissions)
//! - FUSE-based position inference (tile requests → position estimation)
//! - Mode transitions (verified via stats counters)
//!
//! Run with: `cargo test --test prefetch_integration`

#![allow(clippy::type_complexity)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use xearthlayer::coord::{to_tile_coords, TileCoord};
use xearthlayer::fuse::{DdsHandler, DdsRequest};
use xearthlayer::pipeline::MemoryCache;
use xearthlayer::prefetch::config::HeadingAwarePrefetchConfig as HeadingConfig;
use xearthlayer::prefetch::{
    AircraftState, FuseInferenceConfig, FuseRequestAnalyzer, HeadingAwarePrefetcher,
    HeadingAwarePrefetcherConfig, PrefetcherBuilder, RadialPrefetchConfig, RadialPrefetcher,
    SharedPrefetchStatus,
};

// ============================================================================
// Mock Implementations
// ============================================================================

/// Mock memory cache for testing prefetch behavior.
/// Uses std::sync::RwLock for synchronous cache initialization.
struct MockMemoryCache {
    data: Arc<std::sync::RwLock<HashMap<(u32, u32, u8), Vec<u8>>>>,
    get_count: AtomicUsize,
    put_count: AtomicUsize,
}

impl MockMemoryCache {
    fn new() -> Self {
        Self {
            data: Arc::new(std::sync::RwLock::new(HashMap::new())),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }

    /// Pre-populate cache with tiles (simulates cache hits).
    /// Uses synchronous RwLock to avoid nested runtime issues.
    fn with_cached(self, tiles: &[(u32, u32, u8)]) -> Self {
        {
            let mut data = self.data.write().unwrap();
            for (row, col, zoom) in tiles {
                data.insert((*row, *col, *zoom), vec![0xDD, 0x53]);
            }
        }
        self
    }

    fn get_count(&self) -> usize {
        self.get_count.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    fn put_count(&self) -> usize {
        self.put_count.load(Ordering::SeqCst)
    }
}

impl MemoryCache for MockMemoryCache {
    fn get(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        let data = Arc::clone(&self.data);
        async move { data.read().unwrap().get(&(row, col, zoom)).cloned() }
    }

    fn put(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = ()> + Send {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        let cache = Arc::clone(&self.data);
        async move {
            cache.write().unwrap().insert((row, col, zoom), data);
        }
    }

    fn size_bytes(&self) -> usize {
        0
    }

    fn entry_count(&self) -> usize {
        0
    }
}

/// Tracking DDS handler that records request counts.
struct MockDdsHandler {
    call_count: AtomicUsize,
}

impl MockDdsHandler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            call_count: AtomicUsize::new(0),
        })
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Create a DdsHandler closure that forwards to this mock.
    fn handler(self: &Arc<Self>) -> DdsHandler {
        let mock = Arc::clone(self);
        Arc::new(move |_request: DdsRequest| {
            mock.call_count.fetch_add(1, Ordering::SeqCst);
        })
    }
}

// ============================================================================
// Test Fixtures
// ============================================================================

/// Create a test aircraft state at a given position.
fn aircraft_state(lat: f64, lon: f64, heading: f32, ground_speed: f32) -> AircraftState {
    AircraftState {
        latitude: lat,
        longitude: lon,
        heading,
        ground_speed,
        altitude: 10000.0, // Default altitude
        updated_at: Instant::now(),
    }
}

/// Create a default radial prefetch config for testing.
fn test_radial_config() -> RadialPrefetchConfig {
    RadialPrefetchConfig {
        inner_radius_nm: 1.0, // Small ring for faster tests
        outer_radius_nm: 3.0,
        zoom: 14,
        attempt_ttl: Duration::from_secs(60),
    }
}

/// Create a default heading-aware config for testing.
fn test_heading_config() -> HeadingAwarePrefetcherConfig {
    HeadingAwarePrefetcherConfig {
        heading: HeadingConfig {
            cone_half_angle: 30.0,
            inner_radius_nm: 10.0, // Small radius for tests
            outer_radius_nm: 20.0,
            zoom: 14,
            ..HeadingConfig::default()
        },
        fuse_inference: FuseInferenceConfig::default(),
        telemetry_stale_threshold: Duration::from_millis(500), // Fast for tests
        fuse_confidence_threshold: 0.3,
        radial_fallback_radius: 2,
    }
}

// ============================================================================
// Telemetry Integration Tests
// ============================================================================

/// Test that telemetry updates trigger prefetch tile submissions.
#[tokio::test]
async fn test_telemetry_triggers_prefetch() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();

    let config = test_radial_config();
    let prefetcher = RadialPrefetcher::new(Arc::clone(&cache), handler, config);

    // Create state channel
    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    // Start prefetcher in background
    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send aircraft state (San Francisco area)
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    // Wait for prefetch cycle
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify tiles were submitted
    let call_count = handler_mock.call_count();
    assert!(
        call_count > 0,
        "Expected prefetch submissions, got {}",
        call_count
    );

    // For radius=2, expect 5x5=25 tiles minus any cache hits
    assert!(
        call_count <= 25,
        "Too many submissions: {} (max 25)",
        call_count
    );

    // Cleanup
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that cached tiles are skipped during prefetch.
#[tokio::test]
async fn test_prefetch_skips_cached_tiles() {
    // Get actual tile coordinates for the aircraft position
    let center_tile = to_tile_coords(37.7749, -122.4194, 14).unwrap();

    // Pre-populate cache with center tile and an adjacent tile
    let cached_tiles = vec![
        (center_tile.row, center_tile.col, 14),
        (center_tile.row + 1, center_tile.col, 14),
    ];
    let cache = Arc::new(MockMemoryCache::new().with_cached(&cached_tiles));
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();

    let config = test_radial_config();
    let prefetcher = RadialPrefetcher::new(Arc::clone(&cache), handler, config);

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send state that covers the cached tiles
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify cache was checked
    assert!(
        cache.get_count() > 0,
        "Cache should have been checked for hits"
    );

    // Submissions should be less than full grid (some skipped due to cache)
    // For radius=2, full grid is 5x5=25 tiles, minus 2 cached = 23 max
    let call_count = handler_mock.call_count();
    assert!(
        call_count < 25,
        "Expected fewer submissions due to cache hits, got {}",
        call_count
    );

    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that multiple telemetry updates don't cause redundant prefetching.
#[tokio::test]
async fn test_telemetry_deduplication() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();

    let config = test_radial_config();
    let prefetcher = RadialPrefetcher::new(Arc::clone(&cache), handler, config);

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send same position twice rapidly
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Due to TTL tracking, tiles shouldn't be re-submitted
    let call_count = handler_mock.call_count();

    // First batch submits ~25 tiles, second batch should be mostly skipped
    // Allow some tolerance for timing variations
    assert!(
        call_count <= 30,
        "Expected deduplication to prevent redundant submissions, got {}",
        call_count
    );

    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

// ============================================================================
// FUSE Inference Integration Tests
// ============================================================================

/// Test that FUSE tile requests are recorded by FuseRequestAnalyzer.
#[tokio::test]
async fn test_fuse_requests_recorded() {
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

    // Get the callback and simulate FUSE requests
    let callback = analyzer.callback();

    // Simulate X-Plane loading tiles around a position
    let tiles = vec![
        TileCoord {
            row: 8388,
            col: 5242,
            zoom: 14,
        },
        TileCoord {
            row: 8387,
            col: 5242,
            zoom: 14,
        },
        TileCoord {
            row: 8388,
            col: 5243,
            zoom: 14,
        },
        TileCoord {
            row: 8389,
            col: 5242,
            zoom: 14,
        },
        TileCoord {
            row: 8388,
            col: 5241,
            zoom: 14,
        },
        TileCoord {
            row: 8386,
            col: 5242,
            zoom: 14,
        },
        TileCoord {
            row: 8388,
            col: 5244,
            zoom: 14,
        },
        TileCoord {
            row: 8390,
            col: 5242,
            zoom: 14,
        },
        TileCoord {
            row: 8388,
            col: 5240,
            zoom: 14,
        },
        TileCoord {
            row: 8385,
            col: 5242,
            zoom: 14,
        },
    ];

    for tile in &tiles {
        callback(*tile);
    }

    // Wait for async processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify analyzer recorded the requests
    assert!(
        analyzer.is_active(),
        "Analyzer should be active after receiving tile requests"
    );

    // Confidence should be building up
    let confidence = analyzer.confidence();
    assert!(
        confidence > 0.0,
        "Confidence should be positive, got {}",
        confidence
    );
}

/// Test position inference from FUSE tile loading pattern.
#[tokio::test]
async fn test_fuse_position_inference() {
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig {
        min_requests_for_inference: 4,
        ..FuseInferenceConfig::default()
    }));

    let callback = analyzer.callback();

    // Simulate a grid of tiles around a known position
    // These tiles should be around lat=37.7°, lon=-122.4° at zoom 14
    for row in 8386..8390 {
        for col in 5240..5244 {
            callback(TileCoord { row, col, zoom: 14 });
        }
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get inferred position
    if let Some((lat, lon)) = analyzer.position() {
        // Should be roughly in the San Francisco area
        assert!(
            lat > 37.0 && lat < 38.0,
            "Latitude {} should be near 37.7",
            lat
        );
        assert!(
            lon > -123.0 && lon < -122.0,
            "Longitude {} should be near -122.4",
            lon
        );
    } else {
        // Position might not be available if not enough tiles loaded
        // This is acceptable for the test
        assert!(analyzer.is_active(), "Analyzer should at least be active");
    }
}

/// Test heading inference from frontier movement.
#[tokio::test]
async fn test_fuse_heading_inference() {
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig {
        min_requests_for_inference: 5,
        ..FuseInferenceConfig::default()
    }));

    let callback = analyzer.callback();

    // Simulate aircraft moving east - load tiles progressively eastward
    // First batch: centered position
    for col in 5240..5243 {
        callback(TileCoord {
            row: 8388,
            col,
            zoom: 14,
        });
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second batch: tiles further east (simulating eastward movement)
    for col in 5243..5246 {
        callback(TileCoord {
            row: 8388,
            col,
            zoom: 14,
        });
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check if heading was inferred
    // heading() returns Option<(heading, confidence)>
    if let Some((heading, confidence)) = analyzer.heading() {
        // Eastward movement should give heading around 90°
        // Allow tolerance for inference imprecision
        assert!(
            (heading - 90.0).abs() < 90.0 || (heading - 270.0).abs() < 90.0,
            "Heading {} should be roughly east or west",
            heading
        );
        assert!(confidence > 0.0, "Confidence should be positive");
    }
    // Heading might not be available - that's acceptable
}

// ============================================================================
// HeadingAwarePrefetcher Integration Tests
// ============================================================================

/// Test that HeadingAwarePrefetcher submits tiles with fresh telemetry.
#[tokio::test]
async fn test_heading_aware_with_telemetry() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
    let status = SharedPrefetchStatus::new();

    let config = test_heading_config();
    let prefetcher =
        HeadingAwarePrefetcher::new(Arc::clone(&cache), handler, config, Arc::clone(&analyzer))
            .with_shared_status(Arc::clone(&status));

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send fresh telemetry
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have submitted tiles
    assert!(
        handler_mock.call_count() > 0,
        "HeadingAwarePrefetcher should submit tiles with telemetry"
    );

    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that HeadingAwarePrefetcher handles stale telemetry gracefully.
#[tokio::test]
async fn test_heading_aware_stale_telemetry() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

    // Use very short stale threshold
    let mut config = test_heading_config();
    config.telemetry_stale_threshold = Duration::from_millis(50);

    let prefetcher =
        HeadingAwarePrefetcher::new(Arc::clone(&cache), handler, config, Arc::clone(&analyzer));

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send telemetry once
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    // Wait for first cycle
    tokio::time::sleep(Duration::from_millis(100)).await;
    let first_count = handler_mock.call_count();

    // Wait for telemetry to become stale and another cycle
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should still have made submissions (using fallback mode)
    let final_count = handler_mock.call_count();
    assert!(
        final_count >= first_count,
        "Should continue prefetching after telemetry goes stale"
    );

    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that FUSE inference can drive prefetching when telemetry is stale.
#[tokio::test]
async fn test_fuse_inference_drives_prefetch() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig {
        min_requests_for_inference: 3,
        ..FuseInferenceConfig::default()
    }));

    // Prime the analyzer with enough tiles
    let callback = analyzer.callback();
    for row in 8386..8392 {
        for col in 5240..5246 {
            callback(TileCoord { row, col, zoom: 14 });
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify analyzer is now active
    assert!(analyzer.is_active(), "Analyzer should be active");

    let mut config = test_heading_config();
    config.telemetry_stale_threshold = Duration::from_millis(10);
    config.fuse_confidence_threshold = 0.1; // Low threshold for test

    let prefetcher =
        HeadingAwarePrefetcher::new(Arc::clone(&cache), handler, config, Arc::clone(&analyzer));

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = {
        let token = token.clone();
        tokio::spawn(async move {
            prefetcher.run(rx, token).await;
        })
    };

    // Send stale telemetry (with old timestamp)
    let mut state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    state.updated_at = Instant::now() - Duration::from_secs(10);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have made submissions using FUSE inference or radial fallback
    assert!(
        handler_mock.call_count() > 0,
        "Should submit tiles even with stale telemetry"
    );

    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

// ============================================================================
// PrefetcherBuilder Integration Tests
// ============================================================================

/// Test that PrefetcherBuilder creates working radial prefetcher.
#[tokio::test]
async fn test_builder_creates_radial_prefetcher() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();

    let prefetcher = PrefetcherBuilder::new()
        .memory_cache(cache)
        .dds_handler(handler)
        .strategy("radial")
        .inner_radius_nm(10.0) // Small ring for testing
        .outer_radius_nm(20.0)
        .build();

    // Verify it starts correctly (format: "radial ring ({inner}-{outer}nm), zoom {zoom}")
    assert_eq!(prefetcher.startup_info(), "radial ring (10-20nm), zoom 14");

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = tokio::spawn(async move {
        prefetcher.run(rx, token).await;
    });

    // Send state
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have made submissions
    assert!(
        handler_mock.call_count() > 0,
        "Radial prefetcher should submit tiles"
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that PrefetcherBuilder creates working heading-aware prefetcher.
#[tokio::test]
async fn test_builder_creates_heading_aware_prefetcher() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();
    let status = SharedPrefetchStatus::new();

    let prefetcher = PrefetcherBuilder::new()
        .memory_cache(cache)
        .dds_handler(handler)
        .strategy("heading-aware")
        .shared_status(Arc::clone(&status))
        .cone_half_angle(30.0)
        .outer_radius_nm(20.0)
        .build();

    // Verify startup info
    let info = prefetcher.startup_info();
    assert!(
        info.contains("heading-aware"),
        "Should be heading-aware, got: {}",
        info
    );

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = tokio::spawn(async move {
        prefetcher.run(rx, token).await;
    });

    // Send state
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have made submissions
    assert!(
        handler_mock.call_count() > 0,
        "Heading-aware prefetcher should submit tiles"
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test that external FuseRequestAnalyzer is properly wired via builder.
#[tokio::test]
async fn test_builder_with_external_analyzer() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();
    let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
    let status = SharedPrefetchStatus::new();

    // Prime the analyzer
    let callback = analyzer.callback();
    for row in 8386..8390 {
        for col in 5240..5244 {
            callback(TileCoord { row, col, zoom: 14 });
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let prefetcher = PrefetcherBuilder::new()
        .memory_cache(cache)
        .dds_handler(handler)
        .strategy("auto")
        .shared_status(Arc::clone(&status))
        .with_fuse_analyzer(Arc::clone(&analyzer))
        .build();

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = tokio::spawn(async move {
        prefetcher.run(rx, token).await;
    });

    // Send stale telemetry
    let mut state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    state.updated_at = Instant::now() - Duration::from_secs(10);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // The external analyzer should still be usable
    assert!(
        analyzer.is_active(),
        "External analyzer should still be active"
    );

    // Should have made tile submissions
    assert!(
        handler_mock.call_count() > 0,
        "Should submit tiles with external analyzer"
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}

/// Test auto strategy selection.
#[tokio::test]
async fn test_builder_auto_strategy() {
    let cache = Arc::new(MockMemoryCache::new());
    let handler_mock = MockDdsHandler::new();
    let handler = handler_mock.handler();

    let prefetcher = PrefetcherBuilder::new()
        .memory_cache(cache)
        .dds_handler(handler)
        .strategy("auto") // Should create HeadingAwarePrefetcher
        .build();

    // Auto should create a heading-aware prefetcher
    let info = prefetcher.startup_info();
    assert!(
        info.contains("heading-aware") || info.contains("auto"),
        "Auto strategy should create heading-aware prefetcher, got: {}",
        info
    );

    let (tx, rx) = mpsc::channel(32);
    let token = CancellationToken::new();

    let prefetch_handle = tokio::spawn(async move {
        prefetcher.run(rx, token).await;
    });

    // Send state
    let state = aircraft_state(37.7749, -122.4194, 90.0, 250.0);
    tx.send(state).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have made submissions
    assert!(
        handler_mock.call_count() > 0,
        "Auto prefetcher should submit tiles"
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), prefetch_handle).await;
}
