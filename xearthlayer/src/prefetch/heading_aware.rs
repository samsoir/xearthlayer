//! Heading-aware prefetcher with graceful degradation.
//!
//! This module provides a unified prefetcher that selects the best tile generation
//! strategy based on available input:
//!
//! 1. **Telemetry Mode**: Uses precise cone generator when UDP telemetry is available
//! 2. **FUSE Inference Mode**: Uses dynamic envelope when telemetry is stale
//! 3. **Radial Fallback Mode**: Simple radius when no heading data is available
//!
//! # Graceful Degradation
//!
//! The prefetcher automatically transitions between modes:
//!
//! ```text
//! ┌─────────────────┐   stale (>5s)   ┌──────────────────┐   low confidence   ┌────────────────┐
//! │   Telemetry     │ ───────────────▶│  FUSE Inference  │ ──────────────────▶│ Radial Fallback│
//! │  (precise)      │                 │  (fuzzy margins) │                    │   (no heading) │
//! └─────────────────┘                 └──────────────────┘                    └────────────────┘
//!      ~50-80                              ~100-150                               49 tiles
//!    tiles/cycle                         tiles/cycle                             (7×7 grid)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use std::sync::Arc;
//! use xearthlayer::prefetch::{
//!     HeadingAwarePrefetcher, HeadingAwarePrefetcherConfig,
//!     FuseRequestAnalyzer, FuseInferenceConfig,
//! };
//!
//! // Create FUSE analyzer (always active for fallback)
//! let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
//!
//! // Create the prefetcher
//! let prefetcher = HeadingAwarePrefetcher::new(
//!     memory_cache,
//!     dds_handler,
//!     HeadingAwarePrefetcherConfig::default(),
//!     analyzer,
//! );
//!
//! // Run with telemetry receiver
//! prefetcher.run(state_rx, cancellation_token).await;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::coord::{to_tile_coords, TileCoord};
use crate::fuse::{DdsHandler, DdsRequest};
use crate::pipeline::{JobId, MemoryCache};

use super::buffer::BufferGenerator;
use super::cone::ConeGenerator;
use super::config::{FuseInferenceConfig, HeadingAwarePrefetchConfig};
use super::inference::FuseRequestAnalyzer;
use super::state::{AircraftState, GpsStatus, PrefetchMode, SharedPrefetchStatus};
use super::strategy::Prefetcher;
use super::types::{PrefetchTile, PrefetchZone, TurnDirection, TurnState};

/// Input mode for the prefetcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// UDP telemetry available - use precise ConeGenerator.
    Telemetry,
    /// FUSE inference active - use dynamic envelope with fuzzy margins.
    FuseInference,
    /// No heading data - fall back to simple radial.
    RadialFallback,
}

impl std::fmt::Display for InputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputMode::Telemetry => write!(f, "telemetry"),
            InputMode::FuseInference => write!(f, "fuse-inference"),
            InputMode::RadialFallback => write!(f, "radial-fallback"),
        }
    }
}

/// Configuration for the heading-aware prefetcher.
#[derive(Debug, Clone)]
pub struct HeadingAwarePrefetcherConfig {
    /// Configuration for heading-aware cone generation.
    pub heading: HeadingAwarePrefetchConfig,
    /// Configuration for FUSE inference fallback.
    pub fuse_inference: FuseInferenceConfig,
    /// Time before telemetry is considered stale (triggers fallback).
    pub telemetry_stale_threshold: Duration,
    /// Confidence threshold for using FUSE inference.
    pub fuse_confidence_threshold: f32,
    /// Radius for radial fallback (tiles).
    pub radial_fallback_radius: u8,
}

impl Default for HeadingAwarePrefetcherConfig {
    fn default() -> Self {
        Self {
            heading: HeadingAwarePrefetchConfig::default(),
            fuse_inference: FuseInferenceConfig::default(),
            telemetry_stale_threshold: Duration::from_secs(5),
            fuse_confidence_threshold: 0.3,
            radial_fallback_radius: 3,
        }
    }
}

