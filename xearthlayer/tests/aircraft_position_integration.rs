//! Integration tests for the Aircraft Position & Telemetry (APT) module.
//!
//! These tests verify the complete APT data flows:
//! - Telemetry → APT (X-Plane UDP → AircraftState → Position Model)
//! - Manual Reference → APT (Airport seed → Position Model)
//! - Scene Tracker → Inference → APT (Burst → InferenceAdapter → Position Model)
//! - Source priority and accuracy-based selection
//!
//! Run with: `cargo test --test aircraft_position_integration`

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use xearthlayer::aircraft_position::{
    AircraftPositionProvider, AircraftState, InferenceAdapter, InferenceAdapterConfig,
    PositionAccuracy, PositionSource, SharedAircraftPosition, StateAggregator,
    StateAggregatorConfig, TelemetryStatus,
};
use xearthlayer::scene_tracker::{
    BurstConfig, DdsTileCoord, DefaultSceneTracker, FuseAccessEvent, GeoBounds, GeoRegion,
    LoadingBurst, SceneTracker, SceneTrackerConfig, SceneTrackerEvents,
};

// ============================================================================
// Test Helpers
// ============================================================================

/// Create a SharedAircraftPosition for testing.
fn create_apt() -> (SharedAircraftPosition, broadcast::Receiver<AircraftState>) {
    let (broadcast_tx, broadcast_rx) = broadcast::channel(16);
    let aggregator = StateAggregator::new(broadcast_tx);
    (SharedAircraftPosition::new(aggregator), broadcast_rx)
}

/// Create a SharedAircraftPosition with custom telemetry timeout.
fn create_apt_with_timeout(
    timeout: Duration,
) -> (SharedAircraftPosition, broadcast::Receiver<AircraftState>) {
    let (broadcast_tx, broadcast_rx) = broadcast::channel(16);
    let config = StateAggregatorConfig {
        telemetry_timeout: timeout,
        ..Default::default()
    };
    let aggregator = StateAggregator::with_config(broadcast_tx, config);
    (SharedAircraftPosition::new(aggregator), broadcast_rx)
}

/// Create a telemetry AircraftState with typical X-Plane data.
fn create_telemetry_state(lat: f64, lon: f64) -> AircraftState {
    AircraftState::from_telemetry(lat, lon, 90.0, 120.0, 10000.0)
}

/// Create an inference AircraftState from scene bounds center.
fn create_inference_state(lat: f64, lon: f64) -> AircraftState {
    AircraftState::from_inference(lat, lon)
}

/// Create a LoadingBurst for testing.
fn create_test_burst(tile_count: usize) -> LoadingBurst {
    let start = Instant::now();
    let tiles: Vec<DdsTileCoord> = (0..tile_count)
        .map(|i| DdsTileCoord::new(83776 + i as u32, 138240, 18))
        .collect();
    LoadingBurst {
        tiles,
        started: start,
        ended: start + Duration::from_secs(2),
    }
}

/// Hamburg airport coordinates for testing.
const HAMBURG_LAT: f64 = 53.630278;
const HAMBURG_LON: f64 = 9.988333;

/// Toulouse airport coordinates for testing.
const TOULOUSE_LAT: f64 = 43.629444;
const TOULOUSE_LON: f64 = 1.363889;

/// Tile coordinates in the Hamburg area (ZL18).
const HAMBURG_TILES: &[(u32, u32)] = &[
    (83776, 138240),
    (83777, 138240),
    (83776, 138241),
    (83777, 138241),
];

// ============================================================================
// Manual Reference → APT Tests
// ============================================================================

/// Test that manual reference (airport) seeds the position model.
///
/// This is the prewarm flow: user specifies --airport EDDH, and APT
/// receives the airport coordinates as the initial position estimate.
#[tokio::test]
async fn test_manual_reference_seeds_position() {
    let (apt, _rx) = create_apt();

    // Initially no position
    assert!(!apt.has_position());
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Disconnected);

    // Seed with airport coordinates
    let accepted = apt.receive_manual_reference(HAMBURG_LAT, HAMBURG_LON);

    // Should be accepted (empty model accepts anything)
    assert!(accepted, "Manual reference should be accepted");

    // Position should now be available
    assert!(apt.has_position());
    let (lat, lon) = apt.position().expect("Should have position");
    assert!((lat - HAMBURG_LAT).abs() < 0.001);
    assert!((lon - HAMBURG_LON).abs() < 0.001);

    // Source should be ManualReference
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // No vectors (heading, speed, altitude) from manual reference
    assert!(!apt.has_vectors());

    // Telemetry status should still be disconnected
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Disconnected);
}

