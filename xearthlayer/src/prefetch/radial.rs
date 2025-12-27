//! Simplified radial prefetcher.
//!
//! This module provides a naive but effective prefetching strategy:
//! - Maintains a buffer of cached tiles around the current aircraft position
//! - Only fetches tiles that aren't already in the memory cache
//! - Uses TTL tracking to avoid re-requesting recently-attempted tiles
//!
//! The radial prefetcher is simpler than flight-path prediction and provides
//! consistent performance regardless of aircraft heading or speed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace};

use crate::coord::{to_tile_coords, TileCoord};
use crate::fuse::{DdsHandler, DdsRequest};
use crate::pipeline::{JobId, MemoryCache};

use super::state::{AircraftState, PrefetchMode, SharedPrefetchStatus};
use super::strategy::Prefetcher;

/// Default radius in tiles around current position.
const DEFAULT_RADIUS: u8 = 3;

/// Default TTL for recently-attempted tiles (don't re-request for this long).
const DEFAULT_ATTEMPT_TTL: Duration = Duration::from_secs(60);

/// Timeout for prefetch requests (cancel if taking too long).
const PREFETCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum time between prefetch cycles.
const MIN_CYCLE_INTERVAL: Duration = Duration::from_secs(2);

/// Configuration for the radial prefetcher.
#[derive(Debug, Clone)]
pub struct RadialPrefetchConfig {
    /// Radius in tiles around current position (default: 3 = 7×7 grid = 49 tiles).
    pub radius: u8,
    /// Zoom level for tile predictions (default: 14).
    pub zoom: u8,
    /// How long to wait before re-attempting a tile (default: 60s).
    pub attempt_ttl: Duration,
}

impl Default for RadialPrefetchConfig {
    fn default() -> Self {
        Self {
            radius: DEFAULT_RADIUS,
            zoom: 14,
            attempt_ttl: DEFAULT_ATTEMPT_TTL,
        }
    }
}

/// Statistics for monitoring prefetch activity.
#[derive(Debug, Default)]
pub struct RadialPrefetchStats {
    /// Tiles found in cache (no request needed).
    pub cache_hits: AtomicU64,
    /// Tiles submitted for prefetch.
    pub tiles_submitted: AtomicU64,
    /// Tiles skipped due to recent attempt.
    pub ttl_skipped: AtomicU64,
    /// Total prefetch cycles run.
    pub cycles: AtomicU64,
}

