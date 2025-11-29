//! Repository configuration for the package publisher.
//!
//! Configuration is stored in the `.xearthlayer-repo` file and includes
//! settings for archive part size and other repository-wide options.

use std::fs;
use std::path::Path;

use super::{PublishError, PublishResult};
use crate::config::format_size as format_size_usize;

/// Default archive part size in bytes (500 MB).
pub const DEFAULT_PART_SIZE: u64 = 500 * 1024 * 1024;

/// Minimum archive part size (10 MB).
pub const MIN_PART_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum archive part size (2 GB).
pub const MAX_PART_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Configuration section header in .xearthlayer-repo.
const CONFIG_SECTION: &str = "[config]";

/// Repository configuration.
///
/// Stores repository-wide settings that affect archive building and
/// package distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// Size limit for archive parts in bytes.
    /// Archives larger than this will be split.
    pub part_size: u64,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            part_size: DEFAULT_PART_SIZE,
        }
    }
}

impl RepoConfig {
    /// Create a new configuration with the specified part size.
    ///
    /// # Errors
    ///
    /// Returns an error if the part size is outside the valid range.
    pub fn new(part_size: u64) -> PublishResult<Self> {
        Self::validate_part_size(part_size)?;
        Ok(Self { part_size })
    }

    /// Validate that a part size is within the allowed range.
    fn validate_part_size(size: u64) -> PublishResult<()> {
        if size < MIN_PART_SIZE {
            return Err(PublishError::InvalidRepository(format!(
                "part size {} is below minimum {} bytes",
                size, MIN_PART_SIZE
            )));
        }
        if size > MAX_PART_SIZE {
            return Err(PublishError::InvalidRepository(format!(
                "part size {} exceeds maximum {} bytes",
                size, MAX_PART_SIZE
            )));
        }
        Ok(())
    }

    /// Set the part size.
    ///
    /// # Errors
    ///
    /// Returns an error if the size is outside the valid range.
    pub fn set_part_size(&mut self, size: u64) -> PublishResult<()> {
        Self::validate_part_size(size)?;
        self.part_size = size;
        Ok(())
    }

    /// Get the part size in human-readable format.
    pub fn part_size_display(&self) -> String {
        // Safe to cast to usize since MAX_PART_SIZE is 2GB which fits in usize
        format_size_usize(self.part_size as usize)
    }

    /// Parse configuration from repository marker content.
    ///
    /// The marker file has this format:
    /// ```text
    /// XEARTHLAYER PACKAGE REPOSITORY
    /// 1.0.0
    /// 2025-01-01T00:00:00Z
    ///
    /// [config]
    /// part_size = 500000000
    /// ```
    pub fn parse_from_marker(content: &str) -> PublishResult<Self> {
        let mut config = Self::default();

        // Find the config section
        let Some(config_start) = content.find(CONFIG_SECTION) else {
            // No config section, use defaults
            return Ok(config);
        };

        let config_content = &content[config_start + CONFIG_SECTION.len()..];

        for line in config_content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Stop at next section
            if line.starts_with('[') {
                break;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();

                match key {
                    "part_size" => {
                        let size = parse_size(value).map_err(|e| {
                            PublishError::InvalidRepository(format!(
                                "invalid part_size '{}': {}",
                                value, e
                            ))
                        })?;
                        config.set_part_size(size)?;
                    }
                    _ => {
                        // Ignore unknown keys for forward compatibility
                    }
                }
            }
        }

        Ok(config)
    }

    /// Serialize configuration to marker file format.
    ///
    /// Returns the config section to append to the marker file.
    pub fn serialize_to_marker(&self) -> String {
        format!("\n{}\npart_size = {}\n", CONFIG_SECTION, self.part_size)
    }
}

