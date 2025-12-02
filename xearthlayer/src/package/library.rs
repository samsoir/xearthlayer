//! Package library index parsing and serialization.
//!
//! Handles the `xearthlayer_package_library.txt` file format.

use super::types::PackageType;
use chrono::{DateTime, Utc};
use semver::Version;
use std::fmt;
use std::str::FromStr;

/// Field separator used in the file format (two spaces).
const FIELD_SEPARATOR: &str = "  ";

/// Expected header line for library index files.
const LIBRARY_HEADER: &str = "XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY";

/// Package library index from `xearthlayer_package_library.txt`.
///
/// Contains a list of all available packages from a publisher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLibrary {
    /// Specification version of this library format
    pub spec_version: Version,

    /// Library scope (e.g., "EARTH")
    pub scope: String,

    /// Sequence number (increments with each publish)
    pub sequence: u64,

    /// Publication timestamp (UTC)
    pub published_at: DateTime<Utc>,

    /// Available packages
    pub entries: Vec<LibraryEntry>,
}

impl PackageLibrary {
    /// Create a new empty package library.
    pub fn new() -> Self {
        Self {
            spec_version: Version::new(1, 0, 0),
            scope: "EARTH".to_string(),
            sequence: 0,
            published_at: Utc::now(),
            entries: Vec::new(),
        }
    }

    /// Find an entry by region and package type.
    pub fn find(&self, region: &str, package_type: PackageType) -> Option<&LibraryEntry> {
        self.entries
            .iter()
            .find(|e| e.title.eq_ignore_ascii_case(region) && e.package_type == package_type)
    }

    /// Get all entries for a specific region.
    pub fn entries_for_region(&self, region: &str) -> Vec<&LibraryEntry> {
        self.entries
            .iter()
            .filter(|e| e.title.eq_ignore_ascii_case(region))
            .collect()
    }

    /// Get all unique region titles.
    pub fn regions(&self) -> Vec<&str> {
        let mut regions: Vec<&str> = self.entries.iter().map(|e| e.title.as_str()).collect();
        regions.sort();
        regions.dedup();
        regions
    }
}

impl Default for PackageLibrary {
    fn default() -> Self {
        Self::new()
    }
}

/// A single entry in the package library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    /// SHA-256 checksum of the package metadata file
    pub checksum: String,

    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,

    /// Region title (e.g., "EUROPE", "NORTH AMERICA")
    pub title: String,

    /// Package version
    pub version: Version,

    /// URL to the package's metadata file
    pub metadata_url: String,
}

impl LibraryEntry {
    /// Create a new library entry.
    pub fn new(
        checksum: impl Into<String>,
        package_type: PackageType,
        title: impl Into<String>,
        version: Version,
        metadata_url: impl Into<String>,
    ) -> Self {
        Self {
            checksum: checksum.into(),
            package_type,
            title: title.into(),
            version,
            metadata_url: metadata_url.into(),
        }
    }
}

/// Error parsing package library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryParseError {
    /// File is empty or has insufficient lines
    InsufficientLines,
    /// Invalid header line
    InvalidHeader(String),
    /// Invalid specification version
    InvalidSpecVersion(String),
    /// Invalid sequence number
    InvalidSequence(String),
    /// Invalid datetime format
    InvalidDateTime(String),
    /// Invalid package count
    InvalidPackageCount(String),
    /// Invalid entry line format
    InvalidEntryLine(String),
    /// Invalid package type in entry
    InvalidPackageType(String),
    /// Invalid version in entry
    InvalidVersion(String),
    /// Entry count mismatch
    EntryCountMismatch { expected: usize, actual: usize },
}

