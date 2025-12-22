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

use crate::cache::{Cache, CacheKey};
use crate::dds::DdsFormat;
use crate::fuse::{DdsHandler, DdsRequest};
use crate::pipeline::JobId;

use super::predictor::TilePredictor;
use super::state::{AircraftState, SharedPrefetchStatus};

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
    /// Tiles skipped because already in cache.
    pub tiles_cached: AtomicU64,
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
            tiles_cached: self.tiles_cached.load(Ordering::Relaxed),
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
    pub tiles_cached: u64,
    pub tiles_submitted: u64,
    pub tiles_in_flight_skipped: u64,
    pub prediction_cycles: u64,
}

/// Prefetch scheduler that coordinates prediction and request submission.
///
/// The scheduler:
/// 1. Receives aircraft state updates from the telemetry listener
/// 2. Predicts which tiles should be pre-fetched using the TilePredictor
/// 3. Filters out tiles that are already in cache
/// 4. Submits prefetch requests to the DDS pipeline
pub struct PrefetchScheduler {
    /// Tile predictor engine.
    predictor: TilePredictor,
    /// DDS handler for submitting prefetch requests.
    dds_handler: DdsHandler,
    /// Cache for checking if tiles are already cached.
    cache: Arc<dyn Cache>,
    /// Scheduler configuration.
    config: SchedulerConfig,
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
    /// * `dds_handler` - Handler for submitting DDS requests to the pipeline
    /// * `cache` - Cache for checking if tiles are already cached
    /// * `config` - Scheduler configuration
    pub fn new(
        predictor: TilePredictor,
        dds_handler: DdsHandler,
        cache: Arc<dyn Cache>,
        config: SchedulerConfig,
    ) -> Self {
        let stats = Arc::new(PrefetchStats::default());
        Self {
            predictor,
            dds_handler,
            cache,
            config,
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

        // Update shared status with aircraft state
        if let Some(ref status) = self.shared_status {
            status.update_aircraft(state);
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
        let mut cached = 0;
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

            // Skip if already in flight
            if self.in_flight.contains(&tile_key) {
                in_flight_skipped += 1;
                continue;
            }

            // Check if already in cache
            let cache_key = CacheKey::new(&self.config.provider, self.config.dds_format, tile);

            if self.cache.contains(&cache_key) {
                cached += 1;
                trace!(
                    row = tile.row,
                    col = tile.col,
                    "Tile already cached, skipping"
                );
                continue;
            }

            // Submit prefetch request
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let request = DdsRequest {
                job_id: JobId::new(),
                tile,
                result_tx: tx,
                cancellation_token: CancellationToken::new(),
                is_prefetch: true,
            };

            trace!(
                row = tile.row,
                col = tile.col,
                priority = predicted.priority,
                "Submitting prefetch request"
            );

            (self.dds_handler)(request);
            self.in_flight.insert(tile_key);
            submitted += 1;
        }

        // Update statistics
        self.stats
            .tiles_cached
            .fetch_add(cached as u64, Ordering::Relaxed);
        self.stats
            .tiles_submitted
            .fetch_add(submitted as u64, Ordering::Relaxed);
        self.stats
            .tiles_in_flight_skipped
            .fetch_add(in_flight_skipped as u64, Ordering::Relaxed);

        // Log summary
        if submitted > 0 || cached > 0 {
            info!(
                submitted,
                cached,
                in_flight_skipped,
                total_in_flight = self.in_flight.len(),
                "Prefetch: submitted {} new tiles ({} already cached, {} in-flight)",
                submitted,
                cached,
                in_flight_skipped
            );
        }

        // Update shared status with stats
        if let Some(ref status) = self.shared_status {
            status.update_stats(self.stats.snapshot());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheStatistics, CacheStats, NoOpCache};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock cache that tracks contains calls and always returns false.
    struct MockCache {
        contains_count: AtomicUsize,
    }

    impl MockCache {
        fn new() -> Self {
            Self {
                contains_count: AtomicUsize::new(0),
            }
        }
    }

    impl Cache for MockCache {
        fn get(&self, _key: &CacheKey) -> Option<Vec<u8>> {
            None
        }

        fn put(&self, _key: CacheKey, _data: Vec<u8>) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }

        fn contains(&self, _key: &CacheKey) -> bool {
            self.contains_count.fetch_add(1, Ordering::SeqCst);
            false // Always return false to allow prefetch
        }

        fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }

        fn stats(&self) -> CacheStatistics {
            CacheStatistics::from_stats(&CacheStats::default())
        }

        fn provider(&self) -> &str {
            "mock"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// Mock cache that returns true for contains (already cached).
    struct AlwaysCachedMock;

    impl Cache for AlwaysCachedMock {
        fn get(&self, _key: &CacheKey) -> Option<Vec<u8>> {
            Some(vec![1, 2, 3])
        }

        fn put(&self, _key: CacheKey, _data: Vec<u8>) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }

        fn contains(&self, _key: &CacheKey) -> bool {
            true // Everything is cached
        }

        fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }

        fn stats(&self) -> CacheStatistics {
            CacheStatistics::from_stats(&CacheStats::default())
        }

        fn provider(&self) -> &str {
            "cached"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn create_test_handler() -> (DdsHandler, Arc<AtomicUsize>) {
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        let handler: DdsHandler = Arc::new(move |_request: DdsRequest| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        (handler, call_count)
    }

    #[test]
    fn test_scheduler_creation() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (handler, _) = create_test_handler();
        let cache = Arc::new(NoOpCache::new("test"));
        let config = SchedulerConfig::default();

        let scheduler = PrefetchScheduler::new(predictor, handler, cache, config);

        assert!(scheduler.in_flight.is_empty());
        assert!(scheduler.last_state.is_none());
    }

    #[test]
    fn test_process_state_submits_requests() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (handler, call_count) = create_test_handler();
        let cache = Arc::new(MockCache::new());
        let config = SchedulerConfig::default();

        let mut scheduler = PrefetchScheduler::new(predictor, handler, cache, config);

        // Create a moving aircraft state
        let state = AircraftState::new(45.0, -122.0, 90.0, 300.0, 10000.0);
        scheduler.process_state(&state);

        // Should have submitted some requests
        assert!(call_count.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn test_process_state_skips_cached_tiles() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (handler, call_count) = create_test_handler();
        let cache = Arc::new(AlwaysCachedMock);
        let config = SchedulerConfig::default();

        let mut scheduler = PrefetchScheduler::new(predictor, handler, cache, config);

        let state = AircraftState::new(45.0, -122.0, 90.0, 300.0, 10000.0);
        scheduler.process_state(&state);

        // Should not submit any requests since all tiles are "cached"
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_process_state_respects_max_batch() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (handler, call_count) = create_test_handler();
        let cache = Arc::new(MockCache::new());
        let config = SchedulerConfig::default();

        let mut scheduler = PrefetchScheduler::new(predictor, handler, cache, config);

        // Create a fast-moving aircraft that will predict many tiles
        let state = AircraftState::new(45.0, -122.0, 90.0, 500.0, 35000.0);
        scheduler.process_state(&state);

        // Should respect batch_size from config (default 500)
        assert!(call_count.load(Ordering::SeqCst) <= SchedulerConfig::default().batch_size);
    }

    #[test]
    fn test_in_flight_tracking() {
        let predictor = TilePredictor::new(45.0, 100.0, 60.0);
        let (handler, _) = create_test_handler();
        let cache = Arc::new(MockCache::new());
        let config = SchedulerConfig::default();

        let mut scheduler = PrefetchScheduler::new(predictor, handler, cache, config);

        let state = AircraftState::new(45.0, -122.0, 90.0, 300.0, 10000.0);
        scheduler.process_state(&state);

        // Should have some tiles in flight
        assert!(!scheduler.in_flight.is_empty());
    }

    #[test]
    fn test_scheduler_config_default() {
        let config = SchedulerConfig::default();

        assert_eq!(config.zoom, 14);
        assert_eq!(config.provider, "bing");
        assert_eq!(config.dds_format, DdsFormat::BC1);
    }
}
