//! Aircraft state representation for prefetch prediction.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use super::adaptive::FlightPhase;

/// Snapshot of prefetch statistics for UI display.
///
/// This struct tracks cumulative prefetch activity metrics that are
/// displayed in the dashboard to give users visibility into prefetch
/// performance.
#[derive(Debug, Clone, Default)]
pub struct PrefetchStatsSnapshot {
    /// Number of prediction cycles completed.
    pub prediction_cycles: u64,
    /// Total tiles submitted for prefetch.
    pub tiles_submitted: u64,
    /// Tiles skipped because they were already in-flight.
    pub tiles_in_flight_skipped: u64,
    /// Total tiles predicted (may exceed submitted due to filtering).
    pub tiles_predicted: u64,
}

/// What the prefetcher is doing right now, in one word for the dashboard.
///
/// Resolved in precedence order by [`resolve_state`]: the first condition
/// that holds wins, so a disabled prefetcher never reports "ramping".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefetchState {
    /// Prefetch is switched off (`prefetch.enabled = false` or
    /// `run --no-prefetch`).
    Disabled,
    /// The prefetch lane is stood down because FUSE demand is in flight.
    PausedForDemand,
    /// Held back during the takeoff transition.
    HoldingTakeoff,
    /// The last cycle was skipped because the lane or memory was loaded.
    DeferredLoad,
    /// Throttle ramping back up after takeoff; `percent` of full throughput.
    Ramping { percent: u8 },
    /// Tiles are in flight on the prefetch lane.
    Active,
    /// Enabled, nothing to do.
    #[default]
    Idle,
}

impl PrefetchState {
    /// Dashboard text for the state.
    pub fn label(&self) -> String {
        match self {
            Self::Disabled => "Disabled".to_string(),
            Self::PausedForDemand => "Paused (demand)".to_string(),
            Self::HoldingTakeoff => "Holding (takeoff)".to_string(),
            Self::DeferredLoad => "Deferred (load)".to_string(),
            Self::Ramping { percent } => format!("Ramping {percent}%"),
            Self::Active => "Active".to_string(),
            Self::Idle => "Idle".to_string(),
        }
    }
}

/// Everything [`resolve_state`] needs to choose a state.
#[derive(Debug, Clone, Copy)]
pub struct StateInputs {
    pub enabled: bool,
    pub demand_paused: bool,
    pub phase: FlightPhase,
    pub deferred_last_cycle: bool,
    /// `None` when the throttle is idle (full throughput).
    pub throttle_fraction: Option<f64>,
    pub lane_busy: bool,
}

impl Default for StateInputs {
    /// A running prefetcher with nothing to report yet.
    fn default() -> Self {
        Self {
            enabled: true,
            demand_paused: false,
            phase: FlightPhase::default(),
            deferred_last_cycle: false,
            throttle_fraction: None,
            lane_busy: false,
        }
    }
}

/// Choose the single state to show, first match in precedence order.
pub fn resolve_state(inputs: StateInputs) -> PrefetchState {
    if !inputs.enabled {
        return PrefetchState::Disabled;
    }
    if inputs.demand_paused {
        return PrefetchState::PausedForDemand;
    }
    if inputs.phase == FlightPhase::Transition || inputs.throttle_fraction == Some(0.0) {
        return PrefetchState::HoldingTakeoff;
    }
    if inputs.deferred_last_cycle {
        return PrefetchState::DeferredLoad;
    }
    if let Some(fraction) = inputs.throttle_fraction.filter(|f| *f < 1.0) {
        return PrefetchState::Ramping {
            percent: (fraction * 100.0).round().clamp(0.0, 100.0) as u8,
        };
    }
    if inputs.lane_busy {
        PrefetchState::Active
    } else {
        PrefetchState::Idle
    }
}

