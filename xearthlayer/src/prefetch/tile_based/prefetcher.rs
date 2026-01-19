//! Tile-based prefetcher implementation.
//!
//! This prefetcher exploits X-Plane's loading behavior: it loads entire 1° DSF
//! tiles in bursts, then goes quiet for 1-2+ minutes. During quiet periods,
//! this prefetcher predicts and preloads the next row of DSF tiles.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────┐    ┌──────────────────┐    ┌──────────────┐
//! │   FUSE     │───▶│  BurstTracker    │    │  Telemetry   │
//! │  (events)  │    │  (loaded tiles)  │    │  (heading)   │
//! └────────────┘    └────────┬─────────┘    └──────┬───────┘
//!                            │                      │
//!                            ▼                      ▼
//!                   ┌──────────────────────────────────────┐
//!                   │         TileBasedPrefetcher          │
//!                   │                                      │
//!                   │  ┌─────────────┐  ┌───────────────┐  │
//!                   │  │ CircuitBrkr │  │ TilePredictor │  │
//!                   │  │ (quiet det) │  │ (next tiles)  │  │
//!                   │  └─────────────┘  └───────────────┘  │
//!                   └──────────────────────────────────────┘
//!                                      │
//!                                      ▼
//!                              ┌───────────────┐
//!                              │   DdsClient   │
//!                              │ (prefetch req)│
//!                              └───────────────┘
//! ```

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::coord::TileCoord;
use crate::executor::{DdsClient, MemoryCache};
use crate::ortho_union::OrthoUnionIndex;

use super::super::state::AircraftState;
use super::super::strategy::Prefetcher;
use super::super::throttler::PrefetchThrottler;
use super::{DdsAccessEvent, DsfTileCoord, TileBurstTracker, TilePredictor};

/// Configuration for the tile-based prefetcher.
#[derive(Debug, Clone)]
pub struct TileBasedConfig {
    /// Number of tile rows to prefetch ahead on each forward edge.
    ///
    /// Higher values prefetch more aggressively but use more bandwidth.
    /// Default: 1 (prefetch the immediate next row)
    pub rows_ahead: u32,

    /// Interval between circuit breaker checks in the main loop.
    ///
    /// Lower values are more responsive but use more CPU.
    /// Default: 500ms
    pub check_interval: Duration,

    /// Maximum DDS tiles to prefetch per DSF tile.
    ///
    /// Limits prefetch for very large DSF tiles to avoid overwhelming
    /// the network. Default: 500
    pub max_tiles_per_dsf: usize,

    /// TTL for failed tile attempts - don't re-request for this duration.
    ///
    /// Prevents hammering failed tiles. Default: 60s
    pub attempt_ttl: Duration,

    /// Minimum quiet period before prefetching starts.
    ///
    /// This adds a buffer after the circuit breaker closes to ensure
    /// X-Plane has truly finished loading. Default: 1s
    pub quiet_buffer: Duration,
}

impl Default for TileBasedConfig {
    fn default() -> Self {
        Self {
            rows_ahead: 1,
            check_interval: Duration::from_millis(500),
            max_tiles_per_dsf: 500,
            attempt_ttl: Duration::from_secs(60),
            quiet_buffer: Duration::from_secs(1),
        }
    }
}

/// Tile-based prefetcher that aligns with X-Plane's DSF loading behavior.
///
/// Unlike radial or heading-aware prefetchers that operate on individual tiles,
/// this prefetcher works at the DSF tile level (1° × 1° regions), matching
/// X-Plane 12's actual loading patterns.
///
/// # Strategy
///
/// 1. Track which DSF tiles X-Plane is loading via FUSE events
/// 2. Wait for quiet period (circuit breaker closes)
/// 3. Predict next DSF tiles based on heading
/// 4. Prefetch all DDS textures in predicted DSF tiles
/// 5. Clear tracker and repeat
pub struct TileBasedPrefetcher<M: MemoryCache> {
    /// Union index for enumerating DDS files in DSF tiles.
    ortho_index: Arc<OrthoUnionIndex>,

    /// Client for submitting prefetch requests.
    dds_client: Arc<dyn DdsClient>,

    /// Memory cache for checking what's already cached.
    memory_cache: Arc<M>,

