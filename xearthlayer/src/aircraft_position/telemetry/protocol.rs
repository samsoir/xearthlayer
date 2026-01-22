//! X-Plane telemetry protocol parsing.
//!
//! Supports three protocols:
//! - **XGPS/XGPS2** (ForeFlight) - Text format with position, altitude, track, speed
//! - **XATT/XATT2** (ForeFlight) - Text format with heading, pitch, roll
//! - **DATA** (Legacy) - Binary format with configurable data indices

use tracing::trace;

use super::super::state::AircraftState;

/// Size of the X-Plane DATA packet header ("DATA" + 1 byte).
const DATA_HEADER_SIZE: usize = 5;

/// Size of each data record (4-byte index + 8 floats).
const DATA_RECORD_SIZE: usize = 36;

/// X-Plane data index for speeds (ground speed is the 4th float).
const INDEX_SPEEDS: u32 = 3;

/// X-Plane data index for pitch/roll/headings.
const INDEX_HEADINGS: u32 = 17;

/// X-Plane data index for lat/lon/alt.
const INDEX_POSITION: u32 = 20;

/// Conversion factor: meters per second to knots.
const MS_TO_KNOTS: f32 = 1.94384;

/// Conversion factor: meters to feet.
const METERS_TO_FEET: f32 = 3.28084;

/// Parse a telemetry packet (auto-detects protocol).
///
/// Returns a partial state update that can be accumulated with other updates.
pub fn parse_packet(data: &[u8]) -> Option<PartialStateUpdate> {
    // Try ForeFlight text protocol first (X-Plane 12 uses XGPS2/XATT2)
    if data.len() >= 5 {
        if &data[0..5] == b"XGPS2" {
            return parse_foreflight_xgps(data);
        }
        if &data[0..5] == b"XATT2" {
            return parse_foreflight_xatt(data);
        }
    }
    if data.len() >= 4 {
        if &data[0..4] == b"XGPS" {
            return parse_foreflight_xgps(data);
        }
        if &data[0..4] == b"XATT" {
            return parse_foreflight_xatt(data);
        }
        if &data[0..4] == b"DATA" {
            return parse_xplane_data(data);
        }
    }

    None
}

/// Parse ForeFlight XGPS message.
///
/// Format: `XGPSSimName,lon,lat,alt_m,track,gs_m/s`
fn parse_foreflight_xgps(data: &[u8]) -> Option<PartialStateUpdate> {
    let text = std::str::from_utf8(data).ok()?;

    let parts: Vec<&str> = text.split(',').collect();
    if parts.len() < 6 {
        trace!("XGPS packet too short: {} parts", parts.len());
        return None;
    }

    // Parse fields: lon, lat, alt_m, track, gs_m/s
    let longitude: f64 = parts[1].parse().ok()?;
    let latitude: f64 = parts[2].parse().ok()?;
    let altitude_m: f32 = parts[3].parse().ok()?;
    let track: f32 = parts[4].parse().ok()?;
    let groundspeed_ms: f32 = parts[5].parse().ok()?;

    Some(PartialStateUpdate {
        latitude: Some(latitude),
        longitude: Some(longitude),
        heading: Some(normalize_heading(track)),
        ground_speed: Some(groundspeed_ms * MS_TO_KNOTS),
        altitude: Some(altitude_m * METERS_TO_FEET),
    })
}

/// Parse ForeFlight XATT message.
///
/// Format: `XATTSimName,heading,pitch,roll`
fn parse_foreflight_xatt(data: &[u8]) -> Option<PartialStateUpdate> {
    let text = std::str::from_utf8(data).ok()?;

    let parts: Vec<&str> = text.split(',').collect();
    if parts.len() < 4 {
        trace!("XATT packet too short: {} parts", parts.len());
        return None;
    }

    let heading: f32 = parts[1].parse().ok()?;

    Some(PartialStateUpdate {
        heading: Some(normalize_heading(heading)),
        ..Default::default()
    })
}

