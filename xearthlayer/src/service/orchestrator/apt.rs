//! APT (Aircraft Position & Telemetry) wiring for the service orchestrator.
//!
//! Contains the `start_apt_telemetry()` and `start_network_position()` methods,
//! which are independent of the core startup sequence.

use tokio::sync::mpsc;
use tracing::info;

use crate::aircraft_position::{
    spawn_position_logger, TelemetryReceiver, TelemetryReceiverConfig, DEFAULT_LOG_INTERVAL,
};

use super::super::error::ServiceError;
use super::ServiceOrchestrator;

impl ServiceOrchestrator {
    /// Start the APT telemetry receiver.
    ///
    /// This starts listening for X-Plane UDP telemetry on the configured port.
    /// Must be called after mounting to have a runtime handle available.
    pub fn start_apt_telemetry(&self) -> Result<(), ServiceError> {
        let service = self
            .mount_manager
            .get_service()
            .ok_or_else(|| ServiceError::NotStarted("No service available for APT".into()))?;

        let runtime_handle = service.runtime_handle().clone();
        let telemetry_port = self.config.prefetch.udp_port;
        let (telemetry_tx, mut telemetry_rx) = mpsc::channel(32);

        let telemetry_config = TelemetryReceiverConfig {
            port: telemetry_port,
            ..Default::default()
        };
        let receiver = TelemetryReceiver::new(telemetry_config, telemetry_tx);
        let apt_cancellation = self.cancellation.clone();
        let logger_cancellation = apt_cancellation.clone();

        // Start the UDP receiver
        runtime_handle.spawn(async move {
            tokio::select! {
                result = receiver.start() => {
                    match result {
                        Ok(Ok(())) => tracing::debug!("APT telemetry receiver stopped"),
                        Ok(Err(e)) => tracing::warn!("APT telemetry receiver error: {}", e),
                        Err(e) => tracing::warn!("APT telemetry receiver task failed: {}", e),
                    }
                }
                _ = apt_cancellation.cancelled() => {
                    tracing::debug!("APT telemetry receiver cancelled");
                }
            }
        });

        // Bridge task: forward telemetry states to APT aggregator
        let aircraft_position = self.aircraft_position.clone();
        runtime_handle.spawn(async move {
            while let Some(state) = telemetry_rx.recv().await {
                aircraft_position.receive_telemetry(state);
            }
        });

        // Periodic position logger for flight analysis (DEBUG level only)
        if tracing::enabled!(tracing::Level::DEBUG) {
            spawn_position_logger(
                &runtime_handle,
                self.aircraft_position.clone(),
                logger_cancellation,
                DEFAULT_LOG_INTERVAL,
            );
        }

        info!(port = telemetry_port, "APT telemetry receiver started");
        Ok(())
    }

    /// Start the online network position adapter (VATSIM/IVAO/PilotEdge).
    ///
    /// This starts a poll loop daemon that fetches pilot position from an online
    /// ATC network's REST API and feeds it into the APT aggregator.
    ///
    /// The adapter only starts if:
    /// - `online_network.enabled` is true in the config
    /// - `online_network.pilot_id` is non-zero (a valid pilot CID)
    pub fn start_network_position(&self) -> Result<(), ServiceError> {
        let net_config = &self.config.online_network;

        if !net_config.enabled {
            tracing::debug!("Online network position disabled by configuration");
            return Ok(());
        }

        if net_config.pilot_id == 0 {
            tracing::warn!(
                "Online network position enabled but pilot_id is 0 (not configured), skipping"
            );
            return Ok(());
        }

        let service = self.mount_manager.get_service().ok_or_else(|| {
            ServiceError::NotStarted("No service available for network position".into())
        })?;

        let runtime_handle = service.runtime_handle().clone();

        // Create the network client and adapter
        let adapter_config = crate::aircraft_position::NetworkAdapterConfig::from_config(
            net_config.network_type.clone(),
            net_config.pilot_id,
            net_config.poll_interval_secs,
            net_config.max_stale_secs,
        );

        let client = crate::aircraft_position::VatsimClient::new(net_config.pilot_id);
        let (network_tx, mut network_rx) = mpsc::channel(16);
        let adapter =
            crate::aircraft_position::NetworkAdapter::new(client, network_tx, adapter_config);

        // Start the adapter with cancellation
        // adapter.start() calls tokio::spawn() internally, so it must run
        // inside the Tokio runtime context (not from the synchronous caller)
        let adapter_cancellation = self.cancellation.clone();
        runtime_handle.spawn(async move {
            let adapter_handle = adapter.start();
            adapter_cancellation.cancelled().await;
            adapter_handle.abort();
            tracing::debug!("Network position adapter cancelled");
        });

        // Bridge task: forward network states to APT aggregator
        let aircraft_position = self.aircraft_position.clone();
        runtime_handle.spawn(async move {
            while let Some(state) = network_rx.recv().await {
                aircraft_position.receive_network_position(state);
            }
        });

        info!(
            network = %net_config.network_type,
            pilot_id = net_config.pilot_id,
            poll_interval_secs = net_config.poll_interval_secs,
            "Online network position adapter started"
        );
        Ok(())
    }
}