impl RadialPrefetchStats {
    /// Get a snapshot of current statistics.
    pub fn snapshot(&self) -> RadialPrefetchStatsSnapshot {
        RadialPrefetchStatsSnapshot {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            tiles_submitted: self.tiles_submitted.load(Ordering::Relaxed),
            ttl_skipped: self.ttl_skipped.load(Ordering::Relaxed),
            cycles: self.cycles.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of prefetch statistics.
#[derive(Debug, Clone, Default)]
pub struct RadialPrefetchStatsSnapshot {
    pub cache_hits: u64,
    pub tiles_submitted: u64,
    pub ttl_skipped: u64,
    pub cycles: u64,
}

/// Simplified radial prefetcher.
///
/// Maintains a buffer of cached tiles around the aircraft's current position.
/// Uses a simple strategy:
///
/// 1. Convert aircraft position to tile coordinates
/// 2. Calculate all tiles within the configured radius
/// 3. Check memory cache for each tile
/// 4. Submit prefetch requests only for missing tiles
/// 5. Track recently-attempted tiles to avoid hammering failed requests
///
/// This approach is simpler than flight-path prediction and works well for
/// all flight profiles (cruise, pattern work, orbits, etc.).
pub struct RadialPrefetcher<M: MemoryCache> {
    /// Memory cache for checking what's already cached.
    memory_cache: Arc<M>,
    /// DDS handler for submitting prefetch requests.
    dds_handler: DdsHandler,
    /// Configuration.
    config: RadialPrefetchConfig,
    /// Recently-attempted tiles with timestamp (for TTL tracking).
    recently_attempted: HashMap<(u32, u32, u8), Instant>,
    /// Statistics for monitoring.
    stats: Arc<RadialPrefetchStats>,
    /// Optional shared status for TUI display.
    shared_status: Option<Arc<SharedPrefetchStatus>>,
    /// Last position used for prefetch (to detect movement).
    last_tile: Option<TileCoord>,
}

impl<M: MemoryCache> RadialPrefetcher<M> {
    /// Create a new radial prefetcher.
    ///
    /// # Arguments
    ///
    /// * `memory_cache` - The memory cache to check for existing tiles
    /// * `dds_handler` - Handler for submitting prefetch requests
    /// * `config` - Prefetch configuration
    pub fn new(
        memory_cache: Arc<M>,
        dds_handler: DdsHandler,
        config: RadialPrefetchConfig,
    ) -> Self {
        Self {
            memory_cache,
            dds_handler,
            config,
            recently_attempted: HashMap::new(),
            stats: Arc::new(RadialPrefetchStats::default()),
            shared_status: None,
            last_tile: None,
        }
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Get access to statistics for monitoring.
    pub fn stats(&self) -> Arc<RadialPrefetchStats> {
        Arc::clone(&self.stats)
    }

    /// Run the prefetcher, processing state updates from the channel.
    ///
    /// This method runs until the channel is closed or the cancellation token
    /// is triggered.
    pub async fn run(
        mut self,
        mut state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) {
        info!(
            radius = self.config.radius,
            zoom = self.config.zoom,
            ttl_secs = self.config.attempt_ttl.as_secs(),
            "Radial prefetcher started"
        );

        // Initialize last_cycle in the past so first cycle can run immediately
        let mut last_cycle = Instant::now() - MIN_CYCLE_INTERVAL;

        loop {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    info!("Radial prefetcher shutting down");
                    break;
                }

                Some(state) = state_rx.recv() => {
                    // Update shared status with aircraft position
                    if let Some(ref status) = self.shared_status {
                        status.update_aircraft(&state);
                    }

                    // Rate limit prefetch cycles
                    if last_cycle.elapsed() < MIN_CYCLE_INTERVAL {
                        trace!("Skipping prefetch cycle - rate limited");
                        continue;
                    }

                    // Run a prefetch cycle
                    self.prefetch_cycle(&state).await;
                    last_cycle = Instant::now();
                }
            }
        }
    }

    /// Run a single prefetch cycle for the given aircraft state.
    async fn prefetch_cycle(&mut self, state: &AircraftState) {
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);

        // Convert position to tile coordinates
        let current_tile = match to_tile_coords(state.latitude, state.longitude, self.config.zoom) {
            Ok(tile) => tile,
            Err(e) => {
                debug!(error = %e, "Invalid aircraft position for tile conversion");
                return;
            }
        };

        // Check if we've moved to a new tile
        let moved = self
            .last_tile
            .map(|last| last.row != current_tile.row || last.col != current_tile.col)
            .unwrap_or(true);

        if !moved {
            trace!("Aircraft hasn't moved to new tile, skipping cycle");
            return;
        }

        self.last_tile = Some(current_tile);

        // Clean up expired TTL entries
        self.cleanup_expired_attempts();

        // Calculate tiles in radius
        let tiles = self.tiles_in_radius(&current_tile);
        let total_tiles = tiles.len();

        let mut cache_hits = 0u64;
        let mut submitted = 0u64;
        let mut ttl_skipped = 0u64;

        for tile in tiles {
            let tile_key = (tile.row, tile.col, tile.zoom);

            // Check if in memory cache
            if self
                .memory_cache
                .get(tile.row, tile.col, tile.zoom)
                .await
                .is_some()
            {
                cache_hits += 1;
                continue;
            }

            // Check if recently attempted (TTL)
            if let Some(attempt_time) = self.recently_attempted.get(&tile_key) {
                if attempt_time.elapsed() < self.config.attempt_ttl {
                    ttl_skipped += 1;
                    continue;
                }
            }

            // Submit prefetch request with timeout
            let cancellation_token = CancellationToken::new();
            let timeout_token = cancellation_token.clone();

            // Spawn timeout task to cancel the request if it takes too long
            tokio::spawn(async move {
                tokio::time::sleep(PREFETCH_REQUEST_TIMEOUT).await;
                timeout_token.cancel();
            });

            let (tx, _rx) = tokio::sync::oneshot::channel();
            let request = DdsRequest {
                job_id: JobId::new(),
                tile,
                result_tx: tx,
                cancellation_token,
                is_prefetch: true,
            };

            trace!(
                row = tile.row,
                col = tile.col,
                zoom = tile.zoom,
                "Submitting prefetch request"
            );

            (self.dds_handler)(request);
            self.recently_attempted.insert(tile_key, Instant::now());
            submitted += 1;
        }

        // Update statistics
        self.stats
            .cache_hits
            .fetch_add(cache_hits, Ordering::Relaxed);
        self.stats
            .tiles_submitted
            .fetch_add(submitted, Ordering::Relaxed);
        self.stats
            .ttl_skipped
            .fetch_add(ttl_skipped, Ordering::Relaxed);

        // Log summary
        info!(
            lat = format!("{:.4}", state.latitude),
            lon = format!("{:.4}", state.longitude),
            tile_row = current_tile.row,
            tile_col = current_tile.col,
            total = total_tiles,
            cache_hits,
            submitted,
            ttl_skipped,
            "Prefetch cycle complete"
        );

        // Update shared status
        if let Some(ref status) = self.shared_status {
            // Convert to the expected stats format
            let stats = super::scheduler::PrefetchStatsSnapshot {
                tiles_predicted: total_tiles as u64,
                tiles_submitted: submitted,
                tiles_in_flight_skipped: ttl_skipped,
                prediction_cycles: self.stats.cycles.load(Ordering::Relaxed),
            };
            status.update_stats(stats);
            status.update_prefetch_mode(PrefetchMode::Radial);
        }
    }

    /// Calculate all tiles within the configured radius of the center tile.
    fn tiles_in_radius(&self, center: &TileCoord) -> Vec<TileCoord> {
        let radius = self.config.radius as i32;
        let max_coord = 2i64.pow(center.zoom as u32);
        let mut tiles = Vec::with_capacity(((radius * 2 + 1) * (radius * 2 + 1)) as usize);

        for dr in -radius..=radius {
            for dc in -radius..=radius {
                let new_row = center.row as i64 + dr as i64;
                let new_col = center.col as i64 + dc as i64;

                // Validate coordinates
                if new_row >= 0 && new_row < max_coord && new_col >= 0 && new_col < max_coord {
                    tiles.push(TileCoord {
                        row: new_row as u32,
                        col: new_col as u32,
                        zoom: center.zoom,
                    });
                }
            }
        }

        tiles
    }

    /// Clean up TTL entries that have expired.
    fn cleanup_expired_attempts(&mut self) {
        let ttl = self.config.attempt_ttl;
        self.recently_attempted
            .retain(|_, time| time.elapsed() < ttl);
    }
}

// Implement the Prefetcher trait for RadialPrefetcher
impl<M: MemoryCache + 'static> Prefetcher for RadialPrefetcher<M> {
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
        "radial"
    }

    fn description(&self) -> &'static str {
        "Simple cache-aware radial expansion around current position"
    }

    fn startup_info(&self) -> String {
        format!(
            "radial, {}-tile radius, zoom {}",
            self.config.radius, self.config.zoom
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    /// Mock memory cache for testing.
    struct MockMemoryCache {
        data: Mutex<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockMemoryCache {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }

        fn with_cached(self, row: u32, col: u32, zoom: u8) -> Self {
            // Use blocking lock since this is called from sync context in tests
            futures::executor::block_on(async {
                self.data
                    .lock()
                    .await
                    .insert((row, col, zoom), vec![0xDD, 0x53]);
            });
            self
        }
    }

    impl MemoryCache for MockMemoryCache {
        async fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
            self.data.lock().await.get(&(row, col, zoom)).cloned()
        }

        async fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
            self.data.lock().await.insert((row, col, zoom), data);
        }

        fn size_bytes(&self) -> usize {
            0
        }

        fn entry_count(&self) -> usize {
            0
        }
    }

    fn create_test_handler() -> (DdsHandler, Arc<std::sync::atomic::AtomicUsize>) {
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        let handler: DdsHandler = Arc::new(move |_request: DdsRequest| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        (handler, call_count)
    }

    #[test]
    fn test_tiles_in_radius() {
        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig::default();
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        let center = TileCoord {
            row: 100,
            col: 100,
            zoom: 14,
        };

        let tiles = prefetcher.tiles_in_radius(&center);

        // 3-tile radius = 7×7 = 49 tiles
        assert_eq!(tiles.len(), 49);

        // Center tile should be included
        assert!(tiles.iter().any(|t| t.row == 100 && t.col == 100));

        // Corner tiles should be included
        assert!(tiles.iter().any(|t| t.row == 97 && t.col == 97));
        assert!(tiles.iter().any(|t| t.row == 97 && t.col == 103));
        assert!(tiles.iter().any(|t| t.row == 103 && t.col == 97));
        assert!(tiles.iter().any(|t| t.row == 103 && t.col == 103));
    }

    #[test]
    fn test_tiles_in_radius_at_edge() {
        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig::default();
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        // Near the edge of the map
        let center = TileCoord {
            row: 1,
            col: 1,
            zoom: 14,
        };

        let tiles = prefetcher.tiles_in_radius(&center);

        // Should have fewer tiles (some would be negative)
        assert!(tiles.len() < 49);
        assert!(!tiles.is_empty());

        // All tiles should have valid coordinates
        for tile in &tiles {
            assert!(tile.row < 2u32.pow(14));
            assert!(tile.col < 2u32.pow(14));
        }
    }

    #[tokio::test]
    async fn test_prefetch_skips_cached_tiles() {
        // Calculate the actual tile coordinates for (45.0, -122.0) at zoom 14
        let actual_tile = to_tile_coords(45.0, -122.0, 14).unwrap();

        // Pre-cache the center tile
        let cache =
            Arc::new(MockMemoryCache::new().with_cached(actual_tile.row, actual_tile.col, 14));
        let (handler, call_count) = create_test_handler();
        let config = RadialPrefetchConfig {
            radius: 1, // 3×3 = 9 tiles
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };

        let mut prefetcher = RadialPrefetcher::new(cache, handler, config);

        // Aircraft at position that maps to the cached tile
        let state = AircraftState::new(45.0, -122.0, 90.0, 150.0, 10000.0);

        // Force last_tile to be different so cycle runs
        prefetcher.last_tile = Some(TileCoord {
            row: 0,
            col: 0,
            zoom: 14,
        });

        prefetcher.prefetch_cycle(&state).await;

        // Should have submitted less than 9 requests (center tile was cached)
        let submitted = call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(submitted < 9, "Expected < 9 submissions, got {}", submitted);

        // Stats should show at least one cache hit (the center tile)
        assert!(prefetcher.stats.cache_hits.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn test_ttl_prevents_resubmission() {
        let cache = Arc::new(MockMemoryCache::new());
        let (handler, call_count) = create_test_handler();
        let config = RadialPrefetchConfig {
            radius: 1,
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };

        let mut prefetcher = RadialPrefetcher::new(cache, handler, config);
        let state = AircraftState::new(45.0, -122.0, 90.0, 150.0, 10000.0);

        // First cycle - should submit requests
        prefetcher.last_tile = Some(TileCoord {
            row: 0,
            col: 0,
            zoom: 14,
        });
        prefetcher.prefetch_cycle(&state).await;
        let first_count = call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(first_count > 0);

        // Second cycle at same position - should skip (no tile movement)
        prefetcher.prefetch_cycle(&state).await;
        let second_count = call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            first_count, second_count,
            "Should skip when aircraft hasn't moved"
        );

        // Force movement to adjacent tile
        prefetcher.last_tile = Some(TileCoord {
            row: 0,
            col: 0,
            zoom: 14,
        });

        // Third cycle - tiles should be TTL-skipped (same tiles, recently attempted)
        prefetcher.prefetch_cycle(&state).await;
        let ttl_skipped = prefetcher.stats.ttl_skipped.load(Ordering::Relaxed);
        assert!(ttl_skipped > 0, "Should have TTL-skipped some tiles");
    }

    #[test]
    fn test_config_default() {
        let config = RadialPrefetchConfig::default();
        assert_eq!(config.radius, 3);
        assert_eq!(config.zoom, 14);
        assert_eq!(config.attempt_ttl, Duration::from_secs(60));
    }
}