    /// Receiver for DDS access events from FUSE.
    access_rx: mpsc::UnboundedReceiver<DdsAccessEvent>,

    /// Circuit breaker for quiet period detection.
    circuit_breaker: Arc<dyn PrefetchThrottler>,

    /// Tracks DSF tiles in current burst.
    burst_tracker: TileBurstTracker,

    /// Predicts next tiles based on heading.
    predictor: TilePredictor,

    /// Current aircraft heading from telemetry.
    current_heading: Option<f32>,

    /// Recently-attempted tiles (for TTL tracking).
    recently_attempted: HashSet<TileCoord>,

    /// Timestamp of last attempt tracking cleanup.
    last_cleanup: Instant,

    /// Configuration.
    config: TileBasedConfig,

    /// When quiet period started (for quiet_buffer).
    quiet_start: Option<Instant>,
}

impl<M: MemoryCache> TileBasedPrefetcher<M> {
    /// Create a new tile-based prefetcher.
    ///
    /// # Arguments
    ///
    /// * `ortho_index` - Index for enumerating DDS files in DSF tiles
    /// * `dds_client` - Client for submitting prefetch requests
    /// * `memory_cache` - Cache for checking what's already prefetched
    /// * `access_rx` - Channel receiving DDS access events from FUSE
    /// * `circuit_breaker` - For detecting quiet periods
    /// * `config` - Prefetcher configuration
    pub fn new(
        ortho_index: Arc<OrthoUnionIndex>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        access_rx: mpsc::UnboundedReceiver<DdsAccessEvent>,
        circuit_breaker: Arc<dyn PrefetchThrottler>,
        config: TileBasedConfig,
    ) -> Self {
        Self {
            ortho_index,
            dds_client,
            memory_cache,
            access_rx,
            circuit_breaker,
            burst_tracker: TileBurstTracker::new(),
            predictor: TilePredictor::with_rows_ahead(config.rows_ahead),
            current_heading: None,
            recently_attempted: HashSet::new(),
            last_cleanup: Instant::now(),
            config,
            quiet_start: None,
        }
    }

