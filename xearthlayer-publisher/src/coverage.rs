//! Coverage map generator for visualizing package tile coverage.
//!
//! This module generates static PNG images showing the geographic coverage
//! of scenery packages using OpenStreetMap tiles as a base map.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer_publisher::coverage::{CoverageMapGenerator, CoverageConfig};
//!
//! let generator = CoverageMapGenerator::new(CoverageConfig::default());
//! generator.generate("/path/to/packages", "/path/to/output.png")?;
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use regex::Regex;
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapMut, Shader, Stroke, Transform,
};

use super::basemap::{self, Projection};

use super::region_colors::{brighten, resolve, RegionMetadata};
use super::{PublishError, PublishResult};

/// Map style/theme for the base layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapStyle {
    /// Standard OpenStreetMap tiles (light theme).
    #[default]
    Light,
    /// CartoDB Dark Matter tiles (dark theme).
    Dark,
}

impl MapStyle {
    /// Colour of the sea, which is also the background of the whole image.
    pub fn sea_color(&self) -> Color {
        match self {
            MapStyle::Light => Color::from_rgba8(0xc9, 0xdf, 0xef, 255),
            MapStyle::Dark => Color::from_rgba8(0x14, 0x18, 0x1d, 255),
        }
    }

    /// Colour of land the library does not cover. Distinguishing this from the
    /// sea is the only reason a basemap is drawn at all: without it, uncovered
    /// land reads as ocean.
    pub fn land_color(&self) -> Color {
        match self {
            MapStyle::Light => Color::from_rgba8(0xec, 0xe8, 0xe0, 255),
            MapStyle::Dark => Color::from_rgba8(0x2b, 0x30, 0x36, 255),
        }
    }
}

/// Configuration for coverage map generation.
#[derive(Debug, Clone)]
pub struct CoverageConfig {
    /// Width of the output image in pixels.
    pub width: u32,
    /// Height of the output image in pixels.
    pub height: u32,
    /// Padding around the coverage area in pixels (horizontal, vertical).
    pub padding: (u32, u32),
    /// Colors for different regions (region code -> RGBA).
    pub region_colors: HashMap<String, (u8, u8, u8, u8)>,
    /// Default color for regions without a specific color.
    pub default_color: (u8, u8, u8, u8),
    /// Border color for tiles (RGBA).
    pub border_color: (u8, u8, u8, u8),
    /// Border width in pixels.
    pub border_width: f32,
    /// Map style (light or dark theme).
    pub style: MapStyle,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 600,
            padding: (20, 20),
            region_colors: HashMap::new(),
            default_color: (100, 100, 100, 180),
            border_color: (0, 0, 0, 255),
            border_width: 0.5,
            style: MapStyle::default(),
        }
    }
}

impl CoverageConfig {
    /// Create a dark mode configuration with adjusted colors for visibility.
    pub fn dark() -> Self {
        Self {
            width: 1200,
            height: 600,
            padding: (20, 20),
            region_colors: HashMap::new(),
            default_color: (150, 150, 150, 200),
            border_color: (80, 80, 80, 255),
            border_width: 0.3,
            style: MapStyle::Dark,
        }
    }

    /// Populates `region_colors` from region metadata.
    ///
    /// Keys are lowercased because metadata uses uppercase codes ("AS1") while
    /// package directories on disk use lowercase ("as1"). Dark configs brighten
    /// each colour and use alpha 200; light configs use alpha 180.
    ///
    /// Whether to brighten is decided by `self.style`, so a caller cannot pair
    /// a dark config with light colours.
    pub fn with_regions(mut self, metadata: &RegionMetadata) -> PublishResult<Self> {
        let is_dark = self.style == MapStyle::Dark;
        let alpha = if is_dark { 200 } else { 180 };

        let mut colors = HashMap::new();
        for (code, entry) in &metadata.regions {
            let rgb = resolve(code, &entry.color)?;
            let rgb = if is_dark { brighten(rgb) } else { rgb };
            colors.insert(code.to_lowercase(), (rgb.0, rgb.1, rgb.2, alpha));
        }
        self.region_colors = colors;
        Ok(self)
    }
}

