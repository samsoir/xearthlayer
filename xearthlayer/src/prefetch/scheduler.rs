//! Prefetch scheduler that coordinates tile prediction and request submission.
//!
//! The scheduler receives aircraft state updates from the telemetry listener,
//! predicts which tiles should be pre-fetched, and submits requests to the
//! DDS pipeline.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace};

use crate::dds::DdsFormat;
use crate::executor::DdsClient;

use super::condition::PrefetchCondition;
use super::predictor::TilePredictor;
use super::state::{AircraftState, SharedPrefetchStatus};
use super::strategy::Prefetcher;

/// Minimum time between prediction runs (rate limiting).
const MIN_PREDICTION_INTERVAL: Duration = Duration::from_secs(2);

/// Prefetch scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Zoom level for tile predictions.
    pub zoom: u8,
    /// Provider name for cache key lookups.
    pub provider: String,
    /// DDS format for cache key lookups.
    pub dds_format: DdsFormat,
    /// Maximum tiles to submit per prediction cycle.
    pub batch_size: usize,
    /// Maximum concurrent in-flight prefetch requests.
    pub max_in_flight: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            zoom: 14,
            provider: "bing".to_string(),
            dds_format: DdsFormat::BC1,
            batch_size: 500,
            max_in_flight: 2000,
        }
    }
}

/// Prefetch scheduler statistics for monitoring.
#[derive(Debug, Default)]
pub struct PrefetchStats {
    /// Total tiles predicted.
    pub tiles_predicted: AtomicU64,
    /// Tiles submitted for prefetch.
    pub tiles_submitted: AtomicU64,
    /// Tiles skipped because already in-flight.
    pub tiles_in_flight_skipped: AtomicU64,
    /// Prediction cycles run.
    pub prediction_cycles: AtomicU64,
}

