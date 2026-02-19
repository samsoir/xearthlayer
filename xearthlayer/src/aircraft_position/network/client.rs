//! Network client trait and VATSIM implementation.
//!
//! The [`NetworkClient`] trait abstracts over different online ATC networks,
//! allowing the adapter to work with any network that provides pilot position data.
//! The [`VatsimClient`] implementation fetches pilot data directly from the
//! VATSIM V3 JSON data feed via `reqwest`.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

use super::error::NetworkError;

/// Default HTTP timeout for fetching the VATSIM data feed.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Pilot position data from an online ATC network.
///
/// This is our own type, decoupled from any third-party crate.
/// Only includes the fields needed for position tracking.
#[derive(Debug, Clone, Deserialize)]
pub struct PilotPosition {
    pub cid: u64,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: i32,
    pub groundspeed: i32,
    pub heading: i32,
    pub last_updated: String,
}

/// Trait for fetching pilot position from an online ATC network.
///
/// Implementations should fetch the current position data for a specific pilot.
pub trait NetworkClient: Send + Sync {
    /// Fetch the current position for the configured pilot.
    fn fetch_pilot_position(
        &self,
    ) -> impl Future<Output = Result<PilotPosition, NetworkError>> + Send;
}

/// Top-level VATSIM V3 data feed structure.
///
/// We only deserialize the `pilots` array; other fields are ignored.
#[derive(Deserialize)]
struct VatsimData {
    pilots: Vec<PilotPosition>,
}

/// VATSIM client using direct HTTP requests.
///
/// Fetches the VATSIM V3 JSON data feed and extracts the target pilot.
/// Uses a reusable `reqwest::Client` with connection pooling and timeouts.
pub struct VatsimClient {
    /// Pilot CID to look up.
    pilot_cid: u64,

    /// Reusable HTTP client with connection pooling.
    http: reqwest::Client,

    /// URL of the VATSIM V3 data feed.
    data_url: String,
}

impl VatsimClient {
    /// Create a new VATSIM client for a specific pilot CID.
    ///
    /// Uses the provided `data_url` for the V3 JSON data feed.
    pub fn new(pilot_cid: u64, data_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_HTTP_TIMEOUT)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            pilot_cid,
            http,
            data_url,
        }
    }
}

impl NetworkClient for VatsimClient {
    async fn fetch_pilot_position(&self) -> Result<PilotPosition, NetworkError> {
        let response = self
            .http
            .get(&self.data_url)
            .send()
            .await
            .map_err(|e| NetworkError::HttpError(e.to_string()))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| NetworkError::HttpError(e.to_string()))?;

        let data: VatsimData =
            serde_json::from_slice(&bytes).map_err(|e| NetworkError::JsonError(e.to_string()))?;

        let pilot = data.pilots.iter().find(|p| p.cid == self.pilot_cid);

        tracing::debug!(
            total_pilots = data.pilots.len(),
            pilot_cid = self.pilot_cid,
            found = pilot.is_some(),
            "VATSIM data feed fetched"
        );

        pilot
            .cloned()
            .ok_or(NetworkError::PilotNotFound(self.pilot_cid))
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::DEFAULT_VATSIM_DATA_URL;
    use super::*;

    #[test]
    fn test_vatsim_client_creation() {
        let client = VatsimClient::new(1234567, DEFAULT_VATSIM_DATA_URL.to_string());
        assert_eq!(client.pilot_cid, 1234567);
        assert_eq!(client.data_url, DEFAULT_VATSIM_DATA_URL);
    }

    #[test]
    fn test_pilot_position_deserialize() {
        let json = r#"{
            "cid": 1339899,
            "latitude": -37.05505,
            "longitude": 142.80998,
            "altitude": 27866,
            "groundspeed": 383,
            "heading": 290,
            "last_updated": "2026-02-08T06:50:21.3804293Z"
        }"#;

        let pilot: PilotPosition = serde_json::from_str(json).unwrap();
        assert_eq!(pilot.cid, 1339899);
        assert!((pilot.latitude - (-37.05505)).abs() < 0.0001);
        assert!((pilot.longitude - 142.80998).abs() < 0.0001);
        assert_eq!(pilot.altitude, 27866);
        assert_eq!(pilot.groundspeed, 383);
        assert_eq!(pilot.heading, 290);
    }

    #[test]
    fn test_vatsim_data_deserialize_finds_pilot() {
        let json = r#"{
            "general": {"version": 3},
            "pilots": [
                {"cid": 111, "latitude": 0.0, "longitude": 0.0, "altitude": 0, "groundspeed": 0, "heading": 0, "last_updated": "2026-01-01T00:00:00Z"},
                {"cid": 222, "latitude": 33.94, "longitude": -118.4, "altitude": 35000, "groundspeed": 450, "heading": 270, "last_updated": "2026-01-01T00:00:00Z"}
            ]
        }"#;

        let data: VatsimData = serde_json::from_str(json).unwrap();
        assert_eq!(data.pilots.len(), 2);

        let pilot = data.pilots.into_iter().find(|p| p.cid == 222).unwrap();
        assert_eq!(pilot.altitude, 35000);
    }

    #[test]
    fn test_vatsim_data_deserialize_ignores_extra_fields() {
        // The real API has many more fields per pilot — ensure we tolerate them
        let json = r#"{
            "general": {"version": 3, "update": "20260208065024"},
            "pilots": [
                {
                    "cid": 999,
                    "name": "Test Pilot",
                    "callsign": "TST001",
                    "server": "USA-WEST",
                    "pilot_rating": 0,
                    "latitude": 51.47,
                    "longitude": -0.46,
                    "altitude": 5000,
                    "groundspeed": 200,
                    "transponder": "7000",
                    "heading": 90,
                    "qnh_i_hg": 29.92,
                    "qnh_mb": 1013,
                    "flight_plan": null,
                    "logon_time": "2026-02-08T06:00:00Z",
                    "last_updated": "2026-02-08T06:50:00Z"
                }
            ],
            "controllers": [],
            "atis": [],
            "servers": [],
            "prefiles": [],
            "facilities": [],
            "ratings": []
        }"#;

        let data: VatsimData = serde_json::from_str(json).unwrap();
        assert_eq!(data.pilots.len(), 1);
        assert_eq!(data.pilots[0].cid, 999);
    }
}