/// A filled rectangle representing one degree of tile coverage.
pub struct FilledRect {
    /// Minimum latitude (southern edge).
    lat_min: f64,
    /// Maximum latitude (northern edge).
    lat_max: f64,
    /// Minimum longitude (western edge).
    lon_min: f64,
    /// Maximum longitude (eastern edge).
    lon_max: f64,
    /// Fill paint.
    fill_paint: Paint<'static>,
    /// Border paint.
    border_paint: Paint<'static>,
    /// Border width.
    border_width: f32,
}

impl FilledRect {
    /// Create a new filled rectangle from tile coordinates.
    ///
    /// # Arguments
    /// * `lat` - Latitude of the southwest corner
    /// * `lon` - Longitude of the southwest corner
    /// * `fill_rgba` - Fill color as (r, g, b, a)
    /// * `border_rgba` - Border color as (r, g, b, a)
    /// * `border_width` - Border width in pixels
    pub fn from_tile(
        lat: i32,
        lon: i32,
        fill_rgba: (u8, u8, u8, u8),
        border_rgba: (u8, u8, u8, u8),
        border_width: f32,
    ) -> Self {
        let fill_color = Color::from_rgba8(fill_rgba.0, fill_rgba.1, fill_rgba.2, fill_rgba.3);
        let border_color =
            Color::from_rgba8(border_rgba.0, border_rgba.1, border_rgba.2, border_rgba.3);

        Self {
            lat_min: lat as f64,
            lat_max: (lat + 1) as f64,
            lon_min: lon as f64,
            lon_max: (lon + 1) as f64,
            fill_paint: Paint {
                shader: Shader::SolidColor(fill_color),
                anti_alias: true,
                ..Default::default()
            },
            border_paint: Paint {
                shader: Shader::SolidColor(border_color),
                anti_alias: true,
                ..Default::default()
            },
            border_width,
        }
    }
}

impl FilledRect {
    /// Draw this tile onto the map.
    fn draw(&self, projection: &Projection, pixmap: &mut PixmapMut) {
        // Convert lat/lon corners to pixel coordinates (raw values before any clamping)
        let (x1_raw, y1_raw) = projection.project(self.lat_max, self.lon_min); // north west
        let (x2_raw, y2_raw) = projection.project(self.lat_min, self.lon_max); // south east
        let (x1_raw, y1_raw) = (x1_raw as f32, y1_raw as f32);
        let (x2_raw, y2_raw) = (x2_raw as f32, y2_raw as f32);

        let img_width = pixmap.width() as f32;
        let img_height = pixmap.height() as f32;

        // Skip tiles that are completely outside the visible area
        // This prevents wrapped tiles from appearing on the wrong side of the map
        if x2_raw < 0.0 || x1_raw > img_width || y2_raw < 0.0 || y1_raw > img_height {
            return;
        }

        // Skip tiles whose width is impossibly large (indicating world wrapping)
        // A single degree of longitude should never span more than a small fraction of the image
        // At most zoom levels, a 1-degree tile should be less than img_width / 4
        let raw_width = (x2_raw - x1_raw).abs();
        if raw_width > img_width / 4.0 {
            return;
        }

        // Clamp to image bounds for tiles that are partially visible
        let x1 = x1_raw.max(0.0).min(img_width);
        let x2 = x2_raw.max(0.0).min(img_width);
        let y1 = y1_raw.max(0.0).min(img_height);
        let y2 = y2_raw.max(0.0).min(img_height);

        // Skip if the clamped rectangle is too small or invalid
        let width = (x2 - x1).abs();
        let height = (y2 - y1).abs();
        if width < 1.0 || height < 1.0 {
            return;
        }

        // Build rectangle path
        let mut path_builder = PathBuilder::new();
        path_builder.move_to(x1, y1);
        path_builder.line_to(x2, y1);
        path_builder.line_to(x2, y2);
        path_builder.line_to(x1, y2);
        path_builder.close();

        if let Some(path) = path_builder.finish() {
            // Fill the rectangle
            pixmap.fill_path(
                &path,
                &self.fill_paint,
                FillRule::Winding,
                Transform::default(),
                None,
            );

            // Draw border if width > 0
            if self.border_width > 0.0 {
                pixmap.stroke_path(
                    &path,
                    &self.border_paint,
                    &Stroke {
                        width: self.border_width,
                        ..Default::default()
                    },
                    Transform::default(),
                    None,
                );
            }
        }
    }
}