impl fmt::Display for LibraryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LibraryParseError::InsufficientLines => {
                write!(f, "library file has insufficient lines")
            }
            LibraryParseError::InvalidHeader(s) => {
                write!(
                    f,
                    "invalid header: expected '{}', got '{}'",
                    LIBRARY_HEADER, s
                )
            }
            LibraryParseError::InvalidSpecVersion(s) => {
                write!(f, "invalid specification version: {}", s)
            }
            LibraryParseError::InvalidSequence(s) => {
                write!(f, "invalid sequence number: {}", s)
            }
            LibraryParseError::InvalidDateTime(s) => {
                write!(f, "invalid datetime: {}", s)
            }
            LibraryParseError::InvalidPackageCount(s) => {
                write!(f, "invalid package count: {}", s)
            }
            LibraryParseError::InvalidEntryLine(s) => {
                write!(f, "invalid entry line: {}", s)
            }
            LibraryParseError::InvalidPackageType(s) => {
                write!(f, "invalid package type: {}", s)
            }
            LibraryParseError::InvalidVersion(s) => {
                write!(f, "invalid version: {}", s)
            }
            LibraryParseError::EntryCountMismatch { expected, actual } => {
                write!(
                    f,
                    "entry count mismatch: expected {}, got {}",
                    expected, actual
                )
            }
        }
    }
}

impl std::error::Error for LibraryParseError {}

/// Parse package library from string content.
///
/// # Format
///
/// ```text
/// XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
/// <specversion>
/// <scope>
/// <sequence>
/// <datetime>
/// <packagecount>
///
///
/// <sha256sum>  <packagetype>  <title>  <pkgversion>  <metadataurl>
/// ...
/// ```
///
/// Fields within lines are separated by two spaces.
pub fn parse_package_library(content: &str) -> Result<PackageLibrary, LibraryParseError> {
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() < 6 {
        return Err(LibraryParseError::InsufficientLines);
    }

    // Line 1: Header
    let header = lines[0].trim();
    if header != LIBRARY_HEADER {
        return Err(LibraryParseError::InvalidHeader(header.to_string()));
    }

    // Line 2: Spec version
    let spec_version = Version::from_str(lines[1].trim())
        .map_err(|e| LibraryParseError::InvalidSpecVersion(e.to_string()))?;

    // Line 3: Scope
    let scope = lines[2].trim().to_string();

    // Line 4: Sequence number
    let sequence: u64 = lines[3]
        .trim()
        .parse()
        .map_err(|_| LibraryParseError::InvalidSequence(lines[3].trim().to_string()))?;

    // Line 5: DateTime (UTC/Zulu)
    let datetime_str = lines[4].trim();
    let published_at = DateTime::parse_from_rfc3339(datetime_str)
        .map_err(|e| LibraryParseError::InvalidDateTime(e.to_string()))?
        .with_timezone(&Utc);

    // Line 6: Package count
    let package_count: usize = lines[5]
        .trim()
        .parse()
        .map_err(|_| LibraryParseError::InvalidPackageCount(lines[5].trim().to_string()))?;

    // Skip blank lines, then parse entries
    let mut entries = Vec::with_capacity(package_count);
    let mut entry_start_idx = 6;

    // Skip blank lines
    while entry_start_idx < lines.len() && lines[entry_start_idx].trim().is_empty() {
        entry_start_idx += 1;
    }

    // Parse entry lines
    for line in lines.iter().skip(entry_start_idx) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        if fields.len() != 5 {
            return Err(LibraryParseError::InvalidEntryLine(line.to_string()));
        }

        let checksum = fields[0].to_string();

        let type_char = fields[1]
            .chars()
            .next()
            .ok_or_else(|| LibraryParseError::InvalidPackageType("empty".to_string()))?;
        let package_type = PackageType::from_code(type_char)
            .ok_or_else(|| LibraryParseError::InvalidPackageType(type_char.to_string()))?;

        let title = fields[2].to_string();

        let version = Version::from_str(fields[3].trim())
            .map_err(|e| LibraryParseError::InvalidVersion(e.to_string()))?;

        let metadata_url = fields[4].to_string();

        entries.push(LibraryEntry {
            checksum,
            package_type,
            title,
            version,
            metadata_url,
        });
    }

    if entries.len() != package_count {
        return Err(LibraryParseError::EntryCountMismatch {
            expected: package_count,
            actual: entries.len(),
        });
    }

    Ok(PackageLibrary {
        spec_version,
        scope,
        sequence,
        published_at,
        entries,
    })
}

