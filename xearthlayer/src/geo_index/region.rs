//! DSF region coordinate type.
//!
//! A [`DsfRegion`] represents a 1°×1° geographic area used by X-Plane's scenery
//! system. The region is identified by the floor of latitude and longitude,
//! following the DSF (Distribution Scenery Format) convention.

use std::fmt;

/// A 1°×1° DSF region coordinate.
///
/// Represents a geographic area used by X-Plane's scenery system.
/// The region is identified by the floor of latitude and longitude.
///
/// # Examples
///
/// ```
/// use xearthlayer::geo_index::DsfRegion;
///
/// let region = DsfRegion::new(43, 6);
/// assert_eq!(format!("{}", region), "+43+006");
///
/// let region = DsfRegion::from_lat_lon(43.67, 7.23);
/// assert_eq!(region.lat, 43);
/// assert_eq!(region.lon, 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DsfRegion {
    /// Floor of the latitude (south edge of the region).
    pub lat: i32,
    /// Floor of the longitude (west edge of the region).
    pub lon: i32,
}

impl DsfRegion {
    /// Create a new DSF region from integer coordinates.
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    /// Create a DSF region from floating-point lat/lon.
    ///
    /// The coordinates are floored to the nearest integer, matching
    /// X-Plane's DSF region convention.
    pub fn from_lat_lon(lat: f64, lon: f64) -> Self {
        Self {
            lat: lat.floor() as i32,
            lon: lon.floor() as i32,
        }
    }
}

impl fmt::Display for DsfRegion {
    /// Format as a DSF-style region name (e.g., `+43+006`, `-46+012`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:+03}{:+04}", self.lat, self.lon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let region = DsfRegion::new(43, 6);
        assert_eq!(region.lat, 43);
        assert_eq!(region.lon, 6);
    }

    #[test]
    fn test_from_lat_lon_positive() {
        let region = DsfRegion::from_lat_lon(43.67, 7.23);
        assert_eq!(region.lat, 43);
        assert_eq!(region.lon, 7);
    }

    #[test]
    fn test_from_lat_lon_negative() {
        let region = DsfRegion::from_lat_lon(-33.9, -118.4);
        assert_eq!(region.lat, -34);
        assert_eq!(region.lon, -119);
    }

    #[test]
    fn test_from_lat_lon_exact_integer() {
        let region = DsfRegion::from_lat_lon(45.0, 11.0);
        assert_eq!(region.lat, 45);
        assert_eq!(region.lon, 11);
    }

    #[test]
    fn test_from_lat_lon_zero() {
        let region = DsfRegion::from_lat_lon(0.5, 0.5);
        assert_eq!(region.lat, 0);
        assert_eq!(region.lon, 0);
    }

    #[test]
    fn test_display_positive() {
        let region = DsfRegion::new(43, 6);
        assert_eq!(format!("{}", region), "+43+006");
    }

    #[test]
    fn test_display_negative() {
        let region = DsfRegion::new(-46, 12);
        assert_eq!(format!("{}", region), "-46+012");
    }

    #[test]
    fn test_display_mixed() {
        let region = DsfRegion::new(33, -119);
        assert_eq!(format!("{}", region), "+33-119");
    }

    #[test]
    fn test_display_zero() {
        let region = DsfRegion::new(0, 0);
        assert_eq!(format!("{}", region), "+00+000");
    }

    #[test]
    fn test_equality() {
        let a = DsfRegion::new(43, 6);
        let b = DsfRegion::new(43, 6);
        let c = DsfRegion::new(43, 7);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_hash_consistency() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DsfRegion::new(43, 6));
        set.insert(DsfRegion::new(43, 6)); // duplicate
        set.insert(DsfRegion::new(44, 7));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_copy_semantics() {
        let a = DsfRegion::new(43, 6);
        let b = a; // Copy
        assert_eq!(a, b); // a is still valid
    }
}
