//! URL verification and configuration for package distribution.
//!
//! Provides utilities to verify that archive parts are accessible at their
//! configured URLs and that downloaded content matches expected checksums.

use std::io::Read;
use std::time::Duration;

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

use super::{PublishError, PublishResult};

/// Default timeout for URL verification requests (30 seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum file size to download for verification (2 GB).
const MAX_DOWNLOAD_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Result of URL verification.
#[derive(Debug, Clone)]
pub struct UrlVerification {
    /// The URL that was verified.
    pub url: String,

    /// Whether the URL is accessible.
    pub accessible: bool,

    /// Content-Length header if available.
    pub content_length: Option<u64>,

    /// SHA-256 checksum if verification was performed.
    pub checksum: Option<String>,

    /// Whether the checksum matches the expected value.
    pub checksum_valid: Option<bool>,

    /// Error message if verification failed.
    pub error: Option<String>,
}

impl UrlVerification {
    /// Returns true if the URL is valid (accessible and checksum matches).
    pub fn is_valid(&self) -> bool {
        self.accessible && self.checksum_valid.unwrap_or(true)
    }
}

/// URL verifier for archive distribution.
pub struct UrlVerifier {
    client: Client,
}

impl Default for UrlVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl UrlVerifier {
    /// Create a new URL verifier.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent("XEarthLayer-Publisher/1.0")
            .build()
            .expect("failed to create HTTP client");

        Self { client }
    }

    /// Create a URL verifier with a custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("XEarthLayer-Publisher/1.0")
            .build()
            .expect("failed to create HTTP client");

        Self { client }
    }

    /// Check if a URL is accessible (HEAD request).
    pub fn check_accessible(&self, url: &str) -> UrlVerification {
        match self.client.head(url).send() {
            Ok(response) => {
                if response.status().is_success() {
                    let content_length = response
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse().ok());

                    UrlVerification {
                        url: url.to_string(),
                        accessible: true,
                        content_length,
                        checksum: None,
                        checksum_valid: None,
                        error: None,
                    }
                } else {
                    UrlVerification {
                        url: url.to_string(),
                        accessible: false,
                        content_length: None,
                        checksum: None,
                        checksum_valid: None,
                        error: Some(format!("HTTP {}", response.status())),
                    }
                }
            }
            Err(e) => UrlVerification {
                url: url.to_string(),
                accessible: false,
                content_length: None,
                checksum: None,
                checksum_valid: None,
                error: Some(e.to_string()),
            },
        }
    }

    /// Verify a URL by downloading and computing its checksum.
    ///
    /// This downloads the entire file to compute the SHA-256 checksum.
    /// Use `check_accessible` for a quick availability check.
    pub fn verify_checksum(&self, url: &str, expected_checksum: &str) -> UrlVerification {
        // First check accessibility
        let accessibility = self.check_accessible(url);
        if !accessibility.accessible {
            return accessibility;
        }

        // Check content length is within limits
        if let Some(len) = accessibility.content_length {
            if len > MAX_DOWNLOAD_SIZE {
                return UrlVerification {
                    url: url.to_string(),
                    accessible: true,
                    content_length: Some(len),
                    checksum: None,
                    checksum_valid: Some(false),
                    error: Some(format!(
                        "file too large: {} bytes exceeds {} limit",
                        len, MAX_DOWNLOAD_SIZE
                    )),
                };
            }
        }

        // Download and compute checksum
        match self.download_and_hash(url) {
            Ok(actual_checksum) => {
                let matches = actual_checksum.eq_ignore_ascii_case(expected_checksum);
                UrlVerification {
                    url: url.to_string(),
                    accessible: true,
                    content_length: accessibility.content_length,
                    checksum: Some(actual_checksum),
                    checksum_valid: Some(matches),
                    error: if matches {
                        None
                    } else {
                        Some(format!(
                            "checksum mismatch: expected {}, got {}",
                            expected_checksum,
                            // Show first 16 chars for brevity
                            accessibility.content_length.map_or("???".to_string(), |_| {
                                format!(
                                    "{}...",
                                    &expected_checksum[..16.min(expected_checksum.len())]
                                )
                            })
                        ))
                    },
                }
            }
            Err(e) => UrlVerification {
                url: url.to_string(),
                accessible: true,
                content_length: accessibility.content_length,
                checksum: None,
                checksum_valid: None,
                error: Some(format!("download failed: {}", e)),
            },
        }
    }

    /// Download a file and compute its SHA-256 checksum.
    fn download_and_hash(&self, url: &str) -> PublishResult<String> {
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|e| PublishError::InvalidUrl(format!("request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(PublishError::InvalidUrl(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let mut hasher = Sha256::new();
        let mut reader = response;
        let mut buffer = [0u8; 8192];
        let mut total_bytes = 0u64;

        loop {
            let bytes_read = reader
                .read(&mut buffer)
                .map_err(|e| PublishError::InvalidUrl(format!("read error: {}", e)))?;

            if bytes_read == 0 {
                break;
            }

            total_bytes += bytes_read as u64;
            if total_bytes > MAX_DOWNLOAD_SIZE {
                return Err(PublishError::InvalidUrl(format!(
                    "download exceeded {} byte limit",
                    MAX_DOWNLOAD_SIZE
                )));
            }

            hasher.update(&buffer[..bytes_read]);
        }

        let hash = hasher.finalize();
        Ok(format!("{:x}", hash))
    }
}

/// Parse a base URL and generate part URLs.
///
/// Given a base URL like "https://example.com/packages/" and an archive name
/// like "zzXEL_na_ortho-1.0.0.tar.gz", generates URLs for each part suffix.
pub fn generate_part_urls(base_url: &str, archive_name: &str, suffixes: &[&str]) -> Vec<String> {
    let base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{}/", base_url)
    };

    suffixes
        .iter()
        .map(|suffix| format!("{}{}.{}", base, archive_name, suffix))
        .collect()
}