/// Tile information extracted from a package.
#[derive(Debug, Clone)]
pub struct TileCoverage {
    /// Region code (e.g., "na", "eu").
    pub region: String,
    /// Latitude of the southwest corner.
    pub latitude: i32,
    /// Longitude of the southwest corner.
    pub longitude: i32,
}

/// Coverage map generator.
pub struct CoverageMapGenerator {
    config: CoverageConfig,
}

impl CoverageMapGenerator {
    /// Create a new coverage map generator with the given configuration.
    pub fn new(config: CoverageConfig) -> Self {
        Self { config }
    }

    /// Scan packages directory and extract tile coverage information.
    ///
    /// # Arguments
    /// * `packages_dir` - Path to the packages directory
    ///
    /// # Returns
    /// Vector of tile coverage information for all packages.
    pub fn scan_packages(&self, packages_dir: &Path) -> PublishResult<Vec<TileCoverage>> {
        let mut tiles = Vec::new();
        // Regex for 10-degree block directories (e.g., +30-120)
        let block_regex = Regex::new(r"^([+-]\d{2})([+-]\d{3})$").expect("Valid regex");
        // Regex for DSF filenames (e.g., +30-111.dsf)
        let dsf_regex = Regex::new(r"^([+-]\d{2})([+-]\d{3})\.dsf$").expect("Valid regex");

        // Iterate over package directories
        let entries = fs::read_dir(packages_dir).map_err(|e| PublishError::ReadFailed {
            path: packages_dir.to_path_buf(),
            source: e,
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| PublishError::ReadFailed {
                path: packages_dir.to_path_buf(),
                source: e,
            })?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };

            // Parse package name: format is zzXEL_<region>_ortho or yzXEL_<region>_overlay
            let region = if dir_name.starts_with("zzXEL_") || dir_name.starts_with("yzXEL_") {
                let parts: Vec<&str> = dir_name.split('_').collect();
                if parts.len() >= 2 {
                    parts[1].to_string()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // Look for tile directories in Earth nav data
            let earth_nav_path = path.join("Earth nav data");
            if !earth_nav_path.exists() {
                continue;
            }

            let block_entries =
                fs::read_dir(&earth_nav_path).map_err(|e| PublishError::ReadFailed {
                    path: earth_nav_path.clone(),
                    source: e,
                })?;

            // Iterate over 10-degree block directories
            for block_entry in block_entries {
                let block_entry = block_entry.map_err(|e| PublishError::ReadFailed {
                    path: earth_nav_path.clone(),
                    source: e,
                })?;
                let block_path = block_entry.path();

                if !block_path.is_dir() {
                    continue;
                }

                let block_name = match block_path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name,
                    None => continue,
                };

                // Verify it's a valid 10-degree block directory
                if !block_regex.is_match(block_name) {
                    continue;
                }

                // Scan DSF files inside the block directory
                let dsf_entries =
                    fs::read_dir(&block_path).map_err(|e| PublishError::ReadFailed {
                        path: block_path.clone(),
                        source: e,
                    })?;

                for dsf_entry in dsf_entries {
                    let dsf_entry = dsf_entry.map_err(|e| PublishError::ReadFailed {
                        path: block_path.clone(),
                        source: e,
                    })?;

                    let dsf_name = match dsf_entry.file_name().to_str() {
                        Some(name) => name.to_string(),
                        None => continue,
                    };

                    // Parse DSF filename for tile coordinates
                    if let Some(captures) = dsf_regex.captures(&dsf_name) {
                        let lat: i32 = captures[1].parse().unwrap_or(0);
                        let lon: i32 = captures[2].parse().unwrap_or(0);

                        tiles.push(TileCoverage {
                            region: region.clone(),
                            latitude: lat,
                            longitude: lon,
                        });
                    }
                }
            }
        }

        Ok(tiles)
    }

