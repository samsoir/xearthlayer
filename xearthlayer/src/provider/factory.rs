//! Provider factory for centralized provider creation.
//!
//! This module provides a factory pattern for creating imagery providers,
//! eliminating duplication in CLI code and centralizing provider configuration.
//!
//! # Factories
//!
//! - [`ProviderFactory`] - Creates sync `Provider` instances (legacy)
//! - [`AsyncProviderFactory`] - Creates `AsyncProvider` instances (preferred)

use super::bing::{AsyncBingMapsProvider, BingMapsProvider};
use super::go2::{AsyncGo2Provider, Go2Provider};
use super::google::{AsyncGoogleMapsProvider, GoogleMapsProvider};
use super::http::{AsyncReqwestClient, ReqwestClient};
use super::types::{AsyncProvider, Provider, ProviderError};
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

    /// Google Maps GO2 satellite imagery provider.
    ///
    /// No API key required - uses Google's public tile servers.
    /// This is the same endpoint used by Ortho4XP's GO2 provider.
    Go2,

    /// Google Maps satellite imagery provider (official API).
    ///
    /// Requires a valid Google Maps Platform API key with Map Tiles API enabled.
    /// Has usage limits and billing requirements.
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

    /// Create a Google GO2 provider configuration (no API key required).
    pub fn go2() -> Self {
        Self::Go2
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
            Self::Go2 => "Google GO2",
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
            ProviderConfig::Go2 => {
                let provider = Go2Provider::new(self.http_client);
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

/// Factory for creating async provider instances.
///
/// This is the preferred factory for production use. It creates `AsyncProvider`
/// implementations that use non-blocking I/O, avoiding thread pool exhaustion
/// under high concurrent load.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::provider::{AsyncProviderFactory, ProviderConfig, AsyncReqwestClient};
///
/// let http_client = AsyncReqwestClient::new()?;
/// let factory = AsyncProviderFactory::new(http_client);
///
/// // Create a Bing Maps provider (sync operation)
/// let (provider, name, max_zoom) = factory.create_sync(&ProviderConfig::Bing)?;
///
/// // Create a Google Maps provider (async - requires session creation)
/// let google_config = ProviderConfig::google("YOUR_API_KEY");
/// let (provider, name, max_zoom) = factory.create(&google_config).await?;
/// ```
pub struct AsyncProviderFactory {
    http_client: AsyncReqwestClient,
}

/// Enum to hold different async provider types.
///
/// This allows the factory to return different concrete provider types
/// while maintaining a common interface.
pub enum AsyncProviderType {
    Bing(AsyncBingMapsProvider<AsyncReqwestClient>),
    Go2(AsyncGo2Provider<AsyncReqwestClient>),
    Google(AsyncGoogleMapsProvider<AsyncReqwestClient>),
}

impl AsyncProvider for AsyncProviderType {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError> {
        match self {
            Self::Bing(p) => p.download_chunk(row, col, zoom).await,
            Self::Go2(p) => p.download_chunk(row, col, zoom).await,
            Self::Google(p) => p.download_chunk(row, col, zoom).await,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Bing(p) => p.name(),
            Self::Go2(p) => p.name(),
            Self::Google(p) => p.name(),
        }
    }

    fn min_zoom(&self) -> u8 {
        match self {
            Self::Bing(p) => p.min_zoom(),
            Self::Go2(p) => p.min_zoom(),
            Self::Google(p) => p.min_zoom(),
        }
    }

    fn max_zoom(&self) -> u8 {
        match self {
            Self::Bing(p) => p.max_zoom(),
            Self::Go2(p) => p.max_zoom(),
            Self::Google(p) => p.max_zoom(),
        }
    }
}

impl AsyncProviderFactory {
    /// Create a new async provider factory with the given HTTP client.
    pub fn new(http_client: AsyncReqwestClient) -> Self {
        Self { http_client }
    }

    /// Create an async provider from the given configuration.
    ///
    /// This is an async method because some providers (like Google Maps)
    /// require async initialization (session creation).
    ///
    /// Returns a tuple of:
    /// - The provider
    /// - The provider's name (for logging)
    /// - The provider's maximum zoom level
    ///
    /// # Errors
    ///
    /// Returns an error if provider creation fails.
    pub async fn create(
        self,
        config: &ProviderConfig,
    ) -> Result<(AsyncProviderType, String, u8), ProviderError> {
        match config {
            ProviderConfig::Bing => {
                let provider = AsyncBingMapsProvider::new(self.http_client);
                let name = provider.name().to_string();
                let max_zoom = provider.max_zoom();
                Ok((AsyncProviderType::Bing(provider), name, max_zoom))
            }
            ProviderConfig::Go2 => {
                let provider = AsyncGo2Provider::new(self.http_client);
                let name = provider.name().to_string();
                let max_zoom = provider.max_zoom();
                Ok((AsyncProviderType::Go2(provider), name, max_zoom))
            }
            ProviderConfig::Google { api_key } => {
                let provider =
                    AsyncGoogleMapsProvider::new(self.http_client, api_key.clone()).await?;
                let name = provider.name().to_string();
                let max_zoom = provider.max_zoom();
                Ok((AsyncProviderType::Google(provider), name, max_zoom))
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
    fn test_provider_config_go2() {
        let config = ProviderConfig::go2();
        assert_eq!(config.name(), "Google GO2");
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

    #[test]
    fn test_provider_config_go2_debug() {
        let config = ProviderConfig::Go2;
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("Go2"));
    }

    // Note: Factory tests that create real HTTP clients are integration tests
    // and would require network access. We test the factory with mock clients
    // in the integration test suite.
}
