//! Configuration for online network position adapters.

use std::time::Duration;

/// Default VATSIM V3 JSON data feed URL.
pub const DEFAULT_VATSIM_DATA_URL: &str = "https://data.vatsim.net/v3/vatsim-data.json";

/// Default poll interval (VATSIM updates every ~15 seconds).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 15;

/// Default maximum age before position data is considered stale.
pub const DEFAULT_MAX_STALE_SECS: u64 = 60;

/// Configuration for the network position adapter.
#[derive(Debug, Clone)]
pub struct NetworkAdapterConfig {
    /// Whether the online network adapter is enabled.
    pub enabled: bool,

    /// Network type: "vatsim", "ivao", or "pilotedge".
    pub network_type: String,

    /// Pilot identifier (CID for VATSIM).
    pub pilot_id: u64,

    /// API URL (for VATSIM, the V3 JSON data feed).
    pub api_url: String,

    /// How often to poll the API.
    pub poll_interval: Duration,

    /// Maximum age (seconds) before data is considered stale.
    pub max_stale_secs: u64,
}

impl NetworkAdapterConfig {
    /// Create a config from orchestrator settings.
    pub fn from_config(
        network_type: String,
        pilot_id: u64,
        api_url: String,
        poll_interval_secs: u64,
        max_stale_secs: u64,
    ) -> Self {
        Self {
            enabled: true,
            network_type,
            pilot_id,
            api_url,
            poll_interval: Duration::from_secs(poll_interval_secs),
            max_stale_secs,
        }
    }
}

impl Default for NetworkAdapterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            network_type: "vatsim".to_string(),
            pilot_id: 0,
            api_url: DEFAULT_VATSIM_DATA_URL.to_string(),
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NetworkAdapterConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.network_type, "vatsim");
        assert_eq!(config.pilot_id, 0);
        assert_eq!(config.poll_interval, Duration::from_secs(15));
        assert_eq!(config.max_stale_secs, 60);
    }
}