/// Detailed prefetch statistics for dashboard display.
///
/// Provides real-time visibility into prefetch activity beyond the basic
/// mode indicator, allowing users to diagnose performance issues.
#[derive(Debug, Clone, Default)]
pub struct DetailedPrefetchStats {
    /// Total prefetch cycles completed.
    pub cycles: u64,
    /// Tiles submitted in the most recent cycle.
    pub tiles_submitted_last_cycle: u64,
    /// Total tiles submitted across all cycles.
    pub tiles_submitted_total: u64,
    /// Tiles skipped because they were already in cache.
    pub cache_hits: u64,
    /// Zoom levels active in the most recent cycle.
    pub active_zoom_levels: Vec<u8>,
    /// Tile coordinates currently being prefetched (limited to 10 for display).
    /// Each tuple is (latitude, longitude) of the tile's southwest corner.
    pub loading_tiles: Vec<(i32, i32)>,
    /// Number of prefetch cycles deferred due to executor backpressure.
    pub deferred_cycles: u64,
}

/// Shared prefetch status for display in the UI.
///
/// This provides a thread-safe way to share the current prefetch state
/// (state inputs, box geometry and statistics) with the dashboard.
#[derive(Debug, Default)]
pub struct SharedPrefetchStatus {
    inner: RwLock<PrefetchStatusSnapshot>,
}

impl SharedPrefetchStatus {
    /// Create a new shared prefetch status.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(PrefetchStatusSnapshot::default()),
        })
    }

    /// Update the prefetch statistics.
    pub fn update_stats(&self, stats: PrefetchStatsSnapshot) {
        if let Ok(mut inner) = self.inner.write() {
            inner.stats = stats;
        }
    }

    /// Publish the coordinator's raw state inputs. The dashboard completes
    /// them (demand pause, lane occupancy) and calls [`resolve_state`] once.
    pub fn update_state_inputs(&self, inputs: StateInputs) {
        if let Ok(mut inner) = self.inner.write() {
            inner.state_inputs = inputs;
        }
    }

    /// Record that no coordinator runs (`--no-prefetch`, `prefetch.enabled = false`).
    pub fn mark_disabled(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.state_inputs.enabled = false;
        }
    }

    /// Update the detailed prefetch statistics.
    ///
    /// Called by the prefetcher after each cycle to provide real-time
    /// visibility into prefetch activity.
    pub fn update_detailed_stats(&self, stats: DetailedPrefetchStats) {
        if let Ok(mut inner) = self.inner.write() {
            inner.detailed_stats = Some(stats);
        }
    }

    /// Update the current prefetch box extent.
    ///
    /// Called by the coordinator after each cruise-phase cycle with the
    /// speed-proportional extent so the debug map can render it accurately.
    pub fn update_box_extent(&self, extent: f64) {
        if let Ok(mut inner) = self.inner.write() {
            inner.box_extent = extent;
        }
    }

    /// Update the current prefetch box bounds.
    ///
    /// Called by the coordinator with the actual geographic bounds used
    /// for region enumeration. The debug map consumes these directly
    /// rather than recomputing, ensuring the overlay matches reality
    /// (including phase-specific bias: symmetric on ground, heading-biased
    /// in cruise). `None` means no active box this cycle (Transition,
    /// disabled, or no position).
    pub fn update_box_bounds(&self, bounds: Option<BoxBoundsSnapshot>) {
        if let Ok(mut inner) = self.inner.write() {
            inner.box_bounds = bounds;
        }
    }

    /// Get a snapshot of the current status.
    pub fn snapshot(&self) -> PrefetchStatusSnapshot {
        self.inner.read().map(|r| r.clone()).unwrap_or_default()
    }
}

