//! Telemetry Receiver - UDP listener for X-Plane position data.
//!
//! Listens for UDP broadcasts from X-Plane and converts them to `AircraftState` updates.
//!
//! # Supported Protocols
//!
//! - **ForeFlight** (XGPS2/XATT2) - Recommended for X-Plane 12
//! - **Legacy DATA** - Binary format for older X-Plane versions
//!
//! # Setup
//!
//! In X-Plane: Settings → Network → "Send position to ForeFlight"
//!
//! # Example
//!
//! ```ignore
//! let (tx, rx) = mpsc::channel(16);
//! let receiver = TelemetryReceiver::new(TelemetryReceiverConfig::default(), tx);
//! let handle = receiver.start();
//!
//! while let Some(state) = rx.recv().await {
//!     println!("Position: {}, {}", state.latitude, state.longitude);
//! }
//! ```

mod protocol;

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use super::state::AircraftState;
use protocol::{parse_packet, PartialState};

/// Maximum packet size we expect.
const MAX_PACKET_SIZE: usize = 1024;

/// Telemetry receiver configuration.
#[derive(Debug, Clone)]
pub struct TelemetryReceiverConfig {
    /// UDP port to listen on (default: 49002 for ForeFlight).
    pub port: u16,

    /// Minimum interval between position updates.
    pub min_update_interval: Duration,

    /// Timeout for socket receive operations.
    pub recv_timeout: Duration,
}

impl Default for TelemetryReceiverConfig {
    fn default() -> Self {
        Self {
            port: 49002,
            min_update_interval: Duration::from_millis(500),
            recv_timeout: Duration::from_millis(500),
        }
    }
}

/// Error type for telemetry receiver.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// Failed to bind the UDP socket.
    #[error("Failed to bind UDP socket on port {port}: {source}")]
    SocketBindError {
        port: u16,
        #[source]
        source: std::io::Error,
    },
}

/// Telemetry receiver for X-Plane UDP broadcasts.
///
/// Listens on a UDP socket for position data and sends `AircraftState`
/// updates to the aggregator via a channel.
pub struct TelemetryReceiver {
    config: TelemetryReceiverConfig,
    state_tx: mpsc::Sender<AircraftState>,
}

impl TelemetryReceiver {
    /// Create a new telemetry receiver.
    pub fn new(config: TelemetryReceiverConfig, state_tx: mpsc::Sender<AircraftState>) -> Self {
        Self { config, state_tx }
    }

    /// Create with default configuration.
    pub fn with_defaults(state_tx: mpsc::Sender<AircraftState>) -> Self {
        Self::new(TelemetryReceiverConfig::default(), state_tx)
    }

    /// Get the configured port.
    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// Start the telemetry receiver.
    ///
    /// Spawns an async task that listens for UDP broadcasts.
    pub fn start(self) -> tokio::task::JoinHandle<Result<(), TelemetryError>> {
        tokio::spawn(self.run())
    }

    /// Run the telemetry receiver loop.
    async fn run(self) -> Result<(), TelemetryError> {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.config.port))
            .await
            .map_err(|e| TelemetryError::SocketBindError {
                port: self.config.port,
                source: e,
            })?;

        let local_addr = socket.local_addr().ok();
        info!(
            port = self.config.port,
            local_addr = ?local_addr,
            "Telemetry receiver started"
        );

        let mut buffer = [0u8; MAX_PACKET_SIZE];
        let mut partial_state = PartialState::default();
        let mut last_send_time = Instant::now();
        let mut packets_received: u64 = 0;
        let mut states_sent: u64 = 0;

        loop {
            if self.state_tx.is_closed() {
                debug!("Telemetry channel closed, stopping receiver");
                break;
            }

            let recv_result =
                tokio::time::timeout(self.config.recv_timeout, socket.recv(&mut buffer)).await;

            match recv_result {
                Ok(Ok(len)) => {
                    packets_received += 1;
                    self.log_first_packet(packets_received, &buffer[..len]);

                    if let Some(updates) = parse_packet(&buffer[..len]) {
                        partial_state.apply(updates);

                        if partial_state.is_complete()
                            && last_send_time.elapsed() >= self.config.min_update_interval
                        {
                            if let Some(state) = partial_state.to_aircraft_state() {
                                states_sent += 1;
                                self.log_state(&state, states_sent);
                                self.send_state(state, states_sent);
                                last_send_time = Instant::now();
                            }
                        }
                    } else if packets_received <= 5 {
                        let preview = String::from_utf8_lossy(&buffer[..len.min(50)]);
                        debug!(packet_num = packets_received, preview = %preview, "Failed to parse packet");
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "UDP receive error");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    self.log_waiting(packets_received, last_send_time);
                    trace!("No telemetry data received (timeout)");
                }
            }
        }

        info!(packets_received, states_sent, "Telemetry receiver stopped");
        Ok(())
    }

    fn log_first_packet(&self, packets_received: u64, data: &[u8]) {
        if packets_received == 1 {
            let header = if data.len() >= 4 {
                String::from_utf8_lossy(&data[..4]).to_string()
            } else {
                format!("{:?}", data)
            };
            info!(
                port = self.config.port,
                header = %header,
                len = data.len(),
                "Received first telemetry packet"
            );
        }
    }

    fn log_state(&self, state: &AircraftState, states_sent: u64) {
        if states_sent == 1 {
            info!(
                lat = format!("{:.4}", state.latitude),
                lon = format!("{:.4}", state.longitude),
                hdg = format!("{:.0}", state.heading),
                gs = format!("{:.0}", state.ground_speed),
                alt = format!("{:.0}", state.altitude),
                "First complete aircraft state"
            );
        } else {
            debug!(
                lat = format!("{:.4}", state.latitude),
                lon = format!("{:.4}", state.longitude),
                "Aircraft state #{}",
                states_sent
            );
        }
    }

    fn send_state(&self, state: AircraftState, states_sent: u64) {
        match self.state_tx.try_send(state) {
            Ok(()) => {
                if states_sent == 1 {
                    info!("State sent to aggregator");
                }
            }
            Err(e) => {
                if states_sent <= 3 {
                    warn!("Failed to send state: {}", e);
                }
            }
        }
    }

    fn log_waiting(&self, packets_received: u64, last_send_time: Instant) {
        if packets_received == 0 && last_send_time.elapsed().as_secs().is_multiple_of(10) {
            info!(
                port = self.config.port,
                elapsed_secs = last_send_time.elapsed().as_secs(),
                "Waiting for telemetry data..."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TelemetryReceiverConfig::default();
        assert_eq!(config.port, 49002);
        assert_eq!(config.min_update_interval, Duration::from_millis(500));
    }

    #[tokio::test]
    async fn test_receiver_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let receiver = TelemetryReceiver::with_defaults(tx);
        assert_eq!(receiver.port(), 49002);
    }
}
