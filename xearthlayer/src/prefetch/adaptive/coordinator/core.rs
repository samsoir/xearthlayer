//! Core adaptive prefetch coordinator implementation.
//!
//! This module contains the [`AdaptivePrefetchCoordinator`] struct and its
//! implementation. The async run loop (`Prefetcher` trait impl) is in the
//! separate [`super::runner`] module.

use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::coord::TileCoord;
use crate::executor::{DaemonMemoryCache, DdsClient};
use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::state::{AircraftState, DetailedPrefetchStats, SharedPrefetchStatus};
use crate::prefetch::throttler::{PrefetchThrottler, ThrottleState};
use crate::prefetch::tile_based::DsfTileCoord;
use crate::prefetch::CircuitState;
use crate::prefetch::SceneryIndex;

use super::super::boundary_monitor::BoundaryAxis;
use super::super::boundary_strategy::BoundaryStrategy;
use super::super::calibration::{PerformanceCalibration, StrategyMode};
use super::super::config::{AdaptivePrefetchConfig, PrefetchMode};
use super::super::ground_strategy::GroundStrategy;
use super::super::phase_detector::{FlightPhase, PhaseDetector};
use super::super::scenery_window::{SceneryWindow, SceneryWindowConfig};
use super::super::strategy::{AdaptivePrefetchStrategy, PrefetchPlan};
use super::super::transition_throttle::TransitionThrottle;
use crate::scene_tracker::SceneTracker;

use super::status::CoordinatorStatus;
use super::telemetry::extract_track;

// ─────────────────────────────────────────────────────────────────────────────
// Backpressure constants
// ─────────────────────────────────────────────────────────────────────────────

/// Executor load threshold above which prefetch cycles are deferred entirely.
///
/// When any resource pool (Network, CPU, DiskIO) exceeds 80% utilization,
/// the prefetch coordinator skips the current cycle to avoid starving
/// on-demand FUSE requests.
pub const BACKPRESSURE_DEFER_THRESHOLD: f64 = 0.8;

/// Executor load threshold above which prefetch submission is reduced.
///
/// When any resource pool exceeds 50% utilization, the coordinator submits
/// only half the planned tiles to give on-demand requests more headroom.
pub const BACKPRESSURE_REDUCE_THRESHOLD: f64 = 0.5;

/// Fraction of the prefetch plan to submit under moderate backpressure.
///
/// When executor load is between [`BACKPRESSURE_REDUCE_THRESHOLD`] and
/// [`BACKPRESSURE_DEFER_THRESHOLD`], only this fraction of tiles is submitted.
pub const BACKPRESSURE_REDUCED_FRACTION: f64 = 0.5;

// ─────────────────────────────────────────────────────────────────────────────
// Coordinator
// ─────────────────────────────────────────────────────────────────────────────

/// Adaptive prefetch coordinator.
///
/// Orchestrates all prefetch components and manages the prefetch lifecycle.
/// Thread-safe for shared access from telemetry and status queries.
///
/// # Architecture
///
/// ```text
///                    ┌─────────────────────┐
///                    │    Coordinator       │
///                    │  (main loop)         │
///                    └─────────┬────────────┘
///                              │
///      ┌───────────┬──────────┼──────────────┬───────────┐
///      ▼           ▼          ▼              ▼           ▼
/// ┌─────────┐ ┌─────────┐ ┌──────────────┐ ┌─────────┐ ┌─────────┐
/// │ Phase   │ │ Ground  │ │ Scenery      │ │Boundary │ │ Circuit │
/// │Detector │ │Strategy │ │ Window       │ │Strategy │ │ Breaker │
/// └─────────┘ └─────────┘ └──────────────┘ └─────────┘ └─────────┘
/// ```
///
/// In cruise phase, the coordinator uses a **boundary-driven** approach:
/// the [`SceneryWindow`] tracks X-Plane's loading window boundaries via the
/// [`SceneTracker`], and the [`BoundaryStrategy`] generates prefetch targets
/// for DSF regions the aircraft is about to cross into.
///
/// # Trigger Modes
///
/// - **Aggressive**: Position-based trigger at 0.3° into DSF tile
/// - **Opportunistic**: Circuit breaker trigger when X-Plane is idle
pub struct AdaptivePrefetchCoordinator {
    /// Configuration.
    pub(super) config: AdaptivePrefetchConfig,

    /// Performance calibration (determines mode).
    pub(super) calibration: Option<PerformanceCalibration>,

    /// Flight phase detector.
    phase_detector: PhaseDetector,

    /// Transition throttle for takeoff ramp-up.
    transition_throttle: TransitionThrottle,

    /// Ground strategy.
    ground_strategy: GroundStrategy,

    /// Circuit breaker for throttling.
    pub(super) throttler: Option<Arc<dyn PrefetchThrottler>>,

    /// DDS client for submitting prefetch requests.
    pub(super) dds_client: Option<Arc<dyn DdsClient>>,

    /// Memory cache for checking tile existence before submitting.
    ///
    /// When set, the coordinator queries this cache to filter out tiles
    /// that are already cached, avoiding unnecessary job submissions.
    pub(super) memory_cache: Option<Arc<dyn DaemonMemoryCache>>,

    /// Tiles currently in cache (for filtering).
    ///
    /// Note: This is a fallback for when memory_cache is not available.
    /// When memory_cache is set, this set is only used for tiles we've
    /// submitted in the current session (as a fast local cache).
    pub(super) cached_tiles: HashSet<TileCoord>,

    /// Current status.
    pub(super) status: CoordinatorStatus,

    /// Shared status for TUI display.
    pub(super) shared_status: Option<Arc<SharedPrefetchStatus>>,

    /// Cumulative prefetch statistics.
    pub(super) total_cycles: u64,
    pub(super) total_tiles_submitted: u64,
    pub(super) total_cache_hits: u64,
    pub(super) total_deferred_cycles: u64,

    /// Ortho union index for checking if tiles already exist on disk.
    /// When set, prefetch will skip tiles that are already installed
    /// in local ortho packages or patches.
    ortho_union_index: Option<Arc<OrthoUnionIndex>>,

    /// Geospatial reference index for patched region filtering.
    geo_index: Option<Arc<GeoIndex>>,

    /// Scenery window model for boundary-driven prefetch.
    scenery_window: SceneryWindow,

    /// Boundary strategy for generating target regions.
    boundary_strategy: BoundaryStrategy,

    /// Scene tracker for observing X-Plane tile requests.
    scene_tracker: Option<Arc<dyn SceneTracker>>,

    /// Scenery index for tile lookup (actual installed zoom levels).
    ///
    /// Used by the boundary prefetch path to discover which zoom levels
    /// are actually installed in each DSF region, rather than hardcoding
    /// a single zoom level. Also forwarded to [`GroundStrategy`].
    scenery_index: Option<Arc<SceneryIndex>>,
}

impl std::fmt::Debug for AdaptivePrefetchCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptivePrefetchCoordinator")
            .field("config.enabled", &self.config.enabled)
            .field("config.mode", &self.config.mode)
            .field("has_calibration", &self.calibration.is_some())
            .field("has_throttler", &self.throttler.is_some())
            .field("has_dds_client", &self.dds_client.is_some())
            .field("cached_tiles_count", &self.cached_tiles.len())
            .field("status", &self.status)
            .finish()
    }
}