    /// Run the prefetcher main loop.
    ///
    /// Processes events from:
    /// - FUSE (DDS access events)
    /// - Telemetry (aircraft state updates)
    /// - Timer (circuit breaker checks)
    pub async fn run(
        mut self,
        mut state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) {
        info!(
            rows_ahead = self.config.rows_ahead,
            max_tiles_per_dsf = self.config.max_tiles_per_dsf,
            check_interval_ms = self.config.check_interval.as_millis(),
            "Tile-based prefetcher started"
        );

        let mut check_interval = tokio::time::interval(self.config.check_interval);
        check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;

                // Shutdown signal
                _ = cancellation_token.cancelled() => {
                    info!("Tile-based prefetcher shutting down");
                    break;
                }

                // Process DDS access events from FUSE
                Some(event) = self.access_rx.recv() => {
                    self.handle_access_event(event);
                }

                // Process telemetry updates (for heading)
                Some(state) = state_rx.recv() => {
                    self.handle_telemetry_update(&state);
                }

                // Check for quiet period and run prefetch cycle
                _ = check_interval.tick() => {
                    self.check_and_prefetch().await;
                }
            }
        }
    }

    /// Handle a DDS access event from FUSE.
    fn handle_access_event(&mut self, event: DdsAccessEvent) {
        self.burst_tracker.record_access(event.dsf_tile);

        // Reset quiet period tracking - X-Plane is loading
        self.quiet_start = None;

        trace!(
            dsf_tile = %event.dsf_tile,
            tiles_in_burst = self.burst_tracker.tile_count(),
            "DDS access recorded"
        );
    }

    /// Handle a telemetry update.
    fn handle_telemetry_update(&mut self, state: &AircraftState) {
        // Update heading for prediction
        self.current_heading = Some(state.heading);

        trace!(
            lat = format!("{:.4}", state.latitude),
            lon = format!("{:.4}", state.longitude),
            heading = format!("{:.1}", state.heading),
            "Telemetry update received"
        );
    }

    /// Check for quiet period and run prefetch cycle if appropriate.
    async fn check_and_prefetch(&mut self) {
        // Periodic cleanup of recently attempted set
        if self.last_cleanup.elapsed() > self.config.attempt_ttl {
            self.recently_attempted.clear();
            self.last_cleanup = Instant::now();
            trace!("Cleared recently attempted tiles (TTL cleanup)");
        }

        // Check circuit breaker
        let is_throttled = self.circuit_breaker.should_throttle();

        if is_throttled {
            // X-Plane is actively loading - reset quiet tracking
            self.quiet_start = None;
            return;
        }

        // Circuit is closed - X-Plane stopped loading
        // Track when quiet period started
        let now = Instant::now();
        let quiet_start = *self.quiet_start.get_or_insert(now);

        // Wait for quiet buffer before prefetching
        if now.duration_since(quiet_start) < self.config.quiet_buffer {
            trace!(
                elapsed_ms = now.duration_since(quiet_start).as_millis(),
                buffer_ms = self.config.quiet_buffer.as_millis(),
                "Waiting for quiet buffer"
            );
            return;
        }

        // Only prefetch if we have tiles to predict from
        if !self.burst_tracker.has_tiles() {
            return;
        }

        // Run prefetch cycle
        self.run_prefetch_cycle().await;
    }

    /// Run a prefetch cycle - predict and prefetch next tiles.
    async fn run_prefetch_cycle(&mut self) {
        let loaded_tiles = self.burst_tracker.current_tiles().clone();
        let heading = self.current_heading;

        debug!(
            loaded_count = loaded_tiles.len(),
            heading = heading
                .map(|h| format!("{:.1}°", h))
                .unwrap_or_else(|| "none".to_string()),
            "Starting prefetch cycle"
        );

        // Predict next DSF tiles
        let next_tiles = self.predictor.predict_next_tiles(&loaded_tiles, heading);

        if next_tiles.is_empty() {
            debug!("No tiles predicted for prefetch");
            self.burst_tracker.clear();
            return;
        }

        debug!(
            predicted_count = next_tiles.len(),
            tiles = ?next_tiles.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "Predicted DSF tiles for prefetch"
        );

        // Prefetch each predicted DSF tile
        let mut total_submitted = 0usize;
        for dsf_tile in &next_tiles {
            // Check if circuit opened during prefetch
            if self.circuit_breaker.should_throttle() {
                debug!("Circuit opened during prefetch, stopping");
                break;
            }

            let submitted = self.prefetch_dsf_tile(dsf_tile).await;
            total_submitted += submitted;
        }

        info!(
            dsf_tiles = next_tiles.len(),
            dds_submitted = total_submitted,
            "Prefetch cycle complete"
        );

        // Clear tracker for next burst
        self.burst_tracker.clear();
        self.quiet_start = None;
    }

    /// Prefetch all DDS tiles within a DSF tile.
    ///
    /// Enumerates DDS files in the tile, checks cache, and submits
    /// prefetch requests for missing tiles.
    ///
    /// # Returns
    ///
    /// Number of tiles submitted for prefetch.
    async fn prefetch_dsf_tile(&mut self, dsf_tile: &DsfTileCoord) -> usize {
        // Get DDS files in this DSF tile from the ortho index
        let dds_tiles = self.enumerate_dds_tiles(dsf_tile);

        if dds_tiles.is_empty() {
            trace!(dsf_tile = %dsf_tile, "No DDS tiles found in DSF tile");
            return 0;
        }

        let mut submitted = 0;

        for tile in dds_tiles {
            // Skip if already in memory cache
            if self
                .memory_cache
                .get(tile.row, tile.col, tile.zoom)
                .await
                .is_some()
            {
                continue;
            }

            // Skip if recently attempted
            if self.recently_attempted.contains(&tile) {
                continue;
            }

            // Submit prefetch request
            self.dds_client.prefetch(tile);
            self.recently_attempted.insert(tile);
            submitted += 1;

            // Limit tiles per DSF to avoid overwhelming network
            if submitted >= self.config.max_tiles_per_dsf {
                warn!(
                    dsf_tile = %dsf_tile,
                    limit = self.config.max_tiles_per_dsf,
                    "Hit max tiles per DSF limit"
                );
                break;
            }
        }

        trace!(
            dsf_tile = %dsf_tile,
            submitted = submitted,
            "Prefetched DSF tile"
        );

        submitted
    }

    /// Enumerate DDS tiles within a DSF tile.
    ///
    /// Uses the OrthoUnionIndex's `dds_files_in_dsf_tile()` method to find
    /// all DDS textures within the 1° × 1° DSF tile boundary.
    fn enumerate_dds_tiles(&self, dsf_tile: &DsfTileCoord) -> Vec<TileCoord> {
        // Use the optimized index method
        let dds_files = self.ortho_index.dds_files_in_dsf_tile(dsf_tile);

        // Convert (virtual_path, real_path) to TileCoord
        dds_files
            .iter()
            .filter_map(|(virtual_path, _real_path)| {
                // Get filename from virtual path (e.g., "textures/100000_125184_BI18.dds")
                let filename = virtual_path.file_name()?.to_str()?;
                Self::parse_dds_filename_to_tile(filename)
            })
            .collect()
    }

    /// Parse a DDS filename to get the tile coordinates.
    ///
    /// # Arguments
    ///
    /// * `filename` - Filename like "100000_125184_BI18.dds"
    ///
    /// # Returns
    ///
    /// Tile coordinates for prefetching, or None if parsing fails.
    fn parse_dds_filename_to_tile(filename: &str) -> Option<TileCoord> {
        // Remove .dds extension
        let stem = filename.strip_suffix(".dds")?;

        // Split by underscore: row_col_typeZoom (e.g., "100000_125184_BI18")
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() != 3 {
            return None;
        }

        // Parse row and column (these are DDS tile coordinates)
        let row: u32 = parts[0].parse().ok()?;
        let col: u32 = parts[1].parse().ok()?;

        // Extract zoom from the third part (e.g., "BI16" -> 16, "GO218" -> 18)
        let zoom_part = parts[2];
        let zoom: u8 = zoom_part
            .chars()
            .skip_while(|c| c.is_alphabetic() || *c == '2') // Skip "BI", "GO2", "GO"
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;

        Some(TileCoord { row, col, zoom })
    }
}

