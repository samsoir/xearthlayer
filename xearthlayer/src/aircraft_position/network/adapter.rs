//! Network adapter - poll loop daemon for online ATC network position.
//!
//! The [`NetworkAdapter`] periodically fetches pilot position from an online
//! network (VATSIM/IVAO/PilotEdge) and converts it to [`AircraftState`] updates
//! sent to the aggregator via an mpsc channel.
//!
//! # Design
//!
//! Follows the same pattern as [`InferenceAdapter`]:
//! - `new()` + `start()` → spawns async task
//! - Async `run()` loop with `tokio::time::interval`
//! - Channel close detection for graceful shutdown
//! - Exponential backoff on errors (2^n seconds, capped at 5 minutes)
//!
//! # Timestamp Handling
//!
//! VATSIM's `last_updated` field is an ISO 8601 timestamp. We parse it to
//! compute the age of the data, then create an `Instant` offset from `now()`
//! so the aggregator's staleness logic works correctly.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::client::NetworkClient;
use super::config::NetworkAdapterConfig;
use super::error::NetworkError;
use crate::aircraft_position::state::AircraftState;

/// Maximum backoff duration (5 minutes).
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Network adapter - poll loop daemon.
///
/// Periodically fetches pilot position from an online ATC network
/// and sends `AircraftState` updates to the aggregator.
pub struct NetworkAdapter<C: NetworkClient> {
    /// Client for fetching pilot data.
    client: C,

    /// Channel to send position updates to the aggregator.
    state_tx: mpsc::Sender<AircraftState>,

    /// Configuration.
    config: NetworkAdapterConfig,
}

impl<C: NetworkClient + 'static> NetworkAdapter<C> {
    /// Create a new network adapter.
    pub fn new(
        client: C,
        state_tx: mpsc::Sender<AircraftState>,
        config: NetworkAdapterConfig,
    ) -> Self {
        Self {
            client,
            state_tx,
            config,
        }
    }

    /// Start the adapter as an async task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Run the poll loop.
    async fn run(self) {
        tracing::info!(
            network = %self.config.network_type,
            pilot_id = self.config.pilot_id,
            poll_interval_secs = self.config.poll_interval.as_secs(),
            "Online network position adapter started"
        );

        let mut interval = tokio::time::interval(self.config.poll_interval);
        let mut consecutive_errors: u32 = 0;

        loop {
            interval.tick().await;

            // Apply backoff if we've had consecutive errors
            if consecutive_errors > 0 {
                let backoff = calculate_backoff(consecutive_errors);
                tracing::debug!(
                    backoff_secs = backoff.as_secs(),
                    consecutive_errors,
                    "Backing off after errors"
                );
                tokio::time::sleep(backoff).await;
            }

            match self.fetch_and_convert().await {
                Ok(state) => {
                    consecutive_errors = 0;

                    if self.state_tx.send(state).await.is_err() {
                        tracing::debug!("Network adapter channel closed, stopping");
                        break;
                    }
                }
                Err(NetworkError::PilotNotFound(cid)) => {
                    // Pilot disconnected - not an error, just skip
                    tracing::trace!(cid, "Pilot not found in network data (disconnected?)");
                    consecutive_errors = 0; // Don't backoff for expected absence
                }
                Err(NetworkError::DataTooStale { age_secs, max_secs }) => {
                    tracing::trace!(age_secs, max_secs, "Network position data too stale");
                    consecutive_errors = 0; // Don't backoff for stale data
                }
                Err(NetworkError::ChannelClosed) => {
                    tracing::debug!("Network adapter channel closed, stopping");
                    break;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!(
                        error = %e,
                        consecutive_errors,
                        "Failed to fetch network position"
                    );
                }
            }
        }

        tracing::info!("Online network position adapter stopped");
    }

    /// Fetch pilot data and convert to `AircraftState`.
    async fn fetch_and_convert(&self) -> Result<AircraftState, NetworkError> {
        let pilot = self.client.fetch_pilot_position().await?;

        convert_to_aircraft_state(&pilot, self.config.max_stale_secs)
    }
}

/// Convert a VATSIM pilot record to an `AircraftState`.
///
/// Parses the `last_updated` ISO 8601 timestamp to compute the data age,
/// then creates an `Instant` offset from `now()` so the aggregator's
/// staleness logic works correctly.
fn convert_to_aircraft_state(
    pilot: &vatsim_utils::models::Pilot,
    max_stale_secs: u64,
) -> Result<AircraftState, NetworkError> {
    // Parse the last_updated timestamp (ISO 8601: "2026-02-07T21:51:24.829965Z")
    let last_updated = chrono::DateTime::parse_from_rfc3339(&pilot.last_updated)
        .map_err(|e| NetworkError::TimestampParseError(e.to_string()))?;

    let now_utc = chrono::Utc::now();
    let age = now_utc
        .signed_duration_since(last_updated)
        .to_std()
        .unwrap_or(Duration::ZERO);

    // Check staleness
    if age.as_secs() > max_stale_secs {
        return Err(NetworkError::DataTooStale {
            age_secs: age.as_secs(),
            max_secs: max_stale_secs,
        });
    }

    // Create an Instant that reflects the actual data age
    let timestamp = Instant::now() - age;

    Ok(AircraftState::from_network_position(
        pilot.latitude,
        pilot.longitude,
        pilot.heading as f32,
        pilot.groundspeed as f32,
        pilot.altitude as f32,
        timestamp,
    ))
}

