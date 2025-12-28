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

    /// Parse a runway line (row code 100) to get airport coordinates.
    ///
    /// Format: `100 <width> <surface> <shoulder> <smoothness> <centerline> <edge> <dist_signs> <lat1> <lon1> ...`
    ///
    /// X-Plane apt.dat runway format (row code 100):
    /// - parts[0]: Row code (100)
    /// - parts[1]: Width in meters
    /// - parts[2]: Surface type (1=asphalt, 2=concrete, etc.)
    /// - parts[3]: Shoulder type
    /// - parts[4]: Smoothness (0.00-1.00)
    /// - parts[5]: Centerline lights (0/1)
    /// - parts[6]: Edge lights (0=none, 1=LIRL, 2=MIRL, 3=HIRL)
    /// - parts[7]: Auto-generate distance signs (0/1)
    /// - parts[8]: Runway end 1 latitude
    /// - parts[9]: Runway end 1 longitude
    /// - parts[10..]: More runway properties (displacement, markings, etc.)
    fn parse_runway(&mut self, line: &str) -> Option<()> {
        // Only take coordinates from the first runway
        if self.current_lat.is_some() {
            return Some(());
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 10 {
            return None;
        }

        // Runway end 1 coordinates are at fixed indices 8 and 9
        if let (Ok(lat), Ok(lon)) = (parts[8].parse::<f64>(), parts[9].parse::<f64>()) {
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
                            // Land runway - extract coordinates
                            self.parse_runway(&line);
                        }
                        "101" => {
                            // Water runway - also has coordinates
                            // Format: 101 <width> <perimeter_buoys> <lat1> <lon1> <lat2> <lon2>
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 6 && self.current_lat.is_none() {
                                if let (Ok(lat), Ok(lon)) =
                                    (parts[3].parse::<f64>(), parts[4].parse::<f64>())
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
                            // Helipad - also has coordinates
                            // Format: 102 <id> <lat> <lon> ...
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 4 && self.current_lat.is_none() {
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
    fn test_parse_simple_airport() {
        let apt_dat = r#"
I
1000 Version - generated

1 1500 0 0 LFBO Toulouse-Blagnac
100 45.00 1 0 0.25 0 0 0 43.6294 1.3678 0 0 0 0 43.6294 1.3700 0 0 0 0

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
1000 Version

1 1500 0 0 LFBO Toulouse-Blagnac
100 45.00 1 0 0.25 0 0 0 43.6294 1.3678 0 0 0 0 43.6294 1.3700 0 0 0 0

1 13 0 0 KJFK John F Kennedy Intl
100 60.00 1 0 0.25 0 0 0 40.6413 -73.7781 0 0 0 0 40.6413 -73.7500 0 0 0 0

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 2);
        assert_eq!(airports[0].icao, "LFBO");
        assert_eq!(airports[1].icao, "KJFK");
    }

    #[test]
    fn test_skip_airport_without_runway() {
        let apt_dat = r#"
I
1000 Version

1 1500 0 0 NORUNWAY Test Airport

1 1500 0 0 HASRWY Has Runway
100 45.00 1 0 0.25 0 0 0 43.6294 1.3678 0 0 0 0 43.6294 1.3700 0 0 0 0

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "HASRWY");
    }

    #[test]
    fn test_parse_helipad() {
        let apt_dat = r#"
I
1000 Version

17 100 0 0 HELI Test Heliport
102 H1 40.7128 -74.0060 90.00 100 100 1 0 0 0.25 0

99
"#;
        let airports: Vec<Airport> = AptDatParser::parse_all(apt_dat.as_bytes()).unwrap();
        assert_eq!(airports.len(), 1);
        assert_eq!(airports[0].icao, "HELI");
        assert!((airports[0].latitude - 40.7128).abs() < 0.001);
    }
}
