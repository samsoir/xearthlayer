//! Common types and utilities shared across CLI commands.

use clap::ValueEnum;
use xearthlayer::config::ConfigFile;
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

/// Load the configuration file.
///
/// A missing file gives the defaults. A file that does not parse is an
/// error naming it, never the defaults: a command acting on the defaults
/// would use the wrong cache and package directories without saying so, and
/// one that saves would replace every setting in the file with its default
/// (#401 made a hand-edited `cache.dds_disk_ratio` of 1.5 such a file).
pub fn load_config() -> Result<ConfigFile, CliError> {
    load_config_from(&xearthlayer::config::config_file_path())
}

/// [`load_config`] for the file at `path`.
pub fn load_config_from(path: &std::path::Path) -> Result<ConfigFile, CliError> {
    ConfigFile::load_from(path).map_err(|e| {
        CliError::Config(format!(
            "{} could not be read: {e}. Correct that setting in the file; \
             nothing was changed.",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_config_from(&dir.path().join("config.ini")).unwrap();
        assert_eq!(
            config.provider.provider_type,
            ConfigFile::default().provider.provider_type
        );
    }

    #[test]
    fn a_config_file_that_does_not_parse_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(
            &path,
            "[provider]\ntype = go2\n\n[cache]\ndds_disk_ratio = 1.5\n",
        )
        .unwrap();

        let err = load_config_from(&path).expect_err("must not fall back to the defaults");
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("dds_disk_ratio"), "{message}");
    }
}
