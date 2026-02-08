//! Network client trait and VATSIM implementation.
//!
//! The [`NetworkClient`] trait abstracts over different online ATC networks,
//! allowing the adapter to work with any network that provides pilot position data.
//! The [`VatsimClient`] implementation uses the `vatsim_utils` crate to fetch
//! pilot data from the VATSIM V3 API.

use std::future::Future;

use vatsim_utils::live_api::Vatsim;
use vatsim_utils::models::Pilot;

use super::error::NetworkError;

/// Trait for fetching pilot position from an online ATC network.
///
/// Implementations should fetch the current position data for a specific pilot.
/// The trait is object-safe via async_trait-style pattern with boxed futures.
pub trait NetworkClient: Send + Sync {
    /// Fetch the current position for the configured pilot.
    fn fetch_pilot_position(&self) -> impl Future<Output = Result<Pilot, NetworkError>> + Send;
}

/// VATSIM client using the `vatsim_utils` crate.
///
/// Fetches pilot position data from the VATSIM V3 API.
/// Uses `vatsim_utils::live_api::Vatsim` internally, which auto-discovers
/// the V3 data endpoint from the VATSIM status URL.
pub struct VatsimClient {
    /// Pilot CID to look up.
    pilot_cid: u64,
}

impl VatsimClient {
    /// Create a new VATSIM client for a specific pilot CID.
    pub fn new(pilot_cid: u64) -> Self {
        Self { pilot_cid }
    }
}

impl NetworkClient for VatsimClient {
    async fn fetch_pilot_position(&self) -> Result<Pilot, NetworkError> {
        // Initialize the VATSIM API client (discovers V3 endpoint)
        let api = Vatsim::new().await?;

        // Fetch all online data
        let data = api.get_v3_data().await?;

        // Find our pilot by CID
        data.pilots
            .into_iter()
            .find(|p| p.cid == self.pilot_cid)
            .ok_or(NetworkError::PilotNotFound(self.pilot_cid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vatsim_client_creation() {
        let client = VatsimClient::new(1234567);
        assert_eq!(client.pilot_cid, 1234567);
    }
}
