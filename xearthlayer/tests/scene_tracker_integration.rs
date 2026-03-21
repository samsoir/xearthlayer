//! Integration tests for the Scene Tracker.
//!
//! These tests verify the complete Scene Tracker flow including:
//! - FUSE event → Scene Tracker → loaded regions
//! - Burst detection with realistic timing
//! - Event subscriptions and broadcasting
//!
//! Run with: `cargo test --test scene_tracker_integration`

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use xearthlayer::scene_tracker::{
    DdsTileCoord, DefaultSceneTracker, FuseAccessEvent, SceneTracker,
};

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a FUSE access event for a tile at the given coordinates.
fn make_event(row: u32, col: u32, zoom: u8) -> FuseAccessEvent {
    let tile = DdsTileCoord::new(row, col, zoom);
    FuseAccessEvent::new(tile)
}

/// Create a tile at the given coordinates (ZL18 default).
fn make_tile(row: u32, col: u32) -> DdsTileCoord {
    DdsTileCoord::new(row, col, 18)
}

/// Tile coordinates in the Hamburg area (ZL18).
/// These are typical coordinates X-Plane would request.
const HAMBURG_TILES: &[(u32, u32)] = &[
    (83776, 138240),
    (83777, 138240),
    (83776, 138241),
    (83777, 138241),
    (83778, 138240),
    (83778, 138241),
    (83779, 138240),
    (83779, 138241),
];

/// Tile coordinates in the London area (ZL18).
const LONDON_TILES: &[(u32, u32)] = &[
    (86016, 131072),
    (86017, 131072),
    (86016, 131073),
    (86017, 131073),
];

// ============================================================================
// Integration Tests
// ============================================================================

/// Test that FUSE events flow through to Scene Tracker and update loaded regions.
///
/// This simulates the complete pipeline:
/// 1. FUSE receives DDS read request
/// 2. FUSE sends event via channel
/// 3. Scene Tracker receives event
/// 4. Scene Tracker updates its internal state
/// 5. Queries return updated data
#[tokio::test]
async fn test_fuse_to_scene_tracker_flow() {
    // Create Scene Tracker with channel
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    // Start the Scene Tracker processing loop
    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Simulate FUSE sending events (fire-and-forget pattern)
    for (row, col) in HAMBURG_TILES {
        let event = make_event(*row, *col, 18);
        tx.send(event).expect("Channel should not be closed");
    }

    // Give time for async processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify tiles were tracked
    assert_eq!(
        tracker.total_requests(),
        HAMBURG_TILES.len() as u64,
        "All tile requests should be counted"
    );

    // Verify specific tiles are marked as requested
    for (row, col) in HAMBURG_TILES {
        assert!(
            tracker.is_tile_requested(&make_tile(*row, *col)),
            "Tile ({}, {}) should be marked as requested",
            row,
            col
        );
    }

    // Verify loaded regions derived from tiles
    let regions = tracker.loaded_regions();
    assert!(
        !regions.is_empty(),
        "Should have at least one loaded region"
    );

    // All Hamburg tiles should map to nearby regions
    // (they're all in the same general area)
    let hamburg_region = make_tile(83776, 138240).to_geo_region();
    assert!(
        tracker.is_region_loaded(&hamburg_region),
        "Hamburg region should be loaded"
    );

    // Clean shutdown
    drop(tx);
    handle
        .await
        .expect("Scene tracker task should complete cleanly");
}

/// Test tile access subscription for real-time tracking.
#[tokio::test]
async fn test_tile_access_subscription() {
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    // Subscribe to tile access events
    let mut tile_rx = tracker.subscribe_tile_access();

    // Start the Scene Tracker
    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Send a single tile event
    let test_tile = make_tile(83776, 138240);
    tx.send(FuseAccessEvent::new(test_tile)).unwrap();

    // Should receive the tile access event
    match tokio::time::timeout(Duration::from_millis(100), tile_rx.recv()).await {
        Ok(Ok(received)) => {
            assert_eq!(received, test_tile, "Should receive the exact tile");
        }
        Ok(Err(_)) => panic!("Tile receiver was closed"),
        Err(_) => panic!("Timeout waiting for tile event"),
    }

    // Clean shutdown
    drop(tx);
    handle.await.unwrap();
}

