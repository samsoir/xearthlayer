//! Provider factory for centralized provider creation.
//!
//! This module provides a factory pattern for creating imagery providers,
//! eliminating duplication in CLI code and centralizing provider configuration.

use super::bing::BingMapsProvider;
use super::google::GoogleMapsProvider;
use super::http::ReqwestClient;
use super::types::{Provider, ProviderError};
use std::sync::Arc;

/// Configuration for creating a provider.
///
/// Encapsulates all the settings needed to create a specific provider type,
/// following the Open-Closed Principle - new providers can be added as new
/// enum variants without modifying existing code.
///
/// # Example
///
/// ```
/// use xearthlayer::provider::ProviderConfig;
///
/// // Bing Maps (no API key required)
/// let bing_config = ProviderConfig::Bing;
///
/// // Google Maps (requires API key)
/// let google_config = ProviderConfig::Google {
///     api_key: "YOUR_API_KEY".to_string(),
/// };
/// ```
#[derive(Debug, Clone)]
pub enum ProviderConfig {
    /// Bing Maps satellite imagery provider.
    ///
    /// No API key required - uses public Bing Maps tile servers.
    Bing,

    /// Google Maps satellite imagery provider.
    ///
    /// Requires a valid Google Maps Platform API key with Map Tiles API enabled.
    Google {
        /// Google Maps Platform API key
        api_key: String,
    },
}

impl ProviderConfig {
    /// Create a Bing Maps provider configuration.
    pub fn bing() -> Self {
        Self::Bing
    }

    /// Create a Google Maps provider configuration with the given API key.
    pub fn google(api_key: impl Into<String>) -> Self {
        Self::Google {
            api_key: api_key.into(),
        }
    }

    /// Returns the provider name for this configuration.
    pub fn name(&self) -> &str {
        match self {
            Self::Bing => "Bing Maps",
            Self::Google { .. } => "Google Maps",
        }
    }

    /// Returns whether this provider requires an API key.
    pub fn requires_api_key(&self) -> bool {
        matches!(self, Self::Google { .. })
    }
}

/// Factory for creating provider instances.
///
/// Centralizes provider creation logic, eliminating duplication across
/// CLI commands and making it easier to add new providers.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{ProviderFactory, ProviderConfig, ReqwestClient};
///
/// let http_client = ReqwestClient::new()?;
/// let factory = ProviderFactory::new(http_client);
///
/// // Create a Bing Maps provider
/// let (provider, name, max_zoom) = factory.create(&ProviderConfig::Bing)?;
///
/// // Create a Google Maps provider
/// let google_config = ProviderConfig::google("YOUR_API_KEY");
/// let (provider, name, max_zoom) = factory.create(&google_config)?;
/// ```
pub struct ProviderFactory {
    http_client: ReqwestClient,
}

impl ProviderFactory {
    /// Create a new provider factory with the given HTTP client.
    pub fn new(http_client: ReqwestClient) -> Self {
        Self { http_client }
    }

    /// Create a provider from the given configuration.
    ///
    /// Returns a tuple of:
    /// - The provider as a trait object
    /// - The provider's name (for logging)
    /// - The provider's maximum zoom level
    ///
    /// # Errors
    ///
    /// Returns an error if provider creation fails (e.g., Google session
    /// creation fails due to invalid API key).
    pub fn create(
        self,
        config: &ProviderConfig,
    ) -> Result<(Arc<dyn Provider>, String, u8), ProviderError> {
        match config {
            ProviderConfig::Bing => {
                let provider = BingMapsProvider::new(self.http_client);
                let name = provider.name().to_string();
                let max_zoom = provider.max_zoom();
                Ok((Arc::new(provider), name, max_zoom))
            }
            ProviderConfig::Google { api_key } => {
                let provider = GoogleMapsProvider::new(self.http_client, api_key.clone())?;
                let name = provider.name().to_string();
                let max_zoom = provider.max_zoom();
                Ok((Arc::new(provider), name, max_zoom))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_config_bing() {
        let config = ProviderConfig::bing();
        assert_eq!(config.name(), "Bing Maps");
        assert!(!config.requires_api_key());
    }

    #[test]
    fn test_provider_config_google() {
        let config = ProviderConfig::google("test_key");
        assert_eq!(config.name(), "Google Maps");
        assert!(config.requires_api_key());

        if let ProviderConfig::Google { api_key } = config {
            assert_eq!(api_key, "test_key");
        } else {
            panic!("Expected Google config");
        }
    }

    #[test]
    fn test_provider_config_clone() {
        let config = ProviderConfig::google("api_key");
        let cloned = config.clone();
        assert_eq!(config.name(), cloned.name());
    }

    #[test]
    fn test_provider_config_debug() {
        let config = ProviderConfig::Bing;
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("Bing"));
    }

    // Note: Factory tests that create real HTTP clients are integration tests
    // and would require network access. We test the factory with mock clients
    // in the integration test suite.
}
