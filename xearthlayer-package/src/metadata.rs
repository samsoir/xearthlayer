//! Package metadata parsing and serialization.
//!
//! Handles the `xearthlayer_scenery_package.txt` file format.

use super::types::{ArchivePart, PackageType};
use chrono::{DateTime, Utc};
use semver::Version;
use std::fmt;
use std::str::FromStr;

/// Field separator used in the file format (two spaces).
const FIELD_SEPARATOR: &str = "  ";

/// Expected header line for package metadata files.
const METADATA_HEADER: &str = "REGIONAL SCENERY PACKAGE";

/// Package metadata from `xearthlayer_scenery_package.txt`.
///
/// Contains all information needed to identify, download, and verify
/// a scenery package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMetadata {
    /// Specification version of this metadata format
    pub spec_version: Version,

    /// Human-readable region title (e.g., "EUROPE", "NORTH AMERICA")
    pub title: String,

    /// Package version (semantic versioning)
    pub package_version: Version,

    /// Publication timestamp (UTC)
    pub published_at: DateTime<Utc>,

    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,

    /// Mount point folder name in Custom Scenery
    pub mountpoint: String,

    /// Complete archive filename (before splitting)
    pub filename: String,

    /// Archive parts for download
    pub parts: Vec<ArchivePart>,
}

/// Validation context for package metadata.
///
/// Different stages of the publish workflow require different validation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationContext {
    /// Initial metadata before archive is built.
    /// Parts are optional, URLs are not required.
    Initial,

    /// After archive is built, awaiting URL configuration.
    /// Parts are required, URLs may be empty.
    AwaitingUrls,

    /// Ready for release or already released.
    /// Parts are required, all URLs must be present.
    Release,
}

/// Validation error for package metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataValidationError {
    /// No archive parts defined
    NoParts,
    /// Missing URL for a part
    MissingUrl { part_index: usize, filename: String },
    /// Empty checksum for a part
    EmptyChecksum { part_index: usize, filename: String },
}

impl fmt::Display for MetadataValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataValidationError::NoParts => {
                write!(f, "no archive parts defined")
            }
            MetadataValidationError::MissingUrl {
                part_index,
                filename,
            } => {
                write!(f, "missing URL for part {} ({})", part_index + 1, filename)
            }
            MetadataValidationError::EmptyChecksum {
                part_index,
                filename,
            } => {
                write!(
                    f,
                    "empty checksum for part {} ({})",
                    part_index + 1,
                    filename
                )
            }
        }
    }
}

impl std::error::Error for MetadataValidationError {}

impl PackageMetadata {
    /// Generate the folder name for this package in Custom Scenery.
    ///
    /// Uses the mountpoint field which should already be in the correct format.
    pub fn folder_name(&self) -> &str {
        &self.mountpoint
    }

    /// Validate metadata for a specific context.
    ///
    /// Returns all validation errors found.
    pub fn validate(&self, context: ValidationContext) -> Vec<MetadataValidationError> {
        let mut errors = Vec::new();

        match context {
            ValidationContext::Initial => {
                // Initial state: parts are optional, no URL requirements
                // Just validate checksums if parts exist
                for (i, part) in self.parts.iter().enumerate() {
                    if part.checksum.is_empty() {
                        errors.push(MetadataValidationError::EmptyChecksum {
                            part_index: i,
                            filename: part.filename.clone(),
                        });
                    }
                }
            }
            ValidationContext::AwaitingUrls => {
                // After build: parts required, URLs may be empty
                if self.parts.is_empty() {
                    errors.push(MetadataValidationError::NoParts);
                }
                for (i, part) in self.parts.iter().enumerate() {
                    if part.checksum.is_empty() {
                        errors.push(MetadataValidationError::EmptyChecksum {
                            part_index: i,
                            filename: part.filename.clone(),
                        });
                    }
                }
            }
            ValidationContext::Release => {
                // Ready for release: parts and URLs required
                if self.parts.is_empty() {
                    errors.push(MetadataValidationError::NoParts);
                }
                for (i, part) in self.parts.iter().enumerate() {
                    if part.checksum.is_empty() {
                        errors.push(MetadataValidationError::EmptyChecksum {
                            part_index: i,
                            filename: part.filename.clone(),
                        });
                    }
                    if part.url.is_empty() {
                        errors.push(MetadataValidationError::MissingUrl {
                            part_index: i,
                            filename: part.filename.clone(),
                        });
                    }
                }
            }
        }

        errors
    }