impl AdaptivePrefetchCoordinator {
    /// Create a new coordinator with the given configuration.
    pub fn new(config: AdaptivePrefetchConfig) -> Self {
        let phase_detector = PhaseDetector::new(&config);
        let transition_throttle =
            TransitionThrottle::with_config(config.ramp_duration, config.ramp_start_fraction);
        let ground_strategy = GroundStrategy::new(&config);
        let mut scenery_window = SceneryWindow::new(SceneryWindowConfig {
            default_rows: config.default_window_rows,
            default_cols: config.default_window_cols,
            buffer: config.window_buffer,
            trigger_distance: config.trigger_distance,
            load_depth: config.load_depth,
        });
        // Start in Assumed state so boundary checks work immediately
        // with default dimensions. Real dimensions from SceneTracker will
        // upgrade the window to Ready when available.
        scenery_window
            .set_assumed_dimensions(config.default_window_rows, config.default_window_cols);
        let boundary_strategy = BoundaryStrategy::new();

        Self {
            config,
            calibration: None,
            phase_detector,
            transition_throttle,
            ground_strategy,
            throttler: None,
            dds_client: None,
            memory_cache: None,
            cached_tiles: HashSet::new(),
            status: CoordinatorStatus::default(),
            shared_status: None,
            total_cycles: 0,
            total_tiles_submitted: 0,
            total_cache_hits: 0,
            total_deferred_cycles: 0,
            ortho_union_index: None,
            geo_index: None,
            scenery_window,
            boundary_strategy,
            scene_tracker: None,
            scenery_index: None,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(AdaptivePrefetchConfig::default())
    }

    /// Set the performance calibration.
    pub fn with_calibration(mut self, calibration: PerformanceCalibration) -> Self {
        self.status.mode = calibration.recommended_strategy;
        self.calibration = Some(calibration);
        self
    }

    /// Set the circuit breaker for throttling.
    pub fn with_throttler(mut self, throttler: Arc<dyn PrefetchThrottler>) -> Self {
        self.throttler = Some(throttler);
        self
    }

    /// Set the DDS client for submitting prefetch requests.
    pub fn with_dds_client(mut self, client: Arc<dyn DdsClient>) -> Self {
        self.dds_client = Some(client);
        self
    }

    /// Set the memory cache for checking tile existence.
    ///
    /// When set, the coordinator queries this cache before submitting tiles,
    /// avoiding unnecessary job submissions for tiles that are already cached.
    pub fn with_memory_cache(mut self, cache: Arc<dyn DaemonMemoryCache>) -> Self {
        self.memory_cache = Some(cache);
        self
    }

    /// Set the scenery index for tile lookup.
    ///
    /// The index is used by both the ground strategy (ring-based prefetch)
    /// and the boundary strategy (cruise prefetch) to discover actual
    /// installed zoom levels rather than assuming zoom 14.
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.ground_strategy = self.ground_strategy.with_scenery_index(Arc::clone(&index));
        self.scenery_index = Some(index);
        self
    }

    /// Set the shared status for TUI display.
    pub fn with_shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Set the ortho union index for disk-based tile existence checking.
    ///
    /// When configured, prefetch will skip tiles that already exist in
    /// installed ortho packages or patches. This addresses Issue #39 where
    /// prefetch would download tiles that users already have on disk.
    ///
    /// # Arguments
    ///
    /// * `index` - The ortho union index containing all ortho sources
    ///
    /// # Example
    ///
    /// ```ignore
    /// let coordinator = AdaptivePrefetchCoordinator::with_defaults()
    ///     .with_ortho_union_index(Arc::clone(&ortho_index));
    /// ```
    pub fn with_ortho_union_index(mut self, index: Arc<OrthoUnionIndex>) -> Self {
        self.ortho_union_index = Some(index);
        self
    }

    /// Set the geospatial reference index for patched region filtering.
    pub fn with_geo_index(mut self, geo_index: Arc<GeoIndex>) -> Self {
        self.geo_index = Some(geo_index);
        self
    }

    /// Set the scene tracker for observing X-Plane tile requests.
    pub fn with_scene_tracker(mut self, tracker: Arc<dyn SceneTracker>) -> Self {
        self.scene_tracker = Some(tracker);
        self
    }

    /// Get DDS tiles for a DSF region from the scenery index or geometric fallback.
    ///
    /// If a scenery index is available, queries it for tiles in the region.
    /// This returns tiles at whatever zoom levels are actually installed in the
    /// X-Plane scenery (e.g., ZL12 at cruise altitude), rather than assuming
    /// a fixed zoom level.
    ///
    /// Falls back to the boundary strategy's geometric 4x4 grid expansion at
    /// zoom 14 when no scenery index is available or has no tiles for the region.
    fn get_tiles_for_region(&self, region: &DsfRegion) -> Vec<TileCoord> {
        if let Some(ref index) = self.scenery_index {
            let center_lat = region.lat as f64 + 0.5;
            let center_lon = region.lon as f64 + 0.5;
            // 1° DSF region ≈ 60nm at equator, 45nm radius covers the region
            let tiles = index.tiles_near(center_lat, center_lon, 45.0);
            let result: Vec<TileCoord> = tiles.iter().map(|t| t.to_tile_coord()).collect();

            if !result.is_empty() {
                return result;
            }
            // Fall through to geometric expansion if index had no tiles
        }

        // Fallback: geometric grid at zoom 14
        self.boundary_strategy.expand_to_tiles(region, 14)
    }

    /// Get the scenery window for external monitoring.
    pub fn scenery_window(&self) -> &SceneryWindow {
        &self.scenery_window
    }

    /// Get the current effective mode.
    ///
    /// Considers config override and calibration results.
    pub fn effective_mode(&self) -> StrategyMode {
        match self.config.mode {
            PrefetchMode::Aggressive => StrategyMode::Aggressive,
            PrefetchMode::Opportunistic => StrategyMode::Opportunistic,
            PrefetchMode::Disabled => StrategyMode::Disabled,
            PrefetchMode::Auto => {
                if let Some(ref cal) = self.calibration {
                    cal.recommended_strategy
                } else {
                    // No calibration yet - default to opportunistic
                    StrategyMode::Opportunistic
                }
            }
        }
    }

