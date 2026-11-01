//! Integration tests for the Aircraft Position & Telemetry (APT) module.
//!
//! These tests verify the complete APT data flows:
//! - Telemetry → APT (X-Plane Web API → AircraftState → Position Model)
//! - Manual Reference → APT (Airport seed → Position Model)
//! - Source priority and accuracy-based selection
//!
//! Run with: `cargo test --test aircraft_position_integration`

use std::time::Duration;

use tokio::sync::broadcast;

use xearthlayer::aircraft_position::{
    AircraftPositionProvider, AircraftState, PositionAccuracy, PositionSource,
    SharedAircraftPosition, StateAggregator, StateAggregatorConfig, TelemetryStatus, TrackSource,
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
    AircraftState::from_telemetry(
        lat,
        lon,
        Some(90.0), // Track from the Web API
        TrackSource::Telemetry,
        90.0, // Heading
        120.0,
        10000.0,
    )
}

/// Hamburg airport coordinates for testing.
const HAMBURG_LAT: f64 = 53.630278;
const HAMBURG_LON: f64 = 9.988333;

/// Toulouse airport coordinates for testing.
const TOULOUSE_LAT: f64 = 43.629444;
const TOULOUSE_LON: f64 = 1.363889;

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
// Source Priority Tests
// ============================================================================

/// Test the accuracy hierarchy: Telemetry > ManualReference.
#[tokio::test]
async fn test_accuracy_hierarchy() {
    let (apt, _rx) = create_apt();

    // 1. Start with a manual reference (~100m)
    apt.receive_manual_reference(51.0, 6.0);
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // 2. Telemetry beats manual reference (~10m < ~100m)
    apt.receive_telemetry(create_telemetry_state(52.0, 7.0));
    assert_eq!(apt.position_source(), Some(PositionSource::Telemetry));

    // 3. Lower accuracy sources rejected while telemetry is fresh
    apt.receive_manual_reference(53.0, 8.0);
    assert_eq!(
        apt.position_source(),
        Some(PositionSource::Telemetry),
        "Manual reference should not override fresh telemetry"
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

/// Test that multiple data sources can coexist and the best one is used.
#[tokio::test]
async fn test_multi_source_coexistence() {
    let (apt, _rx) = create_apt();

    // Simulate a typical startup sequence:
    // 1. User specifies --airport, seeds with manual reference
    apt.receive_manual_reference(TOULOUSE_LAT, TOULOUSE_LON);
    assert_eq!(apt.position_source(), Some(PositionSource::ManualReference));

    // 2. X-Plane starts sending telemetry (best accuracy)
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