/// Parse a size string with optional suffix (K, M, G).
///
/// Examples: "500M", "1G", "500000000"
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty string".to_string());
    }

    let (num_str, multiplier) = if let Some(prefix) = s.strip_suffix(['K', 'k']) {
        (prefix, 1024u64)
    } else if let Some(prefix) = s.strip_suffix(['M', 'm']) {
        (prefix, 1024 * 1024)
    } else if let Some(prefix) = s.strip_suffix(['G', 'g']) {
        (prefix, 1024 * 1024 * 1024)
    } else {
        (s, 1u64)
    };

    let num: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid number '{}'", num_str))?;

    num.checked_mul(multiplier)
        .ok_or_else(|| "size overflow".to_string())
}

/// Read repository configuration from the marker file.
pub fn read_config(repo_root: &Path) -> PublishResult<RepoConfig> {
    let marker_path = repo_root.join(".xearthlayer-repo");

    let content = fs::read_to_string(&marker_path).map_err(|e| PublishError::ReadFailed {
        path: marker_path,
        source: e,
    })?;

    RepoConfig::parse_from_marker(&content)
}

/// Write repository configuration to the marker file.
///
/// This preserves the existing header and appends/updates the config section.
pub fn write_config(repo_root: &Path, config: &RepoConfig) -> PublishResult<()> {
    let marker_path = repo_root.join(".xearthlayer-repo");

    let content = fs::read_to_string(&marker_path).map_err(|e| PublishError::ReadFailed {
        path: marker_path.clone(),
        source: e,
    })?;

    // Remove existing config section if present
    let new_content = if let Some(config_start) = content.find(CONFIG_SECTION) {
        content[..config_start].trim_end().to_string()
    } else {
        content.trim_end().to_string()
    };

    // Append new config
    let final_content = format!("{}{}", new_content, config.serialize_to_marker());

    fs::write(&marker_path, final_content).map_err(|e| PublishError::WriteFailed {
        path: marker_path,
        source: e,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = RepoConfig::default();
        assert_eq!(config.part_size, DEFAULT_PART_SIZE);
    }

    #[test]
    fn test_new_valid_size() {
        let config = RepoConfig::new(100 * 1024 * 1024).unwrap();
        assert_eq!(config.part_size, 100 * 1024 * 1024);
    }

    #[test]
    fn test_new_too_small() {
        let result = RepoConfig::new(1024);
        assert!(result.is_err());
    }

    #[test]
    fn test_new_too_large() {
        let result = RepoConfig::new(10 * 1024 * 1024 * 1024);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_part_size_valid() {
        let mut config = RepoConfig::default();
        config.set_part_size(1024 * 1024 * 1024).unwrap();
        assert_eq!(config.part_size, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_set_part_size_invalid() {
        let mut config = RepoConfig::default();
        assert!(config.set_part_size(100).is_err());
    }

    #[test]
    fn test_part_size_display_gb() {
        let config = RepoConfig::new(1024 * 1024 * 1024).unwrap();
        assert_eq!(config.part_size_display(), "1 GB");
    }

    #[test]
    fn test_part_size_display_mb() {
        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        assert_eq!(config.part_size_display(), "500 MB");
    }

    #[test]
    fn test_parse_size_plain() {
        assert_eq!(parse_size("500000000").unwrap(), 500_000_000);
    }

    #[test]
    fn test_parse_size_kb() {
        assert_eq!(parse_size("100K").unwrap(), 100 * 1024);
        assert_eq!(parse_size("100k").unwrap(), 100 * 1024);
    }

    #[test]
    fn test_parse_size_mb() {
        assert_eq!(parse_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size("500m").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_gb() {
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_with_space() {
        assert_eq!(parse_size(" 500M ").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_invalid() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-100").is_err());
    }

    #[test]
    fn test_parse_from_marker_no_config() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n1.0.0\n2025-01-01T00:00:00Z\n";
        let config = RepoConfig::parse_from_marker(content).unwrap();
        assert_eq!(config.part_size, DEFAULT_PART_SIZE);
    }

    #[test]
    fn test_parse_from_marker_with_config() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       part_size = 1073741824\n";
        let config = RepoConfig::parse_from_marker(content).unwrap();
        assert_eq!(config.part_size, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_from_marker_with_suffix() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       part_size = 1G\n";
        let config = RepoConfig::parse_from_marker(content).unwrap();
        assert_eq!(config.part_size, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_from_marker_ignores_comments() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       # This is a comment\n\
                       part_size = 500M\n";
        let config = RepoConfig::parse_from_marker(content).unwrap();
        assert_eq!(config.part_size, 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_from_marker_ignores_unknown_keys() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       unknown_key = value\n\
                       part_size = 500M\n";
        let config = RepoConfig::parse_from_marker(content).unwrap();
        assert_eq!(config.part_size, 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_from_marker_invalid_part_size() {
        let content = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       part_size = 100\n"; // Too small
        let result = RepoConfig::parse_from_marker(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_to_marker() {
        let config = RepoConfig::new(1024 * 1024 * 1024).unwrap();
        let serialized = config.serialize_to_marker();
        assert!(serialized.contains("[config]"));
        assert!(serialized.contains("part_size = 1073741824"));
    }

    #[test]
    fn test_roundtrip() {
        let original = RepoConfig::new(750 * 1024 * 1024).unwrap();
        let header = "XEARTHLAYER PACKAGE REPOSITORY\n1.0.0\n2025-01-01T00:00:00Z";
        let content = format!("{}{}", header, original.serialize_to_marker());
        let parsed = RepoConfig::parse_from_marker(&content).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_read_write_config() {
        let temp = TempDir::new().unwrap();
        let marker_path = temp.path().join(".xearthlayer-repo");

        // Write initial marker file
        let initial = "XEARTHLAYER PACKAGE REPOSITORY\n1.0.0\n2025-01-01T00:00:00Z\n";
        fs::write(&marker_path, initial).unwrap();

        // Write config
        let config = RepoConfig::new(750 * 1024 * 1024).unwrap();
        write_config(temp.path(), &config).unwrap();

        // Read it back
        let loaded = read_config(temp.path()).unwrap();
        assert_eq!(loaded.part_size, 750 * 1024 * 1024);
    }

    #[test]
    fn test_write_config_preserves_header() {
        let temp = TempDir::new().unwrap();
        let marker_path = temp.path().join(".xearthlayer-repo");

        // Write initial marker file
        let initial = "XEARTHLAYER PACKAGE REPOSITORY\n1.0.0\n2025-01-01T00:00:00Z\n";
        fs::write(&marker_path, initial).unwrap();

        // Write config
        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        write_config(temp.path(), &config).unwrap();

        // Verify header is preserved
        let content = fs::read_to_string(&marker_path).unwrap();
        assert!(content.starts_with("XEARTHLAYER PACKAGE REPOSITORY"));
        assert!(content.contains("1.0.0"));
        assert!(content.contains("2025-01-01T00:00:00Z"));
    }

    #[test]
    fn test_write_config_replaces_existing() {
        let temp = TempDir::new().unwrap();
        let marker_path = temp.path().join(".xearthlayer-repo");

        // Write marker with existing config
        let initial = "XEARTHLAYER PACKAGE REPOSITORY\n\
                       1.0.0\n\
                       2025-01-01T00:00:00Z\n\n\
                       [config]\n\
                       part_size = 100000000\n";
        fs::write(&marker_path, initial).unwrap();

        // Write new config
        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        write_config(temp.path(), &config).unwrap();

        // Verify old config is replaced
        let loaded = read_config(temp.path()).unwrap();
        assert_eq!(loaded.part_size, 500 * 1024 * 1024);

        // Verify only one config section exists
        let content = fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content.matches("[config]").count(), 1);
    }

    #[test]
    fn test_read_config_missing_file() {
        let temp = TempDir::new().unwrap();
        let result = read_config(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_config_clone_and_eq() {
        let config1 = RepoConfig::new(500 * 1024 * 1024).unwrap();
        let config2 = config1.clone();
        assert_eq!(config1, config2);
    }

    #[test]
    fn test_config_debug() {
        let config = RepoConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("RepoConfig"));
        assert!(debug.contains("part_size"));
    }
}