/// Parse legacy X-Plane binary DATA packet.
fn parse_xplane_data(data: &[u8]) -> Option<PartialStateUpdate> {
    if data.len() < DATA_HEADER_SIZE {
        return None;
    }

    let mut updates = PartialStateUpdate::default();

    let records = &data[DATA_HEADER_SIZE..];
    for chunk in records.chunks_exact(DATA_RECORD_SIZE) {
        let index = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);

        let floats: [f32; 8] = std::array::from_fn(|i| {
            let offset = 4 + i * 4;
            f32::from_le_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ])
        });

        match index {
            INDEX_SPEEDS => {
                // [vind_kias, vind_keas, vtrue_ktas, vtrue_ktgs, ...]
                updates.ground_speed = Some(floats[3]);
            }
            INDEX_HEADINGS => {
                // [pitch, roll, hding_true, hding_mag, ...]
                updates.heading = Some(normalize_heading(floats[2]));
            }
            INDEX_POSITION => {
                // [lat, lon, alt_ind_ft, alt_msl_ft, ...]
                updates.latitude = Some(floats[0] as f64);
                updates.longitude = Some(floats[1] as f64);
                updates.altitude = Some(floats[3]);
            }
            _ => {}
        }
    }

    if updates.has_any() {
        Some(updates)
    } else {
        None
    }
}

