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

    /// Online network (VATSIM/IVAO/PilotEdge) - meter-level precision.
    ///
    /// Position data from online ATC network APIs (updated every ~15 seconds).
    /// Same accuracy as telemetry, but confidence decays with timestamp age.
    pub const ONLINE_NETWORK: Self = Self(10.0);

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

    /// Get the accuracy value in meters.
    #[inline]
    pub fn meters(&self) -> f32 {
        self.0
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
    /// From an online ATC network (VATSIM, IVAO, PilotEdge).
    OnlineNetwork,
    /// User-provided reference point (airport ICAO, manual coordinates, flight plan, etc.).
    ManualReference,
    /// Inferred from Scene Tracker loaded bounds.
    SceneInference,
}

/// Source of ground track data.
///
/// Indicates how the track value was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSource {
    /// From XGPS2 telemetry (authoritative GPS track).
    Telemetry,
    /// Calculated from position history.
    Derived,
    /// Not available (insufficient data or non-telemetry source).
    #[default]
    Unavailable,
}

impl std::fmt::Display for TrackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telemetry => write!(f, "Telemetry"),
            Self::Derived => write!(f, "Derived"),
            Self::Unavailable => write!(f, "Unavailable"),
        }
    }
}

impl std::fmt::Display for PositionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telemetry => write!(f, "Telemetry"),
            Self::OnlineNetwork => write!(f, "Network"),
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
/// # Track vs Heading
///
/// - **Track** (`track`): The actual ground path direction - where the aircraft
///   is moving over the ground. Comes from XGPS2's track field or derived from
///   position history. Used by prefetch for band calculation.
///
/// - **Heading** (`heading`): Where the aircraft nose is pointing. Comes from
///   XATT2's heading field. Differs from track due to wind (crosswind correction).
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

    /// Ground track in degrees (0-360).
    ///
    /// The actual direction of movement over the ground.
    /// From XGPS2's track field or derived from position history.
    /// `None` if not available (e.g., stationary or from non-telemetry source).
    pub track: Option<f32>,

    /// How the track value was determined.
    pub track_source: TrackSource,

    /// True heading in degrees (0-360).
    ///
    /// Where the aircraft nose is pointing (from XATT2).
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
    ///
    /// # Arguments
    ///
    /// * `latitude` - Latitude in degrees
    /// * `longitude` - Longitude in degrees
    /// * `track` - Ground track in degrees (from XGPS2), or None if not available
    /// * `track_source` - How the track was determined
    /// * `heading` - True heading in degrees (from XATT2)
    /// * `ground_speed` - Ground speed in knots
    /// * `altitude` - Altitude MSL in feet
    pub fn from_telemetry(
        latitude: f64,
        longitude: f64,
        track: Option<f32>,
        track_source: TrackSource,
        heading: f32,
        ground_speed: f32,
        altitude: f32,
    ) -> Self {
        Self {
            latitude,
            longitude,
            track,
            track_source,
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
            track: None,
            track_source: TrackSource::Unavailable,
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
            track: None,
            track_source: TrackSource::Unavailable,
            heading: 0.0,
            ground_speed: 0.0,
            altitude: 0.0,
            timestamp: Instant::now(),
            source: PositionSource::SceneInference,
            accuracy: PositionAccuracy::SCENE_INFERENCE,
        }
    }

    /// Create a new aircraft state from an online network position (VATSIM/IVAO/PilotEdge).
    ///
    /// Online networks provide heading and ground speed but not GPS track.
    /// Track is set to `None` (derived later by the aggregator from position history).
    ///
    /// # Arguments
    ///
    /// * `latitude` - Latitude in degrees
    /// * `longitude` - Longitude in degrees
    /// * `heading` - Heading in degrees (from network data)
    /// * `ground_speed` - Ground speed in knots
    /// * `altitude` - Altitude MSL in feet
    /// * `timestamp` - When the network reported this position
    pub fn from_network_position(
        latitude: f64,
        longitude: f64,
        heading: f32,
        ground_speed: f32,
        altitude: f32,
        timestamp: Instant,
    ) -> Self {
        Self {
            latitude,
            longitude,
            track: None,
            track_source: TrackSource::Unavailable,
            heading,
            ground_speed,
            altitude,
            timestamp,
            source: PositionSource::OnlineNetwork,
            accuracy: PositionAccuracy::ONLINE_NETWORK,
        }
    }

    /// Get the effective track for prefetch calculations.
    ///
    /// Returns the ground track if available, otherwise falls back to heading.
    /// For non-telemetry sources, returns None.
    pub fn effective_track(&self) -> Option<f32> {
        if !self.has_vectors() {
            return None;
        }
        // Prefer track if available, fall back to heading
        self.track.or(Some(self.heading))
    }

    /// Update track with derived value if no authoritative track available.
    ///
    /// Used by the aggregator to enrich state with position-derived track.
    pub fn with_derived_track(mut self, track: f32) -> Self {
        if self.track.is_none() {
            self.track = Some(track);
            self.track_source = TrackSource::Derived;
        }
        self
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
    /// Returns true for sources that provide actual flight vector data:
    /// telemetry (XGPS2) and online networks (VATSIM/IVAO/PilotEdge).
    pub fn has_vectors(&self) -> bool {
        matches!(
            self.source,
            PositionSource::Telemetry | PositionSource::OnlineNetwork
        )
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
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            Some(88.0),
            TrackSource::Telemetry,
            90.0,
            120.0,
            10000.0,
        );

        assert_eq!(state.latitude, 53.5);
        assert_eq!(state.longitude, 10.0);
        assert_eq!(state.track, Some(88.0)); // Ground track from XGPS2
        assert_eq!(state.track_source, TrackSource::Telemetry);
        assert_eq!(state.heading, 90.0); // Heading from XATT2
        assert_eq!(state.ground_speed, 120.0);
        assert_eq!(state.altitude, 10000.0);
        assert_eq!(state.source, PositionSource::Telemetry);
        assert_eq!(state.accuracy, PositionAccuracy::TELEMETRY);
        assert!(state.has_vectors());
    }

    #[test]
    fn test_effective_track_prefers_track_over_heading() {
        // With track available
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            Some(88.0),
            TrackSource::Telemetry,
            90.0,
            120.0,
            10000.0,
        );
        assert_eq!(state.effective_track(), Some(88.0)); // Prefers track

        // Without track, falls back to heading
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            None,
            TrackSource::Unavailable,
            90.0,
            120.0,
            10000.0,
        );
        assert_eq!(state.effective_track(), Some(90.0)); // Falls back to heading
    }

    #[test]
    fn test_effective_track_none_for_non_telemetry() {
        let state = AircraftState::from_manual_reference(43.6, 1.4);
        assert_eq!(state.effective_track(), None);
        assert_eq!(state.track_source, TrackSource::Unavailable);

        let state = AircraftState::from_inference(53.5, 10.0);
        assert_eq!(state.effective_track(), None);
        assert_eq!(state.track_source, TrackSource::Unavailable);
    }

    #[test]
    fn test_with_derived_track() {
        // State without track
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            None,
            TrackSource::Unavailable,
            90.0,
            120.0,
            10000.0,
        );

        // Enrich with derived track
        let enriched = state.with_derived_track(85.0);
        assert_eq!(enriched.track, Some(85.0));
        assert_eq!(enriched.track_source, TrackSource::Derived);
    }

    #[test]
    fn test_with_derived_track_preserves_authoritative() {
        // State with authoritative track
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            Some(88.0),
            TrackSource::Telemetry,
            90.0,
            120.0,
            10000.0,
        );

        // Derived track should NOT overwrite authoritative track
        let enriched = state.with_derived_track(85.0);
        assert_eq!(enriched.track, Some(88.0)); // Preserved
        assert_eq!(enriched.track_source, TrackSource::Telemetry); // Preserved
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
        let state = AircraftState::from_telemetry(
            53.5,
            10.0,
            Some(88.0),
            TrackSource::Telemetry,
            90.0,
            120.0,
            10000.0,
        );
        let status = AircraftPositionStatus::from_state(state, TelemetryStatus::Connected);

        assert!(status.state.is_some());
        assert_eq!(status.telemetry_status, TelemetryStatus::Connected);
        assert!(status.vectors_available);
    }

    #[test]
    fn test_track_source_display() {
        assert_eq!(TrackSource::Telemetry.to_string(), "Telemetry");
        assert_eq!(TrackSource::Derived.to_string(), "Derived");
        assert_eq!(TrackSource::Unavailable.to_string(), "Unavailable");
    }

    #[test]
    fn test_online_network_accuracy_constant() {
        // Same accuracy as telemetry (10m)
        assert_eq!(PositionAccuracy::ONLINE_NETWORK.meters(), 10.0);
        assert_eq!(
            PositionAccuracy::ONLINE_NETWORK,
            PositionAccuracy::TELEMETRY
        );
    }

    #[test]
    fn test_aircraft_state_from_network_position() {
        let timestamp = Instant::now();
        let state = AircraftState::from_network_position(
            33.9425, -118.408, 270.0, 150.0, 35000.0, timestamp,
        );

        assert_eq!(state.latitude, 33.9425);
        assert_eq!(state.longitude, -118.408);
        assert_eq!(state.heading, 270.0);
        assert_eq!(state.ground_speed, 150.0);
        assert_eq!(state.altitude, 35000.0);
        assert_eq!(state.source, PositionSource::OnlineNetwork);
        assert_eq!(state.accuracy, PositionAccuracy::ONLINE_NETWORK);
        assert_eq!(state.track, None); // No track from network
        assert_eq!(state.track_source, TrackSource::Unavailable);
        assert!(state.has_vectors());
    }

    #[test]
    fn test_network_position_effective_track_falls_back_to_heading() {
        let state = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            Instant::now(),
        );

        // has_vectors() is true for OnlineNetwork, so effective_track() should
        // fall back to heading when track is None
        assert_eq!(state.effective_track(), Some(270.0));
    }

    #[test]
    fn test_position_source_display_network() {
        assert_eq!(PositionSource::OnlineNetwork.to_string(), "Network");
    }

    #[test]
    fn test_online_network_has_vectors() {
        let state = AircraftState::from_network_position(
            33.9425,
            -118.408,
            270.0,
            150.0,
            35000.0,
            Instant::now(),
        );
        assert!(state.has_vectors());

        // Contrast with non-vector sources
        assert!(!AircraftState::from_manual_reference(43.6, 1.4).has_vectors());
        assert!(!AircraftState::from_inference(53.5, 10.0).has_vectors());
    }
}
