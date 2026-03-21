//! APT (Aircraft Position & Telemetry) wiring for the service orchestrator.
//!
//! Contains the `start_web_api_adapter()` and `start_network_position()` methods,
//! which are independent of the core startup sequence.

use tokio::sync::mpsc;
use tracing::info;

use crate::aircraft_position::{spawn_position_logger, DEFAULT_LOG_INTERVAL};

use super::super::error::ServiceError;
use super::ServiceOrchestrator;

impl ServiceOrchestrator {
    /// Start the X-Plane Web API adapter.
    ///
    /// Connects to X-Plane's built-in Web API via REST + WebSocket for
    /// 10Hz position and sim state updates. Requires no user configuration
    /// (Web API is enabled by default since X-Plane 12.1.1).
    pub fn start_web_api_adapter(&self) -> Result<(), ServiceError> {
        let service = self
            .mount_manager
            .get_service()
            .ok_or_else(|| ServiceError::NotStarted("No service available for Web API".into()))?;

        let runtime_handle = service.runtime_handle().clone();

        let web_api_config = crate::aircraft_position::web_api::config::WebApiConfig::default();
        let (position_tx, mut position_rx) = mpsc::channel(32);
        let sim_state = self.sim_state.clone();

        let adapter = crate::aircraft_position::web_api::WebApiAdapter::new(
            web_api_config.clone(),
            position_tx,
            sim_state,
        );

        // Spawn the adapter with cancellation
        let adapter_cancellation = self.cancellation.clone();
        runtime_handle.spawn(async move {
            adapter.run(adapter_cancellation).await;
            tracing::debug!("Web API adapter task exited");
        });

        // Bridge task: forward position updates to APT aggregator
        let aircraft_position = self.aircraft_position.clone();
        runtime_handle.spawn(async move {
            while let Some(state) = position_rx.recv().await {
                aircraft_position.receive_telemetry(state);
            }
        });

        // Periodic position logger for flight analysis (DEBUG level only)
        if tracing::enabled!(tracing::Level::DEBUG) {
            spawn_position_logger(
                &runtime_handle,
                self.aircraft_position.clone(),
                self.cancellation.clone(),
                DEFAULT_LOG_INTERVAL,
            );
        }

        info!(
            port = web_api_config.port,
            "X-Plane Web API adapter started"
        );
        Ok(())
    }
}
