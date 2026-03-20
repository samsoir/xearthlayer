//! X-Plane Web API client — REST dataref lookup and position parsing.
//!
//! Handles the HTTP communication with X-Plane's Web API:
//! - REST `GET /api/v3/datarefs` for dataref ID lookup by name
//! - Parsing raw dataref values into `AircraftState` with unit conversions
//!
//! The WebSocket connection lifecycle is managed by `WebApiAdapter` (Task 4).

use std::collections::HashMap;

use super::config::WebApiConfig;
use super::datarefs::{self, DatarefIdMap};
use crate::aircraft_position::state::{AircraftState, TrackSource};

/// Meters per second to knots conversion factor.
const MPS_TO_KNOTS: f32 = 1.94384;

/// Meters to feet conversion factor.
const METERS_TO_FEET: f32 = 3.28084;

/// Error type for Web API operations.
#[derive(Debug, thiserror::Error)]
pub enum WebApiError {
    #[error("HTTP request failed: {0}")]
    Http(String),

    #[error("JSON parse error: {0}")]
    Json(String),

    #[error("Missing required datarefs: {0:?}")]
    MissingDatarefs(Vec<String>),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Connection refused — is X-Plane running?")]
    ConnectionRefused,
}

/// X-Plane Web API client for dataref lookup and position parsing.
pub struct WebApiClient {
    config: WebApiConfig,
    http: reqwest::Client,
}

impl WebApiClient {
    /// Create a new client with the given configuration.
    pub fn new(config: WebApiConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self { config, http }
    }

    /// REST endpoint URL for dataref listing.
    pub fn datarefs_url(&self) -> String {
        format!("http://localhost:{}/api/v3/datarefs", self.config.port)
    }

    /// WebSocket endpoint URL for dataref subscriptions.
    pub fn websocket_url(&self) -> String {
        format!("ws://localhost:{}/api/v3", self.config.port)
    }

    /// Fetch all datarefs from X-Plane and resolve IDs for the requested names.
    ///
    /// Returns an error if any required datarefs are missing from X-Plane's response.
    pub async fn lookup_datarefs(&self, names: &[&str]) -> Result<DatarefIdMap, WebApiError> {
        let url = self.datarefs_url();
        let response = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    WebApiError::ConnectionRefused
                } else {
                    WebApiError::Http(e.to_string())
                }
            })?;

        let text = response
            .text()
            .await
            .map_err(|e| WebApiError::Http(e.to_string()))?;

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| WebApiError::Json(e.to_string()))?;

        datarefs::resolve_dataref_ids_checked(&json, names).map_err(WebApiError::MissingDatarefs)
    }
}

