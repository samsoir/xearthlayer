//! Airport database for X-Plane scenery.
//!
//! This module provides airport coordinate lookup for features like
//! pre-warming the tile cache around a specific airport on startup.
//!
//! # Data Source
//!
//! Airport data is read from X-Plane's `apt.dat` file located at:
//! `{XPlane}/Resources/default scenery/default apt dat/Earth nav data/apt.dat`
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::airport::AirportIndex;
//!
//! let index = AirportIndex::from_xplane_path("/path/to/X-Plane 12")?;
//! if let Some(airport) = index.get("LFBO") {
//!     println!("{} is at ({}, {})", airport.name, airport.latitude, airport.longitude);
//! }
//! ```

mod index;
mod parser;

pub use index::{AirportIndex, AirportIndexError};
pub use parser::{AptDatParser, ParseError};

/// An airport with ICAO code and location.
#[derive(Debug, Clone)]
pub struct Airport {
    /// ICAO code (e.g., "LFBO", "KJFK").
    pub icao: String,
    /// Airport name.
    pub name: String,
    /// Latitude in decimal degrees.
    pub latitude: f64,
    /// Longitude in decimal degrees.
    pub longitude: f64,
    /// Elevation in feet.
    pub elevation_ft: f32,
}

impl Airport {
    /// Create a new airport.
    pub fn new(icao: &str, name: &str, latitude: f64, longitude: f64, elevation_ft: f32) -> Self {
        Self {
            icao: icao.to_string(),
            name: name.to_string(),
            latitude,
            longitude,
            elevation_ft,
        }
    }
}