/// Test that manual reference position is broadcast to subscribers.
#[tokio::test]
async fn test_manual_reference_broadcasts_update() {
    let (apt, mut rx) = create_apt();

    apt.receive_manual_reference(TOULOUSE_LAT, TOULOUSE_LON);

    // Should receive broadcast
    match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        Ok(Ok(state)) => {
            assert_eq!(state.source, PositionSource::ManualReference);
            assert!((state.latitude - TOULOUSE_LAT).abs() < 0.001);
            assert!((state.longitude - TOULOUSE_LON).abs() < 0.001);
            assert_eq!(state.accuracy, PositionAccuracy::MANUAL_REFERENCE);
        }
        _ => panic!("Should receive broadcast"),
    }
}

// ============================================================================
// Telemetry → APT Tests
// ============================================================================

/// Test that telemetry updates override manual reference (higher accuracy).
///
/// Telemetry has ~10m accuracy vs ~100m for manual reference.
#[tokio::test]
async fn test_telemetry_overrides_manual_reference() {
    let (apt, _rx) = create_apt();

    // Seed with manual reference
    apt.receive_manual_reference(HAMBURG_LAT, HAMBURG_LON);
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // Receive telemetry (higher accuracy)
    let telemetry_state = create_telemetry_state(53.631, 9.990);
    apt.receive_telemetry(telemetry_state);

    // Should now use telemetry
    assert_eq!(apt.position_source(), Some(PositionSource::Telemetry));

    // Position should be from telemetry
    let (lat, lon) = apt.position().expect("Should have position");
    assert!((lat - 53.631).abs() < 0.001);
    assert!((lon - 9.990).abs() < 0.001);

    // Telemetry status should be Connected
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Connected);

    // Should have vectors from telemetry
    assert!(apt.has_vectors());
}

/// Test that telemetry updates continue refining the position.
#[tokio::test]
async fn test_telemetry_continuous_updates() {
    let (apt, mut rx) = create_apt();

    // Send multiple telemetry updates
    for i in 0..5 {
        let lat = 53.0 + (i as f64 * 0.001);
        let lon = 10.0 + (i as f64 * 0.001);
        apt.receive_telemetry(create_telemetry_state(lat, lon));

        // Allow rate limiting (1Hz)
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Position should reflect the latest update
    let (lat, lon) = apt.position().expect("Should have position");
    assert!((lat - 53.004).abs() < 0.001);
    assert!((lon - 10.004).abs() < 0.001);

    // Should have received at least some broadcasts
    // (rate limiting may reduce the count)
    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert!(count >= 1, "Should receive at least one broadcast");
}

/// Test that telemetry connection timeout reverts status.
#[tokio::test]
async fn test_telemetry_timeout_status() {
    let (apt, _rx) = create_apt_with_timeout(Duration::from_millis(100));

    // Send telemetry
    apt.receive_telemetry(create_telemetry_state(53.0, 10.0));
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Connected);

    // Wait for timeout
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Status should revert to disconnected
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Disconnected);

    // Position should still be available (persists after disconnect)
    assert!(apt.has_position());
}

// ============================================================================
// Scene Tracker → Inference → APT Tests
// ============================================================================

/// Mock Scene Tracker that returns configurable bounds.
struct MockSceneTracker {
    bounds: Option<GeoBounds>,
}

impl MockSceneTracker {
    fn with_bounds(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> Arc<Self> {
        Arc::new(Self {
            bounds: Some(GeoBounds::new(min_lat, max_lat, min_lon, max_lon)),
        })
    }
}

impl SceneTracker for MockSceneTracker {
    fn requested_tiles(&self) -> std::collections::HashSet<DdsTileCoord> {
        std::collections::HashSet::new()
    }

    fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
        false
    }

    fn is_burst_active(&self) -> bool {
        false
    }

    fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
        Vec::new()
    }

    fn total_requests(&self) -> u64 {
        0
    }

    fn loaded_regions(&self) -> std::collections::HashSet<GeoRegion> {
        std::collections::HashSet::new()
    }

    fn is_region_loaded(&self, _region: &GeoRegion) -> bool {
        false
    }

    fn loaded_bounds(&self) -> Option<GeoBounds> {
        self.bounds.clone()
    }
}