/// Normalize a heading to [0, 360) degrees.
fn normalize_heading(heading: f32) -> f32 {
    let mut h = heading % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

/// Partial state update from a single packet.
#[derive(Default)]
pub struct PartialStateUpdate {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub heading: Option<f32>,
    pub ground_speed: Option<f32>,
    pub altitude: Option<f32>,
}

impl PartialStateUpdate {
    pub fn has_any(&self) -> bool {
        self.latitude.is_some()
            || self.longitude.is_some()
            || self.heading.is_some()
            || self.ground_speed.is_some()
            || self.altitude.is_some()
    }
}

/// Accumulated state from multiple packets.
///
/// The legacy DATA protocol sends position, heading, and speed in separate
/// packets. This struct accumulates updates until we have a complete state.
#[derive(Default)]
pub struct PartialState {
    latitude: Option<f64>,
    longitude: Option<f64>,
    heading: Option<f32>,
    ground_speed: Option<f32>,
    altitude: Option<f32>,
}

impl PartialState {
    /// Apply an update to the accumulated state.
    pub fn apply(&mut self, update: PartialStateUpdate) {
        if let Some(v) = update.latitude {
            self.latitude = Some(v);
        }
        if let Some(v) = update.longitude {
            self.longitude = Some(v);
        }
        if let Some(v) = update.heading {
            self.heading = Some(v);
        }
        if let Some(v) = update.ground_speed {
            self.ground_speed = Some(v);
        }
        if let Some(v) = update.altitude {
            self.altitude = Some(v);
        }
    }

    /// Check if we have all required fields for a complete state.
    pub fn is_complete(&self) -> bool {
        self.latitude.is_some()
            && self.longitude.is_some()
            && self.heading.is_some()
            && self.ground_speed.is_some()
    }

    /// Convert to an AircraftState if complete.
    pub fn to_aircraft_state(&self) -> Option<AircraftState> {
        Some(AircraftState::from_telemetry(
            self.latitude?,
            self.longitude?,
            self.heading?,
            self.ground_speed?,
            self.altitude.unwrap_or(0.0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_data_packet(records: &[(u32, [f32; 8])]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(DATA_HEADER_SIZE + records.len() * DATA_RECORD_SIZE);
        packet.extend_from_slice(b"DATA");
        packet.push(0);

        for (index, floats) in records {
            packet.extend_from_slice(&index.to_le_bytes());
            for f in floats {
                packet.extend_from_slice(&f.to_le_bytes());
            }
        }
        packet
    }

    // ==================== DATA protocol tests ====================

    #[test]
    fn test_parse_position_packet() {
        let packet = create_data_packet(&[(
            INDEX_POSITION,
            [45.5, -122.5, 5000.0, 5100.0, 0.0, 0.0, 0.0, 0.0],
        )]);

        let updates = parse_packet(&packet).unwrap();
        assert!((updates.latitude.unwrap() - 45.5).abs() < 0.001);
        assert!((updates.longitude.unwrap() - (-122.5)).abs() < 0.001);
        assert!((updates.altitude.unwrap() - 5100.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_heading_packet() {
        let packet =
            create_data_packet(&[(INDEX_HEADINGS, [5.0, 2.0, 270.0, 268.0, 0.0, 0.0, 0.0, 0.0])]);

        let updates = parse_packet(&packet).unwrap();
        assert!((updates.heading.unwrap() - 270.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_speed_packet() {
        let packet = create_data_packet(&[(
            INDEX_SPEEDS,
            [150.0, 148.0, 165.0, 145.0, 0.0, 0.0, 0.0, 0.0],
        )]);

        let updates = parse_packet(&packet).unwrap();
        assert!((updates.ground_speed.unwrap() - 145.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_combined_packet() {
        let packet = create_data_packet(&[
            (
                INDEX_POSITION,
                [40.0, -75.0, 3000.0, 3050.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (INDEX_HEADINGS, [0.0, 0.0, 90.0, 88.0, 0.0, 0.0, 0.0, 0.0]),
            (INDEX_SPEEDS, [0.0, 0.0, 0.0, 120.0, 0.0, 0.0, 0.0, 0.0]),
        ]);

        let updates = parse_packet(&packet).unwrap();
        assert!(updates.latitude.is_some());
        assert!(updates.longitude.is_some());
        assert!(updates.heading.is_some());
        assert!(updates.ground_speed.is_some());
    }

    #[test]
    fn test_parse_invalid_header() {
        assert!(parse_packet(b"NOTD\x00").is_none());
    }

    #[test]
    fn test_parse_too_short() {
        assert!(parse_packet(b"DAT").is_none());
    }

    // ==================== ForeFlight tests ====================

    #[test]
    fn test_parse_foreflight_xgps() {
        let packet = b"XGPSX-Plane,-122.5,45.5,3048.0,270.5,154.3";
        let updates = parse_packet(packet).unwrap();

        assert!((updates.latitude.unwrap() - 45.5).abs() < 0.001);
        assert!((updates.longitude.unwrap() - (-122.5)).abs() < 0.001);
        assert!((updates.altitude.unwrap() - 10000.0).abs() < 10.0); // 3048m ≈ 10000ft
        assert!((updates.heading.unwrap() - 270.5).abs() < 0.1);
        assert!((updates.ground_speed.unwrap() - 300.0).abs() < 1.0); // 154.3 m/s ≈ 300kt
    }

    #[test]
    fn test_parse_foreflight_xatt() {
        let packet = b"XATTX-Plane,45.5,5.2,-3.1";
        let updates = parse_packet(packet).unwrap();

        assert!((updates.heading.unwrap() - 45.5).abs() < 0.1);
        assert!(updates.latitude.is_none());
        assert!(updates.longitude.is_none());
    }

    #[test]
    fn test_parse_foreflight_xgps_too_short() {
        assert!(parse_packet(b"XGPSX-Plane,-122.5,45.5,3048.0,270.5").is_none());
    }

    #[test]
    fn test_parse_foreflight_xatt_too_short() {
        assert!(parse_packet(b"XATTX-Plane,45.5,5.2").is_none());
    }

    #[test]
    fn test_protocol_auto_detection() {
        assert!(parse_packet(b"XGPSX-Plane,-122.5,45.5,3048.0,270.5,154.3").is_some());
        assert!(parse_packet(b"XATTX-Plane,45.5,5.2,-3.1").is_some());
        assert!(parse_packet(&create_data_packet(&[(
            INDEX_POSITION,
            [45.5, -122.5, 5000.0, 5100.0, 0.0, 0.0, 0.0, 0.0]
        )]))
        .is_some());
        assert!(parse_packet(b"UNKN,some,data").is_none());
    }

    // ==================== State accumulation tests ====================

    #[test]
    fn test_partial_state_accumulation() {
        let mut partial = PartialState::default();

        partial.apply(PartialStateUpdate {
            latitude: Some(45.0),
            longitude: Some(-122.0),
            altitude: Some(5000.0),
            ..Default::default()
        });
        assert!(!partial.is_complete());

        partial.apply(PartialStateUpdate {
            heading: Some(90.0),
            ..Default::default()
        });
        assert!(!partial.is_complete());

        partial.apply(PartialStateUpdate {
            ground_speed: Some(120.0),
            ..Default::default()
        });
        assert!(partial.is_complete());

        let state = partial.to_aircraft_state().unwrap();
        assert!((state.latitude - 45.0).abs() < 0.001);
        assert!((state.ground_speed - 120.0).abs() < 0.1);
    }

    // ==================== normalize_heading tests ====================

    #[test]
    fn test_normalize_heading() {
        assert!((normalize_heading(0.0) - 0.0).abs() < 0.001);
        assert!((normalize_heading(90.0) - 90.0).abs() < 0.001);
        assert!((normalize_heading(-90.0) - 270.0).abs() < 0.001);
        assert!((normalize_heading(360.0) - 0.0).abs() < 0.001);
        assert!((normalize_heading(450.0) - 90.0).abs() < 0.001);
    }
}
