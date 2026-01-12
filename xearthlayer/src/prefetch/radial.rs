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
use crate::executor::DdsClient;
use crate::executor::{JobId, MemoryCache};
use crate::fuse::{DdsHandler, DdsRequest};

use super::circuit_breaker::CircuitState;
use super::coordinates::{distance_to_tile, tile_size_nm_at_lat};
use super::state::{AircraftState, DetailedPrefetchStats, PrefetchMode, SharedPrefetchStatus};
use super::strategy::Prefetcher;
use super::throttler::{PrefetchThrottler, ThrottleState};

/// Default inner radius in nautical miles (inside this = X-Plane's preload zone).
const DEFAULT_INNER_RADIUS_NM: f32 = 85.0;

/// Default outer radius in nautical miles (prefetch out to this distance).
const DEFAULT_OUTER_RADIUS_NM: f32 = 120.0;

/// Default TTL for recently-attempted tiles (don't re-request for this long).
const DEFAULT_ATTEMPT_TTL: Duration = Duration::from_secs(60);

/// Timeout for prefetch requests (cancel if taking too long).
const PREFETCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum time between prefetch cycles.
const MIN_CYCLE_INTERVAL: Duration = Duration::from_secs(2);

/// Configuration for the radial prefetcher.
///
/// The prefetcher maintains an annular (ring-shaped) buffer zone around the
/// aircraft position. Tiles inside the inner radius are assumed to be in
/// X-Plane's preload zone and are not prefetched. Tiles between the inner
/// and outer radius are prefetched to create a buffer.
#[derive(Debug, Clone)]
pub struct RadialPrefetchConfig {
    /// Inner radius in nautical miles (default: 85nm).
    /// Tiles closer than this are in X-Plane's preload zone.
    pub inner_radius_nm: f32,
    /// Outer radius in nautical miles (default: 120nm).
    /// Tiles beyond this are too far ahead to prefetch.
    pub outer_radius_nm: f32,
    /// Zoom level for tile predictions (default: 14).
    pub zoom: u8,
    /// How long to wait before re-attempting a tile (default: 60s).
    pub attempt_ttl: Duration,
}

