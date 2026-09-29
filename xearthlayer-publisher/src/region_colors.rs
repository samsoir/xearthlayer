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
use xearthlayer_package::PackageType;

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

/// Record a package's download and installed sizes for one region.
///
/// The file is edited as a `serde_json::Value` rather than through
/// [`RegionMetadata`], which models only `color`. A typed round trip would
/// delete `name`, `coverage`, `status`, `supersedes` and anything the website
/// adds later. Key order is preserved by serde_json's `preserve_order` feature,
/// so a release shows a two line diff rather than a reordered file.
///
/// A region with no entry is an error rather than a new entry: the coverage map
/// would reject it anyway for having no colour, and inventing a half populated
/// region is how a wrong map shipped once already (#200).
pub fn write_region_size(
    metadata_path: &Path,
    region: &str,
    package_type: PackageType,
    download_bytes: u64,
    installed_bytes: u64,
) -> PublishResult<()> {
    if !metadata_path.exists() {
        return Err(PublishError::RegionMetadataNotFound(
            metadata_path.to_path_buf(),
        ));
    }

    let contents =
        std::fs::read_to_string(metadata_path).map_err(|source| PublishError::ReadFailed {
            path: metadata_path.to_path_buf(),
            source,
        })?;

    let mut document: serde_json::Value =
        serde_json::from_str(&contents).map_err(|e| PublishError::InvalidRegionMetadata {
            path: metadata_path.to_path_buf(),
            message: e.to_string(),
        })?;

    let key = region.to_uppercase();

    let entry = document
        .get_mut("regions")
        .and_then(|regions| regions.get_mut(&key))
        .ok_or_else(|| PublishError::InvalidRegionMetadata {
            path: metadata_path.to_path_buf(),
            message: format!("no entry for region {key}; add it before releasing"),
        })?;

    let sizes = entry
        .as_object_mut()
        .ok_or_else(|| PublishError::InvalidRegionMetadata {
            path: metadata_path.to_path_buf(),
            message: format!("entry for region {key} is not an object"),
        })?
        .entry("size")
        .or_insert_with(|| serde_json::json!({}));

    sizes[package_type.folder_suffix()] = serde_json::json!({
        "download_bytes": download_bytes,
        "installed_bytes": installed_bytes,
    });

    let mut rendered = serde_json::to_string_pretty(&document).map_err(|e| {
        PublishError::InvalidRegionMetadata {
            path: metadata_path.to_path_buf(),
            message: e.to_string(),
        }
    })?;
    rendered.push('\n');

    std::fs::write(metadata_path, rendered).map_err(|source| PublishError::WriteFailed {
        path: metadata_path.to_path_buf(),
        source,
    })
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

    const METADATA_WITH_NA: &str = r#"{
  "schema_version": 2,
  "regions": {
    "NA-USA-MX-CENTRAL": {
      "name": "North America: United States, Mexico and Central America",
      "coverage": "CONUS, Alaska, Hawaii, Mexico, Central America, the Caribbean and Bermuda",
      "color": "blue",
      "status": "staging",
      "supersedes": ["NA"]
    },
    "OC": {
      "name": "Oceania",
      "coverage": "Australia and New Zealand",
      "color": "purple"
    }
  }
}"#;

    fn parse(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn writing_a_size_records_both_figures_under_the_package_type() {
        let file = write_temp(METADATA_WITH_NA);

        write_region_size(
            file.path(),
            "na-usa-mx-central",
            PackageType::Ortho,
            34_000_000_000,
            52_000_000_000,
        )
        .unwrap();

        let size = &parse(file.path())["regions"]["NA-USA-MX-CENTRAL"]["size"]["ortho"];
        assert_eq!(size["download_bytes"], 34_000_000_000u64);
        assert_eq!(size["installed_bytes"], 52_000_000_000u64);
    }

    #[test]
    fn writing_a_size_preserves_fields_this_crate_does_not_model() {
        let file = write_temp(METADATA_WITH_NA);

        write_region_size(file.path(), "NA-USA-MX-CENTRAL", PackageType::Ortho, 1, 2).unwrap();

        let region = &parse(file.path())["regions"]["NA-USA-MX-CENTRAL"];
        assert_eq!(region["color"], "blue");
        assert_eq!(region["status"], "staging");
        assert_eq!(region["supersedes"][0], "NA");
        assert!(region["name"].is_string(), "name must survive the write");
        assert!(
            region["coverage"].is_string(),
            "coverage must survive the write"
        );
        assert_eq!(
            parse(file.path())["schema_version"],
            2,
            "top level keys must survive the write"
        );
    }

    #[test]
    fn writing_one_package_type_leaves_the_other_alone() {
        let file = write_temp(METADATA_WITH_NA);

        write_region_size(file.path(), "NA-USA-MX-CENTRAL", PackageType::Ortho, 10, 20).unwrap();
        write_region_size(
            file.path(),
            "NA-USA-MX-CENTRAL",
            PackageType::Overlay,
            30,
            40,
        )
        .unwrap();

        let size = &parse(file.path())["regions"]["NA-USA-MX-CENTRAL"]["size"];
        assert_eq!(size["ortho"]["download_bytes"], 10);
        assert_eq!(size["overlay"]["download_bytes"], 30);
    }

    #[test]
    fn writing_a_size_leaves_other_regions_untouched() {
        let file = write_temp(METADATA_WITH_NA);

        write_region_size(file.path(), "NA-USA-MX-CENTRAL", PackageType::Ortho, 1, 2).unwrap();

        let other = &parse(file.path())["regions"]["OC"];
        assert_eq!(other["color"], "purple");
        assert!(other.get("size").is_none(), "OC must not gain a size block");
    }

    #[test]
    fn an_absent_region_is_an_error_naming_it() {
        let file = write_temp(METADATA_WITH_NA);

        let err = write_region_size(file.path(), "sa-north", PackageType::Ortho, 1, 2)
            .expect_err("a region with no metadata entry must not be invented");

        assert!(
            err.to_string().contains("SA-NORTH"),
            "the error should name the missing region: {err}"
        );
    }

    #[test]
    fn an_absent_metadata_file_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();

        write_region_size(
            &temp.path().join("region_metadata.json"),
            "NA",
            PackageType::Ortho,
            1,
            2,
        )
        .expect_err("a missing metadata file must not be created silently");
    }
}
