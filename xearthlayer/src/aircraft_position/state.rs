//! Core state types for aircraft position tracking.
//!
//! This module defines the fundamental types used throughout the APT system:
//!
//! - [`PositionAccuracy`] - Numeric accuracy in meters (lower is better)
//! - [`TelemetryStatus`] - Is X-Plane sending telemetry?
//! - [`PositionSource`] - Where did this position come from?
//! - [`AircraftState`] - Complete position snapshot with metadata
//! - [`AircraftPositionStatus`] - Full status for consumers

use std::time::Instant;

/// Position accuracy in meters (lower is better).
///
/// Each position source reports its inherent measurement precision.
/// This is combined with timestamp freshness to select the best position.
///
/// # Design
///
/// Using a numeric score rather than an enum allows extensibility - future sources
/// (VATSIM, IVAO, PilotEdge, SimConnect) can report their precision without
/// modifying any enums. The model's selection logic remains unchanged.
///
/// # Ordering
///
/// Lower values indicate higher accuracy:
/// - `TELEMETRY` (10m) > `AIRPORT_FIX` (100m) > `SCENE_INFERENCE` (100,000m)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionAccuracy(pub f32);

impl PositionAccuracy {
    /// GPS telemetry - meter-level precision.
    ///
    /// Used for position data from X-Plane's XGPS2/ForeFlight UDP broadcast.
    pub const TELEMETRY: Self = Self(10.0);

    /// Manual reference point - user-provided position.
    ///
    /// Used for positions from airport ICAO, manual coordinates, flight plans, etc.
    /// Accurate but confidence decays rapidly once aircraft moves.
    pub const MANUAL_REFERENCE: Self = Self(100.0);

    /// Scene inference - derived from tile bounds (~60nm).
    ///
    /// Used for position inferred from X-Plane's scenery loading patterns.
    pub const SCENE_INFERENCE: Self = Self(100_000.0);

    /// Returns true if this accuracy is better (lower meters) than other.
    #[inline]
    pub fn is_better_than(&self, other: &Self) -> bool {
        self.0 < other.0
    }
}

impl PartialOrd for PositionAccuracy {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Lower meters = higher accuracy = greater in ordering
        // So we reverse the comparison
        other.0.partial_cmp(&self.0)
    }
}

/// Telemetry connection status.
///
/// Indicates whether X-Plane is actively sending XGPS2 UDP telemetry.
/// This is independent of the position source - we might be using an
/// inferred position even when telemetry is nominally "connected" (brief gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryStatus {
    /// Receiving XGPS2 UDP data from X-Plane.
    Connected,
    /// Not receiving telemetry data (timeout or not configured).
    #[default]
    Disconnected,
}

impl std::fmt::Display for TelemetryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "Connected"),
            Self::Disconnected => write!(f, "Disconnected"),
        }
    }
}

/// Source of the current position data.
///
/// Indicates which input provided the position in the current `AircraftState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// From GPS telemetry (XGPS2/ForeFlight UDP).
    Telemetry,
    /// User-provided reference point (airport ICAO, manual coordinates, flight plan, etc.).
    ManualReference,
    /// Inferred from Scene Tracker loaded bounds.
    SceneInference,
}

impl std::fmt::Display for PositionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telemetry => write!(f, "Telemetry"),
            Self::ManualReference => write!(f, "Manual"),
            Self::SceneInference => write!(f, "Inferred"),
        }
    }
}

/// Aircraft state snapshot.
///
/// Contains position, vector data (if available), and metadata about
/// the source and accuracy of the data.
///
/// # Timestamp
///
/// The `timestamp` field indicates when this data was measured or inferred.
/// Consumers should use this to judge freshness - there is no separate
/// "confidence" field because different consumers have different staleness
/// tolerances.
#[derive(Debug, Clone)]
pub struct AircraftState {
    /// Latitude in degrees (-90 to 90).
    pub latitude: f64,

    /// Longitude in degrees (-180 to 180).
    pub longitude: f64,

    /// True heading in degrees (0-360).
    ///
    /// Set to 0.0 if unknown (e.g., from scene inference).
    pub heading: f32,

    /// Ground speed in knots.
    ///
    /// Set to 0.0 if unknown (e.g., from scene inference).
    pub ground_speed: f32,

    /// Altitude MSL in feet.
    ///
    /// Set to 0.0 if unknown (e.g., from scene inference).
    pub altitude: f32,

    /// When this position was measured/inferred.
    ///
    /// Consumers use this to judge freshness.
    pub timestamp: Instant,

    /// Source of this position data.
    pub source: PositionSource,

    /// Accuracy of this position in meters.
    pub accuracy: PositionAccuracy,
}

impl AircraftState {
    /// Create a new aircraft state from telemetry.
    pub fn from_telemetry(
        latitude: f64,
        longitude: f64,
        heading: f32,
        ground_speed: f32,
        altitude: f32,
    ) -> Self {
        Self {
            latitude,
            longitude,
            heading,
            ground_speed,
            altitude,
            timestamp: Instant::now(),
            source: PositionSource::Telemetry,
            accuracy: PositionAccuracy::TELEMETRY,
        }
    }

    /// Create a new aircraft state from a manual reference point.
    ///
    /// Used for user-provided positions such as airport ICAO, manual coordinates,
    /// or imported flight plan waypoints.
    pub fn from_manual_reference(latitude: f64, longitude: f64) -> Self {
        Self {
            latitude,
            longitude,
            heading: 0.0,
            ground_speed: 0.0,
            altitude: 0.0,
            timestamp: Instant::now(),
            source: PositionSource::ManualReference,
            accuracy: PositionAccuracy::MANUAL_REFERENCE,
        }
    }

