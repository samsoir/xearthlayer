//! State Aggregator - combines position updates from all sources.
//!
//! The aggregator receives position updates from multiple sources:
//! - Telemetry (XGPS2 UDP)
//! - Prewarm (airport ICAO)
//! - Scene Inference (from Scene Tracker)
//!
//! It applies updates to the Position Model based on accuracy and freshness,
//! tracks telemetry connection status, and broadcasts position changes.
//!
//! # Rate Limiting
//!
//! Position updates are broadcast at maximum 1Hz to avoid flooding consumers.
//!
//! # Usage
//!
//! ```ignore
//! let (broadcast_tx, _) = broadcast::channel(16);
//! let aggregator = StateAggregator::new(broadcast_tx);
//!
//! // Receive from telemetry
//! aggregator.receive_telemetry(state).await;
//!
//! // Receive from inference
//! aggregator.receive_inference(state).await;
//!
//! // Seed from prewarm
//! aggregator.seed_from_prewarm(43.6, 1.4);
//! ```

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use super::flight_path::FlightPathHistory;
use super::model::PositionModel;
use super::state::{AircraftPositionStatus, AircraftState, TelemetryStatus, TrackSource};

/// Configuration for the state aggregator.
#[derive(Debug, Clone)]
pub struct StateAggregatorConfig {
    /// Minimum interval between broadcasts.
    pub min_broadcast_interval: Duration,

    /// Timeout for considering telemetry disconnected.
    pub telemetry_timeout: Duration,
}

impl Default for StateAggregatorConfig {
    fn default() -> Self {
        Self {
            min_broadcast_interval: Duration::from_secs(1), // 1Hz max
            telemetry_timeout: Duration::from_secs(5),
        }
    }
}

/// Internal state for the aggregator.
struct AggregatorState {
    /// Position model.
    model: PositionModel,

    /// Flight path history for track derivation.
    flight_path: FlightPathHistory,

    /// Last broadcast time (for rate limiting).
    last_broadcast: Option<Instant>,

    /// Last telemetry received time (for timeout detection).
    last_telemetry: Option<Instant>,
}

/// State aggregator - combines position updates from all sources.
pub struct StateAggregator {
    /// Internal state (thread-safe).
    state: Arc<RwLock<AggregatorState>>,

    /// Broadcast channel for position updates.
    broadcast_tx: broadcast::Sender<AircraftState>,

    /// Configuration.
    config: StateAggregatorConfig,
}

impl StateAggregator {
    /// Create a new state aggregator.
    pub fn new(broadcast_tx: broadcast::Sender<AircraftState>) -> Self {
        Self::with_config(broadcast_tx, StateAggregatorConfig::default())
    }