/// Serialize package library to string.
pub fn serialize_package_library(library: &PackageLibrary) -> String {
    let mut output = String::new();

    // Line 1: Header
    output.push_str(LIBRARY_HEADER);
    output.push('\n');

    // Line 2: Spec version
    output.push_str(&library.spec_version.to_string());
    output.push('\n');

    // Line 3: Scope
    output.push_str(&library.scope);
    output.push('\n');

    // Line 4: Sequence
    output.push_str(&library.sequence.to_string());
    output.push('\n');

    // Line 5: DateTime
    output.push_str(
        &library
            .published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    output.push('\n');

    // Line 6: Entry count
    output.push_str(&library.entries.len().to_string());
    output.push('\n');

    // Two blank lines
    output.push('\n');
    output.push('\n');

    // Entries
    for entry in &library.entries {
        output.push_str(&entry.checksum);
        output.push_str(FIELD_SEPARATOR);
        output.push(entry.package_type.code());
        output.push_str(FIELD_SEPARATOR);
        output.push_str(&entry.title);
        output.push_str(FIELD_SEPARATOR);
        output.push_str(&entry.version.to_string());
        output.push_str(FIELD_SEPARATOR);
        output.push_str(&entry.metadata_url);
        output.push('\n');
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_library_content() -> &'static str {
        r#"XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
1
2025-12-20T20:49:23Z
4


55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725  Z  EUROPE  1.0.0  https://example.com/eur/ortho/xearthlayer_scenery_package.txt
a1b2c3d4e5f6789012345678901234567890123456789012345678901234abcd  Y  EUROPE  1.0.0  https://example.com/eur/overlay/xearthlayer_scenery_package.txt
b91f75f976768c62f6215d4e16e1e0e8a5948dfb9fccf73e67699a6818933335  Z  NORTH AMERICA  1.0.0  https://example.com/na/ortho/xearthlayer_scenery_package.txt
c2d3e4f5a6b7890123456789012345678901234567890123456789012345bcde  Y  NORTH AMERICA  1.0.0  https://example.com/na/overlay/xearthlayer_scenery_package.txt
"#
    }

    #[test]
    fn test_parse_library() {
        let library = parse_package_library(sample_library_content()).unwrap();

        assert_eq!(library.spec_version, Version::new(1, 0, 0));
        assert_eq!(library.scope, "EARTH");
        assert_eq!(library.sequence, 1);
        assert_eq!(library.entries.len(), 4);
    }

    #[test]
    fn test_parse_library_entries() {
        let library = parse_package_library(sample_library_content()).unwrap();

        let eur_ortho = &library.entries[0];
        assert_eq!(eur_ortho.title, "EUROPE");
        assert_eq!(eur_ortho.package_type, PackageType::Ortho);
        assert_eq!(eur_ortho.version, Version::new(1, 0, 0));

        let na_overlay = &library.entries[3];
        assert_eq!(na_overlay.title, "NORTH AMERICA");
        assert_eq!(na_overlay.package_type, PackageType::Overlay);
    }

    #[test]
    fn test_library_find() {
        let library = parse_package_library(sample_library_content()).unwrap();

        let entry = library.find("EUROPE", PackageType::Ortho).unwrap();
        assert_eq!(entry.title, "EUROPE");
        assert_eq!(entry.package_type, PackageType::Ortho);

        let entry = library.find("europe", PackageType::Overlay).unwrap();
        assert_eq!(entry.title, "EUROPE");
        assert_eq!(entry.package_type, PackageType::Overlay);

        assert!(library.find("AUSTRALIA", PackageType::Ortho).is_none());
    }

    #[test]
    fn test_library_entries_for_region() {
        let library = parse_package_library(sample_library_content()).unwrap();

        let europe_entries = library.entries_for_region("EUROPE");
        assert_eq!(europe_entries.len(), 2);

        let na_entries = library.entries_for_region("north america");
        assert_eq!(na_entries.len(), 2);

        let au_entries = library.entries_for_region("AUSTRALIA");
        assert_eq!(au_entries.len(), 0);
    }

    #[test]
    fn test_library_regions() {
        let library = parse_package_library(sample_library_content()).unwrap();

        let regions = library.regions();
        assert_eq!(regions.len(), 2);
        assert!(regions.contains(&"EUROPE"));
        assert!(regions.contains(&"NORTH AMERICA"));
    }

    #[test]
    fn test_parse_library_invalid_header() {
        let content = "WRONG HEADER\n1.0.0\nEARTH\n1\n2025-12-20T20:49:23Z\n0\n";
        let result = parse_package_library(content);
        assert!(matches!(result, Err(LibraryParseError::InvalidHeader(_))));
    }

    #[test]
    fn test_parse_library_insufficient_lines() {
        let content = "XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY\n1.0.0\n";
        let result = parse_package_library(content);
        assert!(matches!(result, Err(LibraryParseError::InsufficientLines)));
    }

    #[test]
    fn test_parse_library_invalid_sequence() {
        let content = r#"XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
notanumber
2025-12-20T20:49:23Z
0
"#;
        let result = parse_package_library(content);
        assert!(matches!(result, Err(LibraryParseError::InvalidSequence(_))));
    }

    #[test]
    fn test_parse_library_entry_count_mismatch() {
        let content = r#"XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
1
2025-12-20T20:49:23Z
5


abc  Z  EUROPE  1.0.0  https://example.com/meta.txt
"#;
        let result = parse_package_library(content);
        assert!(matches!(
            result,
            Err(LibraryParseError::EntryCountMismatch {
                expected: 5,
                actual: 1
            })
        ));
    }

    #[test]
    fn test_serialize_library() {
        let library = parse_package_library(sample_library_content()).unwrap();
        let serialized = serialize_package_library(&library);

        let reparsed = parse_package_library(&serialized).unwrap();

        assert_eq!(library.spec_version, reparsed.spec_version);
        assert_eq!(library.scope, reparsed.scope);
        assert_eq!(library.sequence, reparsed.sequence);
        assert_eq!(library.entries.len(), reparsed.entries.len());
    }

    #[test]
    fn test_roundtrip() {
        let original = parse_package_library(sample_library_content()).unwrap();
        let serialized = serialize_package_library(&original);
        let reparsed = parse_package_library(&serialized).unwrap();

        assert_eq!(original, reparsed);
    }

    #[test]
    fn test_library_entry_new() {
        let entry = LibraryEntry::new(
            "abc123",
            PackageType::Ortho,
            "TEST REGION",
            Version::new(1, 2, 3),
            "https://example.com/meta.txt",
        );

        assert_eq!(entry.checksum, "abc123");
        assert_eq!(entry.package_type, PackageType::Ortho);
        assert_eq!(entry.title, "TEST REGION");
        assert_eq!(entry.version, Version::new(1, 2, 3));
        assert_eq!(entry.metadata_url, "https://example.com/meta.txt");
    }

    #[test]
    fn test_library_parse_error_display() {
        let err = LibraryParseError::InvalidHeader("wrong".to_string());
        assert!(err.to_string().contains("wrong"));

        let err = LibraryParseError::EntryCountMismatch {
            expected: 5,
            actual: 3,
        };
        assert!(err.to_string().contains("5"));
        assert!(err.to_string().contains("3"));
    }

    #[test]
    fn test_empty_library() {
        let content = r#"XEARTHLAYER REGIONAL SCENERY PACKAGE LIBRARY
1.0.0
EARTH
0
2025-12-20T20:49:23Z
0
"#;
        let library = parse_package_library(content).unwrap();
        assert_eq!(library.entries.len(), 0);
        assert!(library.regions().is_empty());
    }
}
