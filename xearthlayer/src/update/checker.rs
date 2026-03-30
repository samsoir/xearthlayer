//! Update checker trait and remote implementation.
//!
//! Provides [`UpdateChecker`] as the abstraction for version checking,
//! and [`RemoteUpdateChecker`] as the production implementation that
//! fetches `version.json` via HTTP with a 24-hour disk cache.

use std::path::PathBuf;

use thiserror::Error;

use super::{UpdateInfo, VersionManifest};

/// Errors that can occur during update checking.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// Failed to parse a version string.
    #[error("Failed to parse version '{0}': {1}")]
    VersionParse(String, #[source] semver::Error),

    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Failed to read or write cache file.
    #[error("Cache I/O error: {0}")]
    CacheIo(#[from] std::io::Error),

    /// Failed to parse JSON (manifest or cache).
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Abstraction for version update checking.
///
/// Implementations may fetch from a remote URL, read from cache,
/// or return fixed values for testing.
pub trait UpdateChecker: Send + Sync {
    /// Check whether a newer version is available.
    ///
    /// Returns `Ok(Some(info))` if an update exists, `Ok(None)` if up-to-date,
    /// or `Err` on failure.
    fn check(&self, current_version: &str) -> Result<Option<UpdateInfo>, UpdateError>;
}

/// Production update checker that fetches `version.json` from a remote URL.
///
/// Implements a 24-hour disk cache at `~/.xearthlayer/version_check.json`
/// to avoid unnecessary network requests on every startup.
pub struct RemoteUpdateChecker {
    url: String,
    cache_path: PathBuf,
}

impl Default for RemoteUpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteUpdateChecker {
    /// Default URL for the version manifest.
    pub const DEFAULT_URL: &str = "https://github.com/samsoir/xearthlayer/raw/main/version.json";

    /// Cache validity duration (24 hours).
    const CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

    /// Create a new checker with the default URL and cache path.
    pub fn new() -> Self {
        let cache_path = crate::config::config_directory().join("version_check.json");
        Self {
            url: Self::DEFAULT_URL.to_string(),
            cache_path,
        }
    }

    /// Create a checker with custom URL and cache path (for testing).
    pub fn with_url_and_cache(url: String, cache_path: PathBuf) -> Self {
        Self { url, cache_path }
    }

    /// Try to load a cached manifest that is still fresh (< 24h old).
    fn load_cached(&self) -> Option<VersionManifest> {
        let metadata = std::fs::metadata(&self.cache_path).ok()?;
        let modified = metadata.modified().ok()?;
        let age = modified.elapsed().ok()?;

        if age > Self::CACHE_MAX_AGE {
            return None;
        }

        let contents = std::fs::read_to_string(&self.cache_path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save a manifest to the disk cache.
    fn save_cache(&self, manifest: &VersionManifest) {
        if let Some(parent) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = match serde_json::to_string_pretty(manifest) {
            Ok(j) => j,
            Err(_) => return,
        };
        let _ = std::fs::write(&self.cache_path, json);
    }

    /// Fetch the version manifest from the remote URL.
    fn fetch_remote(&self) -> Result<VersionManifest, UpdateError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent(format!("XEarthLayer/{}", crate::VERSION))
            .build()?;

        let response = client.get(&self.url).send()?;
        let body = response.text()?;
        let manifest: VersionManifest = serde_json::from_str(&body)?;
        Ok(manifest)
    }
}

impl UpdateChecker for RemoteUpdateChecker {
    fn check(&self, current_version: &str) -> Result<Option<UpdateInfo>, UpdateError> {
        // Try cache first
        if let Some(cached) = self.load_cached() {
            return cached.check_update(current_version);
        }

        // Fetch from remote
        let manifest = self.fetch_remote()?;
        self.save_cache(&manifest);
        manifest.check_update(current_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // =========================================================================
    // Mock UpdateChecker for trait verification
    // =========================================================================

    struct MockUpdateChecker {
        result: Result<Option<UpdateInfo>, String>,
    }

    impl MockUpdateChecker {
        fn update_available(info: UpdateInfo) -> Self {
            Self {
                result: Ok(Some(info)),
            }
        }

        fn up_to_date() -> Self {
            Self { result: Ok(None) }
        }
    }

    impl UpdateChecker for MockUpdateChecker {
        fn check(&self, _current_version: &str) -> Result<Option<UpdateInfo>, UpdateError> {
            match &self.result {
                Ok(info) => Ok(info.clone()),
                Err(msg) => Err(UpdateError::VersionParse(
                    msg.clone(),
                    semver::Version::parse("bad").unwrap_err(),
                )),
            }
        }
    }

    #[test]
    fn test_mock_checker_returns_update() {
        let info = UpdateInfo {
            current: semver::Version::new(0, 4, 1),
            latest: semver::Version::new(0, 5, 0),
            homepage: "https://xearthlayer.app".to_string(),
        };
        let checker = MockUpdateChecker::update_available(info);
        let result = checker.check("0.4.1").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().latest, semver::Version::new(0, 5, 0));
    }

    #[test]
    fn test_mock_checker_returns_up_to_date() {
        let checker = MockUpdateChecker::up_to_date();
        let result = checker.check("0.4.1").unwrap();
        assert!(result.is_none());
    }

    // =========================================================================
    // Cache behavior
    // =========================================================================

    fn create_cache_file(dir: &TempDir, manifest: &VersionManifest) -> PathBuf {
        let cache_path = dir.path().join("version_check.json");
        let json = serde_json::to_string_pretty(manifest).unwrap();
        let mut file = std::fs::File::create(&cache_path).unwrap();
        file.write_all(json.as_bytes()).unwrap();
        cache_path
    }

    #[test]
    fn test_fresh_cache_is_used() {
        let dir = TempDir::new().unwrap();
        let manifest = VersionManifest {
            version: "0.5.0".to_string(),
            tag: "v0.5.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };
        let cache_path = create_cache_file(&dir, &manifest);

        let checker = RemoteUpdateChecker::with_url_and_cache(
            "http://invalid.test/version.json".to_string(),
            cache_path,
        );

        // Should use cache (not hit network) and find an update
        let result = checker.check("0.4.1").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().latest, semver::Version::new(0, 5, 0));
    }

    #[test]
    fn test_stale_cache_triggers_fetch() {
        let dir = TempDir::new().unwrap();
        let manifest = VersionManifest {
            version: "0.5.0".to_string(),
            tag: "v0.5.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };
        let cache_path = create_cache_file(&dir, &manifest);

        // Make the cache file old by setting mtime to 25 hours ago
        let old_time = filetime::FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 25 * 3600,
            0,
        );
        filetime::set_file_mtime(&cache_path, old_time).unwrap();

        let checker = RemoteUpdateChecker::with_url_and_cache(
            "http://invalid.test/version.json".to_string(),
            cache_path,
        );

        // Stale cache should attempt fetch — which will fail on invalid URL
        let result = checker.check("0.4.1");
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_cache_triggers_fetch() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("nonexistent.json");

        let checker = RemoteUpdateChecker::with_url_and_cache(
            "http://invalid.test/version.json".to_string(),
            cache_path,
        );

        // No cache — should attempt fetch and fail
        let result = checker.check("0.4.1");
        assert!(result.is_err());
    }

    #[test]
    fn test_corrupt_cache_triggers_fetch() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("version_check.json");
        std::fs::write(&cache_path, "not valid json").unwrap();

        let checker = RemoteUpdateChecker::with_url_and_cache(
            "http://invalid.test/version.json".to_string(),
            cache_path,
        );

        // Corrupt cache should attempt fetch and fail
        let result = checker.check("0.4.1");
        assert!(result.is_err());
    }

    #[test]
    fn test_save_cache_creates_file() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("subdir").join("version_check.json");

        let checker = RemoteUpdateChecker::with_url_and_cache(
            "http://invalid.test/version.json".to_string(),
            cache_path.clone(),
        );

        let manifest = VersionManifest {
            version: "0.5.0".to_string(),
            tag: "v0.5.0".to_string(),
            homepage: "https://xearthlayer.app".to_string(),
        };

        checker.save_cache(&manifest);
        assert!(cache_path.exists());

        let contents = std::fs::read_to_string(&cache_path).unwrap();
        let loaded: VersionManifest = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.version, "0.5.0");
    }
}