/// Statistics for the heading-aware prefetcher.
#[derive(Debug, Default)]
pub struct HeadingAwarePrefetchStats {
    /// Total prefetch cycles.
    pub cycles: AtomicU64,
    /// Cycles using telemetry mode.
    pub telemetry_cycles: AtomicU64,
    /// Cycles using FUSE inference mode.
    pub fuse_inference_cycles: AtomicU64,
    /// Cycles using radial fallback mode.
    pub radial_fallback_cycles: AtomicU64,
    /// Tiles submitted.
    pub tiles_submitted: AtomicU64,
    /// Cache hits (tiles already cached).
    pub cache_hits: AtomicU64,
    /// Tiles skipped due to TTL.
    pub ttl_skipped: AtomicU64,
}

impl HeadingAwarePrefetchStats {
    /// Get a snapshot of current statistics.
    pub fn snapshot(&self) -> HeadingAwarePrefetchStatsSnapshot {
        HeadingAwarePrefetchStatsSnapshot {
            cycles: self.cycles.load(Ordering::Relaxed),
            telemetry_cycles: self.telemetry_cycles.load(Ordering::Relaxed),
            fuse_inference_cycles: self.fuse_inference_cycles.load(Ordering::Relaxed),
            radial_fallback_cycles: self.radial_fallback_cycles.load(Ordering::Relaxed),
            tiles_submitted: self.tiles_submitted.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            ttl_skipped: self.ttl_skipped.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of prefetch statistics.
#[derive(Debug, Clone, Default)]
pub struct HeadingAwarePrefetchStatsSnapshot {
    pub cycles: u64,
    pub telemetry_cycles: u64,
    pub fuse_inference_cycles: u64,
    pub radial_fallback_cycles: u64,
    pub tiles_submitted: u64,
    pub cache_hits: u64,
    pub ttl_skipped: u64,
}

/// Timeout for prefetch requests.
const PREFETCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Unified heading-aware prefetcher with graceful degradation.
///
/// Selects the best tile generation strategy based on available input:
/// - **Telemetry**: Precise cone-based prefetching
/// - **FUSE Inference**: Dynamic envelope with fuzzy margins
/// - **Radial Fallback**: Simple radius when no heading data available
pub struct HeadingAwarePrefetcher<M: MemoryCache> {
    /// Memory cache for checking existing tiles.
    memory_cache: Arc<M>,
    /// DDS handler for submitting requests.
    dds_handler: DdsHandler,
    /// Configuration.
    config: HeadingAwarePrefetcherConfig,

    // Telemetry mode generators
    /// Cone generator for forward prefetch.
    cone_generator: ConeGenerator,
    /// Buffer generator for lateral/rear coverage.
    buffer_generator: BufferGenerator,

    // FUSE inference (always active for fallback)
    /// FUSE request analyzer for telemetry-free operation.
    fuse_analyzer: Arc<FuseRequestAnalyzer>,

    // State tracking
    /// Current turn state for cone widening.
    turn_state: TurnState,
    /// Previous heading for turn detection.
    prev_heading: Option<f32>,
    /// Time of last heading update (for turn rate calculation).
    prev_heading_time: Option<Instant>,
    /// Recently-attempted tiles with timestamp.
    recently_attempted: HashMap<(u32, u32, u8), Instant>,
    /// Last telemetry update time.
    last_telemetry: Option<Instant>,
    /// Last telemetry state.
    last_telemetry_state: Option<AircraftState>,

    // Stats and status
    /// Statistics.
    stats: Arc<HeadingAwarePrefetchStats>,
    /// Optional shared status for TUI display.
    shared_status: Option<Arc<SharedPrefetchStatus>>,
}

impl<M: MemoryCache> HeadingAwarePrefetcher<M> {
    /// Create a new heading-aware prefetcher.
    ///
    /// # Arguments
    ///
    /// * `memory_cache` - Memory cache for checking existing tiles
    /// * `dds_handler` - Handler for submitting prefetch requests
    /// * `config` - Prefetcher configuration
    /// * `fuse_analyzer` - FUSE request analyzer (always active for fallback)
    pub fn new(
        memory_cache: Arc<M>,
        dds_handler: DdsHandler,
        config: HeadingAwarePrefetcherConfig,
        fuse_analyzer: Arc<FuseRequestAnalyzer>,
    ) -> Self {
        let cone_generator = ConeGenerator::new(config.heading.clone());
        let buffer_generator = BufferGenerator::new(config.heading.clone());

        Self {
            memory_cache,
            dds_handler,
            config,
            cone_generator,
            buffer_generator,
            fuse_analyzer,
            turn_state: TurnState::default(),
            prev_heading: None,
            prev_heading_time: None,
            recently_attempted: HashMap::new(),
            last_telemetry: None,
            last_telemetry_state: None,
            stats: Arc::new(HeadingAwarePrefetchStats::default()),
            shared_status: None,
        }
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Get access to statistics.
    pub fn stats(&self) -> Arc<HeadingAwarePrefetchStats> {
        Arc::clone(&self.stats)
    }

    /// Get the FUSE analyzer for callback wiring.
    pub fn fuse_analyzer(&self) -> Arc<FuseRequestAnalyzer> {
        Arc::clone(&self.fuse_analyzer)
    }

    /// Run the prefetcher, processing state updates.
    pub async fn run(
        mut self,
        mut state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) {
        info!(
            cone_half_angle = self.config.heading.cone_half_angle,
            inner_radius_nm = self.config.heading.inner_radius_nm,
            outer_radius_nm = self.config.heading.outer_radius_nm,
            zoom = self.config.heading.zoom,
            "Heading-aware prefetcher started"
        );

        let cycle_interval = self.config.heading.cycle_interval();
        let mut interval = tokio::time::interval(cycle_interval);

        loop {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    info!("Heading-aware prefetcher shutting down");
                    break;
                }

                Some(state) = state_rx.recv() => {
                    self.last_telemetry = Some(Instant::now());
                    self.last_telemetry_state = Some(state.clone());
                    self.update_turn_state(state.heading);

                    if let Some(ref status) = self.shared_status {
                        status.update_aircraft(&state);
                    }
                }

                _ = interval.tick() => {
                    self.run_prefetch_cycle().await;
                }
            }
        }
    }

    /// Determine the current input mode.
    fn determine_input_mode(&self) -> InputMode {
        // 1. Fresh telemetry?
        if let Some(last) = self.last_telemetry {
            if last.elapsed() < self.config.telemetry_stale_threshold {
                return InputMode::Telemetry;
            }
        }

        // 2. FUSE inference active with sufficient confidence?
        if self.fuse_analyzer.is_active() {
            let confidence = self.fuse_analyzer.confidence();
            if confidence >= self.config.fuse_confidence_threshold {
                return InputMode::FuseInference;
            }
        }

        // 3. Fall back to radial
        InputMode::RadialFallback
    }

    /// Run a single prefetch cycle.
    async fn run_prefetch_cycle(&mut self) {
        let mode = self.determine_input_mode();
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);

        let tiles: Vec<PrefetchTile> = match mode {
            InputMode::Telemetry => {
                self.stats.telemetry_cycles.fetch_add(1, Ordering::Relaxed);
                if let Some(ref state) = self.last_telemetry_state {
                    self.generate_telemetry_tiles(state)
                } else {
                    Vec::new()
                }
            }
            InputMode::FuseInference => {
                self.stats
                    .fuse_inference_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.fuse_analyzer.prefetch_tiles()
            }
            InputMode::RadialFallback => {
                self.stats
                    .radial_fallback_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.generate_radial_tiles()
            }
        };

        if tiles.is_empty() {
            trace!(mode = %mode, "No tiles to prefetch this cycle");
            return;
        }

        // Submit tiles
        let submitted = self.submit_tiles(&tiles).await;

        debug!(
            mode = %mode,
            generated = tiles.len(),
            submitted,
            "Prefetch cycle complete"
        );

        // Update shared status
        if let Some(ref status) = self.shared_status {
            let stats = super::scheduler::PrefetchStatsSnapshot {
                tiles_predicted: tiles.len() as u64,
                tiles_submitted: submitted as u64,
                tiles_in_flight_skipped: 0,
                prediction_cycles: self.stats.cycles.load(Ordering::Relaxed),
            };
            status.update_stats(stats);

            // Update prefetch mode for UI display
            let display_mode = match mode {
                InputMode::Telemetry => PrefetchMode::Telemetry,
                InputMode::FuseInference => PrefetchMode::FuseInference,
                InputMode::RadialFallback => PrefetchMode::Radial,
            };
            status.update_prefetch_mode(display_mode);

            // Update GPS status based on input mode
            // Telemetry mode: GPS is connected (update_aircraft already sets this)
            // FUSE/Radial mode: GPS is inferred (using FUSE-based position)
            let gps_status = match mode {
                InputMode::Telemetry => GpsStatus::Connected,
                InputMode::FuseInference | InputMode::RadialFallback => GpsStatus::Inferred,
            };
            status.update_gps_status(gps_status);
        }

        // Clean up expired attempts
        self.cleanup_expired_attempts();
    }

    /// Generate tiles using telemetry-based cone generator.
    fn generate_telemetry_tiles(&self, state: &AircraftState) -> Vec<PrefetchTile> {
        let position = (state.latitude, state.longitude);

        // Generate cone tiles (turn state affects widening internally)
        let cone_tiles =
            self.cone_generator
                .generate_cone_tiles(position, state.heading, &self.turn_state);

        // Generate buffer tiles
        let buffer_tiles =
            self.buffer_generator
                .generate_buffer_tiles(position, state.heading, &self.turn_state);

        // Merge and sort by priority
        let mut merged = super::buffer::merge_prefetch_tiles(vec![cone_tiles, buffer_tiles]);
        merged.sort_by_key(|t| t.priority);

        // Limit to max tiles per cycle
        merged.truncate(self.config.heading.max_tiles_per_cycle);

        merged
    }

    /// Generate tiles using radial fallback.
    fn generate_radial_tiles(&self) -> Vec<PrefetchTile> {
        // Use last known position from telemetry, FUSE inference, or nothing
        let position = if let Some(ref state) = self.last_telemetry_state {
            Some((state.latitude, state.longitude))
        } else {
            self.fuse_analyzer.position()
        };

        let Some((lat, lon)) = position else {
            trace!("No position available for radial fallback");
            return Vec::new();
        };

        let center = match to_tile_coords(lat, lon, self.config.heading.zoom) {
            Ok(tile) => tile,
            Err(e) => {
                warn!(error = %e, "Invalid position for radial fallback");
                return Vec::new();
            }
        };

        let radius = self.config.radial_fallback_radius as i32;
        let max_coord = 2i64.pow(center.zoom as u32);
        let mut tiles = Vec::new();

        for dr in -radius..=radius {
            for dc in -radius..=radius {
                let new_row = center.row as i64 + dr as i64;
                let new_col = center.col as i64 + dc as i64;

                if new_row >= 0 && new_row < max_coord && new_col >= 0 && new_col < max_coord {
                    let coord = TileCoord {
                        row: new_row as u32,
                        col: new_col as u32,
                        zoom: center.zoom,
                    };

                    // Calculate priority based on distance from center
                    let dist = (dr.abs() + dc.abs()) as u32;
                    let priority = PrefetchZone::ForwardEdge.base_priority() + dist;

                    tiles.push(PrefetchTile::new(
                        coord,
                        priority,
                        PrefetchZone::ForwardEdge,
                    ));
                }
            }
        }

        tiles
    }

    /// Submit tiles to the pipeline.
    async fn submit_tiles(&mut self, tiles: &[PrefetchTile]) -> usize {
        let mut submitted = 0;
        let mut cache_hits = 0;
        let mut ttl_skipped = 0;

        for tile in tiles {
            let tile_key = (tile.coord.row, tile.coord.col, tile.coord.zoom);

            // Check memory cache
            if self
                .memory_cache
                .get(tile.coord.row, tile.coord.col, tile.coord.zoom)
                .await
                .is_some()
            {
                cache_hits += 1;
                continue;
            }

            // Check TTL
            if let Some(attempt_time) = self.recently_attempted.get(&tile_key) {
                if attempt_time.elapsed() < self.config.heading.attempt_ttl() {
                    ttl_skipped += 1;
                    continue;
                }
            }

            // Submit request
            let cancellation_token = CancellationToken::new();
            let timeout_token = cancellation_token.clone();

            tokio::spawn(async move {
                tokio::time::sleep(PREFETCH_REQUEST_TIMEOUT).await;
                timeout_token.cancel();
            });

            let (tx, _rx) = tokio::sync::oneshot::channel();
            let request = DdsRequest {
                job_id: JobId::new(),
                tile: tile.coord,
                result_tx: tx,
                cancellation_token,
                is_prefetch: true,
            };

            trace!(
                row = tile.coord.row,
                col = tile.coord.col,
                zone = ?tile.zone,
                priority = tile.priority,
                "Submitting prefetch request"
            );

            (self.dds_handler)(request);
            self.recently_attempted.insert(tile_key, Instant::now());
            submitted += 1;
        }

        self.stats
            .cache_hits
            .fetch_add(cache_hits, Ordering::Relaxed);
        self.stats
            .tiles_submitted
            .fetch_add(submitted, Ordering::Relaxed);
        self.stats
            .ttl_skipped
            .fetch_add(ttl_skipped, Ordering::Relaxed);

        submitted as usize
    }

    /// Update turn state based on heading change.
    fn update_turn_state(&mut self, heading: f32) {
        if let (Some(prev), Some(prev_time)) = (self.prev_heading, self.prev_heading_time) {
            let elapsed = prev_time.elapsed().as_secs_f32();
            if elapsed > 0.0 {
                // Calculate turn rate
                let mut diff = heading - prev;
                if diff > 180.0 {
                    diff -= 360.0;
                } else if diff < -180.0 {
                    diff += 360.0;
                }

                let turn_rate = diff / elapsed;

                // Update turn state
                if turn_rate.abs() > self.config.heading.turn_rate_threshold {
                    let direction = if turn_rate > 0.0 {
                        TurnDirection::Right
                    } else {
                        TurnDirection::Left
                    };

                    self.turn_state = TurnState::Turning {
                        rate: turn_rate.abs(),
                        direction,
                    };
                } else {
                    self.turn_state = TurnState::Straight;
                }
            }
        }

        self.prev_heading = Some(heading);
        self.prev_heading_time = Some(Instant::now());
    }

    /// Clean up expired TTL entries.
    fn cleanup_expired_attempts(&mut self) {
        let ttl = self.config.heading.attempt_ttl();
        self.recently_attempted
            .retain(|_, time| time.elapsed() < ttl);
    }
}

// Implement the Prefetcher trait
impl<M: MemoryCache + 'static> Prefetcher for HeadingAwarePrefetcher<M> {
    fn run(
        self: Box<Self>,
        state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move { (*self).run(state_rx, cancellation_token).await })
    }

    fn name(&self) -> &'static str {
        "heading-aware"
    }

    fn description(&self) -> &'static str {
        "Direction-aware prefetching with graceful degradation"
    }

    fn startup_info(&self) -> String {
        format!(
            "heading-aware, {}° cone, {}-{}nm zone, zoom {}",
            self.config.heading.cone_half_angle,
            self.config.heading.inner_radius_nm,
            self.config.heading.outer_radius_nm,
            self.config.heading.zoom
        )
    }
}