    /// Check if metadata is valid for a specific context.
    pub fn is_valid(&self, context: ValidationContext) -> bool {
        self.validate(context).is_empty()
    }

    /// Check if all parts have URLs configured.
    pub fn has_all_urls(&self) -> bool {
        !self.parts.is_empty() && self.parts.iter().all(|p| !p.url.is_empty())
    }

    /// Check if the package has any parts defined.
    pub fn has_parts(&self) -> bool {
        !self.parts.is_empty()
    }
}

/// Error parsing package metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataParseError {
    /// File is empty or has insufficient lines
    InsufficientLines,
    /// Invalid header line
    InvalidHeader(String),
    /// Invalid specification version
    InvalidSpecVersion(String),
    /// Invalid title/version line format
    InvalidTitleLine(String),
    /// The title is not usable as a region code, so it cannot safely become a
    /// directory name. See [`crate::validate_region`].
    InvalidRegionTitle(crate::InvalidRegion),
    /// Invalid datetime format
    InvalidDateTime(String),
    /// Invalid package type
    InvalidPackageType(String),
    /// Invalid part count
    InvalidPartCount(String),
    /// Invalid part line format
    InvalidPartLine(String),
    /// Part count mismatch
    PartCountMismatch { expected: usize, actual: usize },
    /// The file's specification major is newer than this build understands.
    UnsupportedSpecVersion {
        found: Version,
        supported_major: u64,
    },
}

impl fmt::Display for MetadataParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataParseError::InsufficientLines => {
                write!(f, "metadata file has insufficient lines")
            }
            MetadataParseError::InvalidHeader(s) => {
                write!(
                    f,
                    "invalid header: expected '{}', got '{}'",
                    METADATA_HEADER, s
                )
            }
            MetadataParseError::InvalidSpecVersion(s) => {
                write!(f, "invalid specification version: {}", s)
            }
            MetadataParseError::InvalidTitleLine(s) => {
                write!(f, "invalid title/version line: {}", s)
            }
            MetadataParseError::InvalidRegionTitle(e) => write!(f, "{}", e),
            MetadataParseError::InvalidDateTime(s) => {
                write!(f, "invalid datetime: {}", s)
            }
            MetadataParseError::InvalidPackageType(s) => {
                write!(f, "invalid package type: {}", s)
            }
            MetadataParseError::InvalidPartCount(s) => {
                write!(f, "invalid part count: {}", s)
            }
            MetadataParseError::InvalidPartLine(s) => {
                write!(f, "invalid part line: {}", s)
            }
            MetadataParseError::PartCountMismatch { expected, actual } => {
                write!(
                    f,
                    "part count mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            MetadataParseError::UnsupportedSpecVersion {
                found,
                supported_major,
            } => {
                write!(
                    f,
                    "package metadata uses specification {} but this version of XEarthLayer reads up to {}.x. Upgrade XEarthLayer to use this package",
                    found, supported_major
                )
            }
        }
    }
}

impl std::error::Error for MetadataParseError {}

