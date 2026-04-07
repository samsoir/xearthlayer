//! WebApiAdapter — async lifecycle for the X-Plane Web API connection.
//!
//! Manages the connection lifecycle: REST dataref lookup → WebSocket
//! subscribe → 10Hz message loop → reconnect on disconnect.
//!
//! ```text
//! run()
//!   └─ loop:
//!       ├─ connect_and_run()
//!       │    ├─ REST: lookup dataref IDs
//!       │    ├─ WebSocket: connect + subscribe
//!       │    └─ message loop (10Hz updates)
//!       └─ on error: sleep(reconnect_interval), retry
//! ```

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::client::{parse_position, WebApiClient, WebApiError};
use super::config::WebApiConfig;
use super::datarefs::{self, invert_id_map, parse_dataref_update, IdToNameMap};
use super::sim_state::SimState;
use super::SharedSimState;
use crate::aircraft_position::state::AircraftState;

/// X-Plane Web API adapter.
///
/// Connects to X-Plane's Web API via REST + WebSocket, sending position
/// updates through an mpsc channel and sim state to a shared lock.
/// Reconnects automatically on disconnect.
pub struct WebApiAdapter {
    client: WebApiClient,
    position_tx: mpsc::Sender<AircraftState>,
    sim_state: SharedSimState,
    config: WebApiConfig,
}

impl WebApiAdapter {
    /// Create a new adapter.
    pub fn new(
        config: WebApiConfig,
        position_tx: mpsc::Sender<AircraftState>,
        sim_state: SharedSimState,
    ) -> Self {
        let client = WebApiClient::new(config.clone());
        Self {
            client,
            position_tx,
            sim_state,
            config,
        }
    }

