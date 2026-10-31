//! A lit texture's ground footprint.
//!
//! Whether any of it is lit is `RecipeField::has_light`, which shares the
//! generator's sampling model.

use std::f64::consts::PI;

use crate::fuse::DdsFilename;

/// Web Mercator tiles along each side of one Ortho4XP texture.
pub const TILES_PER_TEXTURE: u32 = 16;

/// The ground one lit texture covers, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Footprint {
    pub north: f64,
    pub south: f64,
    pub west: f64,
    pub east: f64,
    pub zoom: u8,
}

impl Footprint {
    /// The footprint of `<til_y_top>_<til_x_left>_<map><zoom>` (R5).
    pub fn from_name(name: &DdsFilename) -> Self {
        let n = f64::from(1u32 << name.zoom);
        let lon = |x: f64| x / n * 360.0 - 180.0;
        let lat = |y: f64| (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
        let (x, y) = (f64::from(name.col), f64::from(name.row));
        let side = f64::from(TILES_PER_TEXTURE);
        Self {
            north: lat(y),
            south: lat(y + side),
            west: lon(x),
            east: lon(x + side),
            zoom: name.zoom,
        }
    }

    /// The footprint grown by `degrees` on every side.
    pub fn expanded(&self, degrees: f64) -> Self {
        Self {
            north: self.north + degrees,
            south: self.south - degrees,
            west: self.west - degrees,
            east: self.east + degrees,
            zoom: self.zoom,
        }
    }

    /// South-west corners of every 1 degree tile the footprint touches.
    pub fn tiles(&self) -> Vec<(i32, i32)> {
        let lats = (self.south.floor() as i32)..=(self.north.ceil() as i32 - 1);
        let lons = (self.west.floor() as i32)..=(self.east.ceil() as i32 - 1);
        lats.flat_map(|lat| lons.clone().map(move |lon| (lat, lon)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::parse_dds_filename;

    #[test]
    fn expanded_grows_every_edge() {
        let f = fp(37.24, 37.16, -122.74, -122.71).expanded(0.01);
        assert!((f.north - 37.25).abs() < 1e-12 && (f.south - 37.15).abs() < 1e-12);
        assert!((f.west - -122.75).abs() < 1e-12 && (f.east - -122.70).abs() < 1e-12);
        assert_eq!(f.zoom, 16);
    }

    fn fp(north: f64, south: f64, west: f64, east: f64) -> Footprint {
        Footprint {
            north,
            south,
            west,
            east,
            zoom: 16,
        }
    }

    #[test]
    fn footprint_from_an_ortho4xp_name() {
        // A real California texture: LOAD_CENTER 42.19597 -122.38770.
        let f = Footprint::from_name(&parse_dds_filename("24272_10480_GO216_LIT.dds").unwrap());
        assert!((f.west - -122.431640625).abs() < 1e-9);
        assert!((f.east - -122.34375).abs() < 1e-9);
        assert!(((f.north + f.south) / 2.0 - 42.19597).abs() < 0.01);
        assert!(f.north > f.south);
        assert_eq!(f.tiles(), vec![(42, -123)]);
    }

    #[test]
    fn tiles_straddling_a_degree_line() {
        let f = fp(38.02, 37.98, -122.03, -121.97);
        let mut t = f.tiles();
        t.sort();
        assert_eq!(t, vec![(37, -123), (37, -122), (38, -123), (38, -122)]);
    }
}