    /// Choose the viewport that frames `tiles`.
    ///
    /// Extracted so a test can ask the same question the renderer does. A test
    /// that assumes a projection instead of asking for one checks a different
    /// image than the one generated, which is how two assertions here first
    /// came back reading the land colour.
    pub fn viewport(&self, tiles: &[TileCoverage]) -> Projection {
        // Calculate geographic bounds of all tiles
        let mut min_lat = 90i32;
        let mut max_lat = -90i32;
        let mut min_lon = 180i32;
        let mut max_lon = -180i32;

        for tile in tiles {
            min_lat = min_lat.min(tile.latitude);
            max_lat = max_lat.max(tile.latitude + 1); // +1 for tile extent
            min_lon = min_lon.min(tile.longitude);
            max_lon = max_lon.max(tile.longitude + 1);
        }

        // Calculate the longitude span
        let lon_span = (max_lon - min_lon) as f64;

        // Calculate zoom level and center to prevent world wrapping/repetition
        // At zoom z, world width = 256 * 2^z pixels
        // To prevent repetition: world_width >= image_width
        //
        // Zoom 1: 512px world  → repeats on 1200px image
        // Zoom 2: 1024px world → slight repetition on 1200px image
        // Zoom 3: 2048px world → no repetition, fits well

        // For global coverage (span > 180°), use zoom 2 centered appropriately
        // This provides a good balance between showing all regions and minimizing repetition
        let (center_lat, center_lon, zoom) = if lon_span > 180.0 {
            // Center latitude on our coverage (not equator) for better framing
            let center_lat = (min_lat + max_lat) as f64 / 2.0;

            // For longitude, center between our westernmost (NA ~-160°) and easternmost (OC ~+170°)
            // The "visual center" going eastward is approximately the Atlantic/Europe area
            // Using 0° (prime meridian) puts NA on the left and OC on the right, with the gap in the Pacific
            let center_lon = 0.0;

            // Zoom 2 shows 1024px world in 1200px image - slight edge repetition but all regions visible
            // The draw() function will filter out any wrapped tiles outside the visible area
            (center_lat, center_lon, 2_u8)
        } else {
            // For regional coverage, center on the tile bounds
            let center_lat = (min_lat + max_lat) as f64 / 2.0;
            let center_lon = (min_lon + max_lon) as f64 / 2.0;

            // Calculate zoom that fits the span with some padding
            // At zoom z, 360° of longitude = 256 * 2^z pixels
            // We want: span_in_pixels = (lon_span / 360) * 256 * 2^z <= width * 0.9 (leave 10% margin)
            let max_span_pixels = self.config.width as f64 * 0.9;
            let zoom = ((max_span_pixels * 360.0) / (lon_span * 256.0))
                .log2()
                .floor() as u8;
            let zoom = zoom.clamp(1, 6);

            (center_lat, center_lon, zoom)
        };

        Projection::new(
            center_lat,
            center_lon,
            zoom,
            self.config.width,
            self.config.height,
        )
    }