    /// Run the adapter until cancellation. Reconnects on disconnect.
    pub async fn run(&self, cancellation: CancellationToken) {
        info!(port = self.config.port, "X-Plane Web API adapter starting");

        loop {
            if cancellation.is_cancelled() {
                debug!("Web API adapter cancelled");
                return;
            }

            match self.connect_and_run(&cancellation).await {
                Ok(()) => {
                    debug!("Web API connection closed cleanly");
                }
                Err(WebApiError::ConnectionRefused) => {
                    debug!(
                        reconnect_secs = self.config.reconnect_interval.as_secs(),
                        "X-Plane Web API not available, will retry"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        reconnect_secs = self.config.reconnect_interval.as_secs(),
                        "Web API error, will reconnect"
                    );
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(self.config.reconnect_interval) => {}
                _ = cancellation.cancelled() => {
                    debug!("Web API adapter cancelled during reconnect wait");
                    return;
                }
            }
        }
    }

    /// Single connection attempt: lookup datarefs, connect WebSocket, run message loop.
    async fn connect_and_run(&self, cancellation: &CancellationToken) -> Result<(), WebApiError> {
        // REST — resolve dataref IDs by name
        let all_names = datarefs::all_datarefs();
        let name_refs: Vec<&str> = all_names.to_vec();
        let id_map = self.client.lookup_datarefs(&name_refs).await?;

        let ids: Vec<u64> = id_map.values().copied().collect();
        let id_to_name = invert_id_map(&id_map);

        info!(
            datarefs_resolved = id_map.len(),
            "Connected to X-Plane Web API"
        );

        // WebSocket — connect and subscribe
        let ws_url = self.client.websocket_url();
        let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| WebApiError::WebSocket(e.to_string()))?;

        let (mut ws_write, mut ws_read) = ws_stream.split();

        let subscribe_msg = datarefs::build_subscribe_message(1, &ids);
        ws_write
            .send(tokio_tungstenite::tungstenite::Message::Text(subscribe_msg))
            .await
            .map_err(|e| WebApiError::WebSocket(e.to_string()))?;

        debug!(count = ids.len(), "WebSocket subscribed to datarefs");

        // Message loop — accumulate values across delta updates
        let mut accumulated = HashMap::new();

        loop {
            tokio::select! {
                msg = ws_read.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            self.process_message(&text, &id_to_name, &mut accumulated).await;
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                            debug!("WebSocket closed by X-Plane");
                            return Ok(());
                        }
                        Some(Ok(_)) => {} // Ping/Pong/Binary — ignore
                        Some(Err(e)) => {
                            return Err(WebApiError::WebSocket(e.to_string()));
                        }
                        None => return Ok(()), // Stream ended
                    }
                }
                _ = cancellation.cancelled() => {
                    debug!("Web API adapter cancelled during message loop");
                    return Ok(());
                }
            }
        }
    }

    /// Process a single WebSocket message — update position and sim state.
    ///
    /// The WebSocket sends delta updates (only changed datarefs), so we
    /// merge each message into `accumulated` and build state from the
    /// complete set.
    async fn process_message(
        &self,
        text: &str,
        id_to_name: &IdToNameMap,
        accumulated: &mut HashMap<String, f64>,
    ) {
        let json: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "Failed to parse WebSocket message");
                return;
            }
        };

        let updates = parse_dataref_update(&json, id_to_name);
        if updates.is_empty() {
            return;
        }

        // Merge delta into accumulated state
        accumulated.extend(updates);

        // Build position from accumulated values
        if let Some(state) = parse_position(accumulated) {
            // Apply on_ground from SimState before sending.
            // SimState is updated after this block, so the first message will have
            // on_ground: false. The correct value propagates within 100ms (10Hz updates).
            let on_ground = self.sim_state.read().is_ok_and(|s| s.on_ground);
            let state = state.with_on_ground(on_ground);
            if self.position_tx.send(state).await.is_err() {
                debug!("Position channel closed");
            }
        }

        // Update shared sim state (always, even with partial data)
        let new_sim_state = SimState::from_dataref_values(accumulated);
        if let Ok(mut current) = self.sim_state.write() {
            *current = new_sim_state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_shared_sim_state_default() {
        let state = super::super::shared_sim_state();
        let current = state.read().unwrap();
        assert!(current.should_prefetch());
        assert!(!current.on_ground);
    }

    #[tokio::test]
    async fn test_process_message_updates_position_channel() {
        let (tx, mut rx) = mpsc::channel(16);
        let sim_state = super::super::shared_sim_state();
        let config = WebApiConfig::default();
        let adapter = WebApiAdapter::new(config, tx, sim_state);

        let id_to_name: IdToNameMap = HashMap::from([
            (100, datarefs::LATITUDE.to_string()),
            (200, datarefs::LONGITUDE.to_string()),
            (300, datarefs::TRUE_HEADING.to_string()),
            (400, datarefs::GROUND_SPEED.to_string()),
            (500, datarefs::ELEVATION.to_string()),
            (600, datarefs::TRACK.to_string()),
        ]);

        let msg = serde_json::json!({
            "type": "dataref_update_values",
            "data": {
                "100": 48.116, "200": 16.566, "300": 205.6,
                "400": 100.0, "500": 10000.0, "600": 210.0
            }
        });

        let mut accumulated = HashMap::new();
        adapter
            .process_message(&msg.to_string(), &id_to_name, &mut accumulated)
            .await;

        let state = rx.try_recv().expect("Should receive position update");
        assert!((state.latitude - 48.116).abs() < 0.001);
        assert!((state.longitude - 16.566).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_process_message_updates_sim_state() {
        let (tx, _rx) = mpsc::channel(16);
        let sim_state = super::super::shared_sim_state();
        let config = WebApiConfig::default();
        let adapter = WebApiAdapter::new(config, tx, Arc::clone(&sim_state));

        let id_to_name: IdToNameMap = HashMap::from([
            (100, datarefs::LATITUDE.to_string()),
            (200, datarefs::LONGITUDE.to_string()),
            (300, datarefs::TRUE_HEADING.to_string()),
            (700, datarefs::PAUSED.to_string()),
            (800, datarefs::ON_GROUND.to_string()),
            (900, datarefs::SCENERY_LOADING.to_string()),
        ]);

        let msg = serde_json::json!({
            "type": "dataref_update_values",
            "data": {
                "100": 48.0, "200": 16.0, "300": 90.0,
                "700": 1.0, "800": 1.0, "900": 0.0
            }
        });

        let mut accumulated = HashMap::new();
        adapter
            .process_message(&msg.to_string(), &id_to_name, &mut accumulated)
            .await;

        let current = sim_state.read().unwrap();
        assert!(current.paused);
        assert!(current.on_ground);
        assert!(!current.scenery_loading);
    }

    #[tokio::test]
    async fn test_process_message_accumulates_across_messages() {
        let (tx, mut rx) = mpsc::channel(16);
        let sim_state = super::super::shared_sim_state();
        let config = WebApiConfig::default();
        let adapter = WebApiAdapter::new(config, tx, sim_state);

        let id_to_name: IdToNameMap = HashMap::from([
            (100, datarefs::LATITUDE.to_string()),
            (200, datarefs::LONGITUDE.to_string()),
            (300, datarefs::TRUE_HEADING.to_string()),
        ]);

        let mut accumulated = HashMap::new();

        // First message — only latitude (not enough for position)
        let msg1 = serde_json::json!({
            "type": "dataref_update_values",
            "data": { "100": 48.0 }
        });
        adapter
            .process_message(&msg1.to_string(), &id_to_name, &mut accumulated)
            .await;
        assert!(rx.try_recv().is_err(), "Not enough data yet");

        // Second message — longitude + heading (now complete)
        let msg2 = serde_json::json!({
            "type": "dataref_update_values",
            "data": { "200": 16.0, "300": 90.0 }
        });
        adapter
            .process_message(&msg2.to_string(), &id_to_name, &mut accumulated)
            .await;

        let state = rx
            .try_recv()
            .expect("Should have position after accumulation");
        assert!((state.latitude - 48.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_process_message_propagates_on_ground_from_sim_state() {
        let (tx, mut rx) = mpsc::channel(16);
        let sim_state = super::super::shared_sim_state();

        // Pre-seed the sim_state with on_ground = true
        {
            let mut s = sim_state.write().unwrap();
            s.on_ground = true;
        }

        let config = WebApiConfig::default();
        let adapter = WebApiAdapter::new(config, tx, Arc::clone(&sim_state));

        let id_to_name: IdToNameMap = HashMap::from([
            (100, datarefs::LATITUDE.to_string()),
            (200, datarefs::LONGITUDE.to_string()),
            (300, datarefs::TRUE_HEADING.to_string()),
            (400, datarefs::GROUND_SPEED.to_string()),
            (500, datarefs::ELEVATION.to_string()),
            (600, datarefs::TRACK.to_string()),
        ]);

        let msg = serde_json::json!({
            "type": "dataref_update_values",
            "data": {
                "100": 48.116, "200": 16.566, "300": 205.6,
                "400": 100.0, "500": 10000.0, "600": 210.0
            }
        });

        let mut accumulated = HashMap::new();
        adapter
            .process_message(&msg.to_string(), &id_to_name, &mut accumulated)
            .await;

        let state = rx.try_recv().expect("Should receive position update");
        assert!(
            state.on_ground,
            "on_ground should be propagated from SimState"
        );
    }

    #[tokio::test]
    async fn test_process_message_ignores_invalid_json() {
        let (tx, mut rx) = mpsc::channel(16);
        let sim_state = super::super::shared_sim_state();
        let config = WebApiConfig::default();
        let adapter = WebApiAdapter::new(config, tx, sim_state);

        let id_to_name: IdToNameMap = HashMap::new();
        let mut accumulated = HashMap::new();

        adapter
            .process_message("not valid json", &id_to_name, &mut accumulated)
            .await;

        assert!(
            rx.try_recv().is_err(),
            "Should not produce state from bad JSON"
        );
    }
}