/// Snapshot of prefetch status for display.
#[derive(Debug, Clone, Default)]
pub struct PrefetchStatusSnapshot {
    /// Prefetch statistics.
    pub stats: PrefetchStatsSnapshot,
    /// Raw inputs to [`resolve_state`], as last published by the coordinator.
    pub state_inputs: StateInputs,
    /// Detailed prefetch statistics for dashboard display.
    pub detailed_stats: Option<DetailedPrefetchStats>,
    /// Current prefetch box extent in degrees (speed-proportional; 0.0 when unknown).
    pub box_extent: f64,
    /// Current prefetch box bounds as used by the coordinator this cycle.
    ///
    /// Populated directly from the coordinator's computed bounds — the
    /// debug map reads this to render the overlay, which guarantees the
    /// UI matches the actual prefetch geometry (no drift, no recomputation).
    pub box_bounds: Option<BoxBoundsSnapshot>,
}

/// Geographic bounds of the current prefetch box.
///
/// Published by the coordinator each cycle so UI consumers (debug map,
/// dashboard) render the *actual* box used for region enumeration rather
/// than reconstructing one from config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxBoundsSnapshot {
    pub lat_min: f64,
    pub lat_max: f64,
    pub lon_min: f64,
    pub lon_max: f64,
}

impl PrefetchStatusSnapshot {
    /// The state as the coordinator alone sees it (no demand pause).
    pub fn coordinator_state(&self) -> PrefetchState {
        resolve_state(self.state_inputs)
    }

    /// Format the prefetch stats as a single line.
    pub fn stats_line(&self) -> String {
        if self.stats.prediction_cycles > 0 {
            format!(
                "Prefetch: {} submitted, {} in-flight skipped, {} predicted (cycle #{})",
                self.stats.tiles_submitted,
                self.stats.tiles_in_flight_skipped,
                self.stats.tiles_predicted,
                self.stats.prediction_cycles
            )
        } else {
            "Prefetch: Idle".to_string()
        }
    }
}

/// Current aircraft position and motion.
///
/// This struct holds the latest telemetry data received from X-Plane
/// and is used by the prediction engine to calculate which tiles
/// should be pre-fetched.
#[derive(Debug, Clone)]
pub struct AircraftState {
    /// Latitude in degrees (-90 to 90).
    pub latitude: f64,
    /// Longitude in degrees (-180 to 180).
    pub longitude: f64,
    /// True heading in degrees (0-360).
    pub heading: f32,
    /// Ground speed in knots.
    pub ground_speed: f32,
    /// Altitude MSL in feet.
    pub altitude: f32,
    /// Whether the aircraft is on the ground (from SimState).
    pub on_ground: bool,
    /// Timestamp when this state was received.
    pub updated_at: Instant,
}

impl AircraftState {
    /// Create a new aircraft state.
    pub fn new(
        latitude: f64,
        longitude: f64,
        heading: f32,
        ground_speed: f32,
        altitude: f32,
        on_ground: bool,
    ) -> Self {
        Self {
            latitude,
            longitude,
            heading,
            ground_speed,
            altitude,
            on_ground,
            updated_at: Instant::now(),
        }
    }

    /// Calculate the position at a future time based on current velocity.
    ///
    /// Uses simple dead reckoning: extrapolates position along the heading
    /// at the current ground speed.
    ///
    /// # Arguments
    ///
    /// * `seconds_ahead` - How many seconds in the future to predict
    ///
    /// # Returns
    ///
    /// A tuple of (latitude, longitude) at the predicted position.
    pub fn extrapolate(&self, seconds_ahead: f32) -> (f64, f64) {
        // Convert ground speed from knots to nautical miles per second
        let speed_nm_per_sec = self.ground_speed as f64 / 3600.0;

        // Distance traveled in nautical miles
        let distance_nm = speed_nm_per_sec * seconds_ahead as f64;

        // Convert heading to radians (use f64 for precision)
        let heading_rad = (self.heading as f64).to_radians();

        // Convert distance to degrees (1 nm ≈ 1/60 degree at equator)
        // For latitude, this is accurate. For longitude, we need to adjust for latitude.
        let distance_deg = distance_nm / 60.0;

        // Calculate displacement
        let delta_lat = distance_deg * heading_rad.cos();
        let delta_lon = distance_deg * heading_rad.sin() / self.latitude.to_radians().cos();

        let new_lat = (self.latitude + delta_lat).clamp(-85.05112878, 85.05112878);
        let new_lon = self.longitude + delta_lon;

        // Normalize longitude to -180..180
        let new_lon = if new_lon > 180.0 {
            new_lon - 360.0
        } else if new_lon < -180.0 {
            new_lon + 360.0
        } else {
            new_lon
        };

        (new_lat, new_lon)
    }