    /// Generate a coverage map from tile coverage data.
    ///
    /// # Arguments
    /// * `tiles` - Vector of tile coverage information
    /// * `output_path` - Path to save the PNG image
    pub fn generate_map(&self, tiles: &[TileCoverage], output_path: &Path) -> PublishResult<()> {
        if tiles.is_empty() {
            return Err(PublishError::InvalidSource(
                "No tiles found to generate coverage map".to_string(),
            ));
        }

        let projection = self.viewport(tiles);

        // Render locally rather than fetching raster tiles. See basemap.rs and
        // issue #289: the tile server began answering HTTP 200 with a watermark,
        // which every check short of looking at the image accepted.
        let mut pixmap = Pixmap::new(self.config.width, self.config.height).ok_or_else(|| {
            PublishError::ArchiveFailed(format!(
                "invalid map dimensions {}x{}",
                self.config.width, self.config.height
            ))
        })?;
        let mut canvas = pixmap.as_mut();

        basemap::draw(
            &mut canvas,
            &projection,
            self.config.style.sea_color(),
            self.config.style.land_color(),
        );

        for tile in tiles {
            let fill_color = self
                .config
                .region_colors
                .get(&tile.region.to_lowercase())
                .copied()
                .unwrap_or(self.config.default_color);

            FilledRect::from_tile(
                tile.latitude,
                tile.longitude,
                fill_color,
                self.config.border_color,
                self.config.border_width,
            )
            .draw(&projection, &mut canvas);
        }

        pixmap
            .save_png(output_path)
            .map_err(|e| PublishError::WriteFailed {
                path: output_path.to_path_buf(),
                source: std::io::Error::other(e.to_string()),
            })?;

        Ok(())
    }

    /// Generate a coverage map from a packages directory.
    ///
    /// This is a convenience method that combines `scan_packages` and `generate_map`.
    ///
    /// # Arguments
    /// * `packages_dir` - Path to the packages directory
    /// * `output_path` - Path to save the PNG image
    pub fn generate(&self, packages_dir: &Path, output_path: &Path) -> PublishResult<()> {
        let tiles = self.scan_packages(packages_dir)?;
        self.generate_map(&tiles, output_path)
    }