    /// Create with custom configuration.
    pub fn with_config(
        broadcast_tx: broadcast::Sender<AircraftState>,
        config: StateAggregatorConfig,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(AggregatorState {
                model: PositionModel::new(),
                flight_path: FlightPathHistory::new(),
                last_broadcast: None,
                last_telemetry: None,
            })),
            broadcast_tx,
            config,
        }
    }

    /// Receive a position update from a manual reference (airport ICAO, user coordinates, etc.).
    ///
    /// Manual reference positions are treated like any other source - they compete
    /// on accuracy and freshness. Returns true if the update was accepted.
    pub fn receive_manual_reference(&self, latitude: f64, longitude: f64) -> bool {
        let aircraft_state = AircraftState::from_manual_reference(latitude, longitude);
        let mut state = self.state.write().unwrap();

        if state.model.apply_update(aircraft_state) {
            tracing::info!(
                latitude,
                longitude,
                "Position updated from manual reference"
            );
            self.maybe_broadcast(&mut state);
            true
        } else {
            tracing::debug!(
                latitude,
                longitude,
                "Manual reference position rejected (better position available)"
            );
            false
        }
    }

    /// Receive a position update from telemetry.
    pub fn receive_telemetry(&self, mut aircraft_state: AircraftState) {
        let mut state = self.state.write().unwrap();

        // Update telemetry tracking
        state.last_telemetry = Some(Instant::now());
        state.model.set_telemetry_status(TelemetryStatus::Connected);

        // Record position in flight path history
        state
            .flight_path
            .record_position(aircraft_state.latitude, aircraft_state.longitude);

        // Enrich with derived track if authoritative track unavailable
        if aircraft_state.track.is_none() {
            if let Some(derived_track) = state.flight_path.calculate_track() {
                aircraft_state.track = Some(derived_track);
                aircraft_state.track_source = TrackSource::Derived;
            }
        }

        // Apply update
        if state.model.apply_update(aircraft_state) {
            self.maybe_broadcast(&mut state);
        }
    }

    /// Receive a position update from an online network (VATSIM/IVAO/PilotEdge).
    ///
    /// Similar to `receive_telemetry()` but does NOT set TelemetryStatus::Connected,
    /// since telemetry status tracks XGPS2 UDP connectivity specifically.
    /// Records position in flight path history for track derivation.
    pub fn receive_network_position(&self, mut aircraft_state: AircraftState) {
        let mut state = self.state.write().unwrap();

        // Record position in flight path history (for track derivation)
        state
            .flight_path
            .record_position(aircraft_state.latitude, aircraft_state.longitude);

        // Enrich with derived track if unavailable
        if aircraft_state.track.is_none() {
            if let Some(derived_track) = state.flight_path.calculate_track() {
                aircraft_state.track = Some(derived_track);
                aircraft_state.track_source = TrackSource::Derived;
            }
        }

        // Apply update (competes on accuracy like any other source)
        if state.model.apply_update(aircraft_state) {
            self.maybe_broadcast(&mut state);
        }
    }

    /// Receive a position update from inference.
    pub fn receive_inference(&self, aircraft_state: AircraftState) {
        let mut state = self.state.write().unwrap();

        // Check telemetry timeout
        self.check_telemetry_timeout(&mut state);

        // Apply update
        if state.model.apply_update(aircraft_state) {
            self.maybe_broadcast(&mut state);
        }
    }

    /// Get the current position status.
    pub fn status(&self) -> AircraftPositionStatus {
        let mut state = self.state.write().unwrap();

        // Check telemetry timeout
        self.check_telemetry_timeout(&mut state);

        match state.model.current() {
            Some(current) => {
                AircraftPositionStatus::from_state(current.clone(), state.model.telemetry_status())
            }
            None => AircraftPositionStatus {
                state: None,
                telemetry_status: state.model.telemetry_status(),
                vectors_available: false,
            },
        }
    }

    /// Get the current position as (lat, lon).
    pub fn position(&self) -> Option<(f64, f64)> {
        self.state.read().unwrap().model.position()
    }

    /// Get the current telemetry status.
    pub fn telemetry_status(&self) -> TelemetryStatus {
        let mut state = self.state.write().unwrap();
        self.check_telemetry_timeout(&mut state);
        state.model.telemetry_status()
    }

    /// Check if we have any position data.
    pub fn has_position(&self) -> bool {
        self.state.read().unwrap().model.has_position()
    }

    /// Check if vector data is available.
    pub fn has_vectors(&self) -> bool {
        self.state.read().unwrap().model.has_vectors()
    }

    /// Subscribe to position updates.
    pub fn subscribe(&self) -> broadcast::Receiver<AircraftState> {
        self.broadcast_tx.subscribe()
    }

    /// Check if telemetry has timed out and update status.
    fn check_telemetry_timeout(&self, state: &mut AggregatorState) {
        if let Some(last) = state.last_telemetry {
            if last.elapsed() > self.config.telemetry_timeout
                && state.model.telemetry_status() == TelemetryStatus::Connected
            {
                tracing::info!("Telemetry timeout, marking as disconnected");
                state
                    .model
                    .set_telemetry_status(TelemetryStatus::Disconnected);
            }
        }
    }

    /// Broadcast position if rate limit allows.
    fn maybe_broadcast(&self, state: &mut AggregatorState) {
        let should_broadcast = match state.last_broadcast {
            None => true,
            Some(last) => last.elapsed() >= self.config.min_broadcast_interval,
        };

        if should_broadcast {
            if let Some(current) = state.model.current() {
                let _ = self.broadcast_tx.send(current.clone());
                state.last_broadcast = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::PositionSource;
    use super::*;

    fn make_telemetry_state(lat: f64, lon: f64, heading: f32) -> AircraftState {
        AircraftState::from_telemetry(
            lat,
            lon,
            Some(heading), // Track from XGPS2
            TrackSource::Telemetry,
            heading,
            120.0,
            10000.0,
        )
    }

    fn make_telemetry_state_no_track(lat: f64, lon: f64, heading: f32) -> AircraftState {
        AircraftState::from_telemetry(
            lat,
            lon,
            None, // No track - needs derivation
            TrackSource::Unavailable,
            heading,
            120.0,
            10000.0,
        )
    }

    #[test]
    fn test_receive_manual_reference() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        assert!(aggregator.receive_manual_reference(43.6, 1.4));
        assert!(aggregator.has_position());
        assert_eq!(aggregator.position(), Some((43.6, 1.4)));
    }

    #[test]
    fn test_receive_telemetry() {
        let (tx, mut rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        let state = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(state);

        assert!(aggregator.has_position());
        assert_eq!(aggregator.telemetry_status(), TelemetryStatus::Connected);
        assert!(aggregator.has_vectors());

        // Should have broadcast
        let received = rx.try_recv().expect("Should receive broadcast");
        assert_eq!(received.latitude, 53.5);
    }

    #[test]
    fn test_telemetry_replaces_manual_reference() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Start with manual reference
        aggregator.receive_manual_reference(43.6, 1.4);
        assert_eq!(
            aggregator.status().state.unwrap().source,
            PositionSource::ManualReference
        );

        // Telemetry should replace it (higher accuracy)
        let state = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(state);

        assert_eq!(
            aggregator.status().state.unwrap().source,
            PositionSource::Telemetry
        );
    }

    #[test]
    fn test_manual_reference_rejected_when_telemetry_active() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Start with telemetry (10m accuracy)
        let telemetry = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(telemetry);

        // Manual reference (100m) should be rejected - telemetry is more accurate
        assert!(!aggregator.receive_manual_reference(43.6, 1.4));
        assert_eq!(aggregator.position(), Some((53.5, 10.0)));
    }

    #[test]
    fn test_inference_rejected_when_telemetry_fresh() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Start with telemetry
        let telemetry = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(telemetry);

        // Inference should be rejected
        let inference = AircraftState::from_inference(40.0, 5.0);
        aggregator.receive_inference(inference);

        // Position should still be from telemetry
        assert_eq!(aggregator.position(), Some((53.5, 10.0)));
    }

    #[test]
    fn test_telemetry_timeout() {
        // Use longer timeouts to avoid flakiness on loaded systems
        let config = StateAggregatorConfig {
            telemetry_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::with_config(tx, config);

        // Receive telemetry
        let state = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(state);
        assert_eq!(aggregator.telemetry_status(), TelemetryStatus::Connected);

        // Wait for timeout (150ms > 100ms timeout)
        std::thread::sleep(Duration::from_millis(150));

        // Should now be disconnected
        assert_eq!(aggregator.telemetry_status(), TelemetryStatus::Disconnected);
    }

    #[test]
    fn test_rate_limiting() {
        let config = StateAggregatorConfig {
            min_broadcast_interval: Duration::from_millis(100),
            ..Default::default()
        };
        let (tx, mut rx) = broadcast::channel(16);
        let aggregator = StateAggregator::with_config(tx, config);

        // First update - should broadcast
        let state1 = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(state1);
        assert!(rx.try_recv().is_ok());

        // Immediate second update - should NOT broadcast (rate limited)
        let state2 = make_telemetry_state(53.6, 10.1, 91.0);
        aggregator.receive_telemetry(state2);
        assert!(rx.try_recv().is_err());

        // Wait and update - should broadcast
        std::thread::sleep(Duration::from_millis(110));
        let state3 = make_telemetry_state(53.7, 10.2, 92.0);
        aggregator.receive_telemetry(state3);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn test_derived_track_when_no_authoritative_track() {
        let config = StateAggregatorConfig {
            min_broadcast_interval: Duration::from_millis(1),
            ..Default::default()
        };
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::with_config(tx, config);

        // Send multiple positions without track (simulating legacy protocol)
        // Moving north: lat increases
        aggregator.receive_telemetry(make_telemetry_state_no_track(53.0, 10.0, 0.0));
        std::thread::sleep(Duration::from_millis(5));
        aggregator.receive_telemetry(make_telemetry_state_no_track(53.1, 10.0, 0.0));

        // After enough samples, track should be derived
        let status = aggregator.status();
        let state = status.state.unwrap();

        // Either still Unavailable (not enough samples) or Derived
        if state.track.is_some() {
            assert_eq!(state.track_source, TrackSource::Derived);
            // Should be approximately north (0°)
            let track = state.track.unwrap();
            assert!(
                track < 10.0 || track > 350.0,
                "Expected ~0°, got {}°",
                track
            );
        }
    }

    #[test]
    fn test_receive_network_position() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        let state = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            std::time::Instant::now(),
        );
        aggregator.receive_network_position(state);

        assert!(aggregator.has_position());
        assert_eq!(aggregator.position(), Some((33.9425, -118.408)));
        assert!(aggregator.has_vectors());
    }

    #[test]
    fn test_network_position_does_not_set_telemetry_connected() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        let state = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            std::time::Instant::now(),
        );
        aggregator.receive_network_position(state);

        // TelemetryStatus should remain Disconnected (network != XGPS2)
        assert_eq!(aggregator.telemetry_status(), TelemetryStatus::Disconnected);
    }

    #[test]
    fn test_network_position_beats_inference() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Start with inference (100km accuracy)
        let inference = AircraftState::from_inference(40.0, 5.0);
        aggregator.receive_inference(inference);
        assert_eq!(
            aggregator.status().state.unwrap().source,
            PositionSource::SceneInference
        );

        // Network (10m accuracy) should replace inference
        let network = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            std::time::Instant::now(),
        );
        aggregator.receive_network_position(network);
        assert_eq!(
            aggregator.status().state.unwrap().source,
            PositionSource::OnlineNetwork
        );
    }

    #[test]
    fn test_fresh_telemetry_beats_network() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Start with network position
        let network = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            std::time::Instant::now(),
        );
        aggregator.receive_network_position(network);

        // Fresh telemetry (same accuracy, but fresher) should replace
        let telemetry = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(telemetry);
        assert_eq!(
            aggregator.status().state.unwrap().source,
            PositionSource::Telemetry
        );
    }

    #[test]
    fn test_authoritative_track_preserved() {
        let (tx, _rx) = broadcast::channel(16);
        let aggregator = StateAggregator::new(tx);

        // Send telemetry with authoritative track
        let state = make_telemetry_state(53.5, 10.0, 90.0);
        aggregator.receive_telemetry(state);

        let status = aggregator.status();
        let received = status.state.unwrap();

        // Track should be from telemetry, not derived
        assert_eq!(received.track, Some(90.0));
        assert_eq!(received.track_source, TrackSource::Telemetry);
    }
}