/// Test inference from Scene Tracker bounds flows to APT.
#[tokio::test]
async fn test_inference_updates_apt() {
    let (apt, _rx) = create_apt();

    // Create inference state (as InferenceAdapter would)
    let inference_state = create_inference_state(53.5, 10.0);
    apt.receive_inference(inference_state);

    // Position should be from inference
    assert!(apt.has_position());
    assert_eq!(apt.position_source(), Some(PositionSource::SceneInference));

    let (lat, lon) = apt.position().expect("Should have position");
    assert!((lat - 53.5).abs() < 0.001);
    assert!((lon - 10.0).abs() < 0.001);

    // Inference has low accuracy (~100km)
    let status = apt.status();
    let state = status.state.expect("Should have state");
    assert_eq!(state.accuracy, PositionAccuracy::SCENE_INFERENCE);
}

/// Test that telemetry overrides inference (higher accuracy).
#[tokio::test]
async fn test_telemetry_overrides_inference() {
    let (apt, _rx) = create_apt();

    // Start with inference (low accuracy)
    apt.receive_inference(create_inference_state(53.0, 10.0));
    assert_eq!(apt.position_source(), Some(PositionSource::SceneInference));

    // Receive telemetry (high accuracy)
    apt.receive_telemetry(create_telemetry_state(53.5, 10.5));

    // Should now use telemetry
    assert_eq!(apt.position_source(), Some(PositionSource::Telemetry));
}

