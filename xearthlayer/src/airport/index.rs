//! Airport index for O(1) ICAO code lookup.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use super::parser::{AptDatParser, ParseError};
use super::Airport;

/// Error type for airport index operations.
#[derive(Debug, thiserror::Error)]
pub enum AirportIndexError {
    #[error("apt.dat not found at: {0}")]
    NotFound(PathBuf),
    #[error("Failed to parse apt.dat: {0}")]
    ParseError(#[from] ParseError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Index of airports for O(1) ICAO code lookup.
///
/// Built from X-Plane's apt.dat file.
#[derive(Debug)]
pub struct AirportIndex {
    airports: HashMap<String, Airport>,
}

impl AirportIndex {
    /// Create an empty airport index.
    pub fn new() -> Self {
        Self {
            airports: HashMap::new(),
        }
    }

    /// Build an airport index from X-Plane's apt.dat.
    ///
    /// # Arguments
    ///
    /// * `xplane_path` - Path to X-Plane installation (e.g., "/home/user/X-Plane 12")
    ///
    /// The apt.dat file is expected at:
    /// `{xplane_path}/Resources/default scenery/default apt dat/Earth nav data/apt.dat`
    pub fn from_xplane_path<P: AsRef<Path>>(xplane_path: P) -> Result<Self, AirportIndexError> {
        let apt_dat_path = xplane_path
            .as_ref()
            .join("Resources")
            .join("default scenery")
            .join("default apt dat")
            .join("Earth nav data")
            .join("apt.dat");

        Self::from_apt_dat(&apt_dat_path)
    }

    /// Build an airport index from an apt.dat file.
    pub fn from_apt_dat<P: AsRef<Path>>(path: P) -> Result<Self, AirportIndexError> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(AirportIndexError::NotFound(path.to_path_buf()));
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);

        Self::from_reader(reader)
    }

    /// Build an airport index from a reader.
    pub fn from_reader<R: std::io::Read>(reader: R) -> Result<Self, AirportIndexError> {
        let airports = AptDatParser::parse_all(reader)?;
        let mut index = Self::new();

        for airport in airports {
            index.airports.insert(airport.icao.clone(), airport);
        }

        tracing::info!(count = index.airports.len(), "Built airport index");

        Ok(index)
    }

    /// Get an airport by ICAO code.
    ///
    /// Returns `None` if the airport is not found.
    pub fn get(&self, icao: &str) -> Option<&Airport> {
        // Normalize ICAO code to uppercase for lookup
        self.airports.get(&icao.to_uppercase())
    }

    /// Get an airport by ICAO code, case-insensitive.
    ///
    /// Convenience method that normalizes the ICAO code to uppercase.
    pub fn get_ignorecase(&self, icao: &str) -> Option<&Airport> {
        self.get(&icao.to_uppercase())
    }

    /// Returns the number of airports in the index.
    pub fn len(&self) -> usize {
        self.airports.len()
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.airports.is_empty()
    }

    /// Returns an iterator over all airports.
    pub fn iter(&self) -> impl Iterator<Item = &Airport> {
        self.airports.values()
    }
}

impl Default for AirportIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_index() {
        let index = AirportIndex::new();
        assert!(index.is_empty());
        assert!(index.get("KJFK").is_none());
    }

    #[test]
    fn test_lookup_case_insensitive() {
        let apt_dat = r#"
I
1000 Version

1 1500 0 0 LFBO Toulouse-Blagnac
100 45.00 1 0 0.25 0 0 0 43.6294 1.3678 0 0 0 0 43.6294 1.3700 0 0 0 0

99
"#;
        let index = AirportIndex::from_reader(apt_dat.as_bytes()).unwrap();

        assert!(index.get("LFBO").is_some());
        assert!(index.get("lfbo").is_some());
        assert!(index.get("LfBo").is_some());
    }

    #[test]
    fn test_index_count() {
        let apt_dat = r#"
I
1000 Version

1 1500 0 0 LFBO Toulouse-Blagnac
100 45.00 1 0 0.25 0 0 0 43.6294 1.3678 0 0 0 0 43.6294 1.3700 0 0 0 0

1 13 0 0 KJFK John F Kennedy Intl
100 60.00 1 0 0.25 0 0 0 40.6413 -73.7781 0 0 0 0 40.6413 -73.7500 0 0 0 0

1 431 0 0 EGLL London Heathrow
100 50.00 1 0 0.25 0 0 0 51.4775 -0.4614 0 0 0 0 51.4775 -0.4500 0 0 0 0

99
"#;
        let index = AirportIndex::from_reader(apt_dat.as_bytes()).unwrap();

        assert_eq!(index.len(), 3);
        assert!(!index.is_empty());
    }

    #[test]
    fn test_not_found_error() {
        let result = AirportIndex::from_apt_dat("/nonexistent/path/apt.dat");
        assert!(matches!(result, Err(AirportIndexError::NotFound(_))));
    }
}
