//! Core types for the package system.

use std::fmt;

/// Package type identifier.
///
/// Determines how the package is installed and used:
/// - `Ortho`: Mounted via FUSE for on-demand texture generation
/// - `Overlay`: Installed via symlink (static content)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackageType {
    /// Orthophoto scenery package (type code: Z)
    ///
    /// Contains terrain mesh and texture references. DDS textures are
    /// generated on-demand by XEarthLayer FUSE filesystem.
    Ortho,

    /// Overlay scenery package (type code: Y)
    ///
    /// Contains roads, railways, powerlines, and other objects rendered
    /// above orthophoto scenery. All resources are static.
    Overlay,
}

impl PackageType {
    /// Get the single-character type code.
    ///
    /// - `Z` for Ortho
    /// - `Y` for Overlay
    pub fn code(&self) -> char {
        match self {
            PackageType::Ortho => 'Z',
            PackageType::Overlay => 'Y',
        }
    }

    /// Parse from single-character type code.
    ///
    /// # Errors
    ///
    /// Returns `None` if the character is not a valid type code.
    pub fn from_code(code: char) -> Option<Self> {
        match code.to_ascii_uppercase() {
            'Z' => Some(PackageType::Ortho),
            'Y' => Some(PackageType::Overlay),
            _ => None,
        }
    }

    /// Get the folder sort prefix for X-Plane Custom Scenery ordering.
    ///
    /// - `zz` for Ortho (loads last, lowest priority)
    /// - `yz` for Overlay (loads before ortho)
    pub fn sort_prefix(&self) -> &'static str {
        match self {
            PackageType::Ortho => "zz",
            PackageType::Overlay => "yz",
        }
    }

    /// Get the type suffix for folder naming.
    ///
    /// - `ortho` for Ortho
    /// - `overlay` for Overlay
    pub fn folder_suffix(&self) -> &'static str {
        match self {
            PackageType::Ortho => "ortho",
            PackageType::Overlay => "overlay",
        }
    }
}

impl fmt::Display for PackageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageType::Ortho => write!(f, "ortho"),
            PackageType::Overlay => write!(f, "overlay"),
        }
    }
}

/// A single part of a split archive.
///
/// Large packages are split into multiple parts for easier downloading.
/// Each part has its own checksum for verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePart {
    /// SHA-256 checksum of this part
    pub checksum: String,

    /// Local filename for this part (e.g., "zzXEL_eur_ortho-1.0.0.tar.gz.aa")
    pub filename: String,

    /// Remote URL to download this part
    pub url: String,
}

impl ArchivePart {
    /// Create a new archive part.
    pub fn new(
        checksum: impl Into<String>,
        filename: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            checksum: checksum.into(),
            filename: filename.into(),
            url: url.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_type_code() {
        assert_eq!(PackageType::Ortho.code(), 'Z');
        assert_eq!(PackageType::Overlay.code(), 'Y');
    }

    #[test]
    fn test_package_type_from_code() {
        assert_eq!(PackageType::from_code('Z'), Some(PackageType::Ortho));
        assert_eq!(PackageType::from_code('z'), Some(PackageType::Ortho));
        assert_eq!(PackageType::from_code('Y'), Some(PackageType::Overlay));
        assert_eq!(PackageType::from_code('y'), Some(PackageType::Overlay));
        assert_eq!(PackageType::from_code('X'), None);
        assert_eq!(PackageType::from_code('0'), None);
    }

    #[test]
    fn test_package_type_sort_prefix() {
        assert_eq!(PackageType::Ortho.sort_prefix(), "zz");
        assert_eq!(PackageType::Overlay.sort_prefix(), "yz");
    }

    #[test]
    fn test_package_type_folder_suffix() {
        assert_eq!(PackageType::Ortho.folder_suffix(), "ortho");
        assert_eq!(PackageType::Overlay.folder_suffix(), "overlay");
    }

    #[test]
    fn test_package_type_display() {
        assert_eq!(format!("{}", PackageType::Ortho), "ortho");
        assert_eq!(format!("{}", PackageType::Overlay), "overlay");
    }

    #[test]
    fn test_archive_part_new() {
        let part = ArchivePart::new(
            "55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725",
            "zzXEL_eur_ortho-1.0.0.tar.gz.aa",
            "https://example.com/zzXEL_eur_ortho-1.0.0.tar.gz.aa",
        );

        assert_eq!(
            part.checksum,
            "55e772c100c5f01cc148a7e9a66196e266adb22e2ca2116f81f8d138f9d7c725"
        );
        assert_eq!(part.filename, "zzXEL_eur_ortho-1.0.0.tar.gz.aa");
        assert_eq!(
            part.url,
            "https://example.com/zzXEL_eur_ortho-1.0.0.tar.gz.aa"
        );
    }

    #[test]
    fn test_archive_part_equality() {
        let part1 = ArchivePart::new("abc", "file.aa", "http://example.com/file.aa");
        let part2 = ArchivePart::new("abc", "file.aa", "http://example.com/file.aa");
        let part3 = ArchivePart::new("def", "file.aa", "http://example.com/file.aa");

        assert_eq!(part1, part2);
        assert_ne!(part1, part3);
    }

    #[test]
    fn test_archive_part_clone() {
        let part = ArchivePart::new("abc", "file.aa", "http://example.com/file.aa");
        let cloned = part.clone();
        assert_eq!(part, cloned);
    }

    #[test]
    fn test_archive_part_debug() {
        let part = ArchivePart::new("abc", "file.aa", "http://example.com/file.aa");
        let debug = format!("{:?}", part);
        assert!(debug.contains("abc"));
        assert!(debug.contains("file.aa"));
    }
}