impl PrefetchStats {
    /// Get a snapshot of current statistics.
    pub fn snapshot(&self) -> PrefetchStatsSnapshot {
        PrefetchStatsSnapshot {
            tiles_predicted: self.tiles_predicted.load(Ordering::Relaxed),
            tiles_submitted: self.tiles_submitted.load(Ordering::Relaxed),
            tiles_in_flight_skipped: self.tiles_in_flight_skipped.load(Ordering::Relaxed),
            prediction_cycles: self.prediction_cycles.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of prefetch statistics.
#[derive(Debug, Clone, Default)]
pub struct PrefetchStatsSnapshot {
    pub tiles_predicted: u64,
    pub tiles_submitted: u64,
    pub tiles_in_flight_skipped: u64,
    pub prediction_cycles: u64,
}

/// Prefetch scheduler that coordinates prediction and request submission.
///
/// The scheduler:
/// 1. Receives aircraft state updates from the telemetry listener
/// 2. Checks if prefetching should be active (via injected condition)
/// 3. Predicts which tiles should be pre-fetched using the TilePredictor
/// 4. Submits prefetch requests to the DDS pipeline (which handles caching)
pub struct PrefetchScheduler {
    /// Tile predictor engine.
    predictor: TilePredictor,
    /// DDS client for submitting prefetch requests.
    dds_client: Arc<dyn DdsClient>,
    /// Scheduler configuration.
    config: SchedulerConfig,
    /// Condition for determining if prefetching should be active.
    condition: Box<dyn PrefetchCondition>,
    /// Tracks tiles currently being prefetched to avoid duplicates.
    in_flight: HashSet<(u32, u32, u8)>,
    /// Last state used for prediction (for significant change detection).
    last_state: Option<AircraftState>,
    /// Statistics for monitoring.
    stats: Arc<PrefetchStats>,
    /// Optional shared status for TUI display.
    shared_status: Option<Arc<SharedPrefetchStatus>>,
}

impl PrefetchScheduler {
    /// Create a new prefetch scheduler.
    ///
    /// # Arguments
    ///
    /// * `predictor` - Tile predictor for calculating tiles to prefetch
    /// * `dds_client` - Client for submitting prefetch requests
    /// * `config` - Scheduler configuration
    /// * `condition` - Condition for determining when prefetching is active
    pub fn new(
        predictor: TilePredictor,
        dds_client: Arc<dyn DdsClient>,
        config: SchedulerConfig,
        condition: Box<dyn PrefetchCondition>,
    ) -> Self {
        let stats = Arc::new(PrefetchStats::default());
        Self {
            predictor,
            dds_client,
            config,
            condition,
            in_flight: HashSet::new(),
            last_state: None,
            stats,
            shared_status: None,
        }
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Get access to the statistics for monitoring.
    pub fn stats(&self) -> Arc<PrefetchStats> {
        Arc::clone(&self.stats)
    }

    /// Run the scheduler, processing state updates from the channel.
    ///
    /// This method runs until the channel is closed or the cancellation token
    /// is triggered.
    ///
    /// # Arguments
    ///
    /// * `state_rx` - Channel receiving aircraft state updates
    /// * `cancellation_token` - Token to signal shutdown
    pub async fn run(
        mut self,
        mut state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) {
        info!(
            zoom = self.config.zoom,
            provider = %self.config.provider,
            "Prefetch scheduler started"
        );

        let mut last_prediction = std::time::Instant::now();

        loop {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    info!("Prefetch scheduler shutting down");
                    break;
                }

                Some(state) = state_rx.recv() => {
                    // Always update shared status with aircraft position for dashboard display
                    if let Some(ref status) = self.shared_status {
                        status.update_aircraft(&state);
                    }

                    // Rate limit predictions
                    if last_prediction.elapsed() < MIN_PREDICTION_INTERVAL {
                        trace!("Skipping prediction - rate limited");
                        continue;
                    }

                    // Check for significant state change
                    if let Some(ref last) = self.last_state {
                        if !state.has_significant_change(last) && !state.is_stale(Duration::from_secs(5)) {
                            trace!("Skipping prediction - no significant change");
                            continue;
                        }
                    }

                    // Process the state update
                    self.process_state(&state);
                    self.last_state = Some(state);
                    last_prediction = std::time::Instant::now();
                }
            }
        }
    }

    /// Process a state update and submit prefetch requests.
    fn process_state(&mut self, state: &AircraftState) {
        self.stats.prediction_cycles.fetch_add(1, Ordering::Relaxed);

        // Update shared status with aircraft state (always, for dashboard display)
        if let Some(ref status) = self.shared_status {
            status.update_aircraft(state);
        }

        // Check if prefetch condition is met
        if !self.condition.should_prefetch(state) {
            trace!(
                condition = self.condition.description(),
                speed = state.ground_speed,
                "Prefetch: condition not met, skipping prediction"
            );
            return;
        }

        // Check if we're at max in-flight capacity
        if self.in_flight.len() >= self.config.max_in_flight {
            debug!(
                in_flight = self.in_flight.len(),
                max = self.config.max_in_flight,
                "Prefetch at capacity, skipping prediction cycle"
            );
            // Clear some old entries to allow recovery
            if self.in_flight.len() > self.config.max_in_flight * 2 {
                self.in_flight.clear();
                debug!("Cleared stale in-flight tracking");
            }
            return;
        }

        // Predict tiles to prefetch
        let predictions = self.predictor.predict(state, self.config.zoom);
        let predicted_count = predictions.len();

        self.stats
            .tiles_predicted
            .fetch_add(predicted_count as u64, Ordering::Relaxed);

        // Log at info level for visibility
        info!(
            lat = format!("{:.4}", state.latitude),
            lon = format!("{:.4}", state.longitude),
            heading = format!("{:.0}", state.heading),
            speed = format!("{:.0}", state.ground_speed),
            predicted_tiles = predicted_count,
            in_flight = self.in_flight.len(),
            "Prefetch: processing aircraft position"
        );

        // Clean up completed in-flight tracking
        // (In a real implementation, we'd track completion via response channels)
        // For now, we just cap the set size to prevent unbounded growth
        if self.in_flight.len() > 100 {
            self.in_flight.clear();
            debug!("Cleared in-flight tracking (size limit reached)");
        }

        let mut submitted = 0;
        let mut in_flight_skipped = 0;

        for predicted in predictions {
            if submitted >= self.config.batch_size {
                debug!(
                    "Reached max prefetch batch size ({})",
                    self.config.batch_size
                );
                break;
            }

            let tile = predicted.tile;
            let tile_key = (tile.row, tile.col, tile.zoom);

            // Skip if already in flight (coalescer handles cache checks)
            if self.in_flight.contains(&tile_key) {
                in_flight_skipped += 1;
                continue;
            }

            trace!(
                row = tile.row,
                col = tile.col,
                priority = predicted.priority,
                "Submitting prefetch request"
            );

            // Submit prefetch request via DdsClient (fire-and-forget)
            self.dds_client.prefetch(tile);
            self.in_flight.insert(tile_key);
            submitted += 1;
        }

        // Update statistics
        self.stats
            .tiles_submitted
            .fetch_add(submitted as u64, Ordering::Relaxed);
        self.stats
            .tiles_in_flight_skipped
            .fetch_add(in_flight_skipped as u64, Ordering::Relaxed);

        // Log summary
        if submitted > 0 {
            info!(
                submitted,
                in_flight_skipped,
                total_in_flight = self.in_flight.len(),
                "Prefetch: submitted {} tiles ({} skipped, already in-flight)",
                submitted,
                in_flight_skipped
            );
        }

        // Update shared status with stats
        if let Some(ref status) = self.shared_status {
            status.update_stats(self.stats.snapshot());
        }
    }
}

// Implement the Prefetcher trait for PrefetchScheduler
impl Prefetcher for PrefetchScheduler {
    fn run(
        self: Box<Self>,
        state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            // Unbox and run the actual implementation
            (*self).run(state_rx, cancellation_token).await
        })
    }

