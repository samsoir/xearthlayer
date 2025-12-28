//! Parser for X-Plane's apt.dat airport database.
//!
//! The apt.dat format is a line-based text format where:
//! - Line code `1` starts an airport definition with elevation, ICAO, and name
//! - Line code `100` defines a runway with coordinates
//!
//! We extract the ICAO code, name, elevation from the header and coordinates
//! from the first runway of each airport.

use std::io::{BufRead, BufReader, Read};

use super::Airport;

/// Error type for apt.dat parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid apt.dat format at line {line}: {message}")]
    InvalidFormat { line: usize, message: String },
}

/// Parser for X-Plane apt.dat format.
pub struct AptDatParser;

impl AptDatParser {
    /// Parse airports from an apt.dat reader.
    ///
    /// This is a streaming parser that yields airports as they are parsed.
    /// Only airports with at least one runway (and thus valid coordinates) are returned.
    pub fn parse<R: Read>(reader: R) -> impl Iterator<Item = Result<Airport, ParseError>> {
        AptDatIterator::new(BufReader::new(reader))
    }

    /// Parse all airports into a vector.
    ///
    /// Skips airports that fail to parse and logs warnings.
    pub fn parse_all<R: Read>(reader: R) -> Result<Vec<Airport>, ParseError> {
        let mut airports = Vec::new();
        for result in Self::parse(reader) {
            match result {
                Ok(airport) => airports.push(airport),
                Err(e) => {
                    tracing::warn!("Skipping airport due to parse error: {}", e);
                }
            }
        }
        Ok(airports)
    }
}

/// Iterator that yields airports from an apt.dat file.
struct AptDatIterator<R: BufRead> {
    reader: R,
    line_buffer: String,
    line_number: usize,
    // Current airport being parsed
    current_icao: Option<String>,
    current_name: Option<String>,
    current_elevation: Option<f32>,
    current_lat: Option<f64>,
    current_lon: Option<f64>,
}

impl<R: BufRead> AptDatIterator<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_buffer: String::new(),
            line_number: 0,
            current_icao: None,
            current_name: None,
            current_elevation: None,
            current_lat: None,
            current_lon: None,
        }
    }

    /// Finalize the current airport and return it if valid.
    fn finalize_airport(&mut self) -> Option<Airport> {
        let icao = self.current_icao.take()?;
        let name = self.current_name.take()?;
        let elevation = self.current_elevation.take()?;
        let lat = self.current_lat.take()?;
        let lon = self.current_lon.take()?;

        Some(Airport::new(&icao, &name, lat, lon, elevation))
    }

    /// Parse an airport header line (row code 1).
    ///
    /// Format: `1 <elevation_ft> <deprecated> <deprecated> <ICAO> <name...>`
    fn parse_airport_header(&mut self, line: &str) -> Option<()> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            return None;
        }

        // parts[0] = "1"
        // parts[1] = elevation in feet
        // parts[2], [3] = deprecated fields
        // parts[4] = ICAO code
        // parts[5..] = name

        let elevation: f32 = parts[1].parse().ok()?;
        let icao = parts[4].to_string();
        let name = parts[5..].join(" ");

        self.current_icao = Some(icao);
        self.current_name = Some(name);
        self.current_elevation = Some(elevation);
        self.current_lat = None;
        self.current_lon = None;

        Some(())
    }

    /// Parse a metadata line (row code 1302) for airport properties.
    ///
    /// X-Plane apt.dat 1100+ metadata format:
    /// `1302 <key> <value>`
    ///
    /// We're interested in:
    /// - `datum_lat` - Airport reference latitude
    /// - `datum_lon` - Airport reference longitude
    fn parse_metadata(&mut self, line: &str) -> Option<()> {
        let parts: Vec<&str> = line.splitn(3, char::is_whitespace).collect();
        if parts.len() < 3 {
            return None;
        }

        let key = parts[1];
        let value = parts[2].trim();

        match key {
            "datum_lat" => {
                if let Ok(lat) = value.parse::<f64>() {
                    if (-90.0..=90.0).contains(&lat) {
                        self.current_lat = Some(lat);
                    }
                }
            }
            "datum_lon" => {
                if let Ok(lon) = value.parse::<f64>() {
                    if (-180.0..=180.0).contains(&lon) {
                        self.current_lon = Some(lon);
                    }
                }
            }
            _ => {}
        }

        Some(())
    }

    /// Parse a runway line (row code 100) to get airport coordinates.
    ///
    /// Only used as fallback if datum_lat/datum_lon not available.
    /// X-Plane apt.dat 1100+ runway format includes runway designator before coords.
    fn parse_runway(&mut self, line: &str) -> Option<()> {
        // Only take coordinates from runway if no datum coordinates yet
        if self.current_lat.is_some() && self.current_lon.is_some() {
            return Some(());
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 11 {
            return None;
        }

        // Runway end 1 coordinates are at indices 9 and 10 (after runway designator)
        if let (Ok(lat), Ok(lon)) = (parts[9].parse::<f64>(), parts[10].parse::<f64>()) {
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                self.current_lat = Some(lat);
                self.current_lon = Some(lon);
                return Some(());
            }
        }

        None
    }
}