/// Validate a URL format.
pub fn validate_url(url: &str) -> PublishResult<()> {
    if url.is_empty() {
        return Err(PublishError::InvalidUrl("URL cannot be empty".to_string()));
    }

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(PublishError::InvalidUrl(
            "URL must start with http:// or https://".to_string(),
        ));
    }

    // Basic URL validation
    if url.contains(' ') {
        return Err(PublishError::InvalidUrl(
            "URL cannot contain spaces".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_verification_is_valid_accessible() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: true,
            content_length: Some(1000),
            checksum: None,
            checksum_valid: None,
            error: None,
        };
        assert!(v.is_valid());
    }

    #[test]
    fn test_url_verification_is_valid_with_checksum() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: true,
            content_length: Some(1000),
            checksum: Some("abc123".to_string()),
            checksum_valid: Some(true),
            error: None,
        };
        assert!(v.is_valid());
    }

    #[test]
    fn test_url_verification_invalid_not_accessible() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: false,
            content_length: None,
            checksum: None,
            checksum_valid: None,
            error: Some("connection refused".to_string()),
        };
        assert!(!v.is_valid());
    }

    #[test]
    fn test_url_verification_invalid_checksum_mismatch() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: true,
            content_length: Some(1000),
            checksum: Some("wrong".to_string()),
            checksum_valid: Some(false),
            error: Some("checksum mismatch".to_string()),
        };
        assert!(!v.is_valid());
    }

    #[test]
    fn test_validate_url_valid_https() {
        assert!(validate_url("https://example.com/file.tar.gz").is_ok());
    }

    #[test]
    fn test_validate_url_valid_http() {
        assert!(validate_url("http://example.com/file.tar.gz").is_ok());
    }

    #[test]
    fn test_validate_url_empty() {
        let result = validate_url("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_validate_url_no_scheme() {
        let result = validate_url("example.com/file.tar.gz");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http"));
    }

    #[test]
    fn test_validate_url_with_spaces() {
        let result = validate_url("https://example.com/file name.tar.gz");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("spaces"));
    }

    #[test]
    fn test_generate_part_urls() {
        let urls = generate_part_urls(
            "https://example.com/packages",
            "zzXEL_na_ortho-1.0.0.tar.gz",
            &["aa", "ab", "ac"],
        );
        assert_eq!(urls.len(), 3);
        assert_eq!(
            urls[0],
            "https://example.com/packages/zzXEL_na_ortho-1.0.0.tar.gz.aa"
        );
        assert_eq!(
            urls[1],
            "https://example.com/packages/zzXEL_na_ortho-1.0.0.tar.gz.ab"
        );
        assert_eq!(
            urls[2],
            "https://example.com/packages/zzXEL_na_ortho-1.0.0.tar.gz.ac"
        );
    }

    #[test]
    fn test_generate_part_urls_trailing_slash() {
        let urls = generate_part_urls(
            "https://example.com/packages/",
            "zzXEL_na_ortho-1.0.0.tar.gz",
            &["aa"],
        );
        assert_eq!(
            urls[0],
            "https://example.com/packages/zzXEL_na_ortho-1.0.0.tar.gz.aa"
        );
    }

    #[test]
    fn test_url_verifier_default() {
        // Just verify it can be created
        let _verifier = UrlVerifier::default();
    }

    #[test]
    fn test_url_verifier_with_timeout() {
        let _verifier = UrlVerifier::with_timeout(Duration::from_secs(5));
    }

    // Note: Network-dependent tests are skipped in unit tests.
    // Integration tests should cover actual URL verification.

    #[test]
    fn test_url_verification_debug() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: true,
            content_length: Some(1000),
            checksum: None,
            checksum_valid: None,
            error: None,
        };
        let debug = format!("{:?}", v);
        assert!(debug.contains("UrlVerification"));
        assert!(debug.contains("example.com"));
    }

    #[test]
    fn test_url_verification_clone() {
        let v = UrlVerification {
            url: "https://example.com".to_string(),
            accessible: true,
            content_length: Some(1000),
            checksum: None,
            checksum_valid: None,
            error: None,
        };
        let cloned = v.clone();
        assert_eq!(v.url, cloned.url);
        assert_eq!(v.accessible, cloned.accessible);
    }
}