    /// Check if the state has changed significantly from another state.
    ///
    /// Used to determine if predictions need to be recalculated.
    /// Significant changes include:
    /// - Heading change > 15 degrees
    /// - Speed change > 20%
    /// - Position change > 0.01 degrees (~1km)
    pub fn has_significant_change(&self, other: &AircraftState) -> bool {
        // Heading change (accounting for wrap-around at 360)
        let heading_diff = (self.heading - other.heading).abs();
        let heading_diff = if heading_diff > 180.0 {
            360.0 - heading_diff
        } else {
            heading_diff
        };
        if heading_diff > 15.0 {
            return true;
        }

        // Speed change > 20% (avoid division by zero)
        if other.ground_speed > 1.0 {
            let speed_ratio = (self.ground_speed - other.ground_speed).abs() / other.ground_speed;
            if speed_ratio > 0.2 {
                return true;
            }
        }

        // Position change > ~1km
        let lat_diff = (self.latitude - other.latitude).abs();
        let lon_diff = (self.longitude - other.longitude).abs();
        if lat_diff > 0.01 || lon_diff > 0.01 {
            return true;
        }

        false
    }

    /// Get the age of this state (time since it was received).
    pub fn age(&self) -> std::time::Duration {
        self.updated_at.elapsed()
    }

    /// Check if this state is stale (older than the given duration).
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        self.age() > max_age
    }
}

#[cfg(test)]
mod tests {

    fn inputs() -> StateInputs {
        StateInputs {
            enabled: true,
            demand_paused: false,
            phase: FlightPhase::Cruise,
            deferred_last_cycle: false,
            throttle_fraction: None,
            lane_busy: false,
        }
    }

    #[test]
    fn disabled_beats_everything() {
        let all = StateInputs {
            enabled: false,
            demand_paused: true,
            phase: FlightPhase::Transition,
            deferred_last_cycle: true,
            throttle_fraction: Some(0.4),
            lane_busy: true,
        };
        assert_eq!(resolve_state(all), PrefetchState::Disabled);
    }

    #[test]
    fn demand_pause_beats_takeoff_hold_and_deferral() {
        let held = StateInputs {
            demand_paused: true,
            phase: FlightPhase::Transition,
            deferred_last_cycle: true,
            ..inputs()
        };
        assert_eq!(resolve_state(held), PrefetchState::PausedForDemand);
    }

    #[test]
    fn transition_phase_holds_for_takeoff() {
        let s = StateInputs {
            phase: FlightPhase::Transition,
            ..inputs()
        };
        assert_eq!(resolve_state(s), PrefetchState::HoldingTakeoff);
        let s = StateInputs {
            throttle_fraction: Some(0.0),
            ..inputs()
        };
        assert_eq!(resolve_state(s), PrefetchState::HoldingTakeoff);
    }

    #[test]
    fn deferral_beats_ramp_and_activity() {
        let s = StateInputs {
            deferred_last_cycle: true,
            throttle_fraction: Some(0.4),
            lane_busy: true,
            ..inputs()
        };
        assert_eq!(resolve_state(s), PrefetchState::DeferredLoad);
    }

    #[test]
    fn partial_throttle_is_a_ramp_with_percent() {
        let s = StateInputs {
            throttle_fraction: Some(0.4),
            lane_busy: true,
            ..inputs()
        };
        assert_eq!(resolve_state(s), PrefetchState::Ramping { percent: 40 });
    }