/// Parse package metadata from string content.
///
/// # Format
///
/// ```text
/// REGIONAL SCENERY PACKAGE
/// <specversion>
/// <title>  <pkgversion>
/// <datetime>
/// <packagetype>
/// <mountpoint>
/// <filename>
/// <partcount>
///
///
/// <sha256sum>  <localfilename>  <remoteurl>
/// ...
/// ```
///
/// Fields within lines are separated by two spaces.
pub fn parse_package_metadata(content: &str) -> Result<PackageMetadata, MetadataParseError> {
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() < 8 {
        return Err(MetadataParseError::InsufficientLines);
    }

    // Line 1: Header
    let header = lines[0].trim();
    if header != METADATA_HEADER {
        return Err(MetadataParseError::InvalidHeader(header.to_string()));
    }

    // Line 2: Spec version
    let spec_version = Version::from_str(lines[1].trim())
        .map_err(|e| MetadataParseError::InvalidSpecVersion(e.to_string()))?;
    if !super::spec::is_supported_spec_major(&spec_version) {
        return Err(MetadataParseError::UnsupportedSpecVersion {
            found: spec_version,
            supported_major: super::spec::SUPPORTED_SPEC_MAJOR,
        });
    }

    // Line 3: Title and package version (separated by two spaces)
    let title_line = lines[2].trim();
    let title_parts: Vec<&str> = title_line.split(FIELD_SEPARATOR).collect();
    if title_parts.len() != 2 {
        return Err(MetadataParseError::InvalidTitleLine(title_line.to_string()));
    }
    let title = title_parts[0].to_string();
    // The title becomes a directory name under Custom Scenery, and this file
    // is fetched over HTTP, so it is untrusted input to a path.
    crate::validate_region(&title).map_err(MetadataParseError::InvalidRegionTitle)?;
    let package_version = Version::from_str(title_parts[1].trim())
        .map_err(|e| MetadataParseError::InvalidTitleLine(e.to_string()))?;

    // Line 4: DateTime (UTC/Zulu)
    let datetime_str = lines[3].trim();
    let published_at = DateTime::parse_from_rfc3339(datetime_str)
        .map_err(|e| MetadataParseError::InvalidDateTime(e.to_string()))?
        .with_timezone(&Utc);

    // Line 5: Package type
    let type_char = lines[4]
        .trim()
        .chars()
        .next()
        .ok_or_else(|| MetadataParseError::InvalidPackageType("empty".to_string()))?;
    let package_type = PackageType::from_code(type_char)
        .ok_or_else(|| MetadataParseError::InvalidPackageType(type_char.to_string()))?;

    // Line 6: Mountpoint
    let mountpoint = lines[5].trim().to_string();

    // Line 7: Filename
    let filename = lines[6].trim().to_string();

    // Line 8: Part count (0 is allowed for initial/draft metadata)
    let part_count: usize = lines[7]
        .trim()
        .parse()
        .map_err(|_| MetadataParseError::InvalidPartCount(lines[7].trim().to_string()))?;

    // Skip two blank lines, then parse parts
    // Parts start after the blank lines following line 8
    let mut parts = Vec::with_capacity(part_count);
    let mut part_start_idx = 8;

    // Skip blank lines
    while part_start_idx < lines.len() && lines[part_start_idx].trim().is_empty() {
        part_start_idx += 1;
    }

    // Parse part lines
    for line in lines.iter().skip(part_start_idx) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let part_fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        // Accept 2 or 3 fields - URL may be empty/missing for draft metadata
        if part_fields.len() < 2 || part_fields.len() > 3 {
            return Err(MetadataParseError::InvalidPartLine(line.to_string()));
        }

        let url = if part_fields.len() == 3 {
            part_fields[2]
        } else {
            ""
        };

        parts.push(ArchivePart::new(part_fields[0], part_fields[1], url));
    }

    if parts.len() != part_count {
        return Err(MetadataParseError::PartCountMismatch {
            expected: part_count,
            actual: parts.len(),
        });
    }

    Ok(PackageMetadata {
        spec_version,
        title,
        package_version,
        published_at,
        package_type,
        mountpoint,
        filename,
        parts,
    })
}

