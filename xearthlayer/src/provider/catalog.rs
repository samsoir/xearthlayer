//! The set of imagery providers `provider.type` accepts.
//!
//! One list, so that adding a provider does not mean finding every place the
//! names were written out. Before #258 the names appeared as separate literal
//! arrays in `config/parser.rs` and `config/keys.rs`, plus a `match` in the CLI
//! and prose in two doc comments, and the setup wizard was about to be a fifth.
//!
//! Labels live here rather than in the CLI because they describe the provider,
//! not the interface presenting it: the wizard, `config list` help and the docs
//! should all say the same thing about what a provider costs and needs.

/// The credential a provider needs before it can serve tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCredential {
    /// A Google Maps API key, billed by Google. `provider.google_api_key`.
    GoogleApiKey,
    /// A Mapbox access token. `provider.mapbox_access_token`.
    MapboxAccessToken,
}

impl ProviderCredential {
    /// The configuration key that carries this credential.
    pub fn config_key(self) -> &'static str {
        match self {
            Self::GoogleApiKey => "provider.google_api_key",
            Self::MapboxAccessToken => "provider.mapbox_access_token",
        }
    }

    /// Human-readable name, for a prompt or an error.
    pub fn label(self) -> &'static str {
        match self {
            Self::GoogleApiKey => "Google Maps API key",
            Self::MapboxAccessToken => "Mapbox access token",
        }
    }
}

/// One imagery provider, as offered to a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEntry {
    /// The value written to `provider.type`.
    pub key: &'static str,
    /// Short name for display.
    pub name: &'static str,
    /// What choosing this provider means: cost, coverage, credentials.
    pub summary: &'static str,
    /// The credential it needs, if any.
    pub credential: Option<ProviderCredential>,
}

impl ProviderEntry {
    /// Whether this provider can serve tiles with no credential at all.
    pub fn is_free(self) -> bool {
        self.credential.is_none()
    }
}

/// Every provider, ordered as the setup wizard offers them: the options that
/// work with no credentials first, so the default path needs no account.
pub const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry {
        key: "bing",
        name: "Bing Maps",
        summary: "Global coverage, no credentials needed",
        credential: None,
    },
    ProviderEntry {
        key: "apple",
        name: "Apple Maps",
        summary: "Global coverage, no credentials needed, access tokens obtained automatically",
        credential: None,
    },
    ProviderEntry {
        key: "go2",
        name: "Google GO2",
        summary: "Google's public tile servers, no API key needed",
        credential: None,
    },
    ProviderEntry {
        key: "arcgis",
        name: "ArcGIS World Imagery",
        summary: "Esri World Imagery, global coverage, no credentials needed",
        credential: None,
    },
    ProviderEntry {
        key: "usgs",
        name: "USGS Imagery",
        summary: "United States only, no credentials needed",
        credential: None,
    },
    ProviderEntry {
        key: "google",
        name: "Google Maps",
        summary: "Paid, billed by Google, requires an API key",
        credential: Some(ProviderCredential::GoogleApiKey),
    },
    ProviderEntry {
        key: "mapbox",
        name: "Mapbox",
        summary: "Requires an access token",
        credential: Some(ProviderCredential::MapboxAccessToken),
    },
];

/// Every accepted `provider.type` value, as a `'static` slice.
///
/// `OneOfSpec` needs `&'static [&'static str]`, which cannot be derived from
/// [`PROVIDERS`] in const context. Kept beside it, and
/// `provider_keys_match_the_catalog` fails if the two ever disagree, so this is
/// a second spelling of one list rather than a second list.
pub const PROVIDER_KEYS: &[&str] = &["apple", "arcgis", "bing", "go2", "google", "mapbox", "usgs"];

/// Look up a provider by its `provider.type` value. Case-insensitive.
pub fn find(provider_type: &str) -> Option<&'static ProviderEntry> {
    let lowered = provider_type.to_ascii_lowercase();
    PROVIDERS.iter().find(|p| p.key == lowered)
}

/// Every accepted `provider.type` value.
pub fn keys() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.key).collect()
}

/// The accepted values as a comma-separated list, for a validation message.
///
/// Sorted, because this is read by a user comparing it against what they typed,
/// not by the wizard, whose order is deliberate.
pub fn keys_sorted_csv() -> String {
    let mut keys = keys();
    keys.sort_unstable();
    keys.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_unique_and_lowercase() {
        let mut seen = Vec::new();
        for entry in PROVIDERS {
            assert_eq!(
                entry.key,
                entry.key.to_ascii_lowercase(),
                "keys are compared lowercased, so they must be stored that way"
            );
            assert!(!seen.contains(&entry.key), "duplicate key {}", entry.key);
            seen.push(entry.key);
        }
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(find("BING").map(|p| p.key), Some("bing"));
        assert_eq!(find("bing").map(|p| p.key), Some("bing"));
        assert_eq!(find("nope"), None);
    }

    #[test]
    fn only_google_and_mapbox_need_credentials() {
        // The wizard prompts for a credential exactly when this says to, so the
        // set must not drift silently.
        let needing: Vec<&str> = PROVIDERS
            .iter()
            .filter(|p| !p.is_free())
            .map(|p| p.key)
            .collect();
        assert_eq!(needing, vec!["google", "mapbox"]);
    }

    #[test]
    fn free_providers_come_first() {
        // A first-time user should reach a working choice without an account,
        // so the no-credential options are offered before the paid ones.
        let first_paid = PROVIDERS.iter().position(|p| !p.is_free());
        let last_free = PROVIDERS.iter().rposition(|p| p.is_free());
        assert!(
            last_free < first_paid,
            "all credential-free providers must precede those needing credentials"
        );
    }

    #[test]
    fn credential_config_keys_match_the_settings_fields() {
        assert_eq!(
            ProviderCredential::GoogleApiKey.config_key(),
            "provider.google_api_key"
        );
        assert_eq!(
            ProviderCredential::MapboxAccessToken.config_key(),
            "provider.mapbox_access_token"
        );
    }

    #[test]
    fn provider_keys_match_the_catalog() {
        let mut from_entries = keys();
        from_entries.sort_unstable();
        let mut statics = PROVIDER_KEYS.to_vec();
        statics.sort_unstable();
        assert_eq!(
            statics, from_entries,
            "PROVIDER_KEYS and PROVIDERS must describe the same set"
        );
    }

    #[test]
    fn keys_sorted_csv_lists_every_provider() {
        assert_eq!(
            keys_sorted_csv(),
            "apple, arcgis, bing, go2, google, mapbox, usgs"
        );
    }
}