impl Default for RadialPrefetchConfig {
    fn default() -> Self {
        Self {
            inner_radius_nm: DEFAULT_INNER_RADIUS_NM,
            outer_radius_nm: DEFAULT_OUTER_RADIUS_NM,
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
    /// DDS handler for submitting prefetch requests (legacy).
    dds_handler: DdsHandler,
    /// DDS client for the new daemon architecture (preferred if set).
    dds_client: Option<Arc<dyn DdsClient>>,
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
    /// Throttler for pausing prefetch during high load.
    throttler: Option<Arc<dyn PrefetchThrottler>>,
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
            dds_client: None,
            config,
            recently_attempted: HashMap::new(),
            stats: Arc::new(RadialPrefetchStats::default()),
            shared_status: None,
            last_tile: None,
            throttler: None,
        }
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Set the throttler for pausing prefetch during high load.
    ///
    /// The throttler (typically a circuit breaker) monitors system load
    /// and pauses prefetching when X-Plane is actively loading scenery.
    pub fn with_throttler(mut self, throttler: Arc<dyn PrefetchThrottler>) -> Self {
        self.throttler = Some(throttler);
        self
    }

    /// Set the DDS client for the new daemon architecture.
    ///
    /// When set, the prefetcher will use the DdsClient for submitting
    /// prefetch requests instead of the legacy DdsHandler callback.
    /// This enables integration with the new channel-based daemon model.
    pub fn with_dds_client(mut self, client: Arc<dyn DdsClient>) -> Self {
        self.dds_client = Some(client);
        self
    }

    /// Get access to statistics for monitoring.
    pub fn stats(&self) -> Arc<RadialPrefetchStats> {
        Arc::clone(&self.stats)
    }

    /// Check if prefetching should be throttled.
    ///
    /// Returns true if prefetch should be paused.
    fn check_throttle(&self) -> bool {
        let Some(ref throttler) = self.throttler else {
            return false;
        };

        let should_throttle = throttler.should_throttle();

        // Update shared status for TUI display
        if let Some(ref status) = self.shared_status {
            if should_throttle {
                status.update_prefetch_mode(PrefetchMode::CircuitOpen);
            }
            // Update detailed stats with throttle state
            let circuit_state = match throttler.state() {
                ThrottleState::Active => Some(CircuitState::Closed),
                ThrottleState::Paused => Some(CircuitState::Open),
                ThrottleState::Resuming => Some(CircuitState::HalfOpen),
            };
            status.update_detailed_stats(DetailedPrefetchStats {
                cycles: self.stats.cycles.load(Ordering::Relaxed),
                tiles_submitted_last_cycle: 0,
                tiles_submitted_total: self.stats.tiles_submitted.load(Ordering::Relaxed),
                cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
                ttl_skipped: self.stats.ttl_skipped.load(Ordering::Relaxed),
                active_zoom_levels: vec![self.config.zoom],
                is_active: !should_throttle,
                circuit_state,
            });
        }

        should_throttle
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
            inner_nm = self.config.inner_radius_nm,
            outer_nm = self.config.outer_radius_nm,
            zoom = self.config.zoom,
            ttl_secs = self.config.attempt_ttl.as_secs(),
            "Radial prefetcher started (ring-based)"
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

                    // Check throttler (e.g., circuit breaker)
                    if self.check_throttle() {
                        trace!("Prefetch cycle skipped - throttled");
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

        // Calculate tiles in ring (annulus) around aircraft position
        let tiles = self.tiles_in_ring(state.latitude, state.longitude);
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

            trace!(
                row = tile.row,
                col = tile.col,
                zoom = tile.zoom,
                using_daemon = self.dds_client.is_some(),
                "Submitting prefetch request"
            );

            // Submit prefetch request via DdsClient or legacy DdsHandler
            if let Some(ref client) = self.dds_client {
                // New daemon architecture - fire-and-forget prefetch
                client.prefetch(tile);
            } else {
                // Legacy architecture - DdsHandler callback with timeout
                let cancellation_token = CancellationToken::new();
                let timeout_token = cancellation_token.clone();

                // Spawn timeout task to cancel the request if it takes too long
                tokio::spawn(async move {
                    tokio::time::sleep(PREFETCH_REQUEST_TIMEOUT).await;
                    timeout_token.cancel();
                });

                let (tx, _rx) = tokio::sync::oneshot::channel();
                let request = DdsRequest {
                    job_id: JobId::auto(),
                    tile,
                    result_tx: tx,
                    cancellation_token,
                    is_prefetch: true,
                };

                (self.dds_handler)(request);
            }
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

        // Log summary at debug level (high volume - every cycle)
        debug!(
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

            // Update detailed stats for TUI dashboard
            let circuit_state = self.throttler.as_ref().map(|t| match t.state() {
                ThrottleState::Active => CircuitState::Closed,
                ThrottleState::Paused => CircuitState::Open,
                ThrottleState::Resuming => CircuitState::HalfOpen,
            });
            status.update_detailed_stats(DetailedPrefetchStats {
                cycles: self.stats.cycles.load(Ordering::Relaxed),
                tiles_submitted_last_cycle: submitted,
                tiles_submitted_total: self.stats.tiles_submitted.load(Ordering::Relaxed),
                cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
                ttl_skipped: self.stats.ttl_skipped.load(Ordering::Relaxed),
                active_zoom_levels: vec![self.config.zoom],
                is_active: submitted > 0,
                circuit_state,
            });
        }
    }

    /// Calculate all tiles within the configured ring (annulus) around a position.
    ///
    /// This method finds all tiles whose centers are between inner_radius_nm
    /// and outer_radius_nm from the given position. The result is an annular
    /// (ring-shaped) set of tiles.
    ///
    /// # Arguments
    ///
    /// * `lat` - Aircraft latitude in degrees
    /// * `lon` - Aircraft longitude in degrees
    ///
    /// # Returns
    ///
    /// Vector of tiles in the ring, ordered by proximity to the inner edge.
    fn tiles_in_ring(&self, lat: f64, lon: f64) -> Vec<TileCoord> {
        let zoom = self.config.zoom;
        let inner_nm = self.config.inner_radius_nm;
        let outer_nm = self.config.outer_radius_nm;

        // Calculate tile size at this latitude to determine search grid
        let tile_size = tile_size_nm_at_lat(lat, zoom) as f32;

        // Search radius in tiles (add buffer for edge cases)
        let search_radius_tiles = ((outer_nm / tile_size) as i32) + 2;

        // Get center tile for the aircraft position
        let center = match to_tile_coords(lat, lon, zoom) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };

        let max_coord = 2i64.pow(zoom as u32);
        let mut tiles = Vec::new();

        // Scan a square grid and filter by distance
        for dr in -search_radius_tiles..=search_radius_tiles {
            for dc in -search_radius_tiles..=search_radius_tiles {
                let new_row = center.row as i64 + dr as i64;
                let new_col = center.col as i64 + dc as i64;

                // Validate coordinates
                if new_row < 0 || new_row >= max_coord || new_col < 0 || new_col >= max_coord {
                    continue;
                }

                let tile = TileCoord {
                    row: new_row as u32,
                    col: new_col as u32,
                    zoom,
                };

                // Calculate distance from aircraft to tile center
                let dist = distance_to_tile((lat, lon), &tile);

                // Keep tiles within the ring
                if dist >= inner_nm && dist <= outer_nm {
                    tiles.push(tile);
                }
            }
        }

        // Sort by distance from inner edge (closest to inner radius first)
        let position = (lat, lon);
        tiles.sort_by(|a, b| {
            let dist_a = distance_to_tile(position, a);
            let dist_b = distance_to_tile(position, b);
            dist_a
                .partial_cmp(&dist_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

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
        "Ring-based prefetching (nautical mile annulus)"
    }

    fn startup_info(&self) -> String {
        format!(
            "radial ring ({:.0}-{:.0}nm), zoom {}",
            self.config.inner_radius_nm, self.config.outer_radius_nm, self.config.zoom
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

    #[tokio::test]
    async fn test_ttl_prevents_resubmission() {
        // Use a small ring for faster testing
        let cache = Arc::new(MockMemoryCache::new());
        let (handler, call_count) = create_test_handler();
        let config = RadialPrefetchConfig {
            inner_radius_nm: 1.0,
            outer_radius_nm: 3.0,
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
        assert!((config.inner_radius_nm - 85.0).abs() < 0.01);
        assert!((config.outer_radius_nm - 120.0).abs() < 0.01);
        assert_eq!(config.zoom, 14);
        assert_eq!(config.attempt_ttl, Duration::from_secs(60));
    }

    // ==================== tiles_in_ring tests (TDD) ====================

    #[test]
    fn test_tiles_in_ring_excludes_inner() {
        // Ring-based prefetching: tiles inside the inner radius should be excluded
        // Using 85nm inner radius and 120nm outer radius (per plan)
        use super::super::coordinates::distance_to_tile;

        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig {
            inner_radius_nm: 85.0,
            outer_radius_nm: 120.0,
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        // Position at mid-latitude (Portland, OR area)
        let lat = 45.5;
        let lon = -122.7;
        let tiles = prefetcher.tiles_in_ring(lat, lon);

        // Verify no tile is closer than the inner radius
        for tile in &tiles {
            let dist = distance_to_tile((lat, lon), tile);
            assert!(
                dist >= 80.0, // Allow small tolerance for tile-center calculations
                "Tile at ({}, {}) is too close: {}nm < inner radius 85nm",
                tile.row,
                tile.col,
                dist
            );
        }
    }

    #[test]
    fn test_tiles_in_ring_includes_outer() {
        // Ring should include tiles out to the outer radius
        use super::super::coordinates::distance_to_tile;

        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig {
            inner_radius_nm: 85.0,
            outer_radius_nm: 120.0,
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        let lat = 45.5;
        let lon = -122.7;
        let tiles = prefetcher.tiles_in_ring(lat, lon);

        // Should have tiles (ring should not be empty for reasonable radii)
        assert!(!tiles.is_empty(), "Ring should contain tiles");

        // Find the furthest tile
        let max_dist = tiles
            .iter()
            .map(|t| distance_to_tile((lat, lon), t))
            .fold(0.0_f32, |a, b| a.max(b));

        // The furthest tile should be near the outer radius
        assert!(
            max_dist >= 100.0, // Should have tiles reasonably close to outer edge
            "Max tile distance {}nm is too short for 120nm outer radius",
            max_dist
        );
        assert!(
            max_dist <= 130.0, // But not too far beyond outer radius
            "Max tile distance {}nm exceeds 120nm outer radius by too much",
            max_dist
        );
    }

    #[test]
    fn test_tiles_in_ring_at_high_latitude() {
        // Tiles at high latitudes are smaller in the east-west direction
        // The ring should still work correctly despite Mercator distortion
        use super::super::coordinates::{distance_to_tile, tile_size_nm_at_lat};

        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig {
            inner_radius_nm: 85.0,
            outer_radius_nm: 120.0,
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        // High latitude position (Alaska)
        let lat = 61.0;
        let lon = -149.0;

        // Verify tile size is smaller at this latitude
        let tile_size = tile_size_nm_at_lat(lat, 14);
        let equator_tile_size = tile_size_nm_at_lat(0.0, 14);
        assert!(
            tile_size < equator_tile_size * 0.6,
            "Tile size at 61°N ({}) should be significantly smaller than at equator ({})",
            tile_size,
            equator_tile_size
        );

        let tiles = prefetcher.tiles_in_ring(lat, lon);

        // Should still produce a valid ring
        assert!(
            !tiles.is_empty(),
            "Ring should contain tiles at high latitude"
        );

        // Verify all tiles are within the expected distance bounds
        for tile in &tiles {
            let dist = distance_to_tile((lat, lon), tile);
            assert!(dist >= 80.0, "High-latitude tile too close: {}nm", dist);
            assert!(dist <= 130.0, "High-latitude tile too far: {}nm", dist);
        }
    }

    #[test]
    fn test_tiles_in_ring_is_annular() {
        // The ring should be annular (donut-shaped), not a filled circle
        // There should be a "hole" in the middle
        use super::super::coordinates::distance_to_tile;

        let cache = Arc::new(MockMemoryCache::new());
        let (handler, _) = create_test_handler();
        let config = RadialPrefetchConfig {
            inner_radius_nm: 85.0,
            outer_radius_nm: 120.0,
            zoom: 14,
            attempt_ttl: Duration::from_secs(60),
        };
        let prefetcher = RadialPrefetcher::new(cache, handler, config);

        let lat = 45.5;
        let lon = -122.7;
        let tiles = prefetcher.tiles_in_ring(lat, lon);

        // Find the minimum distance (should be close to inner radius)
        let min_dist = tiles
            .iter()
            .map(|t| distance_to_tile((lat, lon), t))
            .fold(f32::MAX, |a, b| a.min(b));

        // The minimum distance should be at or above the inner radius
        assert!(
            min_dist >= 80.0,
            "Min tile distance {}nm should be >= inner radius 85nm",
            min_dist
        );
    }
}
