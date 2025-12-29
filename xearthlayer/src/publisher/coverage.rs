//! Coverage map generator for visualizing package tile coverage.
//!
//! This module generates static PNG images showing the geographic coverage
//! of scenery packages using OpenStreetMap tiles as a base map.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::publisher::coverage::{CoverageMapGenerator, CoverageConfig};
//!
//! let generator = CoverageMapGenerator::new(CoverageConfig::default());
//! generator.generate("/path/to/packages", "/path/to/output.png")?;
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use regex::Regex;
use staticmap::tools::Tool;
use staticmap::{lat_to_y, lon_to_x, Bounds, StaticMapBuilder};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Shader, Stroke, Transform};

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
    /// Get the tile server URL template for this style.
    pub fn url_template(&self) -> &'static str {
        match self {
            MapStyle::Light => "https://a.tile.osm.org/{z}/{x}/{y}.png",
            MapStyle::Dark => "https://a.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png",
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
        let mut region_colors = HashMap::new();
        // Blue for NA (matches GeoJSON)
        region_colors.insert("na".to_string(), (51, 136, 255, 180));
        // Orange for EU (matches GeoJSON)
        region_colors.insert("eu".to_string(), (255, 136, 0, 180));
        // Green for other regions
        region_colors.insert("as".to_string(), (0, 200, 83, 180));
        region_colors.insert("oc".to_string(), (156, 39, 176, 180));

        Self {
            width: 1200,
            height: 600,
            padding: (20, 20),
            region_colors,
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
        let mut region_colors = HashMap::new();
        // Brighter colors for dark background
        region_colors.insert("na".to_string(), (100, 180, 255, 200));
        region_colors.insert("eu".to_string(), (255, 180, 100, 200));
        region_colors.insert("as".to_string(), (100, 255, 150, 200));
        region_colors.insert("oc".to_string(), (200, 100, 255, 200));

        Self {
            width: 1200,
            height: 600,
            padding: (20, 20),
            region_colors,
            default_color: (150, 150, 150, 200),
            border_color: (80, 80, 80, 255),
            border_width: 0.5,
            style: MapStyle::Dark,
        }
    }
}

/// A filled rectangle tool for staticmap.
///
/// This implements the `Tool` trait to draw filled rectangles representing
/// tile coverage on the map.
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

impl Tool for FilledRect {
    fn extent(&self, _zoom: u8, _tile_size: f64) -> (f64, f64, f64, f64) {
        (self.lon_min, self.lat_min, self.lon_max, self.lat_max)
    }

    fn draw(&self, bounds: &Bounds, mut pixmap: PixmapMut) {
        let mut path_builder = PathBuilder::new();

        // Convert lat/lon corners to pixel coordinates
        let x1 = bounds.x_to_px(lon_to_x(self.lon_min, bounds.zoom)) as f32;
        let y1 = bounds.y_to_px(lat_to_y(self.lat_max, bounds.zoom)) as f32; // north
        let x2 = bounds.x_to_px(lon_to_x(self.lon_max, bounds.zoom)) as f32;
        let y2 = bounds.y_to_px(lat_to_y(self.lat_min, bounds.zoom)) as f32; // south

        // Build rectangle path
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

        // Create staticmap builder with configured tile server
        let mut map = StaticMapBuilder::default()
            .width(self.config.width)
            .height(self.config.height)
            .padding(self.config.padding)
            .url_template(self.config.style.url_template())
            .build()
            .map_err(|e| PublishError::ArchiveFailed(format!("Failed to create map: {}", e)))?;

        // Add rectangles for each tile
        for tile in tiles {
            let fill_color = self
                .config
                .region_colors
                .get(&tile.region.to_lowercase())
                .copied()
                .unwrap_or(self.config.default_color);

            let rect = FilledRect::from_tile(
                tile.latitude,
                tile.longitude,
                fill_color,
                self.config.border_color,
                self.config.border_width,
            );

            map.add_tool(rect);
        }

        // Save the map
        map.save_png(output_path)
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

    #[test]
    fn test_filled_rect_extent() {
        let rect = FilledRect::from_tile(37, -122, (255, 0, 0, 255), (0, 0, 0, 255), 1.0);

        let (lon_min, lat_min, lon_max, lat_max) = rect.extent(10, 256.0);

        assert_eq!(lon_min, -122.0);
        assert_eq!(lat_min, 37.0);
        assert_eq!(lon_max, -121.0);
        assert_eq!(lat_max, 38.0);
    }

    #[test]
    fn test_coverage_config_default() {
        let config = CoverageConfig::default();

        assert_eq!(config.width, 1200);
        assert_eq!(config.height, 600);
        assert!(config.region_colors.contains_key("na"));
        assert!(config.region_colors.contains_key("eu"));
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
}
