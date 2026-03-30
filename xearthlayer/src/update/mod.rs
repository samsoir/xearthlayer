//! Version update checking for XEarthLayer.
//!
//! This module provides non-blocking version checking against a remote
//! `version.json` manifest. It compares the running version against the
//! latest published version and produces an [`UpdateInfo`] when an update
//! is available.
//!
//! # Components
//!
//! - [`VersionManifest`] — Serde model for the remote `version.json`
//! - [`UpdateInfo`] — Result of a successful update check (only when newer version exists)
//! - [`UpdateChecker`] — Trait for version checking (supports dependency injection)
//! - [`RemoteUpdateChecker`] — Production implementation with HTTP fetch and 24h disk cache

mod checker;

pub use checker::{RemoteUpdateChecker, UpdateChecker, UpdateError};

use semver::Version;
use serde::{Deserialize, Serialize};

/// Remote version manifest matching the published `version.json` schema.
///
/// Only the fields needed for update checking are deserialized;
/// unknown fields are silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    /// Latest published version string (e.g., "0.5.0").
    pub version: String,
    /// Git tag for the release (e.g., "v0.5.0").
    pub tag: String,
    /// Project homepage URL for the update notification.
    pub homepage: String,
}

/// Information about an available update.
///
/// Only constructed when the latest version is strictly greater than
/// the currently running version.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Currently running version.
    pub current: Version,
    /// Latest available version.
    pub latest: Version,
    /// Project homepage URL to display in the notification.
    pub homepage: String,
}

impl VersionManifest {
    /// Compare this manifest against the current version.
    ///
    /// Returns `Some(UpdateInfo)` if the manifest version is strictly newer,
    /// or `None` if the current version is up-to-date (or newer, e.g. dev builds).
    pub fn check_update(&self, current_version: &str) -> Result<Option<UpdateInfo>, UpdateError> {
        let current = Version::parse(current_version)
            .map_err(|e| UpdateError::VersionParse(current_version.to_string(), e))?;
        let latest = Version::parse(&self.version)
            .map_err(|e| UpdateError::VersionParse(self.version.clone(), e))?;

        if latest > current {
            Ok(Some(UpdateInfo {
                current,
                latest,
                homepage: self.homepage.clone(),
            }))
        } else {
            Ok(None)
        }
    }
}

impl std::fmt::Display for UpdateInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Update available: v{} \u{2192} v{} \u{2014} see {}",
            self.current, self.latest, self.homepage
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // VersionManifest deserialization
    // =========================================================================

    #[test]
    fn test_deserialize_valid_manifest() {
        let json = r#"{
            "version": "0.5.0",
            "tag": "v0.5.0",
            "homepage": "https://xearthlayer.app",
            "release_date": "2026-04-01",
            "assets": {},
            "download_base_url": "https://example.com"
        }"#;

        let manifest: VersionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.version, "0.5.0");
        assert_eq!(manifest.tag, "v0.5.0");
        assert_eq!(manifest.homepage, "https://xearthlayer.app");
    }

    #[test]
    fn test_deserialize_minimal_manifest() {
        let json = r#"{
            "version": "1.0.0",
            "tag": "v1.0.0",
            "homepage": "https://example.com"
        }"#;

        let manifest: VersionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.version, "1.0.0");
    }

    #[test]
    fn test_deserialize_missing_required_field() {
        let json = r#"{ "version": "1.0.0", "tag": "v1.0.0" }"#;
        let result = serde_json::from_str::<VersionManifest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_invalid_json() {
        let result = serde_json::from_str::<VersionManifest>("not json");
        assert!(result.is_err());
    }

    // =========================================================================
    // Version comparison via check_update
    // =========================================================================

    #[test]
    fn test_update_available_when_latest_is_newer() {
        let manifest = VersionManifest {
            version: "0.5.0".to_string(),
            tag: "v0.5.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.4.1").unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.current, Version::new(0, 4, 1));
        assert_eq!(info.latest, Version::new(0, 5, 0));
        assert_eq!(info.homepage, "https://xearthlayer.app");
    }

    #[test]
    fn test_no_update_when_versions_equal() {
        let manifest = VersionManifest {
            version: "0.4.1".to_string(),
            tag: "v0.4.1".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.4.1").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_no_update_when_current_is_newer() {
        let manifest = VersionManifest {
            version: "0.4.0".to_string(),
            tag: "v0.4.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.5.0").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_update_available_for_patch_bump() {
        let manifest = VersionManifest {
            version: "0.4.2".to_string(),
            tag: "v0.4.2".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.4.1").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_update_available_for_major_bump() {
        let manifest = VersionManifest {
            version: "1.0.0".to_string(),
            tag: "v1.0.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.99.99").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_invalid_current_version_returns_error() {
        let manifest = VersionManifest {
            version: "0.5.0".to_string(),
            tag: "v0.5.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("not-a-version");
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_manifest_version_returns_error() {
        let manifest = VersionManifest {
            version: "bad".to_string(),
            tag: "vbad".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let result = manifest.check_update("0.4.1");
        assert!(result.is_err());
    }

    // =========================================================================
    // Display
    // =========================================================================

    #[test]
    fn test_update_info_display() {
        let info = UpdateInfo {
            current: Version::new(0, 4, 1),
            latest: Version::new(0, 5, 0),
            homepage: "https://xearthlayer.app".to_string(),
        };

        let display = format!("{}", info);
        assert!(display.contains("v0.4.1"));
        assert!(display.contains("v0.5.0"));
        assert!(display.contains("https://xearthlayer.app"));
        assert!(display.contains("\u{2192}")); // arrow
    }
}