/// Serialize package metadata to string.
pub fn serialize_package_metadata(metadata: &PackageMetadata) -> String {
    let mut output = String::new();

    // Line 1: Header
    output.push_str(METADATA_HEADER);
    output.push('\n');

    // Line 2: Spec version
    output.push_str(&metadata.spec_version.to_string());
    output.push('\n');

    // Line 3: Title and package version
    output.push_str(&metadata.title);
    output.push_str(FIELD_SEPARATOR);
    output.push_str(&metadata.package_version.to_string());
    output.push('\n');

    // Line 4: DateTime
    output.push_str(
        &metadata
            .published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    output.push('\n');

    // Line 5: Package type
    output.push(metadata.package_type.code());
    output.push('\n');

    // Line 6: Mountpoint
    output.push_str(&metadata.mountpoint);
    output.push('\n');

    // Line 7: Filename
    output.push_str(&metadata.filename);
    output.push('\n');

    // Line 8: Part count
    output.push_str(&metadata.parts.len().to_string());
    output.push('\n');

    // Two blank lines
    output.push('\n');
    output.push('\n');

    // Parts
    for part in &metadata.parts {
        output.push_str(&part.checksum);
        output.push_str(FIELD_SEPARATOR);
        output.push_str(&part.filename);
        output.push_str(FIELD_SEPARATOR);
        output.push_str(&part.url);
        output.push('\n');
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata_content() -> &'static str {
        r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
5


55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725  zzXEL_eur_ortho-1.0.0.tar.gz.aa  https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.aa
b91f75f976768c62f6215d4e16e1e0e8a5948dfb9fccf73e67699a6818933335  zzXEL_eur_ortho-1.0.0.tar.gz.ab  https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.ab
f5e5a7881b2156f1b53baf1982b4a11db3662449ce63abe0867841ed139aedf0  zzXEL_eur_ortho-1.0.0.tar.gz.ac  https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.ac
031bf3288532038142cf5bc4a7453eb87340b9441da5b381c567781c23ef33fe  zzXEL_eur_ortho-1.0.0.tar.gz.ad  https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.ad
15d85a6c5d9b8a6db1d6745cac1c2819972a689088e7f12bf5a2f076d7bc03d9  zzXEL_eur_ortho-1.0.0.tar.gz.ae  https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.ae
"#
    }

    #[test]
    fn test_parse_metadata() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();

        assert_eq!(metadata.spec_version, Version::new(1, 0, 0));
        assert_eq!(metadata.title, "EUROPE");
        assert_eq!(metadata.package_version, Version::new(1, 0, 0));
        assert_eq!(metadata.package_type, PackageType::Ortho);
        assert_eq!(metadata.mountpoint, "zzXEL_eur_ortho");
        assert_eq!(metadata.filename, "zzXEL_eur_ortho-1.0.0.tar.gz");
        assert_eq!(metadata.parts.len(), 5);
    }

    #[test]
    fn test_parse_metadata_parts() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();

        assert_eq!(
            metadata.parts[0].checksum,
            "55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725"
        );
        assert_eq!(
            metadata.parts[0].filename,
            "zzXEL_eur_ortho-1.0.0.tar.gz.aa"
        );
        assert_eq!(
            metadata.parts[0].url,
            "https://dl.example.com/zzXEL_eur_ortho-1.0.0.tar.gz.aa"
        );

        assert_eq!(
            metadata.parts[4].filename,
            "zzXEL_eur_ortho-1.0.0.tar.gz.ae"
        );
    }

    #[test]
    fn test_parse_metadata_overlay() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Y
yzXEL_eur_overlay
yzXEL_eur_overlay-1.0.0.tar.gz
1