    #[test]
    fn busy_lane_is_active_and_quiet_lane_is_idle() {
        let busy = StateInputs {
            lane_busy: true,
            ..inputs()
        };
        assert_eq!(resolve_state(busy), PrefetchState::Active);
        assert_eq!(resolve_state(inputs()), PrefetchState::Idle);
    }

    #[test]
    fn mark_disabled_resolves_to_disabled_in_the_snapshot() {
        let status = SharedPrefetchStatus::new();
        assert_eq!(status.snapshot().coordinator_state(), PrefetchState::Idle);
        status.mark_disabled();
        assert_eq!(
            status.snapshot().coordinator_state(),
            PrefetchState::Disabled
        );
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(PrefetchState::PausedForDemand.label(), "Paused (demand)");
        assert_eq!(
            PrefetchState::Ramping { percent: 40 }.label(),
            "Ramping 40%"
        );
    }
    use super::*;

    #[test]
    fn test_extrapolate_north() {
        // Aircraft at 0,0 heading north at 60 knots (1 nm/min)
        let state = AircraftState::new(0.0, 0.0, 0.0, 60.0, 10000.0, false);

        // After 60 seconds, should be ~1 nm north (~0.0167 degrees)
        let (lat, lon) = state.extrapolate(60.0);

        assert!(
            (lat - 0.0167).abs() < 0.001,
            "Expected ~0.0167, got {}",
            lat
        );
        assert!(lon.abs() < 0.001, "Expected ~0, got {}", lon);
    }

    #[test]
    fn test_extrapolate_east() {
        // Aircraft at 0,0 heading east at 60 knots
        let state = AircraftState::new(0.0, 0.0, 90.0, 60.0, 10000.0, false);

        // After 60 seconds, should be ~1 nm east
        let (lat, lon) = state.extrapolate(60.0);

        assert!(lat.abs() < 0.001, "Expected ~0, got {}", lat);
        assert!(
            (lon - 0.0167).abs() < 0.001,
            "Expected ~0.0167, got {}",
            lon
        );
    }

    #[test]
    fn test_extrapolate_stationary() {
        // Aircraft at rest
        let state = AircraftState::new(45.0, -122.0, 90.0, 0.0, 5000.0, false);

        let (lat, lon) = state.extrapolate(600.0);

        assert!((lat - 45.0).abs() < 0.0001, "Should not move");
        assert!((lon - (-122.0)).abs() < 0.0001, "Should not move");
    }

    #[test]
    fn test_has_significant_change_heading() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0, false);
        let state2 = AircraftState::new(45.0, -122.0, 110.0, 120.0, 10000.0, false);

        assert!(state1.has_significant_change(&state2));
    }

    #[test]
    fn test_has_significant_change_heading_wraparound() {
        let state1 = AircraftState::new(45.0, -122.0, 5.0, 120.0, 10000.0, false);
        let state2 = AircraftState::new(45.0, -122.0, 355.0, 120.0, 10000.0, false);

        // 10 degree difference, not significant
        assert!(!state1.has_significant_change(&state2));

        let state3 = AircraftState::new(45.0, -122.0, 340.0, 120.0, 10000.0, false);
        // 25 degree difference, significant
        assert!(state1.has_significant_change(&state3));
    }

    #[test]
    fn test_has_significant_change_speed() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0, false);
        let state2 = AircraftState::new(45.0, -122.0, 90.0, 160.0, 10000.0, false);

        // 33% speed change (40/120), significant
        assert!(state1.has_significant_change(&state2));
    }

    #[test]
    fn test_no_significant_change() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0, false);
        let state2 = AircraftState::new(45.001, -122.001, 92.0, 125.0, 10000.0, false);

        // Small changes in all dimensions, not significant
        assert!(!state1.has_significant_change(&state2));
    }
}