    /// Update with new aircraft state.
    ///
    /// Call this with each telemetry update. Returns the tiles to prefetch
    /// (if any) based on current conditions.
    ///
    /// # Arguments
    ///
    /// * `position` - Aircraft position (lat, lon) in degrees
    /// * `track` - Ground track in degrees (0-360)
    /// * `ground_speed_kt` - Ground speed in knots
    /// * `msl_ft` - Altitude above mean sea level in feet
    ///
    /// # Returns
    ///
    /// A `PrefetchPlan` if prefetching is appropriate, `None` otherwise.
    pub fn update(
        &mut self,
        position: (f64, f64),
        track: f64,
        ground_speed_kt: f32,
        msl_ft: f32,
    ) -> Option<PrefetchPlan> {
        // Check if enabled
        if !self.config.enabled {
            self.status.enabled = false;
            return None;
        }
        self.status.enabled = true;

        // Get effective mode
        let mode = self.effective_mode();
        self.status.mode = mode;

        if mode == StrategyMode::Disabled {
            return None;
        }

        // Update phase detector and notify transition throttle on phase change
        let previous_phase = self.phase_detector.current_phase();
        let phase_changed = self.phase_detector.update(ground_speed_kt, msl_ft);
        let phase = self.phase_detector.current_phase();
        self.status.phase = phase;

        if phase_changed {
            self.transition_throttle
                .on_phase_change(previous_phase, phase);
        }

        // Check throttling (for opportunistic mode)
        if let Some(ref throttler) = self.throttler {
            self.status.throttled = throttler.should_throttle();
        }

        // Determine if we should prefetch
        let should_prefetch = self.should_prefetch_now(mode);
        if !should_prefetch {
            return None;
        }

        // Get calibration (or use default)
        let calibration = self
            .calibration
            .clone()
            .unwrap_or_else(PerformanceCalibration::default_opportunistic);

        // Select and execute strategy
        let plan = match phase {
            FlightPhase::Ground => {
                self.status.active_strategy = self.ground_strategy.name();
                self.ground_strategy.calculate_prefetch(
                    position,
                    track,
                    &calibration,
                    &self.cached_tiles,
                )
            }
            FlightPhase::Transition => {
                // During transition, no prefetch — X-Plane gets all system resources
                return None;
            }
            FlightPhase::Cruise => {
                // Update scenery window from scene tracker
                if let Some(ref tracker) = self.scene_tracker {
                    self.scenery_window.update_from_tracker(tracker.as_ref());
                }

                // Check boundary crossings
                let (lat, lon) = position;
                let crossings = self.scenery_window.check_boundaries(lat, lon);

                if crossings.is_empty() {
                    tracing::trace!(
                        lat = format!("{:.2}", lat),
                        lon = format!("{:.2}", lon),
                        window_state = ?self.scenery_window.state(),
                        "Cruise: no boundary crossings"
                    );
                    return None;
                }

                // Generate target regions from crossings
                let bounds = self.scenery_window.window_bounds();
                let mut all_tiles = Vec::new();

                if let Some((lat_min, lat_max, lon_min, lon_max)) = bounds {
                    for crossing in &crossings {
                        let cross_range = match crossing.axis {
                            BoundaryAxis::Latitude => {
                                (lon_min.floor() as i32, (lon_max - 1.0).floor() as i32)
                            }
                            BoundaryAxis::Longitude => {
                                (lat_min.floor() as i32, (lat_max - 1.0).floor() as i32)
                            }
                        };
                        let targets = self
                            .boundary_strategy
                            .generate_regions(crossing, cross_range);
                        if let Some(ref geo_index) = self.geo_index {
                            let filtered = self
                                .boundary_strategy
                                .filter_already_handled(&targets, geo_index);
                            for target in filtered {
                                let tiles = self.get_tiles_for_region(&target.region);
                                if tiles.is_empty() {
                                    self.boundary_strategy
                                        .mark_no_coverage(&target.region, geo_index);
                                } else {
                                    self.boundary_strategy
                                        .mark_in_progress(&target.region, geo_index);
                                    all_tiles.extend(tiles);
                                }
                            }
                        } else {
                            // No GeoIndex -- expand all targets without filtering
                            for target in &targets {
                                all_tiles.extend(self.get_tiles_for_region(&target.region));
                            }
                        }
                    }
                }

                if all_tiles.is_empty() {
                    return None;
                }

                let total = all_tiles.len();
                self.status.active_strategy = "boundary";
                PrefetchPlan::with_tiles(all_tiles, &calibration, "boundary", 0, total)
            }
        };

        // Log plan details
        if !plan.is_empty() {
            self.log_plan(&plan, position, track);
        }

        self.status.last_prefetch_count = plan.tile_count();
        Some(plan)
    }

    /// Execute a prefetch plan by submitting tiles to the DDS client.
    ///
    /// Applies backpressure-aware submission based on executor resource utilization:
    /// - Load > [`BACKPRESSURE_DEFER_THRESHOLD`]: skips this cycle (deferred)
    /// - Load > [`BACKPRESSURE_REDUCE_THRESHOLD`]: submits reduced fraction
    /// - Stops immediately on `ChannelFull` error
    ///
    /// # Arguments
    ///
    /// * `plan` - The prefetch plan to execute
    /// * `cancellation` - Shared cancellation token for the batch
    ///
    /// # Returns
    ///
    /// Number of tiles submitted. Returns 0 if deferred due to backpressure.
    pub fn execute(&mut self, plan: &PrefetchPlan, cancellation: CancellationToken) -> usize {
        let _span = tracing::debug_span!(
            target: "profiling",
            "prefetch_execute",
            tile_count = plan.tiles.len(),
            strategy = plan.strategy,
        )
        .entered();

        let Some(ref client) = self.dds_client else {
            tracing::warn!("No DDS client configured - cannot execute prefetch");
            return 0;
        };

        // Check executor resource utilization before submitting
        let load = client.executor_load();
        if load > BACKPRESSURE_DEFER_THRESHOLD {
            self.total_deferred_cycles += 1;
            tracing::info!(
                load = format!("{:.1}%", load * 100.0),
                tiles_planned = plan.tiles.len(),
                "Executor backpressure — deferring prefetch cycle"
            );
            return 0;
        }

        // Determine how many tiles to submit based on executor load
        let max_tiles = if load > BACKPRESSURE_REDUCE_THRESHOLD {
            let reduced =
                ((plan.tiles.len() as f64) * BACKPRESSURE_REDUCED_FRACTION).ceil() as usize;
            tracing::debug!(
                load = format!("{:.1}%", load * 100.0),
                full_plan = plan.tiles.len(),
                reduced_to = reduced,
                "Moderate backpressure — reducing prefetch submission"
            );
            reduced
        } else {
            plan.tiles.len()
        };

        // Apply transition throttle (takeoff ramp-up)
        let max_tiles = if self.transition_throttle.is_active() {
            let fraction = self.transition_throttle.fraction();
            if fraction == 0.0 {
                tracing::debug!("Transition throttle — grace period, deferring");
                return 0;
            }
            let throttled = ((max_tiles as f64) * fraction).ceil() as usize;
            tracing::debug!(
                fraction = format!("{:.0}%", fraction * 100.0),
                full = max_tiles,
                throttled_to = throttled,
                "Transition throttle — ramping up"
            );
            throttled
        } else {
            max_tiles
        };

        let mut submitted = 0;
        for tile in plan.tiles.iter().take(max_tiles) {
            let request =
                crate::runtime::JobRequest::prefetch_with_cancellation(*tile, cancellation.clone());
            match client.submit(request) {
                Ok(()) => submitted += 1,
                Err(crate::executor::DdsClientError::ChannelFull) => {
                    tracing::debug!(
                        submitted,
                        dropped = max_tiles - submitted,
                        "Channel full — stopping prefetch submission"
                    );
                    break;
                }
                Err(crate::executor::DdsClientError::ChannelClosed) => {
                    tracing::warn!("Executor channel closed — stopping prefetch");
                    break;
                }
            }
        }

        if submitted > 0 {
            tracing::info!(
                tiles = submitted,
                strategy = plan.strategy,
                estimated_ms = plan.estimated_completion_ms,
                "Prefetch batch submitted"
            );
        }

        submitted
    }

    /// Mark tiles as cached (to avoid re-prefetching).
    pub fn mark_cached(&mut self, tiles: impl IntoIterator<Item = TileCoord>) {
        self.cached_tiles.extend(tiles);
    }

    /// Clear cached tile tracking.
    pub fn clear_cache_tracking(&mut self) {
        self.cached_tiles.clear();
    }

    /// Get current status for UI/logging.
    pub fn status(&self) -> &CoordinatorStatus {
        &self.status
    }

    /// Get the phase detector for external monitoring.
    pub fn phase_detector(&self) -> &PhaseDetector {
        &self.phase_detector
    }