abc123  yzXEL_eur_overlay-1.0.0.tar.gz.aa  https://example.com/file.aa
"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert_eq!(metadata.package_type, PackageType::Overlay);
        assert_eq!(metadata.mountpoint, "yzXEL_eur_overlay");
    }

    #[test]
    fn test_parse_metadata_invalid_header() {
        let content = r#"WRONG HEADER
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1

abc  file.aa  http://example.com/file.aa
"#;
        let result = parse_package_metadata(content);
        assert!(matches!(result, Err(MetadataParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_parse_metadata_insufficient_lines() {
        let content = "REGIONAL SCENERY PACKAGE\n1.0.0\n";
        let result = parse_package_metadata(content);
        assert!(matches!(result, Err(MetadataParseError::InsufficientLines)));
    }

    #[test]
    fn test_parse_metadata_refuses_higher_spec_major() {
        let content = sample_metadata_content().replacen("1.0.0\nEUROPE", "2.0.0\nEUROPE", 1);
        let err = parse_package_metadata(&content).unwrap_err();
        assert_eq!(
            err,
            MetadataParseError::UnsupportedSpecVersion {
                found: Version::new(2, 0, 0),
                supported_major: 1,
            }
        );
        assert!(err.to_string().contains("Upgrade XEarthLayer"), "{err}");
    }

    #[test]
    fn test_parse_metadata_accepts_higher_minor_same_major() {
        let content = sample_metadata_content().replacen("1.0.0\nEUROPE", "1.2.0\nEUROPE", 1);
        let metadata = parse_package_metadata(&content).unwrap();
        assert_eq!(metadata.spec_version, Version::new(1, 2, 0));
    }

    #[test]
    fn test_parse_metadata_invalid_version() {
        let content = r#"REGIONAL SCENERY PACKAGE
invalid
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1


abc  file.aa  http://example.com/file.aa
"#;
        let result = parse_package_metadata(content);
        assert!(matches!(
            result,
            Err(MetadataParseError::InvalidSpecVersion(_))
        ));
    }

    #[test]
    fn test_parse_metadata_invalid_package_type() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
X
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1


abc  file.aa  http://example.com/file.aa
"#;
        let result = parse_package_metadata(content);
        assert!(matches!(
            result,
            Err(MetadataParseError::InvalidPackageType(_))
        ));
    }

    #[test]
    fn test_parse_metadata_part_count_mismatch() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
3


abc  file.aa  http://example.com/file.aa
"#;
        let result = parse_package_metadata(content);
        assert!(matches!(
            result,
            Err(MetadataParseError::PartCountMismatch {
                expected: 3,
                actual: 1
            })
        ));
    }

    #[test]
    fn test_serialize_metadata() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        let serialized = serialize_package_metadata(&metadata);

        // Parse the serialized output
        let reparsed = parse_package_metadata(&serialized).unwrap();

        assert_eq!(metadata.spec_version, reparsed.spec_version);
        assert_eq!(metadata.title, reparsed.title);
        assert_eq!(metadata.package_version, reparsed.package_version);
        assert_eq!(metadata.package_type, reparsed.package_type);
        assert_eq!(metadata.mountpoint, reparsed.mountpoint);
        assert_eq!(metadata.filename, reparsed.filename);
        assert_eq!(metadata.parts.len(), reparsed.parts.len());
    }

    #[test]
    fn test_roundtrip() {
        let original = parse_package_metadata(sample_metadata_content()).unwrap();
        let serialized = serialize_package_metadata(&original);
        let reparsed = parse_package_metadata(&serialized).unwrap();

        assert_eq!(original, reparsed);
    }

    #[test]
    fn test_folder_name() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        assert_eq!(metadata.folder_name(), "zzXEL_eur_ortho");
    }

    #[test]
    fn test_metadata_parse_error_display() {
        let err = MetadataParseError::InvalidHeader("wrong".to_string());
        assert!(err.to_string().contains("wrong"));

        let err = MetadataParseError::PartCountMismatch {
            expected: 5,
            actual: 3,
        };
        assert!(err.to_string().contains("5"));
        assert!(err.to_string().contains("3"));
    }

    #[test]
    fn test_parse_metadata_zero_parts() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
0


"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert!(metadata.parts.is_empty());
    }

    #[test]
    fn test_parse_metadata_parts_without_urls() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
2


abc123  file.aa
def456  file.ab
"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert_eq!(metadata.parts.len(), 2);
        assert!(metadata.parts[0].url.is_empty());
        assert!(metadata.parts[1].url.is_empty());
    }

    #[test]
    fn test_validation_initial_context() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        // Full metadata is valid for initial context
        assert!(metadata.is_valid(ValidationContext::Initial));
        assert!(metadata.validate(ValidationContext::Initial).is_empty());
    }

    #[test]
    fn test_validation_initial_allows_no_parts() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
