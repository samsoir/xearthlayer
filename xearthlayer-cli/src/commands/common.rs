//! Common types and utilities shared across CLI commands.

use clap::ValueEnum;
use xearthlayer::config::ConfigFile;
use xearthlayer::dds::DdsFormat;
use xearthlayer::provider::ProviderConfig;

use crate::error::CliError;

/// Imagery provider selection for CLI arguments.
#[derive(Debug, Clone, ValueEnum, PartialEq)]
pub enum ProviderType {
    /// Apple Maps satellite imagery (no API key required, tokens auto-acquired)
    Apple,
    /// ArcGIS World Imagery (no API key required, global coverage)
    Arcgis,
    /// Bing Maps aerial imagery (no API key required)
    Bing,
    /// Google Maps via public tile servers (no API key required, same as Ortho4XP GO2)
    Go2,
    /// Google Maps official API (requires API key, has usage limits)
    Google,
    /// MapBox satellite imagery (requires access token)
    Mapbox,
    /// USGS orthoimagery (no API key required, US coverage only)
    Usgs,
}

impl ProviderType {
    /// Convert to a ProviderConfig, requiring API key for certain providers.
    pub fn to_config(
        &self,
        api_key: Option<String>,
        mapbox_token: Option<String>,
    ) -> Result<ProviderConfig, CliError> {
        match self {
            ProviderType::Apple => Ok(ProviderConfig::apple()),
            ProviderType::Arcgis => Ok(ProviderConfig::arcgis()),
            ProviderType::Bing => Ok(ProviderConfig::bing()),
            ProviderType::Go2 => Ok(ProviderConfig::go2()),
            ProviderType::Google => {
                let key = api_key.ok_or_else(|| {
                    CliError::Config(
                        "Google Maps provider requires an API key. \
                         Set google_api_key in config.ini or use --google-api-key"
                            .to_string(),
                    )
                })?;
                Ok(ProviderConfig::google(key))
            }
            ProviderType::Mapbox => {
                let token = mapbox_token.ok_or_else(|| {
                    CliError::Config(
                        "MapBox provider requires an access token. \
                         Set mapbox_access_token in config.ini or use --mapbox-token"
                            .to_string(),
                    )
                })?;
                Ok(ProviderConfig::mapbox(token))
            }
            ProviderType::Usgs => Ok(ProviderConfig::usgs()),
        }
    }

    /// Parse from config file string.
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "apple" => Some(ProviderType::Apple),
            "arcgis" => Some(ProviderType::Arcgis),
            "bing" => Some(ProviderType::Bing),
            "go2" => Some(ProviderType::Go2),
            "google" => Some(ProviderType::Google),
            "mapbox" => Some(ProviderType::Mapbox),
            "usgs" => Some(ProviderType::Usgs),
            _ => None,
        }
    }
}

/// DDS compression format selection for CLI arguments.
#[derive(Debug, Clone, ValueEnum)]
pub enum DdsCompression {
    /// BC1/DXT1 compression (4:1, best for opaque textures)
    Bc1,
    /// BC3/DXT5 compression (4:1, with full alpha channel)
    Bc3,
}

impl From<DdsCompression> for DdsFormat {
    fn from(compression: DdsCompression) -> Self {
        match compression {
            DdsCompression::Bc1 => DdsFormat::BC1,
            DdsCompression::Bc3 => DdsFormat::BC3,
        }
    }
}

/// Resolve provider settings from CLI args and config.
pub fn resolve_provider(
    cli_provider: Option<ProviderType>,
    cli_api_key: Option<String>,
    cli_mapbox_token: Option<String>,
    config: &ConfigFile,
) -> Result<ProviderConfig, CliError> {
    // CLI takes precedence, then config
    let provider = cli_provider
        .or_else(|| ProviderType::from_config_str(&config.provider.provider_type))
        .unwrap_or(ProviderType::Bing);

    let api_key = cli_api_key.or_else(|| config.provider.google_api_key.clone());
    let mapbox_token = cli_mapbox_token.or_else(|| config.provider.mapbox_access_token.clone());

    provider.to_config(api_key, mapbox_token)
}

/// Resolve DDS format from CLI args and config.
pub fn resolve_dds_format(cli_format: Option<DdsCompression>, config: &ConfigFile) -> DdsFormat {
    cli_format
        .map(DdsFormat::from)
        .unwrap_or(config.texture.format)
}