impl<R: BufRead> Iterator for AptDatIterator<R> {
    type Item = Result<Airport, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.line_buffer.clear();
            match self.reader.read_line(&mut self.line_buffer) {
                Ok(0) => {
                    // EOF - return any pending airport
                    return self.finalize_airport().map(Ok);
                }
                Ok(_) => {
                    self.line_number += 1;

                    // Clone the line to avoid borrow checker issues
                    // This allows us to mutate self while using line data
                    let line = self.line_buffer.trim().to_string();

                    // Skip empty lines and comments
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }

                    // Get the row code
                    let row_code = line.split_whitespace().next().unwrap_or("");

                    match row_code {
                        "1" => {
                            // New airport - finalize previous if exists
                            let previous = self.finalize_airport();
                            self.parse_airport_header(&line);
                            if let Some(airport) = previous {
                                return Some(Ok(airport));
                            }
                        }
                        "16" | "17" => {
                            // Seaplane base (16) or heliport (17) - treat like airport header
                            let previous = self.finalize_airport();
                            self.parse_airport_header(&line);
                            if let Some(airport) = previous {
                                return Some(Ok(airport));
                            }
                        }
                        "100" => {
                            // Land runway - extract coordinates (fallback if no datum)
                            self.parse_runway(&line);
                        }
                        "1302" => {
                            // Airport metadata - extract datum_lat and datum_lon
                            self.parse_metadata(&line);
                        }
                        "101" => {
                            // Water runway - fallback coordinates if no datum
                            // Format: 101 <width> <perimeter_buoys> <runway_id1> <lat1> <lon1> <runway_id2> <lat2> <lon2>
                            if self.current_lat.is_some() && self.current_lon.is_some() {
                                continue;
                            }
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 6 {
                                if let (Ok(lat), Ok(lon)) =
                                    (parts[4].parse::<f64>(), parts[5].parse::<f64>())
                                {
                                    if (-90.0..=90.0).contains(&lat)
                                        && (-180.0..=180.0).contains(&lon)
                                    {
                                        self.current_lat = Some(lat);
                                        self.current_lon = Some(lon);
                                    }
                                }
                            }
                        }
                        "102" => {
                            // Helipad - fallback coordinates if no datum
                            // Format: 102 <id> <lat> <lon> ...
                            if self.current_lat.is_some() && self.current_lon.is_some() {
                                continue;
                            }
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 4 {
                                if let (Ok(lat), Ok(lon)) =
                                    (parts[2].parse::<f64>(), parts[3].parse::<f64>())
                                {
                                    if (-90.0..=90.0).contains(&lat)
                                        && (-180.0..=180.0).contains(&lon)
                                    {
                                        self.current_lat = Some(lat);
                                        self.current_lon = Some(lon);
                                    }
                                }
                            }
                        }
                        "99" => {
                            // End of file marker - finalize last airport
                            return self.finalize_airport().map(Ok);
                        }
                        _ => {
                            // Other row codes (taxiways, ATC, etc.) - ignore
                        }
                    }
                }
                Err(e) => {
                    return Some(Err(ParseError::Io(e)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_airport_with_datum() {
        // Modern apt.dat format with datum_lat/datum_lon metadata
        let apt_dat = r#"
I
1100 Version - generated

1 1500 0 0 LFBO Toulouse-Blagnac
1302 datum_lat 43.6294
1302 datum_lon 1.3678
1302 icao_code LFBO
100 45.00 1 0 0.25 0 0 0 14 43.6294 1.3678 0 0 0 0 0 2 32 43.6294 1.3700 0 0 0 0 0 2

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "LFBO");
        assert_eq!(airports[0].name, "Toulouse-Blagnac");
        assert!((airports[0].latitude - 43.6294).abs() < 0.001);
        assert!((airports[0].longitude - 1.3678).abs() < 0.001);
        assert!((airports[0].elevation_ft - 1500.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_multiple_airports() {
        let apt_dat = r#"
I
1100 Version

1 1500 0 0 LFBO Toulouse-Blagnac
1302 datum_lat 43.6294
1302 datum_lon 1.3678

1 13 0 0 KJFK John F Kennedy Intl
1302 datum_lat 40.6413
1302 datum_lon -73.7781

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 2);
        assert_eq!(airports[0].icao, "LFBO");
        assert_eq!(airports[1].icao, "KJFK");
        assert!((airports[0].latitude - 43.6294).abs() < 0.001);
        assert!((airports[1].latitude - 40.6413).abs() < 0.001);
    }

    #[test]
    fn test_airport_without_datum_uses_runway_fallback() {
        // Tests the runway fallback when datum coordinates aren't available
        let apt_dat = r#"
I
1100 Version

1 1500 0 0 NODATUM No Datum Airport
100 45.00 1 0 0.25 0 0 0 09 43.6294 1.3678 0 0 0 0 0 2 27 43.6294 1.3700 0 0 0 0 0 2

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "NODATUM");
        assert!((airports[0].latitude - 43.6294).abs() < 0.001);
        assert!((airports[0].longitude - 1.3678).abs() < 0.001);
    }

    #[test]
    fn test_skip_airport_without_coordinates() {
        // Airports without datum or runway coords should be skipped
        let apt_dat = r#"
I
1100 Version

1 1500 0 0 NOCOORDS No Coordinates
1302 city Test City

1 1500 0 0 HASCOORDS Has Coordinates
1302 datum_lat 43.6294
1302 datum_lon 1.3678

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "HASCOORDS");
    }

    #[test]
    fn test_parse_helipad() {
        let apt_dat = r#"
I
1100 Version

17 100 0 0 HELI Test Heliport
1302 datum_lat 40.7128
1302 datum_lon -74.0060
102 H1 40.7128 -74.0060 90.00 100 100 1 0 0 0.25 0

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "HELI");
        assert!((airports[0].latitude - 40.7128).abs() < 0.001);
        assert!((airports[0].longitude - (-74.0060)).abs() < 0.001);
    }

    #[test]
    fn test_datum_preferred_over_runway() {
        // When both datum and runway are present, datum should be used
        let apt_dat = r#"
I
1100 Version

1 1500 0 0 TEST Test Airport
1302 datum_lat 47.458055556
1302 datum_lon 8.548055556
100 58.05 2 230 0.00 1 3 0 10 47.4589472 8.5373764 0 161 6 0 0 2 28 47.4566030 8.5704697 3 82 7 1 0 2

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        // Should use datum coordinates, not runway coordinates
        assert!((airports[0].latitude - 47.458055556).abs() < 0.0001);
        assert!((airports[0].longitude - 8.548055556).abs() < 0.0001);
    }
}