    /// Create a new aircraft state from scene inference.
    pub fn from_inference(latitude: f64, longitude: f64) -> Self {
        Self {
            latitude,
            longitude,
            heading: 0.0,
            ground_speed: 0.0,
            altitude: 0.0,
            timestamp: Instant::now(),
            source: PositionSource::SceneInference,
            accuracy: PositionAccuracy::SCENE_INFERENCE,
        }
    }

    /// Get the age of this state (time since it was created).
    pub fn age(&self) -> std::time::Duration {
        self.timestamp.elapsed()
    }

    /// Check if this state is stale (older than the given duration).
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        self.age() > max_age
    }

    /// Check if vector data (heading, speed, altitude) is meaningful.
    ///
    /// Returns true only for telemetry sources where we have actual data.
    pub fn has_vectors(&self) -> bool {
        matches!(self.source, PositionSource::Telemetry)
    }
}

/// Complete APT status for consumers.
///
/// Provides all information needed to display aircraft position
/// and telemetry status in the UI.
#[derive(Debug, Clone)]
pub struct AircraftPositionStatus {
    /// Current aircraft state (if any position data available).
    pub state: Option<AircraftState>,

    /// Is X-Plane sending telemetry? (independent of position source)
    pub telemetry_status: TelemetryStatus,

    /// Are heading/speed/altitude available? (only from telemetry)
    pub vectors_available: bool,
}

impl Default for AircraftPositionStatus {
    fn default() -> Self {
        Self {
            state: None,
            telemetry_status: TelemetryStatus::Disconnected,
            vectors_available: false,
        }
    }
}

impl AircraftPositionStatus {
    /// Create status with no position data.
    pub fn no_position() -> Self {
        Self::default()
    }

    /// Create status from an aircraft state.
    pub fn from_state(state: AircraftState, telemetry_status: TelemetryStatus) -> Self {
        let vectors_available = state.has_vectors();
        Self {
            state: Some(state),
            telemetry_status,
            vectors_available,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_accuracy_ordering() {
        assert!(PositionAccuracy::TELEMETRY.is_better_than(&PositionAccuracy::MANUAL_REFERENCE));
        assert!(
            PositionAccuracy::MANUAL_REFERENCE.is_better_than(&PositionAccuracy::SCENE_INFERENCE)
        );
        assert!(PositionAccuracy::TELEMETRY.is_better_than(&PositionAccuracy::SCENE_INFERENCE));

        assert!(!PositionAccuracy::SCENE_INFERENCE.is_better_than(&PositionAccuracy::TELEMETRY));
    }

    #[test]
    fn test_position_accuracy_partial_ord() {
        // Higher accuracy (lower meters) should be "greater" in ordering
        assert!(PositionAccuracy::TELEMETRY > PositionAccuracy::MANUAL_REFERENCE);
        assert!(PositionAccuracy::MANUAL_REFERENCE > PositionAccuracy::SCENE_INFERENCE);
    }

    #[test]
    fn test_aircraft_state_from_telemetry() {
        let state = AircraftState::from_telemetry(53.5, 10.0, 90.0, 120.0, 10000.0);

        assert_eq!(state.latitude, 53.5);
        assert_eq!(state.longitude, 10.0);
        assert_eq!(state.heading, 90.0);
        assert_eq!(state.ground_speed, 120.0);
        assert_eq!(state.altitude, 10000.0);
        assert_eq!(state.source, PositionSource::Telemetry);
        assert_eq!(state.accuracy, PositionAccuracy::TELEMETRY);
        assert!(state.has_vectors());
    }

    #[test]
    fn test_aircraft_state_from_manual_reference() {
        let state = AircraftState::from_manual_reference(43.6, 1.4);

        assert_eq!(state.latitude, 43.6);
        assert_eq!(state.longitude, 1.4);
        assert_eq!(state.heading, 0.0);
        assert_eq!(state.ground_speed, 0.0);
        assert_eq!(state.altitude, 0.0);
        assert_eq!(state.source, PositionSource::ManualReference);
        assert_eq!(state.accuracy, PositionAccuracy::MANUAL_REFERENCE);
        assert!(!state.has_vectors());
    }

    #[test]
    fn test_aircraft_state_from_inference() {
        let state = AircraftState::from_inference(53.5, 10.0);

        assert_eq!(state.source, PositionSource::SceneInference);
        assert_eq!(state.accuracy, PositionAccuracy::SCENE_INFERENCE);
        assert!(!state.has_vectors());
    }

    #[test]
    fn test_telemetry_status_display() {
        assert_eq!(TelemetryStatus::Connected.to_string(), "Connected");
        assert_eq!(TelemetryStatus::Disconnected.to_string(), "Disconnected");
    }

    #[test]
    fn test_position_source_display() {
        assert_eq!(PositionSource::Telemetry.to_string(), "Telemetry");
        assert_eq!(PositionSource::ManualReference.to_string(), "Manual");
        assert_eq!(PositionSource::SceneInference.to_string(), "Inferred");
    }

    #[test]
    fn test_aircraft_position_status_default() {
        let status = AircraftPositionStatus::default();

        assert!(status.state.is_none());
        assert_eq!(status.telemetry_status, TelemetryStatus::Disconnected);
        assert!(!status.vectors_available);
    }

    #[test]
    fn test_aircraft_position_status_from_state() {
        let state = AircraftState::from_telemetry(53.5, 10.0, 90.0, 120.0, 10000.0);
        let status = AircraftPositionStatus::from_state(state, TelemetryStatus::Connected);

        assert!(status.state.is_some());
        assert_eq!(status.telemetry_status, TelemetryStatus::Connected);
        assert!(status.vectors_available);
    }
}