/// Calculate exponential backoff: 2^n seconds, capped at MAX_BACKOFF.
fn calculate_backoff(consecutive_errors: u32) -> Duration {
    let secs = 2u64.saturating_pow(consecutive_errors.min(20));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_backoff() {
        assert_eq!(calculate_backoff(0), Duration::from_secs(1));
        assert_eq!(calculate_backoff(1), Duration::from_secs(2));
        assert_eq!(calculate_backoff(2), Duration::from_secs(4));
        assert_eq!(calculate_backoff(3), Duration::from_secs(8));
        assert_eq!(calculate_backoff(10), MAX_BACKOFF); // 1024 > 300
    }

    #[test]
    fn test_convert_to_aircraft_state_fresh() {
        let now = chrono::Utc::now();
        let pilot = vatsim_utils::models::Pilot {
            cid: 1234567,
            latitude: 33.9425,
            longitude: -118.408,
            altitude: 35000,
            groundspeed: 450,
            heading: 270,
            last_updated: now.to_rfc3339(),
            ..Default::default()
        };

        let state = convert_to_aircraft_state(&pilot, 60).unwrap();
        assert!((state.latitude - 33.9425).abs() < 0.001);
        assert!((state.longitude - (-118.408)).abs() < 0.001);
        assert_eq!(state.heading, 270.0);
        assert_eq!(state.ground_speed, 450.0);
        assert_eq!(state.altitude, 35000.0);
        assert_eq!(
            state.source,
            crate::aircraft_position::state::PositionSource::OnlineNetwork
        );
    }

    #[test]
    fn test_convert_to_aircraft_state_stale() {
        let old = chrono::Utc::now() - chrono::Duration::seconds(120);
        let pilot = vatsim_utils::models::Pilot {
            cid: 1234567,
            latitude: 33.9425,
            longitude: -118.408,
            altitude: 35000,
            groundspeed: 450,
            heading: 270,
            last_updated: old.to_rfc3339(),
            ..Default::default()
        };

        let result = convert_to_aircraft_state(&pilot, 60);
        assert!(matches!(result, Err(NetworkError::DataTooStale { .. })));
    }

    #[test]
    fn test_convert_to_aircraft_state_invalid_timestamp() {
        let pilot = vatsim_utils::models::Pilot {
            cid: 1234567,
            latitude: 33.9425,
            longitude: -118.408,
            altitude: 35000,
            groundspeed: 450,
            heading: 270,
            last_updated: "not-a-timestamp".to_string(),
            ..Default::default()
        };

        let result = convert_to_aircraft_state(&pilot, 60);
        assert!(matches!(result, Err(NetworkError::TimestampParseError(_))));
    }

    /// Mock client for testing the adapter.
    struct MockNetworkClient {
        result: std::sync::Mutex<Option<Result<vatsim_utils::models::Pilot, NetworkError>>>,
    }

    impl MockNetworkClient {
        fn with_pilot(pilot: vatsim_utils::models::Pilot) -> Self {
            Self {
                result: std::sync::Mutex::new(Some(Ok(pilot))),
            }
        }

        fn with_error(error: NetworkError) -> Self {
            Self {
                result: std::sync::Mutex::new(Some(Err(error))),
            }
        }
    }

    impl NetworkClient for MockNetworkClient {
        async fn fetch_pilot_position(&self) -> Result<vatsim_utils::models::Pilot, NetworkError> {
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(NetworkError::ChannelClosed))
        }
    }

    #[tokio::test]
    async fn test_adapter_sends_state_on_success() {
        let now = chrono::Utc::now();
        let pilot = vatsim_utils::models::Pilot {
            cid: 1234567,
            latitude: 33.9425,
            longitude: -118.408,
            altitude: 35000,
            groundspeed: 450,
            heading: 270,
            last_updated: now.to_rfc3339(),
            ..Default::default()
        };

        let client = MockNetworkClient::with_pilot(pilot);
        let (tx, rx) = mpsc::channel(16);
        let config = NetworkAdapterConfig {
            enabled: true,
            pilot_id: 1234567,
            ..Default::default()
        };

        let adapter = NetworkAdapter::new(client, tx, config);

        // Manually call fetch_and_convert
        let state = adapter.fetch_and_convert().await.unwrap();
        assert!((state.latitude - 33.9425).abs() < 0.001);

        // Verify the mock client can't be called again (returns ChannelClosed)
        let result = adapter.fetch_and_convert().await;
        assert!(result.is_err());

        drop(rx);
    }

    #[tokio::test]
    async fn test_adapter_handles_pilot_not_found() {
        let client = MockNetworkClient::with_error(NetworkError::PilotNotFound(999));
        let (tx, _rx) = mpsc::channel(16);
        let config = NetworkAdapterConfig::default();

        let adapter = NetworkAdapter::new(client, tx, config);
        let result = adapter.fetch_and_convert().await;
        assert!(matches!(result, Err(NetworkError::PilotNotFound(999))));
    }
}