    /// Get the count of tiles by region.
    pub fn count_by_region(tiles: &[TileCoverage]) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for tile in tiles {
            *counts.entry(tile.region.to_lowercase()).or_insert(0) += 1;
        }
        counts
    }

    /// Generate a GeoJSON file from tile coverage data.
    ///
    /// Creates a GeoJSON FeatureCollection where each tile is represented as a
    /// Polygon feature with styling properties compatible with GitHub's GeoJSON
    /// renderer and other mapping tools.
    ///
    /// # Arguments
    /// * `tiles` - Vector of tile coverage information
    /// * `output_path` - Path to save the GeoJSON file
    pub fn generate_geojson(
        &self,
        tiles: &[TileCoverage],
        output_path: &Path,
    ) -> PublishResult<()> {
        use std::io::Write;

        if tiles.is_empty() {
            return Err(PublishError::InvalidSource(
                "No tiles found to generate GeoJSON".to_string(),
            ));
        }

        let mut file =
            std::fs::File::create(output_path).map_err(|e| PublishError::WriteFailed {
                path: output_path.to_path_buf(),
                source: e,
            })?;

        // Start the FeatureCollection
        write!(file, "{{\"type\": \"FeatureCollection\", \"features\": [").map_err(|e| {
            PublishError::WriteFailed {
                path: output_path.to_path_buf(),
                source: e,
            }
        })?;

        let mut first = true;
        for tile in tiles {
            let (r, g, b, _a) = self
                .config
                .region_colors
                .get(&tile.region.to_lowercase())
                .copied()
                .unwrap_or(self.config.default_color);

            let fill_color = format!("#{:02x}{:02x}{:02x}", r, g, b);
            let fill_opacity = 0.4;
            let stroke_opacity = 0.8;
            let stroke_width = 0.5;

            // Tile coordinates (southwest corner)
            let lat = tile.latitude;
            let lon = tile.longitude;

            // GeoJSON coordinates are [lon, lat] order
            // Polygon: SW -> SE -> NE -> NW -> SW (closed)
            let coords = format!(
                "[[{}, {}], [{}, {}], [{}, {}], [{}, {}], [{}, {}]]",
                lon,
                lat, // SW
                lon + 1,
                lat, // SE
                lon + 1,
                lat + 1, // NE
                lon,
                lat + 1, // NW
                lon,
                lat // Close
            );

            if !first {
                write!(file, ", ").map_err(|e| PublishError::WriteFailed {
                    path: output_path.to_path_buf(),
                    source: e,
                })?;
            }
            first = false;

            write!(
                file,
                "{{\"type\": \"Feature\", \"properties\": {{\"region\": \"{}\", \"fill\": \"{}\", \"fill-opacity\": {}, \"stroke\": \"{}\", \"stroke-width\": {}, \"stroke-opacity\": {}}}, \"geometry\": {{\"type\": \"Polygon\", \"coordinates\": [{}]}}}}",
                tile.region.to_uppercase(),
                fill_color,
                fill_opacity,
                fill_color,
                stroke_width,
                stroke_opacity,
                coords
            ).map_err(|e| PublishError::WriteFailed {
                path: output_path.to_path_buf(),
                source: e,
            })?;
        }

        // Close the FeatureCollection
        writeln!(file, "]}}").map_err(|e| PublishError::WriteFailed {
            path: output_path.to_path_buf(),
            source: e,
        })?;

        Ok(())
    }

    /// Generate a GeoJSON coverage file from a packages directory.
    ///
    /// This is a convenience method that combines `scan_packages` and `generate_geojson`.
    ///
    /// # Arguments
    /// * `packages_dir` - Path to the packages directory
    /// * `output_path` - Path to save the GeoJSON file
    pub fn generate_geojson_from_packages(
        &self,
        packages_dir: &Path,
        output_path: &Path,
    ) -> PublishResult<()> {
        let tiles = self.scan_packages(packages_dir)?;
        self.generate_geojson(&tiles, output_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_fixture() -> crate::RegionMetadata {
        serde_json::from_str(
            r#"{"regions":{
                "NA":{"color":"blue"},
                "AS1":{"color":"firebrick"}
            }}"#,
        )
        .unwrap()
    }

    // Metadata keys are uppercase ("AS1"); package region codes on disk are
    // lowercase ("as1"). Without normalisation every lookup would miss.
    #[test]
    fn with_regions_lowercases_keys() {
        let config = CoverageConfig::default()
            .with_regions(&metadata_fixture())
            .unwrap();
        assert!(config.region_colors.contains_key("na"));
        assert!(config.region_colors.contains_key("as1"));
        assert!(!config.region_colors.contains_key("NA"));
    }

    #[test]
    fn light_config_uses_metadata_colour_with_alpha_180() {
        let config = CoverageConfig::default()
            .with_regions(&metadata_fixture())
            .unwrap();
        assert_eq!(config.region_colors["na"], (0, 0, 255, 180));
    }

    #[test]
    fn dark_config_brightens_and_uses_alpha_200() {
        let config = CoverageConfig::dark()
            .with_regions(&metadata_fixture())
            .unwrap();
        assert_eq!(config.region_colors["na"], (89, 89, 255, 200));
    }

    #[test]
    fn with_regions_propagates_unknown_colour_error() {
        let md: crate::RegionMetadata =
            serde_json::from_str(r#"{"regions":{"EU2":{"color":"tangerine"}}}"#).unwrap();
        let err = CoverageConfig::default().with_regions(&md).unwrap_err();
        assert!(format!("{}", err).contains("EU2"));
    }

    #[test]
    fn a_tile_is_painted_at_its_own_coordinates() {
        // Replaces a former getter test for staticmap's Tool::extent. This
        // asserts the tile lands in the right place instead of restating its
        // fields back to itself.
        let temp = tempfile::TempDir::new().unwrap();
        let out = temp.path().join("map.png");
        let mut config = CoverageConfig::default();
        config
            .region_colors
            .insert("na".to_string(), (255, 0, 0, 255));

        let tiles = [TileCoverage {
            latitude: 40,
            longitude: -100,
            region: "na".into(),
        }];
        let generator = CoverageMapGenerator::new(config);
        generator.generate_map(&tiles, &out).unwrap();

        let img = image_pixel(&out, 1200, 600);
        let (x, y) = generator.viewport(&tiles).project(40.5, -99.5);

        assert_eq!(
            img(x as u32, y as u32),
            (255, 0, 0),
            "the tile should be painted in its region colour at its own location"
        );
    }

    #[test]
    fn test_coverage_config_default() {
        let config = CoverageConfig::default();

        assert_eq!(config.width, 1200);
        assert_eq!(config.height, 600);
        // Colours are no longer hardcoded; region_colors is populated by
        // `with_regions` from `region_metadata.json`, not by `default()`.
        assert!(config.region_colors.is_empty());
    }

    #[test]
    fn test_count_by_region() {
        let tiles = vec![
            TileCoverage {
                region: "na".to_string(),
                latitude: 37,
                longitude: -122,
            },
            TileCoverage {
                region: "na".to_string(),
                latitude: 38,
                longitude: -122,
            },
            TileCoverage {
                region: "eu".to_string(),
                latitude: 51,
                longitude: 0,
            },
        ];

        let counts = CoverageMapGenerator::count_by_region(&tiles);

        assert_eq!(counts.get("na"), Some(&2));
        assert_eq!(counts.get("eu"), Some(&1));
    }

    /// The basemap is rendered locally, so an ocean pixel is exactly the sea
    /// colour rather than whatever a tile server happened to serve. This is the
    /// assertion that raster tiles could not satisfy (#289).
    #[test]
    fn the_ocean_is_rendered_in_the_flat_sea_colour_with_no_network() {
        let temp = tempfile::TempDir::new().unwrap();
        let out = temp.path().join("map.png");
        let config = CoverageConfig::default();
        let sea = config.style.sea_color();

        // A world-spanning set so the renderer frames the whole globe, which is
        // what the published map does.
        let tiles = [
            TileCoverage {
                latitude: 40,
                longitude: -100,
                region: "na".into(),
            },
            TileCoverage {
                latitude: -30,
                longitude: 150,
                region: "oc".into(),
            },
        ];
        let generator = CoverageMapGenerator::new(config);
        generator
            .generate_map(&tiles, &out)
            .expect("a map must render without any network access");

        let img = image_pixel(&out, 1200, 600);
        // Middle of the South Pacific: no land, and no coverage tile.
        let (x, y) = generator.viewport(&tiles).project(-30.0, -130.0);
        assert_eq!(
            img(x as u32, y as u32),
            {
                let c = sea.to_color_u8();
                (c.red(), c.green(), c.blue())
            },
            "open ocean should be the flat sea colour"
        );
    }

    /// Reproducibility is the point of rendering locally: the same packages
    /// must give the same bytes on every run and every machine.
    #[test]
    fn two_runs_produce_identical_bytes() {
        let temp = tempfile::TempDir::new().unwrap();
        let a = temp.path().join("a.png");
        let b = temp.path().join("b.png");
        let tiles = [TileCoverage {
            latitude: 40,
            longitude: -100,
            region: "na".into(),
        }];

        let g = CoverageMapGenerator::new(CoverageConfig::default());
        g.generate_map(&tiles, &a).unwrap();
        g.generate_map(&tiles, &b).unwrap();

        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "rendering is deterministic"
        );
    }

    /// Read a PNG back as a pixel accessor, so a test can assert on what was
    /// actually drawn rather than on the fact that a file exists.
    fn image_pixel(path: &Path, w: u32, _h: u32) -> impl Fn(u32, u32) -> (u8, u8, u8) {
        let data = tiny_skia::Pixmap::decode_png(&std::fs::read(path).unwrap()).unwrap();
        let px = data.take();
        move |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (px[i], px[i + 1], px[i + 2])
        }
    }
}