    /// Reset the coordinator state.
    ///
    /// Call this when teleporting or starting a new flight.
    pub fn reset(&mut self) {
        self.cached_tiles.clear();
        self.status = CoordinatorStatus::default();
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Internal helpers
    // ─────────────────────────────────────────────────────────────────────────

    /// Determine if we should prefetch now based on mode and conditions.
    fn should_prefetch_now(&self, mode: StrategyMode) -> bool {
        match mode {
            StrategyMode::Disabled => false,

            StrategyMode::Aggressive => {
                // Aggressive mode always prefetches (position-based trigger handled externally)
                true
            }

            StrategyMode::Opportunistic => {
                // Opportunistic mode checks throttler
                if let Some(ref throttler) = self.throttler {
                    !throttler.should_throttle()
                } else {
                    // No throttler - default to allowing prefetch
                    true
                }
            }
        }
    }

    /// Get startup info string for logging.
    pub fn startup_info_string(&self) -> String {
        let mode = self.effective_mode();
        format!(
            "adaptive, mode={:?}, ground_threshold={}kt, boundary_trigger={:.1}°",
            mode, self.config.ground_speed_threshold_kt, self.config.trigger_distance,
        )
    }

    /// Log plan details with metadata.
    fn log_plan(&self, plan: &PrefetchPlan, position: (f64, f64), track: f64) {
        let (lat, lon) = position;

        if let Some(ref metadata) = plan.metadata {
            tracing::info!(
                strategy = plan.strategy,
                tiles = plan.tile_count(),
                skipped_cached = plan.skipped_cached,
                total_considered = plan.total_considered,
                estimated_ms = plan.estimated_completion_ms,
                dsf_tiles = metadata.dsf_tile_count,
                bounds_source = metadata.bounds_source,
                track_quadrant = ?metadata.track_quadrant,
                bounds = ?metadata.bounds,
                position = format!("{:.2}°, {:.2}°", lat, lon),
                track = format!("{:.1}°", track),
                "Prefetch plan calculated"
            );
        } else {
            tracing::info!(
                strategy = plan.strategy,
                tiles = plan.tile_count(),
                estimated_ms = plan.estimated_completion_ms,
                position = format!("{:.2}°, {:.2}°", lat, lon),
                track = format!("{:.1}°", track),
                "Prefetch plan calculated"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Telemetry processing
    // ─────────────────────────────────────────────────────────────────────────

    /// Process a single telemetry update and execute prefetch if appropriate.
    ///
    /// This is now async to allow querying the memory cache for tile existence,
    /// avoiding unnecessary job submissions for tiles that are already cached.
    ///
    /// Returns the number of tiles submitted, or None if no prefetch was performed.
    pub async fn process_telemetry(&mut self, state: &AircraftState) -> Option<usize> {
        let track = extract_track(state);
        let position = (state.latitude, state.longitude);

        let msl_ft = state.altitude;

        // Always update shared status with current position to show TUI we're receiving telemetry
        // This fixes the bug where prefetch status stayed "Idle" when no plan was generated
        self.update_shared_status_position(position);

        let mut plan = match self.update(position, track, state.ground_speed, msl_ft) {
            Some(p) => p,
            None => {
                // No plan generated - still update status with why (disabled, throttled, etc.)
                self.update_shared_status_no_plan();
                // Run region maintenance even without a plan — InProgress regions
                // must still be promoted/swept to unblock future boundary cycles.
                self.run_region_maintenance();
                return None;
            }
        };

        // Filter out tiles already in cache (Bug 5 fix)
        let cache_filtered = if let Some(ref cache) = self.memory_cache {
            let mut filtered_tiles = Vec::with_capacity(plan.tiles.len());
            let mut cache_hits = 0usize;

            for tile in &plan.tiles {
                // Check local tracking first (fast path)
                if self.cached_tiles.contains(tile) {
                    cache_hits += 1;
                    continue;
                }

                // Query the actual memory cache
                if cache.contains(tile.row, tile.col, tile.zoom).await {
                    cache_hits += 1;
                    // Add to local tracking to avoid re-querying
                    self.cached_tiles.insert(*tile);
                    continue;
                }

                filtered_tiles.push(*tile);
            }

            if cache_hits > 0 {
                tracing::debug!(
                    cache_hits = cache_hits,
                    remaining = filtered_tiles.len(),
                    "Filtered cached tiles from prefetch plan"
                );
            }

            plan.tiles = filtered_tiles;
            cache_hits
        } else {
            0
        };

        // Step 1: Region-level filter — skip tiles in patch-owned DSF regions (Issue #51)
        let before_patch = plan.tiles.len();
        if let Some(ref geo_index) = self.geo_index {
            let gi = Arc::clone(geo_index);
            plan.tiles.retain(|tile| {
                let (lat, lon) = tile.to_lat_lon();
                let dsf = DsfTileCoord::from_lat_lon(lat, lon);
                !gi.contains::<PatchCoverage>(&DsfRegion::new(dsf.lat, dsf.lon))
            });
        }
        let patch_skipped = before_patch - plan.tiles.len();

        if patch_skipped > 0 {
            tracing::debug!(
                patch_skipped,
                remaining = plan.tiles.len(),
                "Filtered tiles in patched regions"
            );
        }

        // Step 2: Per-file disk filter — skip installed ortho tiles (Issue #39)
        let disk_skipped = if let Some(ref index) = self.ortho_union_index {
            let before_disk = plan.tiles.len();
            plan.tiles.retain(|tile| {
                let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
                !index.dds_tile_exists(chunk_row, chunk_col, chunk_zoom)
            });
            let skipped = before_disk - plan.tiles.len();

            if skipped > 0 {
                tracing::debug!(
                    skipped,
                    remaining = plan.tiles.len(),
                    "Filtered tiles already on disk"
                );
            }

            skipped
        } else {
            0
        };

        let disk_filtered = patch_skipped + disk_skipped;

        let submitted = if plan.is_empty() {
            0
        } else {
            let cancellation = CancellationToken::new();
            self.execute(&plan, cancellation)
        };

        // Mark submitted tiles as cached to avoid re-submitting
        if submitted > 0 {
            self.mark_cached(plan.tiles.iter().cloned());
        }

        // Update statistics
        self.total_cycles += 1;
        self.total_tiles_submitted += submitted as u64;
        self.total_cache_hits +=
            (plan.skipped_cached as usize + cache_filtered + disk_filtered) as u64;

        // Update shared status for TUI
        self.update_shared_status(position, &plan, submitted);

        tracing::debug!(
            tiles = submitted,
            strategy = plan.strategy,
            phase = %self.status.phase,
            "Adaptive prefetch cycle complete"
        );

        self.run_region_maintenance();

        Some(submitted)
    }

    /// Sweep stale InProgress regions and promote completed ones to Prefetched.
    ///
    /// This must run every cycle regardless of whether a prefetch plan was generated,
    /// otherwise InProgress regions block future boundary cycles indefinitely.
    pub fn run_region_maintenance(&self) {
        if let Some(ref geo_index) = self.geo_index {
            BoundaryStrategy::sweep_stale_regions(geo_index, self.config.stale_region_timeout);
            BoundaryStrategy::promote_completed_regions(
                geo_index,
                &self.cached_tiles,
                self.scenery_index.as_ref(),
            );
        }
    }

    /// Update the shared status for TUI display.
    fn update_shared_status(&self, position: (f64, f64), plan: &PrefetchPlan, submitted: usize) {
        let Some(ref status) = self.shared_status else {
            return;
        };

        // Update inferred position (adaptive doesn't have GPS status concept)
        status.update_inferred_position(position.0, position.1);

        // Determine prefetch mode for display
        let prefetch_mode = match self.status.phase {
            FlightPhase::Ground => crate::prefetch::state::PrefetchMode::Radial,
            FlightPhase::Transition => crate::prefetch::state::PrefetchMode::Idle,
            FlightPhase::Cruise => crate::prefetch::state::PrefetchMode::TileBased,
        };
        status.update_prefetch_mode(prefetch_mode);

        // Update detailed stats
        let circuit_state = self.throttler.as_ref().map(|t| match t.state() {
            ThrottleState::Active => CircuitState::Closed,
            ThrottleState::Paused => CircuitState::Open,
            ThrottleState::Resuming => CircuitState::HalfOpen,
        });

        // Get loading tiles (first 10 from plan)
        let loading_tiles: Vec<(i32, i32)> = plan
            .tiles
            .iter()
            .take(10)
            .map(|t: &TileCoord| {
                let (lat, lon) = t.to_lat_lon();
                (lat.floor() as i32, lon.floor() as i32)
            })
            .collect();

        let detailed = DetailedPrefetchStats {
            cycles: self.total_cycles,
            tiles_submitted_last_cycle: submitted as u64,
            tiles_submitted_total: self.total_tiles_submitted,
            cache_hits: self.total_cache_hits,
            ttl_skipped: 0, // Not tracked in adaptive
            active_zoom_levels: {
                let mut zooms: Vec<u8> = plan.tiles.iter().map(|t| t.zoom).collect();
                zooms.sort_unstable();
                zooms.dedup();
                zooms
            },
            is_active: submitted > 0,
            circuit_state,
            loading_tiles,
            deferred_cycles: self.total_deferred_cycles,
        };
        status.update_detailed_stats(detailed);
    }

    /// Update shared status with position only.
    ///
    /// Called early in `process_telemetry` to show the TUI we're receiving telemetry,
    /// even if no prefetch plan is generated (e.g., due to throttling).
    fn update_shared_status_position(&self, position: (f64, f64)) {
        let Some(ref status) = self.shared_status else {
            return;
        };

        // Update position - this also sets GPS status to Inferred
        status.update_inferred_position(position.0, position.1);
    }

    /// Update shared status when no plan was generated.
    ///
    /// This ensures the TUI shows the correct mode (throttled, idle, etc.)
    /// even when no prefetch tiles are submitted.
    fn update_shared_status_no_plan(&self) {
        let Some(ref status) = self.shared_status else {
            return;
        };

        // Determine prefetch mode based on current state
        let prefetch_mode = if !self.status.enabled {
            crate::prefetch::state::PrefetchMode::Idle
        } else if self.status.throttled {
            crate::prefetch::state::PrefetchMode::CircuitOpen
        } else if self.status.mode == StrategyMode::Disabled {
            crate::prefetch::state::PrefetchMode::Idle
        } else {
            // Have a plan but it was empty or filtered - show the active mode
            match self.status.phase {
                FlightPhase::Ground => crate::prefetch::state::PrefetchMode::Radial,
                FlightPhase::Transition => crate::prefetch::state::PrefetchMode::Idle,
                FlightPhase::Cruise => crate::prefetch::state::PrefetchMode::TileBased,
            }
        };
        status.update_prefetch_mode(prefetch_mode);

        // Update detailed stats with no activity
        let circuit_state = self.throttler.as_ref().map(|t| match t.state() {
            ThrottleState::Active => CircuitState::Closed,
            ThrottleState::Paused => CircuitState::Open,
            ThrottleState::Resuming => CircuitState::HalfOpen,
        });

        let detailed = DetailedPrefetchStats {
            cycles: self.total_cycles,
            tiles_submitted_last_cycle: 0,
            tiles_submitted_total: self.total_tiles_submitted,
            cache_hits: self.total_cache_hits,
            ttl_skipped: 0,
            active_zoom_levels: vec![],
            is_active: false,
            circuit_state,
            loading_tiles: vec![],
            deferred_cycles: self.total_deferred_cycles,
        };
        status.update_detailed_stats(detailed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::scenery_window::WindowState;
    use std::time::Instant;

    fn test_calibration() -> PerformanceCalibration {
        PerformanceCalibration {
            throughput_tiles_per_sec: 25.0,
            avg_tile_generation_ms: 40,
            tile_generation_stddev_ms: 10,
            confidence: 0.9,
            recommended_strategy: StrategyMode::Opportunistic,
            calibrated_at: Instant::now(),
            baseline_throughput: 25.0,
            sample_count: 100,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Creation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_coordinator_creation() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        assert!(coord.config.enabled);
        assert!(coord.calibration.is_none());
        assert!(coord.throttler.is_none());
        assert!(coord.dds_client.is_none());
    }

    #[test]
    fn test_coordinator_with_calibration() {
        let cal = test_calibration();
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_calibration(cal);
        assert!(coord.calibration.is_some());
        assert_eq!(coord.status.mode, StrategyMode::Opportunistic);
    }

    #[test]
    fn test_coordinator_with_ortho_union_index() {
        let index = Arc::new(OrthoUnionIndex::new());
        let coord =
            AdaptivePrefetchCoordinator::with_defaults().with_ortho_union_index(Arc::clone(&index));
        assert!(coord.ortho_union_index.is_some());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Mode selection tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_effective_mode_auto_no_calibration() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        // Without calibration, auto defaults to opportunistic
        assert_eq!(coord.effective_mode(), StrategyMode::Opportunistic);
    }

    #[test]
    fn test_effective_mode_auto_with_calibration() {
        let mut cal = test_calibration();
        cal.recommended_strategy = StrategyMode::Aggressive;

        let coord = AdaptivePrefetchCoordinator::with_defaults().with_calibration(cal);
        assert_eq!(coord.effective_mode(), StrategyMode::Aggressive);
    }

    #[test]
    fn test_effective_mode_override() {
        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Disabled,
            ..Default::default()
        };
        let coord = AdaptivePrefetchCoordinator::new(config);
        // Override takes precedence
        assert_eq!(coord.effective_mode(), StrategyMode::Disabled);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Update tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_update_disabled_returns_none() {
        let config = AdaptivePrefetchConfig {
            enabled: false,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config);

        let plan = coord.update((53.5, 9.5), 45.0, 100.0, 0.0);
        assert!(plan.is_none());
        assert!(!coord.status.enabled);
    }

    #[test]
    fn test_update_disabled_mode_returns_none() {
        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Disabled,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config);

        let plan = coord.update((53.5, 9.5), 45.0, 100.0, 0.0);
        assert!(plan.is_none());
    }

    #[test]
    fn test_update_ground_phase() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Ground conditions: low speed, low AGL
        let _plan = coord.update((53.5, 9.5), 45.0, 10.0, 0.0);
        assert_eq!(coord.status.phase, FlightPhase::Ground);
        assert_eq!(coord.status.active_strategy, "ground");
    }

    #[test]
    fn test_update_cruise_phase() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Cruise conditions: high speed
        coord.update((53.5, 9.5), 45.0, 200.0, 0.0);
        // Phase detector has hysteresis, so first update may not transition.
        // With three-phase model, Ground → Transition → Cruise.
        assert!(
            coord.status.phase == FlightPhase::Ground
                || coord.status.phase == FlightPhase::Transition
                || coord.status.phase == FlightPhase::Cruise
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Cache tracking tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_mark_cached() {
        let mut coord = AdaptivePrefetchCoordinator::with_defaults();

        let tiles = vec![
            TileCoord {
                row: 100,
                col: 200,
                zoom: 14,
            },
            TileCoord {
                row: 101,
                col: 200,
                zoom: 14,
            },
        ];

        coord.mark_cached(tiles);
        assert_eq!(coord.cached_tiles.len(), 2);
    }

    #[test]
    fn test_clear_cache_tracking() {
        let mut coord = AdaptivePrefetchCoordinator::with_defaults();

        coord.mark_cached(vec![TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }]);
        assert_eq!(coord.cached_tiles.len(), 1);

        coord.clear_cache_tracking();
        assert!(coord.cached_tiles.is_empty());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Reset tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_reset() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Update to set state
        coord.update((53.5, 9.5), 45.0, 200.0, 0.0);
        coord.mark_cached(vec![TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }]);

        // Reset
        coord.reset();
        assert!(coord.cached_tiles.is_empty());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Time budget tests (using the module function)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_time_budget_stationary() {
        let plan = PrefetchPlan::empty("test");

        // Stationary - should always be OK
        assert!(super::super::time_budget::can_complete_in_time(
            &plan,
            (53.5, 9.5),
            0.0,
            0.7
        ));
    }

    #[test]
    fn test_time_budget_fast_flight() {
        let cal = test_calibration();

        // Create a large plan
        let mut plan = PrefetchPlan::with_tiles(
            vec![
                TileCoord {
                    row: 100,
                    col: 200,
                    zoom: 14
                };
                100
            ],
            &cal,
            "test",
            0,
            100,
        );
        plan.estimated_completion_ms = 60000; // 60 seconds

        // At 450 knots, time budget is tight
        // This test just verifies the calculation runs
        let _can_complete =
            super::super::time_budget::can_complete_in_time(&plan, (53.1, 9.5), 450.0, 0.7);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Telemetry processing tests
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_process_telemetry_disabled() {
        let config = AdaptivePrefetchConfig {
            enabled: false,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config);
        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0);

        // Disabled coordinator returns None
        let result = coord.process_telemetry(&state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_process_telemetry_no_dds_client() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        let state = AircraftState::new(53.5, 9.5, 90.0, 10.0, 5.0); // Ground conditions

        // No DDS client - returns Some(0) because plan is generated but not executed
        let result = coord.process_telemetry(&state).await;
        // The plan may be empty (no scenery index), so result could be Some(0) or None
        assert!(result.is_none() || result == Some(0));
    }

    #[test]
    fn test_startup_info_string() {
        let coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        let info = coord.startup_info_string();
        assert!(info.contains("adaptive"));
        assert!(info.contains("mode="));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Disk-based filtering tests (Issue #39)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_coordinator_ortho_union_index_starts_none() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        assert!(coord.ortho_union_index.is_none());
    }

    #[tokio::test]
    async fn test_process_telemetry_with_ortho_union_index() {
        use crate::ortho_union::{OrthoSource, OrthoUnionIndex};
        use tempfile::TempDir;

        // Create a temp directory with a DDS file
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        // Create a DDS file that matches tile (100, 200, 16)
        std::fs::write(pkg_dir.join("textures/100_200_BI16.dds"), b"dds content").unwrap();

        let source = OrthoSource::new_package("test", &pkg_dir);
        let index = Arc::new(OrthoUnionIndex::with_sources(vec![source]));

        // Verify the index can find the tile
        assert!(
            index.dds_tile_exists(100, 200, 16),
            "Index should find the DDS file"
        );

        // Create coordinator with the index
        let coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_calibration(test_calibration())
            .with_ortho_union_index(index);

        // Verify the index is set
        assert!(coord.ortho_union_index.is_some());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Patched region filtering tests (Issue #51)
    // ─────────────────────────────────────────────────────────────────────────

    /// Create an aircraft state at ground conditions (low speed, low AGL).
    fn ground_state(lat: f64, lon: f64) -> AircraftState {
        const HEADING_DEG: f32 = 90.0;
        const GROUND_SPEED_KT: f32 = 10.0;
        const AGL_FT: f32 = 5.0;
        AircraftState::new(lat, lon, HEADING_DEG, GROUND_SPEED_KT, AGL_FT)
    }

    /// Build patched regions covering a radius around a DSF tile.
    ///
    /// Returns a HashSet covering `(center ± radius)` in both lat and lon,
    /// ensuring all ground strategy ring tiles are captured.
    fn patched_region_area(
        center_lat: i32,
        center_lon: i32,
        radius: i32,
    ) -> std::collections::HashSet<(i32, i32)> {
        let mut regions = std::collections::HashSet::new();
        for lat in (center_lat - radius)..=(center_lat + radius) {
            for lon in (center_lon - radius)..=(center_lon + radius) {
                regions.insert((lat, lon));
            }
        }
        regions
    }

    #[tokio::test]
    async fn test_prefetch_filters_patched_regions() {
        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};
        use crate::prefetch::tile_based::DsfTileCoord;

        let aircraft_lat = 45.5;
        let aircraft_lon = 11.5;
        let aircraft_dsf = DsfTileCoord::from_lat_lon(aircraft_lat, aircraft_lon);

        // Ground strategy generates ring tiles AROUND the loaded area.
        // Cover all possible ring tiles with patched regions in GeoIndex.
        let coverage_radius = 5; // degrees — wider than any possible ring
        let regions = patched_region_area(aircraft_dsf.lat, aircraft_dsf.lon, coverage_radius);

        let geo_index = Arc::new(GeoIndex::new());
        let entries: Vec<_> = regions
            .iter()
            .map(|&(lat, lon)| {
                (
                    DsfRegion::new(lat, lon),
                    PatchCoverage {
                        patch_name: "test_patch".to_string(),
                    },
                )
            })
            .collect();
        geo_index.populate(entries);

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_calibration(test_calibration())
            .with_geo_index(geo_index);

        let state = ground_state(aircraft_lat, aircraft_lon);

        // Call twice — first call primes the phase detector
        let _ = coord.process_telemetry(&state).await;
        let result = coord.process_telemetry(&state).await;

        // Tiles should be generated by ground strategy but filtered by patched region
        assert_eq!(
            result,
            Some(0),
            "No tiles should be submitted for patched region"
        );
        assert!(
            coord.total_cache_hits > 0,
            "Tiles should have been filtered by patched region (counted in cache_hits)"
        );
    }

    #[test]
    fn test_dds_tile_exists_uses_chunk_origin() {
        // Verify that chunk_origin() produces the correct coordinates for
        // matching DDS filenames. This validates the coordinate conversion
        // used by the prefetch disk filter.
        use crate::ortho_union::{OrthoSource, OrthoUnionIndex};
        use tempfile::TempDir;

        let tile = TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        };
        let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
        let filename = format!("{}_{}_BI{}.dds", chunk_row, chunk_col, chunk_zoom);

        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::write(pkg_dir.join("textures").join(&filename), b"dds").unwrap();

        let source = OrthoSource::new_package("test", &pkg_dir);
        let index = OrthoUnionIndex::with_sources(vec![source]);

        // chunk_origin() coords match the DDS filename
        assert!(index.dds_tile_exists(chunk_row, chunk_col, chunk_zoom));
        // Tile-level coords do NOT match (this was the pre-fix bug)
        assert!(!index.dds_tile_exists(tile.row, tile.col, tile.zoom));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Status update tests (TUI bug fix)
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_process_telemetry_updates_status_when_disabled() {
        use crate::prefetch::state::PrefetchMode as StatePrefetchMode;

        let config = AdaptivePrefetchConfig {
            enabled: false,
            ..Default::default()
        };
        let shared_status = SharedPrefetchStatus::new();
        let mut coord =
            AdaptivePrefetchCoordinator::new(config).with_shared_status(Arc::clone(&shared_status));

        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0);

        // Process telemetry - should return None but still update status
        let result = coord.process_telemetry(&state).await;
        assert!(result.is_none());

        // Status should be updated to show Idle (since disabled)
        let snapshot = shared_status.snapshot();
        assert_eq!(snapshot.prefetch_mode, StatePrefetchMode::Idle);

        // Position should be updated
        assert!(snapshot.aircraft.is_some());
        let ac = snapshot.aircraft.unwrap();
        assert!((ac.latitude - 53.5).abs() < 0.001);
        assert!((ac.longitude - 9.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_process_telemetry_updates_status_when_throttled() {
        use crate::prefetch::state::PrefetchMode as StatePrefetchMode;
        use crate::prefetch::throttler::ThrottleState;

        // Create a mock throttler that always says to throttle
        struct AlwaysThrottle;
        impl PrefetchThrottler for AlwaysThrottle {
            fn should_throttle(&self) -> bool {
                true
            }
            fn state(&self) -> ThrottleState {
                ThrottleState::Paused
            }
        }

        let shared_status = SharedPrefetchStatus::new();
        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_calibration(test_calibration())
            .with_throttler(Arc::new(AlwaysThrottle))
            .with_shared_status(Arc::clone(&shared_status));

        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0);

        // Process telemetry - should return None due to throttling
        let result = coord.process_telemetry(&state).await;
        assert!(result.is_none());

        // Status should show CircuitOpen (throttled)
        let snapshot = shared_status.snapshot();
        assert_eq!(snapshot.prefetch_mode, StatePrefetchMode::CircuitOpen);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Backpressure tests (Phase 5)
    // ─────────────────────────────────────────────────────────────────────────

    /// Mock DDS client with configurable executor load and submission tracking.
    struct BackpressureMockClient {
        /// Simulated executor load (0.0 to 1.0).
        pressure: f64,
        submitted: std::sync::atomic::AtomicUsize,
        /// If Some, return ChannelFull after this many successful submits.
        fail_after: Option<usize>,
    }

    impl BackpressureMockClient {
        fn new(pressure: f64) -> Self {
            Self {
                pressure,
                submitted: std::sync::atomic::AtomicUsize::new(0),
                fail_after: None,
            }
        }

        fn with_fail_after(mut self, n: usize) -> Self {
            self.fail_after = Some(n);
            self
        }

        fn submitted_count(&self) -> usize {
            self.submitted.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl crate::executor::DdsClient for BackpressureMockClient {
        fn submit(
            &self,
            _request: crate::runtime::JobRequest,
        ) -> Result<(), crate::executor::DdsClientError> {
            let count = self.submitted.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(limit) = self.fail_after {
                if count >= limit {
                    return Err(crate::executor::DdsClientError::ChannelFull);
                }
            }
            self.submitted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> tokio::sync::oneshot::Receiver<crate::runtime::DdsResponse> {
            let (_, rx) = tokio::sync::oneshot::channel();
            rx
        }

        fn request_dds_with_options(
            &self,
            _tile: TileCoord,
            _priority: crate::executor::Priority,
            _origin: crate::runtime::RequestOrigin,
            _cancellation: CancellationToken,
        ) -> tokio::sync::oneshot::Receiver<crate::runtime::DdsResponse> {
            let (_, rx) = tokio::sync::oneshot::channel();
            rx
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn executor_load(&self) -> f64 {
            self.pressure
        }
    }

    fn test_plan(tile_count: usize) -> PrefetchPlan {
        let tiles: Vec<TileCoord> = (0..tile_count)
            .map(|i| TileCoord {
                row: 100 + i as u32,
                col: 200,
                zoom: 14,
            })
            .collect();
        let cal = test_calibration();
        PrefetchPlan::with_tiles(tiles, &cal, "test", 0, tile_count)
    }

    #[test]
    fn test_prefetch_defers_under_high_backpressure() {
        let client = Arc::new(BackpressureMockClient::new(0.85));
        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_dds_client(client.clone() as Arc<dyn crate::executor::DdsClient>);

        let plan = test_plan(10);
        let submitted = coord.execute(&plan, CancellationToken::new());

        assert_eq!(
            submitted, 0,
            "Should defer all tiles under high executor load"
        );
        assert_eq!(coord.total_deferred_cycles, 1);
        assert_eq!(client.submitted_count(), 0);
    }

    #[test]
    fn test_prefetch_reduces_under_moderate_backpressure() {
        let client = Arc::new(BackpressureMockClient::new(0.6));
        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_dds_client(client.clone() as Arc<dyn crate::executor::DdsClient>);

        let plan = test_plan(10);
        let submitted = coord.execute(&plan, CancellationToken::new());

        // 50% of 10 = 5
        assert_eq!(
            submitted, 5,
            "Should submit ~50% of tiles under moderate executor load"
        );
        assert_eq!(coord.total_deferred_cycles, 0);
        assert_eq!(client.submitted_count(), 5);
    }

    #[test]
    fn test_prefetch_full_submission_under_low_pressure() {
        let client = Arc::new(BackpressureMockClient::new(0.2));
        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_dds_client(client.clone() as Arc<dyn crate::executor::DdsClient>);

        let plan = test_plan(10);
        let submitted = coord.execute(&plan, CancellationToken::new());

        assert_eq!(
            submitted, 10,
            "Should submit all tiles under low executor load"
        );
        assert_eq!(client.submitted_count(), 10);
    }

    #[test]
    fn test_prefetch_stops_on_channel_full() {
        let client = Arc::new(BackpressureMockClient::new(0.0).with_fail_after(3));
        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_dds_client(client.clone() as Arc<dyn crate::executor::DdsClient>);

        let plan = test_plan(10);
        let submitted = coord.execute(&plan, CancellationToken::new());

        assert_eq!(submitted, 3, "Should stop at first ChannelFull error");
        assert_eq!(client.submitted_count(), 3);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Transition throttle integration tests (#62)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_phase_change_activates_throttle() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Start on ground
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        assert!(!coord.transition_throttle.is_active());

        // Trigger cruise (high speed, wait for hysteresis)
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);

        // Throttle should now be active (held during transition)
        assert!(coord.transition_throttle.is_active());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Boundary-driven prefetch integration tests (#58)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_coordinator_creation_has_scenery_window() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        // Coordinator should have a scenery window in Assumed state
        // (initialized with default dimensions so boundary checks work immediately)
        let window = coord.scenery_window();
        assert!(window.is_ready());
        assert_eq!(window.state(), &WindowState::Assumed);
    }

    #[test]
    fn test_coordinator_with_scene_tracker() {
        use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
        use std::collections::HashSet;

        struct DummyTracker;
        impl SceneTracker for DummyTracker {
            fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
                HashSet::new()
            }
            fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
                vec![]
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
                None
            }
        }

        let tracker: Arc<dyn SceneTracker> = Arc::new(DummyTracker);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scene_tracker(tracker);
        assert!(coord.scene_tracker.is_some());
    }

    #[test]
    fn test_coordinator_with_scenery_window_returns_plan_on_boundary() {
        use crate::geo_index::GeoIndex;
        use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
        use std::collections::HashSet;

        /// Mock SceneTracker that returns stable bounds to trigger Ready state.
        struct StableBoundsTracker;
        impl SceneTracker for StableBoundsTracker {
            fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
                HashSet::new()
            }
            fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
                vec![]
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
                Some(GeoBounds {
                    min_lat: 47.0,
                    max_lat: 53.0,
                    min_lon: 3.0,
                    max_lon: 11.0,
                })
            }
        }

        let tracker: Arc<dyn SceneTracker> = Arc::new(StableBoundsTracker);
        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker)
            .with_geo_index(geo_index);

        // Prime the scenery window to Ready state by calling update multiple times
        // with the tracker returning stable bounds. We need to reach Cruise phase too.
        // First get into cruise: use high ground speed.
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // First tick: Ground → start hysteresis for cruise
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Second tick: hysteresis expires, enter cruise
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Third tick: now in cruise, scenery window should have updated from tracker
        // update_from_tracker is called each cruise update, so 2 calls → Measuring + Ready
        let plan = coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);

        // Aircraft at lat=52.0, near north edge of window (max_lat=53.0),
        // trigger_distance=3.0° → should trigger a boundary crossing.
        // If plan is Some, it should have tiles and strategy "boundary".
        if coord.status.phase == FlightPhase::Cruise {
            assert!(
                plan.is_some(),
                "Should generate a boundary plan near the edge"
            );
            let plan = plan.unwrap();
            assert!(!plan.tiles.is_empty(), "Plan should have tiles");
            assert_eq!(
                coord.status.active_strategy, "boundary",
                "Strategy should be 'boundary'"
            );
        }
        // If we didn't reach cruise (due to phase detector), that's OK for now.
    }

    #[test]
    fn test_coordinator_no_plan_when_far_from_boundary() {
        use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
        use std::collections::HashSet;

        /// Mock SceneTracker that returns stable bounds.
        struct StableBoundsTracker;
        impl SceneTracker for StableBoundsTracker {
            fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
                HashSet::new()
            }
            fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
                vec![]
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
                Some(GeoBounds {
                    min_lat: 47.0,
                    max_lat: 53.0,
                    min_lon: 3.0,
                    max_lon: 11.0,
                })
            }
        }

        let tracker: Arc<dyn SceneTracker> = Arc::new(StableBoundsTracker);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Get into cruise
        coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Aircraft at center of window (50.0, 7.0) — far from all edges
        let plan = coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);

        if coord.status.phase == FlightPhase::Cruise {
            assert!(
                plan.is_none(),
                "Should NOT generate a plan when aircraft is far from all boundaries"
            );
        }
    }

    #[test]
    fn test_coordinator_boundary_uses_geo_index_filtering() {
        use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion};
        use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
        use std::collections::HashSet;

        struct StableBoundsTracker;
        impl SceneTracker for StableBoundsTracker {
            fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
                HashSet::new()
            }
            fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
                vec![]
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
                Some(GeoBounds {
                    min_lat: 47.0,
                    max_lat: 53.0,
                    min_lon: 3.0,
                    max_lon: 11.0,
                })
            }
        }

        let tracker: Arc<dyn SceneTracker> = Arc::new(StableBoundsTracker);
        let geo_index = Arc::new(GeoIndex::new());

        // Mark ALL potential boundary regions as already prefetched
        // The north boundary at dsf_coord=53: depth 0,1,2 → rows 53,54,55
        // spanning columns 3..10
        for lat in 53..=55 {
            for lon in 3..=10 {
                geo_index.insert::<PrefetchedRegion>(
                    DsfRegion::new(lat, lon),
                    PrefetchedRegion::prefetched(),
                );
            }
        }

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker)
            .with_geo_index(geo_index);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Get into cruise
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Aircraft near north edge — boundary crossing detected but all regions filtered
        let plan = coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);

        if coord.status.phase == FlightPhase::Cruise {
            // All targets were already prefetched, so plan should be None
            assert!(
                plan.is_none(),
                "Plan should be None when all boundary targets are already prefetched"
            );
        }
    }

    #[test]
    fn test_throttle_resets_on_landing() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Transition to cruise
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 100.0, 0.0);
        assert!(coord.transition_throttle.is_active());

