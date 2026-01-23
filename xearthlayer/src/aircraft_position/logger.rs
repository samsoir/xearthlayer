//! Periodic position logging daemon for flight analysis.
//!
//! This module provides a background task that logs aircraft position at regular
//! intervals, useful for post-flight analysis and debugging scenery loading patterns.
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::aircraft_position::{spawn_position_logger, SharedAircraftPosition};
//! use tokio_util::sync::CancellationToken;
//!
//! let cancellation = CancellationToken::new();
//! let handle = spawn_position_logger(
//!     aircraft_position,
//!     cancellation.clone(),
//!     std::time::Duration::from_secs(20),
//! );
//! ```
//!
//! # Output Format
//!
//! Logs are emitted at DEBUG level with structured fields:
//! - `lat`, `lon` - Position in decimal degrees
//! - `hdg` - Heading in degrees
//! - `gs_kt` - Ground speed in knots
//! - `alt_ft` - Altitude in feet
//! - `accuracy_m` - Position accuracy in meters
//! - `source` - Position source (Telemetry, ManualReference, Inference)
//! - `telemetry` - Telemetry connection status
//! - `dsf_tile` - X-Plane DSF tile name (e.g., "+53+009")

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::provider::AircraftPositionProvider;
use super::SharedAircraftPosition;

/// Default logging interval (20 seconds).
pub const DEFAULT_LOG_INTERVAL: Duration = Duration::from_secs(20);

/// Spawns a background task that periodically logs aircraft position.
///
/// The logger runs at DEBUG level only and stops when the cancellation token
/// is triggered. This is designed for flight analysis and debugging.
///
/// # Arguments
///
/// * `aircraft_position` - Shared position provider to query
/// * `cancellation` - Token to stop the logger
/// * `interval` - How often to log position (default: 20 seconds)
///
/// # Returns
///
/// A `JoinHandle` for the spawned task.
///
/// # Note
///
/// The caller should check if DEBUG logging is enabled before spawning
/// to avoid wasting resources when logs won't be recorded:
///
/// ```ignore
/// if tracing::enabled!(tracing::Level::DEBUG) {
///     spawn_position_logger(apt, cancel, Duration::from_secs(20));
/// }
/// ```
pub fn spawn_position_logger(
    aircraft_position: SharedAircraftPosition,
    cancellation: CancellationToken,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    log_position(&aircraft_position);
                }
                _ = cancellation.cancelled() => {
                    tracing::debug!("APT position logger stopped");
                    break;
                }
            }
        }
    })
}

/// Logs the current aircraft position at DEBUG level.
///
/// Calculates the DSF tile name from coordinates and emits a structured log entry.
fn log_position(aircraft_position: &SharedAircraftPosition) {
    let status = aircraft_position.status();

    if let Some(state) = status.state {
        // Calculate DSF tile name from coordinates
        let dsf_tile = format_dsf_tile(state.latitude, state.longitude);

        tracing::debug!(
            lat = format!("{:.5}", state.latitude),
            lon = format!("{:.5}", state.longitude),
            hdg = format!("{:.1}", state.heading),
            gs_kt = format!("{:.0}", state.ground_speed),
            alt_ft = format!("{:.0}", state.altitude),
            accuracy_m = format!("{:.0}", state.accuracy.meters()),
            source = ?state.source,
            telemetry = ?status.telemetry_status,
            dsf_tile = %dsf_tile,
            "APT position update"
        );
    } else {
        tracing::debug!(
            telemetry = ?status.telemetry_status,
            "APT position update (no position data)"
        );
    }
}

/// Formats coordinates as an X-Plane DSF tile name.
///
/// DSF tiles are 1°×1° tiles named like "+53+009" (53°N, 9°E).
fn format_dsf_tile(latitude: f64, longitude: f64) -> String {
    let dsf_lat = latitude.floor() as i32;
    let dsf_lon = longitude.floor() as i32;

    let lat_sign = if dsf_lat >= 0 { '+' } else { '-' };
    let lon_sign = if dsf_lon >= 0 { '+' } else { '-' };

    format!(
        "{}{:02}{}{:03}",
        lat_sign,
        dsf_lat.abs(),
        lon_sign,
        dsf_lon.abs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_dsf_tile_positive() {
        assert_eq!(format_dsf_tile(53.5, 9.2), "+53+009");
        assert_eq!(format_dsf_tile(0.5, 0.5), "+00+000");
    }

    #[test]
    fn test_format_dsf_tile_negative() {
        assert_eq!(format_dsf_tile(-33.9, 151.2), "-34+151");
        assert_eq!(format_dsf_tile(51.5, -0.1), "+51-001");
    }

    #[test]
    fn test_format_dsf_tile_edge_cases() {
        // Right on boundary
        assert_eq!(format_dsf_tile(53.0, 9.0), "+53+009");
        // Just below boundary
        assert_eq!(format_dsf_tile(52.999, 8.999), "+52+008");
    }
}