/// Test geographic bounds calculation from scattered tiles.
#[tokio::test]
async fn test_loaded_bounds_multiple_regions() {
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Send tiles from Hamburg
    for (row, col) in &HAMBURG_TILES[..2] {
        tx.send(make_event(*row, *col, 18)).unwrap();
    }

    // Send tiles from London
    for (row, col) in &LONDON_TILES[..2] {
        tx.send(make_event(*row, *col, 18)).unwrap();
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify bounds encompass both areas
    let bounds = tracker.loaded_bounds().expect("Should have bounds");

    // Get lat/lon for verification
    let hamburg_tile = make_tile(83776, 138240);
    let london_tile = make_tile(86016, 131072);
    let (hamburg_lat, hamburg_lon) = hamburg_tile.to_lat_lon();
    let (london_lat, london_lon) = london_tile.to_lat_lon();

    // Bounds should encompass both cities
    assert!(
        bounds.min_lat <= hamburg_lat.min(london_lat),
        "Min lat should include both cities"
    );
    assert!(
        bounds.max_lat >= hamburg_lat.max(london_lat),
        "Max lat should include both cities"
    );
    assert!(
        bounds.min_lon <= hamburg_lon.min(london_lon),
        "Min lon should include both cities"
    );
    assert!(
        bounds.max_lon >= hamburg_lon.max(london_lon),
        "Max lon should include both cities"
    );

    // Clean shutdown
    drop(tx);
    handle.await.unwrap();
}

/// Test that Scene Tracker handles high-volume tile requests without blocking FUSE.
///
/// The fire-and-forget channel pattern should allow FUSE to continue
/// processing requests even if Scene Tracker is slow.
#[tokio::test]
async fn test_high_volume_non_blocking() {
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Simulate rapid-fire tile requests (1000 tiles)
    let start = Instant::now();
    for i in 0..1000u32 {
        let row = 83776 + (i / 100);
        let col = 138240 + (i % 100);
        tx.send(make_event(row, col, 18)).unwrap();
    }
    let send_time = start.elapsed();

    // Sending should be nearly instant (unbounded channel)
    assert!(
        send_time < Duration::from_millis(50),
        "Sending 1000 events should be fast, took {:?}",
        send_time
    );

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(100)).await;

    // All events should be processed
    assert_eq!(
        tracker.total_requests(),
        1000,
        "All 1000 requests should be processed"
    );

    // Clean shutdown
    drop(tx);
    handle.await.unwrap();
}

/// Test that duplicate tile requests are still tracked.
///
/// Scene Tracker stores the set of *unique* tiles, but counts *all* requests.
#[tokio::test]
async fn test_duplicate_tile_handling() {
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Send the same tile 10 times
    let test_tile = make_tile(83776, 138240);
    for _ in 0..10 {
        tx.send(FuseAccessEvent::new(test_tile)).unwrap();
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Total requests should count all 10
    assert_eq!(tracker.total_requests(), 10, "Should count all 10 requests");

    // But unique tiles should be 1
    assert_eq!(
        tracker.requested_tiles().len(),
        1,
        "Should have only 1 unique tile"
    );

    // Clean shutdown
    drop(tx);
    handle.await.unwrap();
}

/// Test channel closure behavior.
///
/// When FUSE shuts down and closes the channel, Scene Tracker should
/// gracefully stop processing.
#[tokio::test]
async fn test_graceful_shutdown_on_channel_close() {
    let tracker = Arc::new(DefaultSceneTracker::with_defaults());
    let (tx, rx) = mpsc::unbounded_channel();

    let tracker_clone = Arc::clone(&tracker);
    let handle = tracker_clone.start(rx);

    // Send some events
    tx.send(make_event(83776, 138240, 18)).unwrap();
    tx.send(make_event(83777, 138241, 18)).unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Close the channel (simulates FUSE shutdown)
    drop(tx);

    // Scene Tracker should complete without panicking
    let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
    assert!(
        result.is_ok(),
        "Scene Tracker should complete within timeout"
    );
    assert!(
        result.unwrap().is_ok(),
        "Scene Tracker task should not panic"
    );

    // Data should still be accessible after shutdown
    assert_eq!(tracker.total_requests(), 2);
}
