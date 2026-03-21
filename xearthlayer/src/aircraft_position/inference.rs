//! Inference Adapter - derives position from Scene Tracker data.
//!
//! When telemetry is unavailable, the APT module can infer approximate
//! aircraft position from the Scene Tracker's loaded bounds. This adapter
//! periodically queries the loaded bounds to derive position.
//!
//! # Triggers
//!
//! Position inference occurs on a periodic timer (every 30 seconds by default)
//! for steady-state flying.
//!
//! # Accuracy
//!
//! Inferred positions have ~100km accuracy (the size of the loaded scene area).
//! This is sufficient for prefetch prediction but not for precise positioning.
//!
//! # Usage
//!
//! ```ignore
//! let scene_tracker: Arc<dyn SceneTracker> = /* ... */;
//! let (tx, rx) = mpsc::channel(16);
//!
//! let adapter = InferenceAdapter::new(scene_tracker, tx);
//! let handle = adapter.start();
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::scene_tracker::SceneTracker;

use super::state::AircraftState;

/// Configuration for the inference adapter.
#[derive(Debug, Clone)]
pub struct InferenceAdapterConfig {
    /// Fallback inference interval for steady-state flying.
    pub fallback_interval: Duration,
}

impl Default for InferenceAdapterConfig {
    fn default() -> Self {
        Self {
            fallback_interval: Duration::from_secs(30),
        }
    }
}

/// Inference adapter - derives position from Scene Tracker.
///
/// Periodically queries loaded bounds to infer aircraft position.
pub struct InferenceAdapter {
    /// Scene Tracker for querying loaded bounds.
    scene_tracker: Arc<dyn SceneTracker>,

    /// Channel to send inferred position updates.
    state_tx: mpsc::Sender<AircraftState>,

    /// Configuration.
    config: InferenceAdapterConfig,
}

impl InferenceAdapter {
    /// Create a new inference adapter.
    pub fn new(
        scene_tracker: Arc<dyn SceneTracker>,
        state_tx: mpsc::Sender<AircraftState>,
    ) -> Self {
        Self {
            scene_tracker,
            state_tx,
            config: InferenceAdapterConfig::default(),
        }
    }

    /// Create with custom configuration.
    pub fn with_config(
        scene_tracker: Arc<dyn SceneTracker>,
        state_tx: mpsc::Sender<AircraftState>,
        config: InferenceAdapterConfig,
    ) -> Self {
        Self {
            scene_tracker,
            state_tx,
            config,
        }
    }

    /// Start the inference adapter.
    ///
    /// Spawns an async task that periodically infers position.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Run the inference loop.
    async fn run(self) {
        tracing::debug!("Inference adapter started");

        let mut fallback_interval = tokio::time::interval(self.config.fallback_interval);
        // Don't fire immediately
        fallback_interval.tick().await;

        loop {
            fallback_interval.tick().await;
            tracing::trace!("Fallback timer, inferring position");
            self.infer_and_send().await;
        }
    }

    /// Infer position from Scene Tracker and send to aggregator.
    async fn infer_and_send(&self) {
        if let Some(bounds) = self.scene_tracker.loaded_bounds() {
            let (lat, lon) = bounds.center();
            let state = AircraftState::from_inference(lat, lon);

            tracing::trace!(
                latitude = lat,
                longitude = lon,
                "Inferred position from scene bounds"
            );

            if let Err(e) = self.state_tx.send(state).await {
                tracing::warn!("Failed to send inferred position: {}", e);
            }
        } else {
            tracing::trace!("No loaded bounds available for inference");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion};
    use std::collections::HashSet;

    /// Mock Scene Tracker for testing.
    struct MockSceneTracker {
        bounds: Option<GeoBounds>,
    }

    impl MockSceneTracker {
        fn with_bounds(bounds: GeoBounds) -> Self {
            Self {
                bounds: Some(bounds),
            }
        }

        fn empty() -> Self {
            Self { bounds: None }
        }
    }

    impl SceneTracker for MockSceneTracker {
        fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
            HashSet::new()
        }

        fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
            false
        }

        fn total_requests(&self) -> u64 {
            0
        }

        fn loaded_regions(&self) -> HashSet<GeoRegion> {
            HashSet::new()
        }

        fn is_region_loaded(&self, _region: &GeoRegion) -> bool {
            false
        }

        fn loaded_bounds(&self) -> Option<GeoBounds> {
            self.bounds
        }
    }

    #[tokio::test]
    async fn test_inference_from_bounds() {
        let bounds = GeoBounds {
            min_lat: 53.0,
            max_lat: 54.0,
            min_lon: 9.0,
            max_lon: 11.0,
        };
        let tracker = Arc::new(MockSceneTracker::with_bounds(bounds));

        let (state_tx, mut state_rx) = mpsc::channel(16);

        let adapter = InferenceAdapter::new(tracker, state_tx);

        // Manually trigger inference
        adapter.infer_and_send().await;

        // Should receive inferred state
        let state = state_rx.try_recv().expect("Should receive state");
        assert_eq!(state.latitude, 53.5); // Center of 53-54
        assert_eq!(state.longitude, 10.0); // Center of 9-11
        assert_eq!(
            state.source,
            super::super::state::PositionSource::SceneInference
        );
    }

    #[tokio::test]
    async fn test_no_inference_without_bounds() {
        let tracker = Arc::new(MockSceneTracker::empty());

        let (state_tx, mut state_rx) = mpsc::channel(16);

        let adapter = InferenceAdapter::new(tracker, state_tx);

        // Manually trigger inference
        adapter.infer_and_send().await;

        // Should not receive any state
        assert!(state_rx.try_recv().is_err());
    }
}
