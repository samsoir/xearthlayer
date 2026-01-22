//! Position Model - maintains best-known aircraft position.
//!
//! The Position Model is the core of the APT module. It maintains a persistent
//! position that is refined by multiple data sources based on their accuracy
//! and freshness.
//!
//! # Selection Logic
//!
//! 1. Higher accuracy always wins (lower meters = better)
//! 2. Equal accuracy: fresher timestamp wins
//! 3. Stale high-accuracy can be beaten by fresh lower-accuracy
//!
//! # Example
//!
//! ```ignore
//! let mut model = PositionModel::new();
//!
//! // Seed with manual reference (100m accuracy)
//! model.seed_from_manual_reference(43.6, 1.4);
//!
//! // Scene inference won't replace manual reference (100km < 100m accuracy)
//! // unless manual reference is stale
//! let inference = AircraftState::from_inference(43.5, 1.3);
//! model.apply_update(inference, Duration::from_secs(30));
//!
//! // Telemetry always wins (10m > all others)
//! let telemetry = AircraftState::from_telemetry(43.6001, 1.4001, 90.0, 120.0, 10000.0);
//! model.apply_update(telemetry, Duration::from_secs(30));
//! ```

use std::time::Duration;

use super::state::{AircraftState, PositionSource, TelemetryStatus};

/// Position Model - maintains the best-known aircraft position.
///
/// The model is the source of truth, not any single input. Inputs refine
/// the model based on their accuracy and freshness.
#[derive(Debug)]
pub struct PositionModel {
    /// Current best-known position (None only before any data received).
    current: Option<AircraftState>,

    /// Telemetry connection status (tracked independently of position).
    telemetry_status: TelemetryStatus,

    /// Threshold for considering a position "stale".
    stale_threshold: Duration,
}

impl Default for PositionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionModel {
    /// Default stale threshold (30 seconds).
    pub const DEFAULT_STALE_THRESHOLD: Duration = Duration::from_secs(30);

    /// Create a new empty position model.
    pub fn new() -> Self {
        Self {
            current: None,
            telemetry_status: TelemetryStatus::Disconnected,
            stale_threshold: Self::DEFAULT_STALE_THRESHOLD,
        }
    }

    /// Create a position model with a custom stale threshold.
    pub fn with_stale_threshold(stale_threshold: Duration) -> Self {
        Self {
            current: None,
            telemetry_status: TelemetryStatus::Disconnected,
            stale_threshold,
        }
    }

    /// Get the current position (if any).
    pub fn current(&self) -> Option<&AircraftState> {
        self.current.as_ref()
    }

    /// Get the current telemetry status.
    pub fn telemetry_status(&self) -> TelemetryStatus {
        self.telemetry_status
    }

    /// Set the telemetry status.
    pub fn set_telemetry_status(&mut self, status: TelemetryStatus) {
        self.telemetry_status = status;
    }

    /// Check if we have any position data.
    pub fn has_position(&self) -> bool {
        self.current.is_some()
    }

    /// Determine if an update should replace the current position.
    ///
    /// Decision factors:
    /// 1. Higher accuracy always wins (lower meters = better)
    /// 2. Equal accuracy: fresher timestamp wins
    /// 3. Stale high-accuracy can be beaten by fresh lower-accuracy
    pub fn should_accept(&self, update: &AircraftState) -> bool {
        let Some(current) = &self.current else {
            return true; // No position yet - accept anything
        };

        let current_stale = current.is_stale(self.stale_threshold);
        let update_more_accurate = update.accuracy.is_better_than(&current.accuracy);

        // Higher accuracy always wins
        if update_more_accurate {
            return true;
        }

        // If current is stale, accept fresher data even if less accurate
        if current_stale {
            return true;
        }

        // Same accuracy: accept if fresher
        if update.accuracy == current.accuracy && update.timestamp > current.timestamp {
            return true;
        }

        false
    }

    /// Apply an update to the model.
    ///
    /// Returns true if the update was accepted and applied.
    pub fn apply_update(&mut self, update: AircraftState) -> bool {
        if self.should_accept(&update) {
            self.current = Some(update);
            true
        } else {
            false
        }
    }

    /// Get position as (latitude, longitude) tuple.
    pub fn position(&self) -> Option<(f64, f64)> {
        self.current.as_ref().map(|s| (s.latitude, s.longitude))
    }

    /// Get the source of the current position.
    pub fn position_source(&self) -> Option<PositionSource> {
        self.current.as_ref().map(|s| s.source)
    }

