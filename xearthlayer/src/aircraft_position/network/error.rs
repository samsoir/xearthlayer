//! Error types for online network position adapters.

use thiserror::Error;

/// Errors that can occur when fetching position from an online network.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    HttpError(String),

    /// JSON deserialization failed.
    #[error("Failed to parse response: {0}")]
    JsonError(String),

    /// Pilot not found in the network data (disconnected or invalid CID).
    #[error("Pilot {0} not found in network data")]
    PilotNotFound(u64),

    /// Failed to parse the `last_updated` timestamp from the API.
    #[error("Failed to parse timestamp: {0}")]
    TimestampParseError(String),

    /// The position data is too old (exceeds `max_stale_secs`).
    #[error("Position data is stale (age: {age_secs}s, max: {max_secs}s)")]
    DataTooStale { age_secs: u64, max_secs: u64 },

    /// The channel to the aggregator has been closed.
    #[error("Channel closed")]
    ChannelClosed,
}

impl From<vatsim_utils::errors::VatsimUtilError> for NetworkError {
    fn from(e: vatsim_utils::errors::VatsimUtilError) -> Self {
        NetworkError::HttpError(e.to_string())
    }
}
