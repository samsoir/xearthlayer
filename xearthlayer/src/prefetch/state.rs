//! Aircraft state representation for prefetch prediction.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use super::scheduler::PrefetchStatsSnapshot;

/// GPS/telemetry connection status for UI display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpsStatus {
    /// XGPS2 configured and receiving data from X-Plane.
    Connected,
    /// XGPS2 configured but not yet receiving data.
    #[default]
    Acquiring,
    /// XGPS2 not configured; using FUSE-based position inference.
    Inferred,
}

impl std::fmt::Display for GpsStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "Connected"),
            Self::Acquiring => write!(f, "Acquiring..."),
            Self::Inferred => write!(f, "Inferred"),
        }
    }
}

/// Prefetch operating mode for UI display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefetchMode {
    /// Using UDP telemetry for heading-aware cone prefetch.
    #[default]
    Telemetry,
    /// Using FUSE request analysis for position/heading inference.
    FuseInference,
    /// Fallback to simple radial prefetch (no heading data).
    Radial,
    /// Prefetch system is idle (no data received yet).
    Idle,
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
    /// Tiles skipped due to TTL (recently attempted).
    pub ttl_skipped: u64,
    /// Zoom levels active in the most recent cycle.
    pub active_zoom_levels: Vec<u8>,
    /// Whether the prefetcher is actively submitting tiles.
    pub is_active: bool,
}

impl std::fmt::Display for PrefetchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telemetry => write!(f, "Heading-Aware (Telemetry)"),
            Self::FuseInference => write!(f, "Heading-Aware (Inferred)"),
            Self::Radial => write!(f, "Radial"),
            Self::Idle => write!(f, "Idle"),
        }
    }
}

/// Shared prefetch status for display in the UI.
///
/// This provides a thread-safe way to share the current prefetch state
/// (aircraft position and statistics) with the dashboard.
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

    /// Update the aircraft state from telemetry.
    ///
    /// This also sets GPS status to Connected since we received data.
    pub fn update_aircraft(&self, state: &AircraftState) {
        if let Ok(mut inner) = self.inner.write() {
            inner.aircraft = Some(AircraftSnapshot {
                latitude: state.latitude,
                longitude: state.longitude,
                heading: state.heading,
                ground_speed: state.ground_speed,
                altitude: state.altitude,
            });
            // Receiving telemetry means GPS is connected
            inner.gps_status = GpsStatus::Connected;
        }
    }

    /// Update the prefetch statistics.
    pub fn update_stats(&self, stats: PrefetchStatsSnapshot) {
        if let Ok(mut inner) = self.inner.write() {
            inner.stats = stats;
        }
    }

    /// Update the GPS connection status.
    pub fn update_gps_status(&self, status: GpsStatus) {
        if let Ok(mut inner) = self.inner.write() {
            inner.gps_status = status;
        }
    }

    /// Update the current prefetch operating mode.
    pub fn update_prefetch_mode(&self, mode: PrefetchMode) {
        if let Ok(mut inner) = self.inner.write() {
            inner.prefetch_mode = mode;
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

    /// Get a snapshot of the current status.
    pub fn snapshot(&self) -> PrefetchStatusSnapshot {
        self.inner.read().map(|r| r.clone()).unwrap_or_default()
    }
}

/// Snapshot of prefetch status for display.
#[derive(Debug, Clone, Default)]
pub struct PrefetchStatusSnapshot {
    /// Current aircraft state (if telemetry is active).
    pub aircraft: Option<AircraftSnapshot>,
    /// Prefetch statistics.
    pub stats: PrefetchStatsSnapshot,
    /// GPS/telemetry connection status.
    pub gps_status: GpsStatus,
    /// Current prefetch operating mode.
    pub prefetch_mode: PrefetchMode,
    /// Detailed prefetch statistics for dashboard display.
    pub detailed_stats: Option<DetailedPrefetchStats>,
}

/// Aircraft state snapshot for display (without Instant which can't be cloned easily).
#[derive(Debug, Clone)]
pub struct AircraftSnapshot {
    pub latitude: f64,
    pub longitude: f64,
    pub heading: f32,
    pub ground_speed: f32,
    pub altitude: f32,
}

impl PrefetchStatusSnapshot {
    /// Format the aircraft state as a single line.
    pub fn aircraft_line(&self) -> String {
        if let Some(ref ac) = self.aircraft {
            format!(
                "Pos: {:.4}°, {:.4}° | Hdg: {:.0}° | GS: {:.0}kt | Alt: {:.0}ft",
                ac.latitude, ac.longitude, ac.heading, ac.ground_speed, ac.altitude
            )
        } else {
            "Waiting for X-Plane telemetry...".to_string()
        }
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
    ) -> Self {
        Self {
            latitude,
            longitude,
            heading,
            ground_speed,
            altitude,
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
    use super::*;

    #[test]
    fn test_extrapolate_north() {
        // Aircraft at 0,0 heading north at 60 knots (1 nm/min)
        let state = AircraftState::new(0.0, 0.0, 0.0, 60.0, 10000.0);

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
        let state = AircraftState::new(0.0, 0.0, 90.0, 60.0, 10000.0);

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
        let state = AircraftState::new(45.0, -122.0, 90.0, 0.0, 5000.0);

        let (lat, lon) = state.extrapolate(600.0);

        assert!((lat - 45.0).abs() < 0.0001, "Should not move");
        assert!((lon - (-122.0)).abs() < 0.0001, "Should not move");
    }

    #[test]
    fn test_has_significant_change_heading() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0);
        let state2 = AircraftState::new(45.0, -122.0, 110.0, 120.0, 10000.0);

        assert!(state1.has_significant_change(&state2));
    }

    #[test]
    fn test_has_significant_change_heading_wraparound() {
        let state1 = AircraftState::new(45.0, -122.0, 5.0, 120.0, 10000.0);
        let state2 = AircraftState::new(45.0, -122.0, 355.0, 120.0, 10000.0);

        // 10 degree difference, not significant
        assert!(!state1.has_significant_change(&state2));

        let state3 = AircraftState::new(45.0, -122.0, 340.0, 120.0, 10000.0);
        // 25 degree difference, significant
        assert!(state1.has_significant_change(&state3));
    }

    #[test]
    fn test_has_significant_change_speed() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0);
        let state2 = AircraftState::new(45.0, -122.0, 90.0, 160.0, 10000.0);

        // 33% speed change (40/120), significant
        assert!(state1.has_significant_change(&state2));
    }

    #[test]
    fn test_no_significant_change() {
        let state1 = AircraftState::new(45.0, -122.0, 90.0, 120.0, 10000.0);
        let state2 = AircraftState::new(45.001, -122.001, 92.0, 125.0, 10000.0);

        // Small changes in all dimensions, not significant
        assert!(!state1.has_significant_change(&state2));
    }
}
