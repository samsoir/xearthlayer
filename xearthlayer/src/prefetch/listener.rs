//! UDP telemetry listener for X-Plane position data.
//!
//! Supports two protocols:
//!
//! ## ForeFlight Protocol (Recommended)
//!
//! X-Plane 12 can broadcast position data for ForeFlight. This is the
//! recommended method as it requires only enabling one setting in X-Plane.
//!
//! **Setup:** Settings → Network → "Send position to ForeFlight"
//!
//! **Port:** 49002 (broadcast)
//!
//! **Message Formats:**
//! - XGPS: `XGPSSimName,lon,lat,alt_m,track,gs_m/s` (1 Hz)
//! - XATT: `XATTSimName,heading,pitch,roll` (4-10 Hz)
//!
//! ## X-Plane DATA Protocol (Legacy)
//!
//! The older binary DATA protocol is also supported for backward compatibility.
//!
//! **Setup:**
//! 1. Settings → Data Output
//! 2. Enable "Network via UDP"
//! 3. Set destination IP to localhost (127.0.0.1)
//! 4. Set port (default 49002)
//! 5. Check the boxes for data indices: 3, 17, 20
//!
//! **Packet Format:**
//! ```text
//! Bytes 0-3: "DATA" header
//! Byte 4: Internal use byte
//! Bytes 5+: 36-byte data records (index + 8 floats each)
//! ```

use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{debug, info, trace, warn};

use super::coordinates::normalize_heading;
use super::error::PrefetchError;
use super::state::AircraftState;

/// Size of the X-Plane DATA packet header ("DATA" + 1 byte).
const DATA_HEADER_SIZE: usize = 5;

/// Size of each data record (4-byte index + 8 floats).
const DATA_RECORD_SIZE: usize = 36;

/// Maximum packet size we expect.
const MAX_PACKET_SIZE: usize = 1024;

/// X-Plane data index for speeds (ground speed is the 4th float).
const INDEX_SPEEDS: u32 = 3;

/// X-Plane data index for pitch/roll/headings.
const INDEX_HEADINGS: u32 = 17;

/// X-Plane data index for lat/lon/alt.
const INDEX_POSITION: u32 = 20;

/// Minimum interval between state updates (rate limiting).
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// Conversion factor: meters per second to knots.
const MS_TO_KNOTS: f32 = 1.94384;

/// Conversion factor: meters to feet.
const METERS_TO_FEET: f32 = 3.28084;

/// Telemetry listener for X-Plane UDP broadcasts.
///
/// Listens on a UDP socket for X-Plane position data and sends
/// `AircraftState` updates to a channel.
pub struct TelemetryListener {
    /// UDP port to listen on.
    port: u16,
    /// Receive buffer.
    buffer: [u8; MAX_PACKET_SIZE],
}

impl TelemetryListener {
    /// Create a new telemetry listener for the given port.
    pub fn new(port: u16) -> Self {
        Self {
            port,
            buffer: [0u8; MAX_PACKET_SIZE],
        }
    }

