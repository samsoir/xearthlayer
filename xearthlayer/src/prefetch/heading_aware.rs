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
use crate::executor::{JobId, MemoryCache};
use crate::fuse::{DdsHandler, DdsRequest};

use super::buffer::BufferGenerator;
use super::circuit_breaker::CircuitState;
use super::cone::ConeGenerator;
use super::config::{FuseInferenceConfig, HeadingAwarePrefetchConfig};
use super::inference::FuseRequestAnalyzer;
use super::intersection::{test_zone_intersection, TileBounds, ZoneIntersection};
use super::scenery_index::SceneryIndex;
use super::state::{
    AircraftState, DetailedPrefetchStats, GpsStatus, PrefetchMode, SharedPrefetchStatus,
};
use super::strategy::Prefetcher;
use super::throttler::{PrefetchThrottler, ThrottleState};
use super::types::{InputMode, PrefetchTile, PrefetchZone, TurnDirection, TurnState};

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

/// Results from a single prefetch cycle.
#[derive(Debug, Clone, Default)]
struct CycleResults {
    /// Tiles submitted to the pipeline.
    submitted: u64,
    /// Tiles skipped because they were already in cache.
    cache_hits: u64,
    /// Tiles skipped due to TTL (recently attempted).
    ttl_skipped: u64,
}

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

    // Scenery-aware prefetch (optional)
    /// Optional scenery index for exact tile lookup.
    scenery_index: Option<Arc<SceneryIndex>>,

    // Throttler (optional)
    /// Throttler for pausing prefetch during high load.
    throttler: Option<Arc<dyn PrefetchThrottler>>,
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
            scenery_index: None,
            throttler: None,
        }
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Set the scenery index for scenery-aware tile lookup.
    ///
    /// When set, the prefetcher queries the index for tiles near the aircraft
    /// position instead of calculating coordinates. This ensures:
    /// - Exact zoom levels per tile (read from .ter files)
    /// - Only tiles that exist in the package are prefetched
    /// - Sea tiles can be deprioritized
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.scenery_index = Some(index);
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

    /// Get access to statistics.
    pub fn stats(&self) -> Arc<HeadingAwarePrefetchStats> {
        Arc::clone(&self.stats)
    }

    /// Get the FUSE analyzer for callback wiring.
    pub fn fuse_analyzer(&self) -> Arc<FuseRequestAnalyzer> {
        Arc::clone(&self.fuse_analyzer)
    }

    /// Check if prefetching should be throttled.
    ///
    /// Returns true if prefetch should be paused.
    fn check_throttle(&self) -> bool {
        let Some(ref throttler) = self.throttler else {
            return false;
        };

        let should_throttle = throttler.should_throttle();

        // Gather values before borrowing shared_status
        let zoom_levels = self.active_zoom_levels();
        let cycles = self.stats.cycles.load(Ordering::Relaxed);
        let tiles_submitted_total = self.stats.tiles_submitted.load(Ordering::Relaxed);
        let cache_hits = self.stats.cache_hits.load(Ordering::Relaxed);
        let ttl_skipped = self.stats.ttl_skipped.load(Ordering::Relaxed);

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
                cycles,
                tiles_submitted_last_cycle: 0,
                tiles_submitted_total,
                cache_hits,
                ttl_skipped,
                active_zoom_levels: zoom_levels,
                is_active: !should_throttle,
                circuit_state,
            });
        }

        should_throttle
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
                    // Check throttler (e.g., circuit breaker)
                    if self.check_throttle() {
                        trace!("Prefetch cycle skipped - throttled");
                        continue;
                    }
                    self.run_prefetch_cycle().await;
                }
            }
        }
    }

    /// Get current position from telemetry or FUSE inference.
    fn current_position(&self) -> Option<(f64, f64)> {
        // Prefer telemetry position
        if let Some(ref state) = self.last_telemetry_state {
            return Some((state.latitude, state.longitude));
        }
        // Fall back to FUSE inference
        self.fuse_analyzer.position()
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

        // Get current position (from telemetry or FUSE inference)
        let position = self.current_position();

        // Prefer scenery-aware prefetch when index is available and we have a position
        let tiles: Vec<PrefetchTile> = if self.scenery_index.is_some() {
            if let Some((lat, lon)) = position {
                // Use scenery index for exact tile lookup
                match mode {
                    InputMode::Telemetry => {
                        self.stats.telemetry_cycles.fetch_add(1, Ordering::Relaxed);
                    }
                    InputMode::FuseInference => {
                        self.stats
                            .fuse_inference_cycles
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputMode::RadialFallback => {
                        self.stats
                            .radial_fallback_cycles
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.generate_scenery_tiles(lat, lon)
            } else {
                Vec::new()
            }
        } else {
            // Fall back to coordinate-based generation
            match mode {
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
            }
        };

        if tiles.is_empty() {
            trace!(mode = %mode, "No tiles to prefetch this cycle");
            // Still report stats even when idle
            if let Some(ref status) = self.shared_status {
                let circuit_state = self.throttler.as_ref().map(|t| match t.state() {
                    ThrottleState::Active => CircuitState::Closed,
                    ThrottleState::Paused => CircuitState::Open,
                    ThrottleState::Resuming => CircuitState::HalfOpen,
                });
                status.update_detailed_stats(DetailedPrefetchStats {
                    cycles: self.stats.cycles.load(Ordering::Relaxed),
                    tiles_submitted_last_cycle: 0,
                    tiles_submitted_total: self.stats.tiles_submitted.load(Ordering::Relaxed),
                    cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
                    ttl_skipped: self.stats.ttl_skipped.load(Ordering::Relaxed),
                    active_zoom_levels: self.active_zoom_levels(),
                    is_active: false,
                    circuit_state,
                });
            }
            return;
        }

        // Submit tiles
        let cycle_results = self.submit_tiles(&tiles).await;

        debug!(
            mode = %mode,
            generated = tiles.len(),
            submitted = cycle_results.submitted,
            cache_hits = cycle_results.cache_hits,
            ttl_skipped = cycle_results.ttl_skipped,
            "Prefetch cycle complete"
        );

        // Update shared status
        if let Some(ref status) = self.shared_status {
            let stats = super::scheduler::PrefetchStatsSnapshot {
                tiles_predicted: tiles.len() as u64,
                tiles_submitted: cycle_results.submitted,
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

            // Update detailed stats for dashboard visibility
            let circuit_state = self.throttler.as_ref().map(|t| match t.state() {
                ThrottleState::Active => CircuitState::Closed,
                ThrottleState::Paused => CircuitState::Open,
                ThrottleState::Resuming => CircuitState::HalfOpen,
            });
            status.update_detailed_stats(DetailedPrefetchStats {
                cycles: self.stats.cycles.load(Ordering::Relaxed),
                tiles_submitted_last_cycle: cycle_results.submitted,
                tiles_submitted_total: self.stats.tiles_submitted.load(Ordering::Relaxed),
                cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
                ttl_skipped: self.stats.ttl_skipped.load(Ordering::Relaxed),
                active_zoom_levels: self.active_zoom_levels(),
                is_active: cycle_results.submitted > 0,
                circuit_state,
            });
        }

        // Clean up expired attempts
        self.cleanup_expired_attempts();
    }

    /// Generate tiles using telemetry-based cone generator.
    ///
    /// Generates tiles at the configured zoom level with cone and buffer coverage.
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

        // Merge tiles and sort by priority
        let mut merged = super::buffer::merge_prefetch_tiles(vec![cone_tiles, buffer_tiles]);
        merged.sort_by_key(|t| t.priority);

        // Limit to max tiles per cycle
        merged.truncate(self.config.heading.max_tiles_per_cycle);

        merged
    }

    /// Get the list of active zoom levels for dashboard display.
    fn active_zoom_levels(&self) -> Vec<u8> {
        vec![self.config.heading.zoom]
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

    /// Generate tiles using scenery index lookup with dual-zone prefetch.
    ///
    /// Implements a two-zone prefetch strategy:
    /// 1. **Radial buffer** (85-100nm, 360°): Thin ring around X-Plane's boundary
    ///    for coverage in all directions (handles unexpected turns)
    /// 2. **Heading cone** (85-120nm, forward): Deep lookahead in the direction
    ///    of travel for sustained flight
    ///
    /// Both zones run together and tiles are combined. Coalescing in the pipeline
    /// handles deduplication.
    fn generate_scenery_tiles(&self, lat: f64, lon: f64) -> Vec<PrefetchTile> {
        let Some(ref index) = self.scenery_index else {
            return Vec::new();
        };

        // Get heading for cone filtering (use 0 if no telemetry)
        let heading = self
            .last_telemetry_state
            .as_ref()
            .map(|s| s.heading)
            .unwrap_or(0.0);

        // Determine effective cone half-angle (may be widened during turns)
        let effective_half_angle = match &self.turn_state {
            TurnState::Straight => self.config.heading.cone_half_angle,
            TurnState::Turning { .. } => {
                self.config.heading.cone_half_angle * self.config.heading.turn_widening_factor
            }
        };

        // Zone boundaries
        let inner_radius = self.config.heading.inner_radius_nm;
        let radial_outer = self.config.heading.radial_outer_radius_nm;
        let cone_outer = self.config.heading.cone_outer_radius_nm;

        // Add buffer for tile size to catch tiles whose center is outside but edge is inside.
        // ZL12 tiles are the largest (~10nm at mid-latitudes), so use 15nm buffer to be safe.
        // This ensures ANY tile with ANY part inside the catchment area is included.
        let tile_size_buffer_nm = 15.0;
        let query_radius = cone_outer + tile_size_buffer_nm;

        // Query all tiles within the buffered radius
        let scenery_tiles = index.tiles_near(lat, lon, query_radius);

        if scenery_tiles.is_empty() {
            trace!(lat, lon, cone_outer, "No scenery tiles found in index");
            return Vec::new();
        }

        let mut radial_count = 0u32;
        let mut cone_count = 0u32;

        // Filter tiles using intersection testing
        let mut tiles: Vec<PrefetchTile> = scenery_tiles
            .into_iter()
            .filter_map(|st| {
                // Calculate tile bounds relative to aircraft position
                let bounds = TileBounds::from_scenery_tile(&st, lat, lon);

                // Test which zone(s) the tile intersects
                let zone_result = test_zone_intersection(
                    &bounds,
                    heading,
                    effective_half_angle,
                    inner_radius,
                    radial_outer,
                    cone_outer,
                );

                // Skip tiles that don't intersect any zone
                if !zone_result.intersects() {
                    return None;
                }

                // Map zone result to PrefetchZone and count
                let zone = match zone_result {
                    ZoneIntersection::ConeCenter => {
                        cone_count += 1;
                        PrefetchZone::ForwardCenter
                    }
                    ZoneIntersection::ConeEdge => {
                        cone_count += 1;
                        PrefetchZone::ForwardEdge
                    }
                    ZoneIntersection::RadialBuffer => {
                        radial_count += 1;
                        PrefetchZone::LateralBuffer
                    }
                    ZoneIntersection::None => unreachable!(),
                };

                // Calculate priority
                let base_priority = zone_result.base_priority();

                // Distance priority: tiles at inner edge have higher priority
                let distance_priority =
                    ((bounds.center_distance_nm - inner_radius).max(0.0) * 2.0) as u32;

                // Angle priority for cone tiles
                let angle_priority = match zone_result {
                    ZoneIntersection::ConeCenter | ZoneIntersection::ConeEdge => {
                        (bounds.angle_from_heading(heading) / 5.0) as u32
                    }
                    _ => 0,
                };

                // Deprioritize sea tiles
                let type_priority = if st.is_sea { 200 } else { 0 };

                // Zoom level priority (lower zoom = larger tiles = lower priority)
                let zoom_priority = (20u8.saturating_sub(st.tile_zoom())) as u32 * 5;

                let priority = base_priority
                    + distance_priority
                    + angle_priority
                    + type_priority
                    + zoom_priority;

                Some(PrefetchTile::new(st.to_tile_coord(), priority, zone))
            })
            .collect();

        // Log at debug level (high volume - every cycle)
        debug!(
            lat = format!("{:.2}", lat),
            lon = format!("{:.2}", lon),
            heading = format!("{:.0}", heading),
            radial_tiles = radial_count,
            cone_tiles = cone_count,
            total = tiles.len(),
            "Prefetch cycle: radial={}-{}nm cone={}-{}nm half_angle={}°",
            inner_radius,
            radial_outer,
            inner_radius,
            cone_outer,
            effective_half_angle
        );

        // Sort by priority (lower = higher priority)
        tiles.sort_by_key(|t| t.priority);

        // Limit to max tiles per cycle
        tiles.truncate(self.config.heading.max_tiles_per_cycle);

        tiles
    }

    /// Submit tiles to the pipeline.
    async fn submit_tiles(&mut self, tiles: &[PrefetchTile]) -> CycleResults {
        let mut submitted = 0u64;
        let mut cache_hits = 0u64;
        let mut ttl_skipped = 0u64;

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
                job_id: JobId::auto(),
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

        CycleResults {
            submitted,
            cache_hits,
            ttl_skipped,
        }
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