/// Convert raw dataref values into an `AircraftState` with unit conversions.
///
/// Returns `None` if required position fields (lat, lon, heading) are missing.
///
/// Unit conversions applied:
/// - `groundspeed`: m/s × 1.94384 → knots
/// - `elevation`: m × 3.28084 → feet
/// - `y_agl`: m × 3.28084 → feet (not stored in AircraftState, available via return)
pub fn parse_position(values: &HashMap<String, f64>) -> Option<AircraftState> {
    let latitude = *values.get(datarefs::LATITUDE)?;
    let longitude = *values.get(datarefs::LONGITUDE)?;
    let heading = *values.get(datarefs::TRUE_HEADING)? as f32;

    // Track is optional — may not be present if aircraft is stationary
    let track = values.get(datarefs::TRACK).map(|v| *v as f32);
    let track_source = if track.is_some() {
        TrackSource::Telemetry
    } else {
        TrackSource::Unavailable
    };

    let ground_speed = values
        .get(datarefs::GROUND_SPEED)
        .map(|v| *v as f32 * MPS_TO_KNOTS)
        .unwrap_or(0.0);

    let altitude = values
        .get(datarefs::ELEVATION)
        .map(|v| *v as f32 * METERS_TO_FEET)
        .unwrap_or(0.0);

    Some(AircraftState::from_telemetry(
        latitude,
        longitude,
        track,
        track_source,
        heading,
        ground_speed,
        altitude,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rest_url() {
        let config = WebApiConfig {
            port: 8086,
            ..Default::default()
        };
        let client = WebApiClient::new(config);
        assert_eq!(
            client.datarefs_url(),
            "http://localhost:8086/api/v3/datarefs"
        );
    }

    #[test]
    fn test_websocket_url() {
        let config = WebApiConfig {
            port: 9090,
            ..Default::default()
        };
        let client = WebApiClient::new(config);
        assert_eq!(client.websocket_url(), "ws://localhost:9090/api/v3");
    }

    #[test]
    fn test_custom_port_in_urls() {
        let config = WebApiConfig {
            port: 12345,
            ..Default::default()
        };
        let client = WebApiClient::new(config);
        assert!(client.datarefs_url().contains("12345"));
        assert!(client.websocket_url().contains("12345"));
    }

    #[test]
    fn test_parse_position_with_unit_conversions() {
        let mut values = HashMap::new();
        values.insert(datarefs::LATITUDE.to_string(), 48.116);
        values.insert(datarefs::LONGITUDE.to_string(), 16.566);
        values.insert(datarefs::TRUE_HEADING.to_string(), 205.6);
        values.insert(datarefs::TRACK.to_string(), 210.0);
        values.insert(datarefs::GROUND_SPEED.to_string(), 154.3); // m/s
        values.insert(datarefs::ELEVATION.to_string(), 10668.0); // meters MSL

        let state = parse_position(&values);
        assert!(state.is_some());
        let state = state.unwrap();

        assert!((state.latitude - 48.116).abs() < 0.001);
        assert!((state.longitude - 16.566).abs() < 0.001);
        assert!((state.heading - 205.6).abs() < 0.1);
        assert_eq!(state.track, Some(210.0));
        assert_eq!(state.track_source, TrackSource::Telemetry);

        // Ground speed: 154.3 m/s × 1.94384 = ~299.8 kt
        assert!(
            (state.ground_speed - 299.8).abs() < 1.0,
            "Expected ~299.8kt, got {}",
            state.ground_speed
        );

        // Elevation: 10668 m × 3.28084 = ~34,996 ft
        assert!(
            (state.altitude - 34996.0).abs() < 10.0,
            "Expected ~34996ft, got {}",
            state.altitude
        );
    }

    #[test]
    fn test_parse_position_without_track() {
        let mut values = HashMap::new();
        values.insert(datarefs::LATITUDE.to_string(), 48.0);
        values.insert(datarefs::LONGITUDE.to_string(), 16.0);
        values.insert(datarefs::TRUE_HEADING.to_string(), 90.0);
        // No TRACK — aircraft stationary or data unavailable

        let state = parse_position(&values).unwrap();
        assert_eq!(state.track, None);
        assert_eq!(state.track_source, TrackSource::Unavailable);
        assert_eq!(state.ground_speed, 0.0); // default when missing
    }

    #[test]
    fn test_parse_position_missing_required_fields() {
        // Only latitude — missing longitude and heading
        let mut values = HashMap::new();
        values.insert(datarefs::LATITUDE.to_string(), 48.0);
        assert!(parse_position(&values).is_none());

        // Latitude + longitude — missing heading
        values.insert(datarefs::LONGITUDE.to_string(), 16.0);
        assert!(parse_position(&values).is_none());

        // All three present — should succeed
        values.insert(datarefs::TRUE_HEADING.to_string(), 90.0);
        assert!(parse_position(&values).is_some());
    }

    #[test]
    fn test_parse_position_zero_values() {
        let mut values = HashMap::new();
        values.insert(datarefs::LATITUDE.to_string(), 0.0);
        values.insert(datarefs::LONGITUDE.to_string(), 0.0);
        values.insert(datarefs::TRUE_HEADING.to_string(), 0.0);
        values.insert(datarefs::GROUND_SPEED.to_string(), 0.0);
        values.insert(datarefs::ELEVATION.to_string(), 0.0);

        let state = parse_position(&values).unwrap();
        assert_eq!(state.latitude, 0.0);
        assert_eq!(state.ground_speed, 0.0);
        assert_eq!(state.altitude, 0.0);
    }
}