    fn name(&self) -> &'static str {
        "flight-path"
    }

    fn description(&self) -> &'static str {
        "Complex flight-path prediction with cone and radial calculations"
    }

    fn startup_info(&self) -> String {
        format!("flight-path, {}", self.predictor.startup_info())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::{DdsClientError, Priority};
    use crate::prefetch::{AlwaysActiveCondition, MinimumSpeedCondition, NeverActiveCondition};
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    /// Mock DdsClient that counts calls for testing.
    struct MockDdsClient {
        call_count: Arc<AtomicUsize>,
    }

    impl MockDdsClient {
        fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            let client = Arc::new(Self {
                call_count: Arc::clone(&count),
            });
            (client, count)
        }
    }

    impl DdsClient for MockDdsClient {
        fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            drop(tx);
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            _priority: Priority,
            _origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.request_dds(tile, cancellation)
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    fn create_test_client() -> (Arc<dyn DdsClient>, Arc<AtomicUsize>) {
        let (client, count) = MockDdsClient::new();
        (client as Arc<dyn DdsClient>, count)
    }

    #[test]
    fn test_scheduler_creation() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (client, _) = create_test_client();
        let config = SchedulerConfig::default();
        let condition = Box::new(AlwaysActiveCondition);

        let scheduler = PrefetchScheduler::new(predictor, client, config, condition);

        assert!(scheduler.in_flight.is_empty());
        assert!(scheduler.last_state.is_none());
    }

    #[test]
    fn test_scheduler_config_default() {
        let config = SchedulerConfig::default();

        assert_eq!(config.zoom, 14);
        assert_eq!(config.provider, "bing");
        assert_eq!(config.dds_format, DdsFormat::BC1);
    }

    // Property-based tests for condition behavior
    proptest! {
        /// Property: NeverActiveCondition always blocks prefetch regardless of aircraft state.
        #[test]
        fn prop_never_active_blocks_all_states(
            lat in -85.0f64..85.0f64,
            lon in -180.0f64..180.0f64,
            heading in 0.0f32..360.0f32,
            speed in 0.0f32..600.0f32,
            altitude in 0.0f32..50000.0f32,
        ) {
            let predictor = TilePredictor::new(45.0, 100.0, 60.0);
            let (client, call_count) = create_test_client();
            let config = SchedulerConfig::default();
            let condition = Box::new(NeverActiveCondition);

            let mut scheduler = PrefetchScheduler::new(predictor, client, config, condition);
            let state = AircraftState::new(lat, lon, heading, speed, altitude);
            scheduler.process_state(&state);

            // NeverActiveCondition should always block prefetch
            prop_assert_eq!(call_count.load(Ordering::SeqCst), 0);
        }

        /// Property: AlwaysActiveCondition allows prefetch for any moving aircraft.
        #[test]
        fn prop_always_active_allows_moving_aircraft(
            lat in -85.0f64..85.0f64,
            lon in -180.0f64..180.0f64,
            heading in 0.0f32..360.0f32,
            speed in 50.0f32..600.0f32,  // Fast enough to generate predictions
            altitude in 1000.0f32..50000.0f32,
        ) {
            let predictor = TilePredictor::new(45.0, 100.0, 60.0);
            let (client, call_count) = create_test_client();
            let config = SchedulerConfig::default();
            let condition = Box::new(AlwaysActiveCondition);

            let mut scheduler = PrefetchScheduler::new(predictor, client, config, condition);
            let state = AircraftState::new(lat, lon, heading, speed, altitude);
            scheduler.process_state(&state);

            // AlwaysActiveCondition should allow prefetch
            prop_assert!(call_count.load(Ordering::SeqCst) > 0);
        }

        /// Property: MinimumSpeedCondition blocks aircraft below threshold.
        #[test]
        fn prop_min_speed_blocks_slow_aircraft(
            lat in -85.0f64..85.0f64,
            lon in -180.0f64..180.0f64,
            heading in 0.0f32..360.0f32,
            speed in 0.0f32..29.9f32,  // Below 30kt threshold
            altitude in 0.0f32..50000.0f32,
        ) {
            let predictor = TilePredictor::new(45.0, 100.0, 60.0);
            let (client, call_count) = create_test_client();
            let config = SchedulerConfig::default();
            let condition = Box::new(MinimumSpeedCondition::new(30.0));

            let mut scheduler = PrefetchScheduler::new(predictor, client, config, condition);
            let state = AircraftState::new(lat, lon, heading, speed, altitude);
            scheduler.process_state(&state);

            // Below threshold should be blocked
            prop_assert_eq!(call_count.load(Ordering::SeqCst), 0);
        }

        /// Property: MinimumSpeedCondition allows aircraft at or above threshold.
        #[test]
        fn prop_min_speed_allows_fast_aircraft(
            lat in -85.0f64..85.0f64,
            lon in -180.0f64..180.0f64,
            heading in 0.0f32..360.0f32,
            speed in 50.0f32..600.0f32,  // Above 30kt threshold, fast enough to predict
            altitude in 1000.0f32..50000.0f32,
        ) {
            let predictor = TilePredictor::new(45.0, 100.0, 60.0);
            let (client, call_count) = create_test_client();
            let config = SchedulerConfig::default();
            let condition = Box::new(MinimumSpeedCondition::new(30.0));

            let mut scheduler = PrefetchScheduler::new(predictor, client, config, condition);
            let state = AircraftState::new(lat, lon, heading, speed, altitude);
            scheduler.process_state(&state);

            // Above threshold should allow prefetch
            prop_assert!(call_count.load(Ordering::SeqCst) > 0);
        }

        /// Property: Scheduler respects batch_size limit regardless of predictions.
        #[test]
        fn prop_respects_batch_size_limit(
            lat in -85.0f64..85.0f64,
            lon in -180.0f64..180.0f64,
            heading in 0.0f32..360.0f32,
            speed in 100.0f32..600.0f32,
            altitude in 1000.0f32..50000.0f32,
        ) {
            let predictor = TilePredictor::new(45.0, 100.0, 60.0);
            let (client, call_count) = create_test_client();
            let config = SchedulerConfig::default();
            let batch_size = config.batch_size;
            let condition = Box::new(AlwaysActiveCondition);

            let mut scheduler = PrefetchScheduler::new(predictor, client, config, condition);
            let state = AircraftState::new(lat, lon, heading, speed, altitude);
            scheduler.process_state(&state);

            // Should never exceed batch_size
            prop_assert!(call_count.load(Ordering::SeqCst) <= batch_size);
        }
    }
}