impl<M: MemoryCache> std::fmt::Debug for HeadingAwarePrefetcher<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadingAwarePrefetcher")
            .field("mode", &self.determine_input_mode())
            .field("turn_state", &self.turn_state)
            .field("stats", &self.stats.snapshot())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Mutex;

    /// Mock memory cache for testing.
    struct MockCache {
        tiles: Mutex<StdHashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockCache {
        fn new() -> Self {
            Self {
                tiles: Mutex::new(StdHashMap::new()),
            }
        }
    }

    impl MemoryCache for MockCache {
        fn get(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
            let result = self.tiles.lock().unwrap().get(&(row, col, zoom)).cloned();
            async move { result }
        }

        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            data: Vec<u8>,
        ) -> impl std::future::Future<Output = ()> + Send {
            self.tiles.lock().unwrap().insert((row, col, zoom), data);
            async {}
        }

        fn size_bytes(&self) -> usize {
            self.tiles.lock().unwrap().values().map(|v| v.len()).sum()
        }

        fn entry_count(&self) -> usize {
            self.tiles.lock().unwrap().len()
        }
    }

    #[test]
    fn test_input_mode_display() {
        assert_eq!(InputMode::Telemetry.to_string(), "telemetry");
        assert_eq!(InputMode::FuseInference.to_string(), "fuse-inference");
        assert_eq!(InputMode::RadialFallback.to_string(), "radial-fallback");
    }

    #[test]
    fn test_config_defaults() {
        let config = HeadingAwarePrefetcherConfig::default();

        assert_eq!(config.telemetry_stale_threshold, Duration::from_secs(5));
        assert_eq!(config.fuse_confidence_threshold, 0.3);
        assert_eq!(config.radial_fallback_radius, 3);
    }

    #[test]
    fn test_stats_snapshot() {
        let stats = HeadingAwarePrefetchStats::default();
        stats.cycles.store(10, Ordering::Relaxed);
        stats.telemetry_cycles.store(8, Ordering::Relaxed);
        stats.fuse_inference_cycles.store(2, Ordering::Relaxed);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.cycles, 10);
        assert_eq!(snapshot.telemetry_cycles, 8);
        assert_eq!(snapshot.fuse_inference_cycles, 2);
    }

    #[test]
    fn test_prefetcher_startup_info() {
        let cache = Arc::new(MockCache::new());
        let handler: DdsHandler = Arc::new(|_| {});
        let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

        let prefetcher = HeadingAwarePrefetcher::new(
            cache,
            handler,
            HeadingAwarePrefetcherConfig::default(),
            analyzer,
        );

        let info = prefetcher.startup_info();
        assert!(info.contains("heading-aware"));
        assert!(info.contains("cone"));
    }

    #[test]
    fn test_prefetcher_name() {
        let cache = Arc::new(MockCache::new());
        let handler: DdsHandler = Arc::new(|_| {});
        let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

        let prefetcher = HeadingAwarePrefetcher::new(
            cache,
            handler,
            HeadingAwarePrefetcherConfig::default(),
            analyzer,
        );

        assert_eq!(prefetcher.name(), "heading-aware");
    }

    #[test]
    fn test_determine_mode_radial_initially() {
        let cache = Arc::new(MockCache::new());
        let handler: DdsHandler = Arc::new(|_| {});
        let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

        let prefetcher = HeadingAwarePrefetcher::new(
            cache,
            handler,
            HeadingAwarePrefetcherConfig::default(),
            analyzer,
        );

        // No telemetry, no FUSE data -> radial fallback
        assert_eq!(prefetcher.determine_input_mode(), InputMode::RadialFallback);
    }
}
