//! Provider traits and shared wrapper for aircraft position.
//!
//! This module defines the public interface for consumers of aircraft
//! position data:
//!
//! - [`AircraftPositionProvider`] - Query API (pull)
//! - [`AircraftPositionBroadcaster`] - Subscription API (push)
//! - [`SharedAircraftPosition`] - Thread-safe wrapper combining both

use std::sync::Arc;

use tokio::sync::broadcast;

use super::aggregator::StateAggregator;
use super::state::{AircraftPositionStatus, AircraftState, PositionSource, TelemetryStatus};

/// Trait for querying aircraft position (pull API).
///
/// Provides synchronous access to the current aircraft position and status.
pub trait AircraftPositionProvider: Send + Sync {
    /// Get complete aircraft position status.
    fn status(&self) -> AircraftPositionStatus;

    /// Get current position if available (convenience method).
    fn position(&self) -> Option<(f64, f64)>;

    /// Get telemetry connection status.
    fn telemetry_status(&self) -> TelemetryStatus;

    /// Get the source of current position data.
    fn position_source(&self) -> Option<PositionSource>;

    /// Check if we have any position data.
    fn has_position(&self) -> bool;

    /// Check if vector data (heading, speed, altitude) is available.
    fn has_vectors(&self) -> bool;
}

/// Trait for subscribing to position updates (push API).
///
/// Position updates are broadcast at maximum 1Hz.
pub trait AircraftPositionBroadcaster: Send + Sync {
    /// Subscribe to position updates.
    fn subscribe(&self) -> broadcast::Receiver<AircraftState>;
}

/// Shared aircraft position - thread-safe wrapper for APT.
///
/// Combines [`AircraftPositionProvider`] and [`AircraftPositionBroadcaster`]
/// into a single Arc-wrapped type that can be shared across modules.
///
/// # Usage
///
/// ```ignore
/// // Create shared position
/// let (broadcast_tx, _) = broadcast::channel(16);
/// let aggregator = StateAggregator::new(broadcast_tx);
/// let shared = SharedAircraftPosition::new(aggregator);
///
/// // Query position
/// if let Some((lat, lon)) = shared.position() {
///     println!("Position: {}, {}", lat, lon);
/// }
///
/// // Subscribe to updates
/// let mut rx = shared.subscribe();
/// while let Ok(state) = rx.recv().await {
///     // Handle position update
/// }
/// ```
#[derive(Clone)]
pub struct SharedAircraftPosition {
    inner: Arc<StateAggregator>,
}

impl SharedAircraftPosition {
    /// Create a new shared aircraft position from an aggregator.
    pub fn new(aggregator: StateAggregator) -> Self {
        Self {
            inner: Arc::new(aggregator),
        }
    }

    /// Receive a position update from a manual reference (airport ICAO, user coordinates, etc.).
    ///
    /// Manual reference positions compete on accuracy like any other source.
    /// Returns true if the update was accepted.
    pub fn receive_manual_reference(&self, latitude: f64, longitude: f64) -> bool {
        self.inner.receive_manual_reference(latitude, longitude)
    }

    /// Receive a position update from telemetry.
    pub fn receive_telemetry(&self, state: AircraftState) {
        self.inner.receive_telemetry(state);
    }

    /// Receive a position update from inference.
    pub fn receive_inference(&self, state: AircraftState) {
        self.inner.receive_inference(state);
    }
}

impl AircraftPositionProvider for SharedAircraftPosition {
    fn status(&self) -> AircraftPositionStatus {
        self.inner.status()
    }

    fn position(&self) -> Option<(f64, f64)> {
        self.inner.position()
    }

    fn telemetry_status(&self) -> TelemetryStatus {
        self.inner.telemetry_status()
    }

    fn position_source(&self) -> Option<PositionSource> {
        self.status().state.map(|s| s.source)
    }

    fn has_position(&self) -> bool {
        self.inner.has_position()
    }

    fn has_vectors(&self) -> bool {
        self.inner.has_vectors()
    }
}

impl AircraftPositionBroadcaster for SharedAircraftPosition {
    fn subscribe(&self) -> broadcast::Receiver<AircraftState> {
        self.inner.subscribe()
    }
}

// Allow Arc<SharedAircraftPosition> to be used as provider
impl AircraftPositionProvider for Arc<SharedAircraftPosition> {
    fn status(&self) -> AircraftPositionStatus {
        (**self).status()
    }

    fn position(&self) -> Option<(f64, f64)> {
        (**self).position()
    }

    fn telemetry_status(&self) -> TelemetryStatus {
        (**self).telemetry_status()
    }

    fn position_source(&self) -> Option<PositionSource> {
        (**self).position_source()
    }

    fn has_position(&self) -> bool {
        (**self).has_position()
    }

    fn has_vectors(&self) -> bool {
        (**self).has_vectors()
    }
}

impl AircraftPositionBroadcaster for Arc<SharedAircraftPosition> {
    fn subscribe(&self) -> broadcast::Receiver<AircraftState> {
        (**self).subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_shared() -> SharedAircraftPosition {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);
        SharedAircraftPosition::new(aggregator)
    }

    #[test]
    fn test_shared_no_position() {
        let shared = create_shared();

        assert!(!shared.has_position());
        assert!(shared.position().is_none());
        assert!(shared.position_source().is_none());
        assert_eq!(shared.telemetry_status(), TelemetryStatus::Disconnected);
    }

    #[test]
    fn test_shared_with_manual_reference() {
        let shared = create_shared();

        assert!(shared.receive_manual_reference(43.6, 1.4));

        assert!(shared.has_position());
        assert_eq!(shared.position(), Some((43.6, 1.4)));
        assert_eq!(
            shared.position_source(),
            Some(PositionSource::ManualReference)
        );
    }

    #[test]
    fn test_shared_with_telemetry() {
        let shared = create_shared();

        let state = AircraftState::from_telemetry(53.5, 10.0, 90.0, 120.0, 10000.0);
        shared.receive_telemetry(state);

        assert!(shared.has_position());
        assert_eq!(shared.position(), Some((53.5, 10.0)));
        assert_eq!(shared.position_source(), Some(PositionSource::Telemetry));
        assert_eq!(shared.telemetry_status(), TelemetryStatus::Connected);
        assert!(shared.has_vectors());
    }

    #[test]
    fn test_shared_subscribe() {
        let shared = create_shared();
        let mut rx = shared.subscribe();

        let state = AircraftState::from_telemetry(53.5, 10.0, 90.0, 120.0, 10000.0);
        shared.receive_telemetry(state);

        let received = rx.try_recv().expect("Should receive broadcast");
        assert_eq!(received.latitude, 53.5);
    }

    #[test]
    fn test_arc_wrapped() {
        let shared = Arc::new(create_shared());

        shared.receive_manual_reference(43.6, 1.4);

        // Test through Arc
        assert!(AircraftPositionProvider::has_position(&shared));
        assert_eq!(
            AircraftPositionProvider::position(&shared),
            Some((43.6, 1.4))
        );
    }
}