    /// Start listening for telemetry and send updates to the channel.
    ///
    /// This method runs until the channel is closed or an unrecoverable
    /// error occurs. It should be spawned as a background task.
    ///
    /// # Arguments
    ///
    /// * `state_tx` - Channel to send aircraft state updates
    ///
    /// # Returns
    ///
    /// Returns when the channel is closed or on socket error.
    pub async fn run(mut self, state_tx: mpsc::Sender<AircraftState>) -> Result<(), PrefetchError> {
        // Bind the UDP socket asynchronously
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.port))
            .await
            .map_err(|e| PrefetchError::SocketBindError {
                port: self.port,
                source: e,
            })?;

        // Log the actual bound address for debugging
        let local_addr = socket.local_addr().ok();
        info!(
            port = self.port,
            local_addr = ?local_addr,
            "Telemetry listener started - waiting for X-Plane UDP data"
        );

        // State accumulator (we need data from multiple indices)
        let mut partial_state = PartialState::default();
        let mut last_send_time = Instant::now();
        let mut packets_received: u64 = 0;
        let mut states_sent: u64 = 0;

        loop {
            // Check if channel is closed
            if state_tx.is_closed() {
                debug!("Telemetry channel closed, stopping listener");
                break;
            }

            // Use tokio's async recv with timeout to allow periodic checks
            let recv_result =
                tokio::time::timeout(Duration::from_millis(500), socket.recv(&mut self.buffer))
                    .await;

            match recv_result {
                Ok(Ok(len)) => {
                    packets_received += 1;

                    // Log first packet and then periodically
                    if packets_received == 1 {
                        // Show packet header for debugging
                        let header = if len >= 4 {
                            String::from_utf8_lossy(&self.buffer[..4.min(len)]).to_string()
                        } else {
                            format!("{:?}", &self.buffer[..len])
                        };
                        info!(
                            port = self.port,
                            header = %header,
                            "Received first telemetry packet ({} bytes)", len
                        );
                    }

                    if let Some(updates) = self.parse_packet(&self.buffer[..len]) {
                        partial_state.apply(updates);

                        // Rate limit updates
                        if partial_state.is_complete()
                            && last_send_time.elapsed() >= MIN_UPDATE_INTERVAL
                        {
                            if let Some(state) = partial_state.to_aircraft_state() {
                                states_sent += 1;

                                // Log first state at info level for visibility
                                if states_sent == 1 {
                                    info!(
                                        lat = format!("{:.4}", state.latitude),
                                        lon = format!("{:.4}", state.longitude),
                                        hdg = format!("{:.0}", state.heading),
                                        gs = format!("{:.0}", state.ground_speed),
                                        alt = format!("{:.0}", state.altitude),
                                        "First complete aircraft state received"
                                    );
                                } else {
                                    // Log subsequent updates at debug level
                                    debug!(
                                        lat = format!("{:.4}", state.latitude),
                                        lon = format!("{:.4}", state.longitude),
                                        hdg = format!("{:.0}", state.heading),
                                        gs = format!("{:.0}", state.ground_speed),
                                        alt = format!("{:.0}", state.altitude),
                                        "Aircraft state update #{}",
                                        states_sent
                                    );
                                }

                                // Send state, ignore if channel is full (drop oldest)
                                let _ = state_tx.try_send(state);
                                last_send_time = Instant::now();
                            }
                        }
                    } else if packets_received <= 5 {
                        // Log first few parse failures for debugging
                        let preview = String::from_utf8_lossy(&self.buffer[..len.min(50)]);
                        debug!(
                            packet_num = packets_received,
                            preview = %preview,
                            "Failed to parse telemetry packet"
                        );
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "UDP receive error");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    // Timeout - no data received, this is normal when X-Plane isn't sending
                    // Log periodically at info level to confirm listener is alive
                    if packets_received == 0
                        && last_send_time.elapsed().as_secs().is_multiple_of(10)
                    {
                        info!(
                            port = self.port,
                            elapsed_secs = last_send_time.elapsed().as_secs(),
                            "Still waiting for UDP telemetry data..."
                        );
                    }
                    trace!("No telemetry data received (timeout)");
                }
            }
        }

        info!(packets_received, states_sent, "Telemetry listener stopped");
        Ok(())
    }

    /// Parse a telemetry packet (auto-detects protocol).
    ///
    /// Supports ForeFlight text protocol (XGPS/XATT and XGPS2/XATT2) and legacy
    /// X-Plane binary DATA protocol.
    fn parse_packet(&self, data: &[u8]) -> Option<PartialStateUpdate> {
        // Try ForeFlight text protocol first
        // X-Plane 12 uses XGPS2/XATT2, older versions use XGPS/XATT
        if data.len() >= 5 {
            // Check for XGPS2/XATT2 first (X-Plane 12)
            if &data[0..5] == b"XGPS2" {
                return self.parse_foreflight_xgps(data);
            }
            if &data[0..5] == b"XATT2" {
                return self.parse_foreflight_xatt(data);
            }
        }
        if data.len() >= 4 {
            if &data[0..4] == b"XGPS" {
                return self.parse_foreflight_xgps(data);
            }
            if &data[0..4] == b"XATT" {
                return self.parse_foreflight_xatt(data);
            }
            if &data[0..4] == b"DATA" {
                return self.parse_xplane_data(data);
            }
        }

        None
    }

    /// Parse ForeFlight XGPS message.
    ///
    /// Format: `XGPSSimName,lon,lat,alt_m,track,gs_m/s`
    fn parse_foreflight_xgps(&self, data: &[u8]) -> Option<PartialStateUpdate> {
        let text = std::str::from_utf8(data).ok()?;

        // Split by comma, skip the first part (XGPSSimName)
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
            heading: Some(normalize_heading(track)), // Use track as heading (ground track)
            ground_speed: Some(groundspeed_ms * MS_TO_KNOTS),
            altitude: Some(altitude_m * METERS_TO_FEET),
        })
    }

    /// Parse ForeFlight XATT message.
    ///
    /// Format: `XATTSimName,heading,pitch,roll`
    fn parse_foreflight_xatt(&self, data: &[u8]) -> Option<PartialStateUpdate> {
        let text = std::str::from_utf8(data).ok()?;

        // Split by comma, skip the first part (XATTSimName)
        let parts: Vec<&str> = text.split(',').collect();
        if parts.len() < 4 {
            trace!("XATT packet too short: {} parts", parts.len());
            return None;
        }

        // Parse fields: heading, pitch, roll
        let heading: f32 = parts[1].parse().ok()?;

        // XATT only provides heading, not position/speed
        Some(PartialStateUpdate {
            heading: Some(normalize_heading(heading)),
            ..Default::default()
        })
    }

    /// Parse legacy X-Plane binary DATA packet.
    fn parse_xplane_data(&self, data: &[u8]) -> Option<PartialStateUpdate> {
        // Validate header
        if data.len() < DATA_HEADER_SIZE {
            return None;
        }

        let mut updates = PartialStateUpdate::default();

        // Parse each data record
        let records = &data[DATA_HEADER_SIZE..];
        for chunk in records.chunks_exact(DATA_RECORD_SIZE) {
            let index = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);

            // Extract the 8 floats
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
                    // Index 3: speeds - ground speed is the 4th value (index 3)
                    // [vind_kias, vind_keas, vtrue_ktas, vtrue_ktgs, ...]
                    updates.ground_speed = Some(floats[3]);
                }
                INDEX_HEADINGS => {
                    // Index 17: pitch, roll, true heading, mag heading, ...
                    // [pitch, roll, hding_true, hding_mag, ...]
                    updates.heading = Some(normalize_heading(floats[2])); // True heading
                }
                INDEX_POSITION => {
                    // Index 20: lat, lon, alt_ind, alt_msl, ...
                    // [lat, lon, alt_ind_ft, alt_msl_ft, ...]
                    updates.latitude = Some(floats[0] as f64);
                    updates.longitude = Some(floats[1] as f64);
                    updates.altitude = Some(floats[3]); // MSL altitude
                }
                _ => {
                    // Ignore other indices
                }
            }
        }

        if updates.has_any() {
            Some(updates)
        } else {
            None
        }
    }
}