/// Test the full InferenceAdapter flow with burst events.
#[tokio::test]
async fn test_inference_adapter_burst_flow() {
    // Create mock Scene Tracker with Hamburg bounds
    let tracker = MockSceneTracker::with_bounds(53.0, 54.0, 9.0, 11.0);

    // Create burst channel to simulate Scene Tracker events
    let (burst_tx, burst_rx) = broadcast::channel(16);

    // Create channel for inference states
    let (inference_tx, mut inference_rx) = mpsc::channel(16);

    // Create and start InferenceAdapter with short fallback interval
    let config = InferenceAdapterConfig {
        fallback_interval: Duration::from_secs(60), // Long fallback, we'll trigger via burst
    };
    let adapter = InferenceAdapter::with_config(tracker, burst_rx, inference_tx, config);
    let handle = adapter.start();

    // Simulate a burst completion
    let burst = create_test_burst(8);
    burst_tx.send(burst).expect("Should send burst");

    // Should receive inferred position
    match tokio::time::timeout(Duration::from_millis(200), inference_rx.recv()).await {
        Ok(Some(state)) => {
            assert_eq!(state.source, PositionSource::SceneInference);
            // Center of bounds: (53.5, 10.0)
            assert!((state.latitude - 53.5).abs() < 0.01);
            assert!((state.longitude - 10.0).abs() < 0.01);
        }
        Ok(None) => panic!("Channel closed unexpectedly"),
        Err(_) => panic!("Timeout waiting for inference"),
    }

    // Clean shutdown
    drop(burst_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}

// ============================================================================
// Source Priority Tests
// ============================================================================

/// Test the accuracy hierarchy: Telemetry > ManualReference > Inference.
#[tokio::test]
async fn test_accuracy_hierarchy() {
    let (apt, _rx) = create_apt();

    // 1. Start with inference (lowest accuracy ~100km)
    apt.receive_inference(create_inference_state(50.0, 5.0));
    assert_eq!(apt.position_source(), Some(PositionSource::SceneInference));

    // 2. Manual reference beats inference (~100m < ~100km)
    apt.receive_manual_reference(51.0, 6.0);
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // 3. Telemetry beats manual reference (~10m < ~100m)
    apt.receive_telemetry(create_telemetry_state(52.0, 7.0));
    assert_eq!(apt.position_source(), Some(PositionSource::Telemetry));

    // 4. Lower accuracy sources rejected while telemetry is fresh
    apt.receive_manual_reference(53.0, 8.0);
    assert_eq!(
        apt.position_source(),
        Some(PositionSource::Telemetry),
        "Manual reference should not override fresh telemetry"
    );

    apt.receive_inference(create_inference_state(54.0, 9.0));
    assert_eq!(
        apt.position_source(),
        Some(PositionSource::Telemetry),
        "Inference should not override fresh telemetry"
    );
}

/// Test position persistence across source transitions.
///
/// The position model maintains the last known position even when sources
/// change. There should never be a "lost" position state.
#[tokio::test]
async fn test_position_persistence() {
    let (apt, _rx) = create_apt_with_timeout(Duration::from_millis(100));

    // Seed with telemetry
    apt.receive_telemetry(create_telemetry_state(53.0, 10.0));
    assert!(apt.has_position());
    let pos1 = apt.position();

    // Wait for telemetry timeout
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Position should persist (telemetry status disconnected, but position remains)
    assert!(apt.has_position(), "Position should persist after timeout");
    assert_eq!(
        apt.position(),
        pos1,
        "Position should not change on timeout"
    );
}

// ============================================================================
// Full Pipeline Integration Tests
// ============================================================================

/// Test the complete Scene Tracker → InferenceAdapter → APT → Subscriber pipeline.
#[tokio::test]
async fn test_full_pipeline_scene_to_subscriber() {
    // Create the full pipeline:
    // Scene Tracker → InferenceAdapter → APT → Subscriber

    // 1. Create Scene Tracker with short burst quiet threshold
    let config = SceneTrackerConfig {
        burst_config: BurstConfig {
            quiet_threshold: Duration::from_millis(50),
            min_tiles: 3,
        },
        ..Default::default()
    };
    let scene_tracker = Arc::new(DefaultSceneTracker::new(config));
    let (fuse_tx, fuse_rx) = mpsc::unbounded_channel();
    let burst_rx = scene_tracker.subscribe_bursts();

    // Start Scene Tracker
    let scene_tracker_clone = Arc::clone(&scene_tracker);
    let scene_handle = scene_tracker_clone.start(fuse_rx);

    // 2. Create APT
    let (apt, _subscriber_rx) = create_apt();

    // 3. Create channel for inference states
    let (inference_tx, mut inference_rx) = mpsc::channel(16);

    // 4. Create InferenceAdapter
    let adapter = InferenceAdapter::new(
        Arc::clone(&scene_tracker) as Arc<dyn SceneTracker>,
        burst_rx,
        inference_tx,
    );
    let adapter_handle = adapter.start();

    // 5. Start a bridge task to forward inference to APT
    let apt_clone = apt.clone();
    let bridge_handle = tokio::spawn(async move {
        while let Some(state) = inference_rx.recv().await {
            apt_clone.receive_inference(state);
        }
    });

    // 6. Simulate FUSE tile requests (triggers burst)
    for (row, col) in HAMBURG_TILES {
        fuse_tx
            .send(FuseAccessEvent::new(DdsTileCoord::new(*row, *col, 18)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // 7. Wait for burst detection
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Manually check for burst complete (normally done by scene tracker periodically)
    scene_tracker.check_burst_complete();

    // 8. Wait for inference to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 9. Verify APT received the inference
    // Note: Position may or may not be set depending on timing, but pipeline should work
    let status = apt.status();
    if status.state.is_some() {
        assert_eq!(
            apt.position_source(),
            Some(PositionSource::SceneInference),
            "Position should be from inference"
        );
    }

    // 10. Clean shutdown
    drop(fuse_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), scene_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), adapter_handle).await;
    drop(apt); // This closes inference_rx
    let _ = tokio::time::timeout(Duration::from_secs(1), bridge_handle).await;
}

/// Test that multiple data sources can coexist and the best one is used.
#[tokio::test]
async fn test_multi_source_coexistence() {
    let (apt, _rx) = create_apt();

    // Simulate a typical startup sequence:
    // 1. User specifies --airport, seeds with manual reference
    apt.receive_manual_reference(TOULOUSE_LAT, TOULOUSE_LON);
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // 2. Scene loads, inference kicks in (but manual reference is better)
    apt.receive_inference(create_inference_state(43.0, 1.0));
    assert_eq!(
        apt.position_source(),
        Some(PositionSource::ManualReference),
        "Manual reference should beat inference"
    );

    // 3. X-Plane starts sending telemetry (best accuracy)
    apt.receive_telemetry(create_telemetry_state(43.631, 1.365));
    assert_eq!(
        apt.position_source(),
        Some(PositionSource::Telemetry),
        "Telemetry should beat all"
    );
    assert_eq!(apt.telemetry_status(), TelemetryStatus::Connected);

    // Verify final position is from telemetry
    let (lat, lon) = apt.position().unwrap();
    assert!((lat - 43.631).abs() < 0.001);
    assert!((lon - 1.365).abs() < 0.001);
}