0


"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert!(metadata.is_valid(ValidationContext::Initial));
    }

    #[test]
    fn test_validation_awaiting_urls_requires_parts() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
0


"#;
        let metadata = parse_package_metadata(content).unwrap();
        let errors = metadata.validate(ValidationContext::AwaitingUrls);
        assert!(!errors.is_empty());
        assert!(matches!(errors[0], MetadataValidationError::NoParts));
    }

    #[test]
    fn test_validation_awaiting_urls_allows_empty_urls() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1


abc123  file.aa
"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert!(metadata.is_valid(ValidationContext::AwaitingUrls));
    }

    #[test]
    fn test_validation_release_requires_urls() {
        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1


abc123  file.aa
"#;
        let metadata = parse_package_metadata(content).unwrap();
        let errors = metadata.validate(ValidationContext::Release);
        assert!(!errors.is_empty());
        assert!(matches!(
            errors[0],
            MetadataValidationError::MissingUrl { .. }
        ));
    }

    #[test]
    fn test_validation_release_valid() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        assert!(metadata.is_valid(ValidationContext::Release));
    }

    #[test]
    fn test_validation_empty_checksum() {
        // Create metadata programmatically with empty checksum since
        // the parser splits on "  " which makes it hard to test with raw content
        let mut metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        metadata.parts[0].checksum = String::new();

        let errors = metadata.validate(ValidationContext::Release);
        assert!(errors
            .iter()
            .any(|e| matches!(e, MetadataValidationError::EmptyChecksum { .. })));
    }

    #[test]
    fn test_has_parts() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        assert!(metadata.has_parts());

        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
0


"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert!(!metadata.has_parts());
    }

    #[test]
    fn test_has_all_urls() {
        let metadata = parse_package_metadata(sample_metadata_content()).unwrap();
        assert!(metadata.has_all_urls());

        let content = r#"REGIONAL SCENERY PACKAGE
1.0.0
EUROPE  1.0.0
2025-12-20T20:49:23Z
Z
zzXEL_eur_ortho
zzXEL_eur_ortho-1.0.0.tar.gz
1


abc123  file.aa
"#;
        let metadata = parse_package_metadata(content).unwrap();
        assert!(!metadata.has_all_urls());
    }

    #[test]
    fn test_validation_error_display() {
        let err = MetadataValidationError::NoParts;
        assert!(err.to_string().contains("no archive parts"));

        let err = MetadataValidationError::MissingUrl {
            part_index: 0,
            filename: "file.aa".to_string(),
        };
        assert!(err.to_string().contains("missing URL"));
        assert!(err.to_string().contains("file.aa"));

        let err = MetadataValidationError::EmptyChecksum {
            part_index: 1,
            filename: "file.ab".to_string(),
        };
        assert!(err.to_string().contains("empty checksum"));
    }

    #[test]
    fn a_title_that_could_escape_its_parent_directory_is_rejected() {
        // The title becomes the install directory name under Custom Scenery,
        // and this metadata arrives over HTTP, so a traversal here would let a
        // hostile package extract outside the scenery folder.
        let hostile =
            sample_metadata_content().replace("EUROPE  1.0.0", "x/../../../../tmp/evil  1.0.0");

        let err = parse_package_metadata(&hostile)
            .expect_err("a title containing a path separator must not parse");

        assert!(
            err.to_string().contains("x/../../../../tmp/evil"),
            "the error should name the offending title: {err}"
        );
    }

    #[test]
    fn an_ordinary_hyphenated_title_still_parses() {
        let renamed =
            sample_metadata_content().replace("EUROPE  1.0.0", "NA-USA-MX-CENTRAL  1.0.0");

        let metadata = parse_package_metadata(&renamed)
            .expect("a descriptive region code is a legitimate title");

        assert_eq!(metadata.title, "NA-USA-MX-CENTRAL");
    }
}