/// Partial state update from a single packet.
#[derive(Default)]
struct PartialStateUpdate {
    latitude: Option<f64>,
    longitude: Option<f64>,
    heading: Option<f32>,
    ground_speed: Option<f32>,
    altitude: Option<f32>,
}

impl PartialStateUpdate {
    fn has_any(&self) -> bool {
        self.latitude.is_some()
            || self.longitude.is_some()
            || self.heading.is_some()
            || self.ground_speed.is_some()
            || self.altitude.is_some()
    }
}

/// Accumulated partial state from multiple packets.
#[derive(Default)]
struct PartialState {
    latitude: Option<f64>,
    longitude: Option<f64>,
    heading: Option<f32>,
    ground_speed: Option<f32>,
    altitude: Option<f32>,
}

impl PartialState {
    fn apply(&mut self, update: PartialStateUpdate) {
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

    fn is_complete(&self) -> bool {
        self.latitude.is_some()
            && self.longitude.is_some()
            && self.heading.is_some()
            && self.ground_speed.is_some()
    }

    fn to_aircraft_state(&self) -> Option<AircraftState> {
        Some(AircraftState::new(
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

        // Header
        packet.extend_from_slice(b"DATA");
        packet.push(0); // Internal use byte

        // Records
        for (index, floats) in records {
            packet.extend_from_slice(&index.to_le_bytes());
            for f in floats {
                packet.extend_from_slice(&f.to_le_bytes());
            }
        }

        packet
    }

    #[test]
    fn test_parse_position_packet() {
        let listener = TelemetryListener::new(49003);

        // Create a packet with position data
        let packet = create_data_packet(&[(
            INDEX_POSITION,
            [
                45.5,   // lat
                -122.5, // lon
                5000.0, // alt_ind
                5100.0, // alt_msl
                0.0, 0.0, 0.0, 0.0,
            ],
        )]);

        let updates = listener.parse_packet(&packet).unwrap();
        assert!((updates.latitude.unwrap() - 45.5).abs() < 0.001);
        assert!((updates.longitude.unwrap() - (-122.5)).abs() < 0.001);
        assert!((updates.altitude.unwrap() - 5100.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_heading_packet() {
        let listener = TelemetryListener::new(49003);

        let packet = create_data_packet(&[(
            INDEX_HEADINGS,
            [5.0, 2.0, 270.0, 268.0, 0.0, 0.0, 0.0, 0.0], // pitch, roll, true_hdg, mag_hdg
        )]);

        let updates = listener.parse_packet(&packet).unwrap();
        assert!((updates.heading.unwrap() - 270.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_speed_packet() {
        let listener = TelemetryListener::new(49003);

        let packet = create_data_packet(&[(
            INDEX_SPEEDS,
            [
                150.0, 148.0, 165.0, 145.0, // kias, keas, ktas, ground_speed
                0.0, 0.0, 0.0, 0.0,
            ],
        )]);

        let updates = listener.parse_packet(&packet).unwrap();
        assert!((updates.ground_speed.unwrap() - 145.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_combined_packet() {
        let listener = TelemetryListener::new(49003);

        let packet = create_data_packet(&[
            (
                INDEX_POSITION,
                [40.0, -75.0, 3000.0, 3050.0, 0.0, 0.0, 0.0, 0.0],
            ),
            (INDEX_HEADINGS, [0.0, 0.0, 90.0, 88.0, 0.0, 0.0, 0.0, 0.0]),
            (INDEX_SPEEDS, [0.0, 0.0, 0.0, 120.0, 0.0, 0.0, 0.0, 0.0]),
        ]);

        let updates = listener.parse_packet(&packet).unwrap();
        assert!(updates.latitude.is_some());
        assert!(updates.longitude.is_some());
        assert!(updates.heading.is_some());
        assert!(updates.ground_speed.is_some());
    }

    #[test]
    fn test_parse_invalid_header() {
        let listener = TelemetryListener::new(49003);

        let packet = b"NOTD\x00";
        assert!(listener.parse_packet(packet).is_none());
    }

    #[test]
    fn test_parse_too_short() {
        let listener = TelemetryListener::new(49003);

        let packet = b"DAT";
        assert!(listener.parse_packet(packet).is_none());
    }

    #[test]
    fn test_partial_state_accumulation() {
        let mut partial = PartialState::default();

        // First update: position only
        partial.apply(PartialStateUpdate {
            latitude: Some(45.0),
            longitude: Some(-122.0),
            altitude: Some(5000.0),
            ..Default::default()
        });
        assert!(!partial.is_complete());

        // Second update: heading
        partial.apply(PartialStateUpdate {
            heading: Some(90.0),
            ..Default::default()
        });
        assert!(!partial.is_complete());

        // Third update: speed
        partial.apply(PartialStateUpdate {
            ground_speed: Some(120.0),
            ..Default::default()
        });
        assert!(partial.is_complete());

        let state = partial.to_aircraft_state().unwrap();
        assert!((state.latitude - 45.0).abs() < 0.001);
        assert!((state.ground_speed - 120.0).abs() < 0.1);
    }

    // ForeFlight protocol tests

    #[test]
    fn test_parse_foreflight_xgps() {
        let listener = TelemetryListener::new(49002);

        // XGPS format: XGPSSimName,lon,lat,alt_m,track,gs_m/s
        let packet = b"XGPSX-Plane,-122.5,45.5,3048.0,270.5,154.3";

        let updates = listener.parse_packet(packet).unwrap();

        // Check latitude and longitude
        assert!((updates.latitude.unwrap() - 45.5).abs() < 0.001);
        assert!((updates.longitude.unwrap() - (-122.5)).abs() < 0.001);

        // Altitude: 3048m = 10000ft
        assert!((updates.altitude.unwrap() - 10000.0).abs() < 10.0);

        // Track/heading
        assert!((updates.heading.unwrap() - 270.5).abs() < 0.1);

        // Ground speed: 154.3 m/s = 300 knots
        assert!((updates.ground_speed.unwrap() - 300.0).abs() < 1.0);
    }

    #[test]
    fn test_parse_foreflight_xgps_negative_coords() {
        let listener = TelemetryListener::new(49002);

        // Southern hemisphere, western longitude
        let packet = b"XGPSX-Plane,-33.865,-151.209,100.0,180.0,51.4";

        let updates = listener.parse_packet(packet).unwrap();
        assert!((updates.latitude.unwrap() - (-151.209)).abs() < 0.001);
        assert!((updates.longitude.unwrap() - (-33.865)).abs() < 0.001);
        assert!((updates.heading.unwrap() - 180.0).abs() < 0.1);
        // 51.4 m/s ≈ 100 knots
        assert!((updates.ground_speed.unwrap() - 100.0).abs() < 1.0);
    }

    #[test]
    fn test_parse_foreflight_xatt() {
        let listener = TelemetryListener::new(49002);

        // XATT format: XATTSimName,heading,pitch,roll
        let packet = b"XATTX-Plane,45.5,5.2,-3.1";

        let updates = listener.parse_packet(packet).unwrap();

        // Only heading should be set
        assert!((updates.heading.unwrap() - 45.5).abs() < 0.1);
        assert!(updates.latitude.is_none());
        assert!(updates.longitude.is_none());
        assert!(updates.ground_speed.is_none());
        assert!(updates.altitude.is_none());
    }

    #[test]
    fn test_parse_foreflight_xgps_complete_state() {
        let listener = TelemetryListener::new(49002);
        let mut partial = PartialState::default();

        // Single XGPS packet provides all data needed for complete state
        let packet = b"XGPSX-Plane,-80.11,34.55,1200.1,359.05,28.3";
        let updates = listener.parse_packet(packet).unwrap();
        partial.apply(updates);

        assert!(partial.is_complete());
        let state = partial.to_aircraft_state().unwrap();
        assert!((state.latitude - 34.55).abs() < 0.001);
        assert!((state.longitude - (-80.11)).abs() < 0.001);
    }

    #[test]
    fn test_parse_foreflight_xgps_too_short() {
        let listener = TelemetryListener::new(49002);

        // Missing groundspeed field
        let packet = b"XGPSX-Plane,-122.5,45.5,3048.0,270.5";
        assert!(listener.parse_packet(packet).is_none());
    }

    #[test]
    fn test_parse_foreflight_xatt_too_short() {
        let listener = TelemetryListener::new(49002);

        // Missing roll field
        let packet = b"XATTX-Plane,45.5,5.2";
        assert!(listener.parse_packet(packet).is_none());
    }

    #[test]
    fn test_parse_foreflight_invalid_number() {
        let listener = TelemetryListener::new(49002);

        // Invalid latitude value
        let packet = b"XGPSX-Plane,not_a_number,45.5,3048.0,270.5,154.3";
        assert!(listener.parse_packet(packet).is_none());
    }

    #[test]
    fn test_protocol_auto_detection() {
        let listener = TelemetryListener::new(49002);

        // ForeFlight XGPS
        let xgps = b"XGPSX-Plane,-122.5,45.5,3048.0,270.5,154.3";
        assert!(listener.parse_packet(xgps).is_some());

        // ForeFlight XATT
        let xatt = b"XATTX-Plane,45.5,5.2,-3.1";
        assert!(listener.parse_packet(xatt).is_some());

        // Legacy X-Plane DATA
        let data = create_data_packet(&[(
            INDEX_POSITION,
            [45.5, -122.5, 5000.0, 5100.0, 0.0, 0.0, 0.0, 0.0],
        )]);
        assert!(listener.parse_packet(&data).is_some());

        // Unknown protocol
        let unknown = b"UNKN,some,data";
        assert!(listener.parse_packet(unknown).is_none());
    }

    mod normalize_heading_tests {
        use super::*;

        #[test]
        fn test_normalize_heading_already_valid() {
            // Headings already in [0, 360) should be unchanged
            assert!((normalize_heading(0.0) - 0.0).abs() < 0.001);
            assert!((normalize_heading(90.0) - 90.0).abs() < 0.001);
            assert!((normalize_heading(180.0) - 180.0).abs() < 0.001);
            assert!((normalize_heading(270.0) - 270.0).abs() < 0.001);
            assert!((normalize_heading(359.9) - 359.9).abs() < 0.001);
        }

        #[test]
        fn test_normalize_heading_negative() {
            // Negative headings should be converted to positive
            assert!((normalize_heading(-1.0) - 359.0).abs() < 0.001);
            assert!((normalize_heading(-90.0) - 270.0).abs() < 0.001);
            assert!((normalize_heading(-180.0) - 180.0).abs() < 0.001);
            assert!((normalize_heading(-270.0) - 90.0).abs() < 0.001);
            assert!((normalize_heading(-359.0) - 1.0).abs() < 0.001);
        }

        #[test]
        fn test_normalize_heading_overflow() {
            // Headings >= 360 should wrap around
            assert!((normalize_heading(360.0) - 0.0).abs() < 0.001);
            assert!((normalize_heading(361.0) - 1.0).abs() < 0.001);
            assert!((normalize_heading(450.0) - 90.0).abs() < 0.001);
            assert!((normalize_heading(720.0) - 0.0).abs() < 0.001);
        }

        #[test]
        fn test_normalize_heading_property_always_positive() {
            // Property: output is always in [0, 360)
            let test_values = [
                -720.0, -360.0, -180.0, -90.0, -45.0, -1.0, 0.0, 1.0, 45.0, 90.0, 180.0, 270.0,
                359.0, 360.0, 361.0, 450.0, 720.0, 1000.0, -1000.0,
            ];
            for &h in &test_values {
                let normalized = normalize_heading(h);
                assert!(
                    (0.0..360.0).contains(&normalized),
                    "normalize_heading({}) = {} is not in [0, 360)",
                    h,
                    normalized
                );
            }
        }

        #[test]
        fn test_normalize_heading_property_preserves_direction() {
            // Property: normalized heading points in the same direction
            // (sin and cos should be approximately equal)
            let test_values = [-90.0, -45.0, 45.0, 90.0, 180.0, 270.0, 450.0, -270.0];
            for &h in &test_values {
                let normalized = normalize_heading(h);
                let h_rad = h.to_radians();
                let norm_rad = normalized.to_radians();
                assert!(
                    (h_rad.sin() - norm_rad.sin()).abs() < 0.001,
                    "sin mismatch for heading {}: original sin={}, normalized sin={}",
                    h,
                    h_rad.sin(),
                    norm_rad.sin()
                );
                assert!(
                    (h_rad.cos() - norm_rad.cos()).abs() < 0.001,
                    "cos mismatch for heading {}: original cos={}, normalized cos={}",
                    h,
                    h_rad.cos(),
                    norm_rad.cos()
                );
            }
        }
    }
}
