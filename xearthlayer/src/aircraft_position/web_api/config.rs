//! Configuration for the X-Plane Web API adapter.

use std::time::Duration;

/// Configuration for the X-Plane Web API connection.
#[derive(Debug, Clone)]
pub struct WebApiConfig {
    /// X-Plane Web API port (default 8086).
    pub port: u16,
    /// Reconnect interval on disconnect (default 5s).
    pub reconnect_interval: Duration,
    /// REST request timeout (default 10s).
    pub request_timeout: Duration,
}

impl Default for WebApiConfig {
    fn default() -> Self {
        Self {
            port: 8086,
            reconnect_interval: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = WebApiConfig::default();
        assert_eq!(config.port, 8086);
        assert_eq!(config.reconnect_interval, Duration::from_secs(5));
    }
}