/// Calculate the center lat/lon of a tile (test helper).
#[cfg(test)]
fn tile_center(tile: TileCoord) -> (f64, f64) {
    let n = 2.0_f64.powi(tile.zoom as i32);

    // Add 0.5 for center of tile
    let lon = ((tile.col as f64 + 0.5) / n) * 360.0 - 180.0;

    // Web Mercator latitude
    let lat_rad = std::f64::consts::PI * (1.0 - 2.0 * (tile.row as f64 + 0.5) / n);
    let lat = lat_rad.sinh().atan().to_degrees();

    (lat, lon)
}

// Implement the Prefetcher trait
impl<M: MemoryCache + 'static> Prefetcher for TileBasedPrefetcher<M> {
    fn run(
        self: Box<Self>,
        state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move { (*self).run(state_rx, cancellation_token).await })
    }

    fn name(&self) -> &'static str {
        "tile-based"
    }

    fn description(&self) -> &'static str {
        "DSF tile-based prefetching (1° tiles, burst-aware)"
    }

    fn startup_info(&self) -> String {
        let heading_status = if self.current_heading.is_some() {
            "telemetry"
        } else {
            "all-edges"
        };
        format!(
            "tile-based ({} rows ahead, {} mode)",
            self.config.rows_ahead, heading_status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{DdsClientError, Priority};
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    /// Mock DDS client for testing.
    struct MockDdsClient {
        prefetch_count: AtomicUsize,
    }

    impl MockDdsClient {
        fn new() -> Self {
            Self {
                prefetch_count: AtomicUsize::new(0),
            }
        }

        #[allow(dead_code)]
        fn prefetch_count(&self) -> usize {
            self.prefetch_count.load(Ordering::SeqCst)
        }
    }

    impl DdsClient for MockDdsClient {
        fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
            Ok(())
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (_, rx) = oneshot::channel();
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

        fn prefetch(&self, _tile: TileCoord) {
            self.prefetch_count.fetch_add(1, Ordering::SeqCst);
        }

        fn prefetch_with_cancellation(&self, tile: TileCoord, _cancellation: CancellationToken) {
            self.prefetch(tile);
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    /// Mock memory cache.
    struct MockCache;

    impl MemoryCache for MockCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl Future<Output = Option<Vec<u8>>> + Send {
            async { None } // Always cache miss for testing
        }

        fn put(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
            _data: Vec<u8>,
        ) -> impl Future<Output = ()> + Send {
            async {}
        }

        fn size_bytes(&self) -> usize {
            0
        }

        fn entry_count(&self) -> usize {
            0
        }
    }

    /// Mock throttler that always returns "not throttled" (quiet period).
    struct NeverThrottle;

    impl PrefetchThrottler for NeverThrottle {
        fn should_throttle(&self) -> bool {
            false
        }

        fn state(&self) -> super::super::super::throttler::ThrottleState {
            super::super::super::throttler::ThrottleState::Active
        }
    }

    #[test]
    fn test_config_default() {
        let config = TileBasedConfig::default();
        assert_eq!(config.rows_ahead, 1);
        assert_eq!(config.max_tiles_per_dsf, 500);
    }

    #[test]
    fn test_tile_center_calculation() {
        // Tile at zoom 14, roughly center of the world
        let tile = TileCoord {
            row: 8192,
            col: 8192,
            zoom: 14,
        };
        let (lat, lon) = tile_center(tile);

        // Should be near 0,0
        assert!(lat.abs() < 1.0, "Latitude should be near 0: {}", lat);
        assert!(lon.abs() < 1.0, "Longitude should be near 0: {}", lon);
    }

    #[test]
    fn test_prefetcher_name() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let index = Arc::new(OrthoUnionIndex::default());
        let client: Arc<dyn DdsClient> = Arc::new(MockDdsClient::new());
        let cache = Arc::new(MockCache);
        let throttler: Arc<dyn PrefetchThrottler> = Arc::new(NeverThrottle);

        let prefetcher = TileBasedPrefetcher::new(
            index,
            client,
            cache,
            rx,
            throttler,
            TileBasedConfig::default(),
        );

        assert_eq!(prefetcher.name(), "tile-based");
        assert!(prefetcher.description().contains("DSF"));
    }

    // ========================================================================
    // DDS filename parsing tests
    // ========================================================================

    #[test]
    fn test_parse_dds_filename_bing() {
        let tile =
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("100000_125184_BI18.dds");
        assert!(tile.is_some());
        let tile = tile.unwrap();
        assert_eq!(tile.row, 100000);
        assert_eq!(tile.col, 125184);
        assert_eq!(tile.zoom, 18);
    }

    #[test]
    fn test_parse_dds_filename_google_go2() {
        let tile =
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("25264_10368_GO216.dds");
        assert!(tile.is_some());
        let tile = tile.unwrap();
        assert_eq!(tile.row, 25264);
        assert_eq!(tile.col, 10368);
        assert_eq!(tile.zoom, 16);
    }

    #[test]
    fn test_parse_dds_filename_google_legacy() {
        let tile =
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("100000_125184_GO18.dds");
        assert!(tile.is_some());
        let tile = tile.unwrap();
        assert_eq!(tile.row, 100000);
        assert_eq!(tile.col, 125184);
        assert_eq!(tile.zoom, 18);
    }

    #[test]
    fn test_parse_dds_filename_various_zoom_levels() {
        for zoom in [16, 17, 18, 19] {
            let filename = format!("100000_125184_BI{}.dds", zoom);
            let tile = TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile(&filename);
            assert!(tile.is_some(), "Failed to parse zoom {}", zoom);
            assert_eq!(tile.unwrap().zoom, zoom);
        }
    }

    #[test]
    fn test_parse_dds_filename_invalid_format() {
        // Missing extension
        assert!(
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("100000_125184_BI18")
                .is_none()
        );

        // Wrong format
        assert!(
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("invalid.dds").is_none()
        );

        // Missing parts
        assert!(
            TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("100000_BI18.dds")
                .is_none()
        );
    }

    #[test]
    fn test_parse_dds_filename_zero_coordinates() {
        let tile = TileBasedPrefetcher::<MockCache>::parse_dds_filename_to_tile("0_0_BI16.dds");
        assert!(tile.is_some());
        let tile = tile.unwrap();
        assert_eq!(tile.row, 0);
        assert_eq!(tile.col, 0);
        assert_eq!(tile.zoom, 16);
    }
}