    /// Check if vector data (heading, speed, altitude) is available.
    pub fn has_vectors(&self) -> bool {
        self.current.as_ref().is_some_and(|s| s.has_vectors())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_new_model_has_no_position() {
        let model = PositionModel::new();

        assert!(!model.has_position());
        assert!(model.current().is_none());
        assert_eq!(model.telemetry_status(), TelemetryStatus::Disconnected);
    }

    #[test]
    fn test_manual_reference_as_first_position() {
        let mut model = PositionModel::new();

        let manual_reference = AircraftState::from_manual_reference(43.6, 1.4);
        assert!(model.apply_update(manual_reference));
        assert!(model.has_position());
        assert_eq!(model.position(), Some((43.6, 1.4)));
        assert_eq!(
            model.position_source(),
            Some(PositionSource::ManualReference)
        );
    }

    #[test]
    fn test_manual_reference_can_update_stale_manual_reference() {
        // Use a very short stale threshold for testing
        let mut model = PositionModel::with_stale_threshold(Duration::from_millis(10));

        // First manual reference
        let manual_reference1 = AircraftState::from_manual_reference(43.6, 1.4);
        assert!(model.apply_update(manual_reference1));

        // Wait for it to become stale
        thread::sleep(Duration::from_millis(20));

        // Second manual reference should replace stale first
        let manual_reference2 = AircraftState::from_manual_reference(53.5, 10.0);
        assert!(model.apply_update(manual_reference2));
        assert_eq!(model.position(), Some((53.5, 10.0)));
    }

    #[test]
    fn test_accept_first_update() {
        let mut model = PositionModel::new();
        let state = AircraftState::from_inference(53.5, 10.0);

        assert!(model.apply_update(state));
        assert!(model.has_position());
    }

    #[test]
    fn test_higher_accuracy_wins() {
        let mut model = PositionModel::new();

        // Start with inference (100km accuracy)
        let inference = AircraftState::from_inference(53.5, 10.0);
        model.apply_update(inference);

        // Telemetry (10m) should replace it
        let telemetry = AircraftState::from_telemetry(53.6, 10.1, 90.0, 120.0, 10000.0);
        assert!(model.apply_update(telemetry));
        assert_eq!(model.position_source(), Some(PositionSource::Telemetry));
    }

    #[test]
    fn test_lower_accuracy_rejected_when_fresh() {
        let mut model = PositionModel::new();

        // Start with telemetry (10m accuracy)
        let telemetry = AircraftState::from_telemetry(53.6, 10.1, 90.0, 120.0, 10000.0);
        model.apply_update(telemetry);

        // Inference (100km) should be rejected
        let inference = AircraftState::from_inference(53.5, 10.0);
        assert!(!model.apply_update(inference));
        assert_eq!(model.position_source(), Some(PositionSource::Telemetry));
    }

    #[test]
    fn test_stale_position_can_be_replaced() {
        // Use a very short stale threshold for testing
        let mut model = PositionModel::with_stale_threshold(Duration::from_millis(10));

        // Start with telemetry
        let telemetry = AircraftState::from_telemetry(53.6, 10.1, 90.0, 120.0, 10000.0);
        model.apply_update(telemetry);

        // Wait for it to become stale
        thread::sleep(Duration::from_millis(20));

        // Now inference can replace it
        let inference = AircraftState::from_inference(53.5, 10.0);
        assert!(model.apply_update(inference));
        assert_eq!(
            model.position_source(),
            Some(PositionSource::SceneInference)
        );
    }

    #[test]
    fn test_telemetry_status_independent() {
        let mut model = PositionModel::new();

        // Set telemetry connected but no position yet
        model.set_telemetry_status(TelemetryStatus::Connected);
        assert_eq!(model.telemetry_status(), TelemetryStatus::Connected);
        assert!(!model.has_position());

        // Add inference position - telemetry status unchanged
        let inference = AircraftState::from_inference(53.5, 10.0);
        model.apply_update(inference);
        assert_eq!(model.telemetry_status(), TelemetryStatus::Connected);
    }

    #[test]
    fn test_has_vectors() {
        let mut model = PositionModel::new();

        // Inference has no vectors
        let inference = AircraftState::from_inference(53.5, 10.0);
        model.apply_update(inference);
        assert!(!model.has_vectors());

        // Telemetry has vectors
        let telemetry = AircraftState::from_telemetry(53.6, 10.1, 90.0, 120.0, 10000.0);
        model.apply_update(telemetry);
        assert!(model.has_vectors());
    }

    #[test]
    fn test_manual_reference_vs_inference() {
        let mut model = PositionModel::new();

        // Start with manual reference (100m accuracy)
        let manual_reference = AircraftState::from_manual_reference(43.6, 1.4);
        model.apply_update(manual_reference);
        assert_eq!(
            model.position_source(),
            Some(PositionSource::ManualReference)
        );

        // Inference (100km) should be rejected - manual reference is more accurate
        let inference = AircraftState::from_inference(53.5, 10.0);
        assert!(!model.apply_update(inference));
        assert_eq!(
            model.position_source(),
            Some(PositionSource::ManualReference)
        );
    }
}
