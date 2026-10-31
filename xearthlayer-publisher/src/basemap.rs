//! Locally rendered basemap for coverage maps.
//!
//! A coverage map answers one question, which land do we cover, and that needs
//! a land silhouette and our regions drawn over it. Nothing else. Fetching that
//! silhouette as raster tiles from a third party cost an API key, a network
//! round trip per tile, and a failure mode that reported success: CartoDB began
//! answering HTTP 200 with a watermark reading "API KEY REQUIRED", which is a
//! valid PNG and composites cleanly, so every check short of looking at the
//! image passed (#289).
//!
//! The silhouette is drawn here instead, from Natural Earth 1:110m land
//! polygons vendored in `assets/land_110m.json`. Natural Earth data is in the
//! public domain. Coordinates are rounded to two decimal places, roughly 1.1 km
//! at the equator, which is finer than a pixel on any map this renders: a
//! 1200 pixel wide world gives about 33 km per pixel.
//!
//! Rendering locally makes the output deterministic, reproducible offline and
//! in CI, and immune to anyone else's change of terms.

use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Shader, Transform};

/// Natural Earth 1:110m land, as an array of closed rings of `[lon, lat]`.
const LAND_110M: &str = include_str!("../assets/land_110m.json");

/// Pixels per tile, and so the width of the whole world at zoom 0.
const TILE_SIZE: f64 = 256.0;

/// The latitude at which Web Mercator is conventionally truncated, where the
/// projection would otherwise run to infinity at the poles.
const MERCATOR_LIMIT: f64 = 85.051_128_78;

/// Maps geographic coordinates onto pixels of the output image.
///
/// Web Mercator, the same projection the tile servers used, so the region
/// rectangles drawn over the basemap need no change.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    centre_x: f64,
    centre_y: f64,
    width: f64,
    height: f64,
    world_size: f64,
}

impl Projection {
    /// Build a projection for a viewport of `width` by `height` pixels centred
    /// on `centre_lat`, `centre_lon` at `zoom`.
    pub fn new(centre_lat: f64, centre_lon: f64, zoom: u8, width: u32, height: u32) -> Self {
        let world = TILE_SIZE * (1_u32 << zoom) as f64;
        Self {
            centre_x: Self::world_x(centre_lon, world),
            centre_y: Self::world_y(centre_lat, world),
            width: width as f64,
            height: height as f64,
            world_size: world,
        }
    }

    fn world_x(lon: f64, world: f64) -> f64 {
        (lon + 180.0) / 360.0 * world
    }

    fn world_y(lat: f64, world: f64) -> f64 {
        let lat = lat.clamp(-MERCATOR_LIMIT, MERCATOR_LIMIT).to_radians();
        (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * world
    }

    /// Project a coordinate to a pixel position in the output image.
    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        (
            Self::world_x(lon, self.world_size) - self.centre_x + self.width / 2.0,
            Self::world_y(lat, self.world_size) - self.centre_y + self.height / 2.0,
        )
    }
}

/// The vendored coastline, as closed rings of `(lon, lat)`.
///
/// Parsed on each call. The data is 76 KB and 5,129 points, so this costs
/// microseconds and saves holding a global.
pub fn land_rings() -> Vec<Vec<(f64, f64)>> {
    let raw: Vec<Vec<[f64; 2]>> =
        serde_json::from_str(LAND_110M).expect("vendored coastline is valid JSON");
    raw.into_iter()
        .map(|ring| ring.into_iter().map(|p| (p[0], p[1])).collect())
        .collect()
}

/// Fill the image with sea, then draw the land silhouette over it.
pub fn draw(pixmap: &mut PixmapMut, projection: &Projection, sea: Color, land: Color) {
    pixmap.fill(sea);

    let paint = Paint {
        shader: Shader::SolidColor(land),
        anti_alias: true,
        ..Default::default()
    };

    for ring in land_rings() {
        let mut builder = PathBuilder::new();
        let mut points = ring.iter().map(|&(lon, lat)| projection.project(lat, lon));
        let Some((x, y)) = points.next() else {
            continue;
        };
        builder.move_to(x as f32, y as f32);
        for (x, y) in points {
            builder.line_to(x as f32, y as f32);
        }
        builder.close();

        if let Some(path) = builder.finish() {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At zoom 2 the whole world is 1024 pixels wide, so a 1024 square
    /// viewport centred on the origin holds exactly one copy of it.
    fn world_at_zoom_2() -> Projection {
        Projection::new(0.0, 0.0, 2, 1024, 1024)
    }

    #[test]
    fn the_centre_of_the_viewport_is_the_centre_coordinate() {
        let p = world_at_zoom_2();

        let (x, y) = p.project(0.0, 0.0);

        assert!((x - 512.0).abs() < 0.01, "x was {x}");
        assert!((y - 512.0).abs() < 0.01, "y was {y}");
    }

    #[test]
    fn the_antimeridians_land_on_the_left_and_right_edges() {
        let p = world_at_zoom_2();

        assert!((p.project(0.0, -180.0).0 - 0.0).abs() < 0.01);
        assert!((p.project(0.0, 180.0).0 - 1024.0).abs() < 0.01);
    }

    #[test]
    fn latitude_increases_northward_up_the_image() {
        let p = world_at_zoom_2();

        let north = p.project(45.0, 0.0).1;
        let south = p.project(-45.0, 0.0).1;

        assert!(north < 512.0, "north of the equator must be above centre");
        assert!(south > 512.0, "south of the equator must be below centre");
        assert!(
            ((north - 512.0) + (south - 512.0)).abs() < 0.01,
            "Mercator is symmetric about the equator"
        );
    }

    #[test]
    fn the_vendored_coastline_parses_into_closed_rings() {
        let rings = land_rings();

        assert_eq!(rings.len(), 128, "Natural Earth 1:110m land has 128 rings");
        for ring in &rings {
            assert!(ring.len() >= 4, "a polygon ring needs at least 4 points");
            assert_eq!(
                ring.first(),
                ring.last(),
                "a ring must close back on itself"
            );
        }
    }

    #[test]
    fn every_coastline_coordinate_is_on_the_globe() {
        for ring in land_rings() {
            for (lon, lat) in ring {
                assert!((-180.0..=180.0).contains(&lon), "longitude {lon}");
                assert!((-90.0..=90.0).contains(&lat), "latitude {lat}");
            }
        }
    }

    /// Sea and land must be visually distinct, which is the whole reason the
    /// basemap exists: without it, land we do not cover is indistinguishable
    /// from ocean.
    #[test]
    fn land_is_drawn_over_the_sea_and_the_two_differ() {
        let mut data = vec![0u8; 1024 * 1024 * 4];
        let mut pixmap = PixmapMut::from_bytes(&mut data, 1024, 1024).unwrap();
        let p = world_at_zoom_2();
        let sea = Color::from_rgba8(10, 20, 30, 255);
        let land = Color::from_rgba8(200, 200, 200, 255);

        draw(&mut pixmap, &p, sea, land);

        let at = |lat: f64, lon: f64| {
            let (x, y) = p.project(lat, lon);
            let i = ((y as usize) * 1024 + (x as usize)) * 4;
            (data[i], data[i + 1], data[i + 2])
        };

        // Central Africa is land; the middle of the South Pacific is not.
        assert_eq!(
            at(5.0, 20.0),
            (200, 200, 200),
            "central Africa should be land"
        );
        assert_eq!(
            at(-30.0, -130.0),
            (10, 20, 30),
            "the South Pacific should be sea"
        );
    }
}
