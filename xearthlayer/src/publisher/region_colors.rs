//! Region colour resolution for coverage maps.
//!
//! Colours come from `region_metadata.json` in the package repository root —
//! the same file the website legend reads — so adding a region requires no
//! Rust change. See issue #200.
//!
//! An unresolvable colour is a hard error rather than a grey fallback: a
//! silently grey region is exactly how AS2 v0.1.0 shipped wrong.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::{PublishError, PublishResult};

/// Fraction blended toward white to derive dark-mode colours.
const DARK_BLEND: f32 = 0.35;

/// Parsed `region_metadata.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionMetadata {
    /// Region code (e.g. "NA", "AS1") to its entry.
    pub regions: HashMap<String, RegionEntry>,
}

/// One region's metadata entry.
///
/// Only `color` is modelled — it is the only field the coverage map uses. The
/// real file also carries `name` and `coverage` for the website legend, which
/// reads the JSON directly and never goes through this type. Unknown fields are
/// ignored (no `deny_unknown_fields`), so those keys parse harmlessly and the
/// website can add more without breaking map generation.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionEntry {
    /// CSS colour name or hex string (e.g. "crimson", "#ffaa00").
    pub color: String,
}

impl RegionMetadata {
    /// Reads and parses the metadata file.
    pub fn load(path: &Path) -> PublishResult<Self> {
        if !path.exists() {
            return Err(PublishError::RegionMetadataNotFound(path.to_path_buf()));
        }
        let contents =
            std::fs::read_to_string(path).map_err(|source| PublishError::ReadFailed {
                path: path.to_path_buf(),
                source,
            })?;
        serde_json::from_str(&contents).map_err(|e| PublishError::InvalidRegionMetadata {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }
}

/// Resolves a CSS colour name or hex string to RGB.
///
/// `region` is used only so the error can name the offending entry.
pub fn resolve(region: &str, color: &str) -> PublishResult<(u8, u8, u8)> {
    let parsed =
        color
            .parse::<csscolorparser::Color>()
            .map_err(|_| PublishError::UnknownRegionColor {
                region: region.to_string(),
                color: color.to_string(),
            })?;
    let [r, g, b, _a] = parsed.to_rgba8();
    Ok((r, g, b))
}

/// Derives the dark-mode variant by blending toward white.
///
/// Uniform and predictable: already-bright colours barely move rather than
/// clipping, and saturated primaries lighten instead of turning fluorescent.
pub fn brighten(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    let blend = |c: u8| (c as f32 + (255.0 - c as f32) * DARK_BLEND).round() as u8;
    (blend(rgb.0), blend(rgb.1), blend(rgb.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn loads_valid_metadata() {
        let f = write_temp(
            r#"{"regions":{"NA":{"name":"North America","coverage":"US","color":"blue"}}}"#,
        );
        let md = RegionMetadata::load(f.path()).unwrap();
        assert_eq!(md.regions.get("NA").unwrap().color, "blue");
    }

    // Only `color` is required. The map never reads name/coverage; requiring
    // them would fail generation for a region that merely omits a description.
    #[test]
    fn color_is_the_only_required_field() {
        let f = write_temp(r#"{"regions":{"XX":{"color":"red"}}}"#);
        let md = RegionMetadata::load(f.path()).unwrap();
        assert_eq!(md.regions.get("XX").unwrap().color, "red");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let f = write_temp(r#"{"regions":{"XX":{"color":"red","future_key":42}}}"#);
        assert!(RegionMetadata::load(f.path()).is_ok());
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = RegionMetadata::load(Path::new("/nonexistent/region_metadata.json"));
        assert!(err.is_err());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let f = write_temp("{ not json");
        assert!(RegionMetadata::load(f.path()).is_err());
    }

    #[test]
    fn resolves_css_names() {
        assert_eq!(resolve("NA", "blue").unwrap(), (0, 0, 255));
        assert_eq!(resolve("EU", "orange").unwrap(), (255, 165, 0));
        assert_eq!(resolve("AS2", "crimson").unwrap(), (220, 20, 60));
    }

    #[test]
    fn resolves_hex() {
        assert_eq!(resolve("EU2", "#ffaa00").unwrap(), (255, 170, 0));
    }

    // The error must name the region, or a release engineer cannot tell which
    // metadata entry to fix.
    #[test]
    fn unknown_color_errors_and_names_the_region() {
        let err = resolve("EU2", "tangerine").unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("EU2"), "error should name the region: {}", msg);
        assert!(
            msg.contains("tangerine"),
            "error should name the color: {}",
            msg
        );
    }

    // Proves the ten CSS names actually used by the live repo all resolve.
    // EU2 is deliberately absent: its "tangerine" is not a CSS colour and is
    // being moved to hex in the regional-scenery repo.
    #[test]
    fn every_live_region_colour_resolves() {
        for (region, color) in [
            ("NA", "blue"),
            ("EU", "orange"),
            ("SA", "green"),
            ("OC", "purple"),
            ("AS1", "firebrick"),
            ("AS2", "crimson"),
            ("AS3", "red"),
            ("AS4", "palevioletred"),
            ("AF1", "cyan"),
            ("AF2", "yellowgreen"),
        ] {
            assert!(
                resolve(region, color).is_ok(),
                "{} colour {} should resolve",
                region,
                color
            );
        }
    }

    // name/coverage exist in the real file for the website legend but are not
    // fields on RegionEntry. Serde ignores them, so the map is unaffected.
    #[test]
    fn website_only_fields_are_ignored() {
        let f = write_temp(
            r#"{"regions":{"NA":{"name":"North America","coverage":"US","color":"blue"}}}"#,
        );
        let md = RegionMetadata::load(f.path()).unwrap();
        assert_eq!(md.regions.get("NA").unwrap().color, "blue");
    }

    #[test]
    fn brighten_blends_toward_white_by_35_percent() {
        assert_eq!(brighten((0, 0, 255)), (89, 89, 255));
        // 165 + (255-165)*0.35 == 196.5 exactly in f32; Rust's f32::round()
        // is round-half-away-from-zero, so this rounds to 197, not 196.
        assert_eq!(brighten((255, 165, 0)), (255, 197, 89));
        assert_eq!(brighten((0, 128, 0)), (89, 172, 89));
        assert_eq!(brighten((128, 0, 128)), (172, 89, 172));
        assert_eq!(brighten((220, 20, 60)), (232, 102, 128));
        assert_eq!(brighten((0, 255, 255)), (89, 255, 255));
    }
}