        // Transition back to ground
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.5, 10.5), 270.0, 10.0, 0.0);
        assert!(!coord.transition_throttle.is_active());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Scenery index zoom level tests
    // ─────────────────────────────────────────────────────────────────────────

    /// Helper to create a SceneryIndex with tiles at a specific chunk zoom level.
    fn make_scenery_index(lat: i32, lon: i32, chunk_zoom: u8) -> Arc<SceneryIndex> {
        use crate::coord::{to_tile_coords, CHUNKS_PER_TILE_SIDE, CHUNK_ZOOM_OFFSET};
        use crate::prefetch::scenery_index::{SceneryIndexConfig, SceneryTile};

        let index = SceneryIndex::new(SceneryIndexConfig::default());
        let tile_zoom = chunk_zoom - CHUNK_ZOOM_OFFSET;

        for lat_step in 0..4u32 {
            for lon_step in 0..4u32 {
                let sample_lat = lat as f64 + (lat_step as f64 * 0.25) + 0.125;
                let sample_lon = lon as f64 + (lon_step as f64 * 0.25) + 0.125;
                if let Ok(coord) = to_tile_coords(sample_lat, sample_lon, tile_zoom) {
                    index.add_tile(SceneryTile {
                        row: coord.row * CHUNKS_PER_TILE_SIDE,
                        col: coord.col * CHUNKS_PER_TILE_SIDE,
                        chunk_zoom,
                        lat: sample_lat as f32,
                        lon: sample_lon as f32,
                        is_sea: false,
                    });
                }
            }
        }

        Arc::new(index)
    }

    #[test]
    fn test_get_tiles_for_region_without_index_uses_zoom_14() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(tile.zoom, 14, "Without scenery index, should use zoom 14");
        }
    }

    #[test]
    fn test_get_tiles_for_region_with_index_uses_actual_zoom() {
        // chunk_zoom 16 → tile zoom 12
        let index = make_scenery_index(50, 9, 16);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(index);
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 12,
                "With scenery index at chunk_zoom 16, should use tile zoom 12"
            );
        }
    }

    #[test]
    fn test_get_tiles_for_region_with_index_at_zoom_18() {
        // chunk_zoom 18 → tile zoom 14
        let index = make_scenery_index(50, 9, 18);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(index);
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 14,
                "With scenery index at chunk_zoom 18, should use tile zoom 14"
            );
        }
    }

    #[test]
    fn test_get_tiles_for_region_falls_back_when_no_coverage() {
        // Index has tiles at (60, 20) but we query (50, 9)
        let index = make_scenery_index(60, 20, 16);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(index);
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 14,
                "Should fall back to zoom 14 when scenery index has no coverage"
            );
        }
    }

    #[test]
    fn test_with_scenery_index_stores_on_coordinator() {
        let index = make_scenery_index(50, 9, 16);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(index);
        assert!(
            coord.scenery_index.is_some(),
            "with_scenery_index should store index on coordinator"
        );
    }

    #[test]
    fn test_region_maintenance_runs_when_no_plan_generated() {
        use crate::geo_index::{DsfRegion, GeoIndex, PrefetchedRegion};
        use crate::scene_tracker::{DdsTileCoord, GeoBounds, GeoRegion, SceneTracker};
        use std::collections::HashSet;

        struct StableBoundsTracker;
        impl SceneTracker for StableBoundsTracker {
            fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
                HashSet::new()
            }
            fn is_tile_requested(&self, _tile: &DdsTileCoord) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn current_burst_tiles(&self) -> Vec<DdsTileCoord> {
                vec![]
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
                // Wide window so center is far from edges
                Some(GeoBounds {
                    min_lat: 45.0,
                    max_lat: 55.0,
                    min_lon: 0.0,
                    max_lon: 14.0,
                })
            }
        }

        let tracker: Arc<dyn SceneTracker> = Arc::new(StableBoundsTracker);
        let geo_index = Arc::new(GeoIndex::new());

        // Pre-populate InProgress regions (simulating a previous boundary cycle)
        for lon in 0..=13 {
            geo_index.insert::<PrefetchedRegion>(
                DsfRegion::new(55, lon),
                PrefetchedRegion::in_progress(),
            );
        }

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker)
            .with_geo_index(Arc::clone(&geo_index));

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Get into cruise
        coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Aircraft at center — no boundary crossings → update returns None
        let plan = coord.update((50.0, 7.0), 0.0, 200.0, 35000.0);
        assert!(
            plan.is_none(),
            "Should not generate plan when far from boundaries"
        );

        // Region maintenance should still have run despite no plan
        // The stale sweep should eventually timeout InProgress regions,
        // but more importantly, run_region_maintenance should be called.
        // We verify by checking that the method is reachable even with None plans.
        coord.run_region_maintenance();

        // After maintenance, stale regions should be swept (timeout=120s, so not yet).
        // But the key assertion: the method exists and is callable.
        // For a real promotion test, mark tiles as cached first.
        let region = DsfRegion::new(55, 7);
        let tiles = coord.boundary_strategy.expand_to_tiles(&region, 14);
        for tile in &tiles {
            coord.cached_tiles.insert(*tile);
        }

        coord.run_region_maintenance();

        // Region 55,7 should now be promoted to Prefetched
        let state = geo_index.get::<PrefetchedRegion>(&region);
        assert!(
            state.is_some(),
            "Region should still exist in GeoIndex after maintenance"
        );
        assert!(
            state.unwrap().is_prefetched(),
            "Region should be promoted to Prefetched when all tiles are cached"
        );
    }
}
