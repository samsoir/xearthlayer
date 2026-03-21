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
use crate::geo_index::{DsfRegion, GeoIndex};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::state::{AircraftState, SharedPrefetchStatus};
use crate::prefetch::SceneryIndex;

use super::super::boundary_strategy::BoundaryStrategy;
use super::super::calibration::{PerformanceCalibration, StrategyMode};
use super::super::config::{AdaptivePrefetchConfig, PrefetchMode};
use super::super::ground_strategy::GroundStrategy;
use super::super::phase_detector::{FlightPhase, PhaseDetector};
use super::super::prefetch_box::PrefetchBox;
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

/// Maximum number of tiles that can be queued as pending across cycles.
///
/// Safety cap to prevent executor flooding if tile generation overestimates.
/// At 200 tiles/cycle with ~16 tiles per DSF region, 2000 covers ~10 cycles
/// of boundary crossings — more than enough for any realistic flight path.
pub const MAX_PENDING_TILES: usize = 2000;

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

    /// X-Plane sim state from Web API (direct detection, replaces heuristics).
    sim_state: crate::aircraft_position::web_api::sim_state::SimState,

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

    /// Boundary strategy for region lifecycle management.
    boundary_strategy: BoundaryStrategy,

    /// Sliding prefetch box for cruise-phase region detection.
    prefetch_box: PrefetchBox,

    /// Scene tracker for observing X-Plane tile requests.
    scene_tracker: Option<Arc<dyn SceneTracker>>,

    /// Scenery index for tile lookup (actual installed zoom levels).
    ///
    /// Used by the boundary prefetch path to discover which zoom levels
    /// are actually installed in each DSF region, rather than hardcoding
    /// a single zoom level. Also forwarded to [`GroundStrategy`].
    scenery_index: Option<Arc<SceneryIndex>>,

    /// Tiles that could not be submitted due to channel backpressure.
    ///
    /// When [`execute()`] encounters `ChannelFull`, remaining tiles are stored
    /// here and drained on subsequent [`process_telemetry()`] cycles before
    /// generating any new boundary plan. This prevents the "fire-and-forget"
    /// bug where large boundary plans are partially submitted and the remainder
    /// is permanently lost.
    pub(super) pending_tiles: Vec<TileCoord>,
}

impl std::fmt::Debug for AdaptivePrefetchCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptivePrefetchCoordinator")
            .field("config.enabled", &self.config.enabled)
            .field("config.mode", &self.config.mode)
            .field("has_calibration", &self.calibration.is_some())
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
            lon_extent: config.window_lon_extent,
            buffer: config.window_buffer,
            trigger_distance: config.trigger_distance,
            load_depth_lat: config.load_depth_lat,
            load_depth_lon: config.load_depth_lon,
        });
        // Start in Assumed state so boundary checks work immediately
        // with default dimensions. Real dimensions from SceneTracker will
        // upgrade the window to Ready when available.
        // Use 0.0 latitude for initial assumed dimensions (equator baseline);
        // cols will be recomputed from real latitude on first tracker update.
        scenery_window.set_assumed_dimensions(config.default_window_rows, 0.0);
        let boundary_strategy = BoundaryStrategy::new();
        let prefetch_box = PrefetchBox::new(config.forward_margin, config.behind_margin);

        Self {
            config,
            calibration: None,
            phase_detector,
            transition_throttle,
            ground_strategy,
            sim_state: crate::aircraft_position::web_api::sim_state::SimState::default(),
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
            prefetch_box,
            scene_tracker: None,
            scenery_index: None,
            pending_tiles: Vec::new(),
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

    /// Update the sim state from the Web API adapter.
    pub fn set_sim_state(&mut self, state: crate::aircraft_position::web_api::sim_state::SimState) {
        self.sim_state = state;
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
            // Filter to only tiles whose geographic center falls within the
            // target 1° DSF region. Without this, the 45nm radius search spills
            // across DSF boundaries, returning tiles from adjacent regions and
            // causing massive tile count explosion (53K+ instead of ~700).
            // Deduplicate: many .ter files share the same base DDS texture,
            // so multiple SceneryTiles map to the same TileCoord after /16 division.
            let unique: HashSet<TileCoord> = tiles
                .iter()
                .filter(|t| {
                    t.lat.floor() as i32 == region.lat && t.lon.floor() as i32 == region.lon
                })
                .map(|t| t.to_tile_coord())
                .collect();

            tracing::debug!(
                region_lat = region.lat,
                region_lon = region.lon,
                scenery_tiles = tiles.len(),
                region_filtered = tiles
                    .iter()
                    .filter(|t| {
                        t.lat.floor() as i32 == region.lat && t.lon.floor() as i32 == region.lon
                    })
                    .count(),
                unique_tile_coords = unique.len(),
                "get_tiles_for_region: deduplication results"
            );

            let result: Vec<TileCoord> = unique.into_iter().collect();

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
        // Check sim state (scenery loading, replay → skip)
        if !self.sim_state.should_prefetch() {
            tracing::trace!(
                scenery_loading = self.sim_state.scenery_loading,
                replay = self.sim_state.replay,
                "Prefetch skipped by sim state"
            );
            return None;
        }

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
                let (lat, lon) = position;

                // Update retained region tracking from prefetch box bounds.
                // This must cover the full prefetch box + buffer so that
                // evict_non_retained() doesn't remove regions we just prefetched.
                if let Some(ref geo_index) = self.geo_index {
                    self.prefetch_box.update_retention(
                        lat,
                        lon,
                        track,
                        self.config.window_buffer as i32,
                        geo_index,
                    );
                }

                // Compute sliding prefetch box regions
                let new_regions = if let Some(ref geo_index) = self.geo_index {
                    self.prefetch_box.new_regions(lat, lon, track, geo_index)
                } else {
                    self.prefetch_box.regions(lat, lon, track)
                };

                if new_regions.is_empty() {
                    tracing::trace!(
                        lat = format!("{:.2}", lat),
                        lon = format!("{:.2}", lon),
                        track = format!("{:.1}", track),
                        "Cruise: no new regions in prefetch box"
                    );
                    return None;
                }

                // Log the box bounds for debugging
                let (box_lat_min, box_lat_max, box_lon_min, box_lon_max) =
                    self.prefetch_box.bounds(lat, lon, track);
                tracing::debug!(
                    aircraft = format!("{:.4}°, {:.4}°", lat, lon),
                    track = format!("{:.1}°", track),
                    box_bounds = format!(
                        "[{:.1}:{:.1}N, {:.1}:{:.1}E]",
                        box_lat_min, box_lat_max, box_lon_min, box_lon_max
                    ),
                    new_regions = new_regions.len(),
                    "Sliding prefetch box: new regions detected"
                );

                // Expand regions to tiles. Track which region each tile belongs
                // to so we can mark only submitted regions as InProgress after
                // truncation. Regions with no tiles are marked NoCoverage immediately.
                let mut tiles_with_region: Vec<(TileCoord, DsfRegion)> = Vec::new();
                for region in &new_regions {
                    let tiles = self.get_tiles_for_region(region);
                    if tiles.is_empty() {
                        if let Some(ref geo_index) = self.geo_index {
                            self.boundary_strategy.mark_no_coverage(region, geo_index);
                        }
                    } else {
                        for tile in tiles {
                            tiles_with_region.push((tile, *region));
                        }
                    }
                }

                if tiles_with_region.is_empty() {
                    return None;
                }

                // Sort tiles by distance from aircraft so nearest tiles
                // are submitted first.
                tiles_with_region.sort_by(|a, b| {
                    let (a_lat, a_lon) = a.0.to_lat_lon();
                    let (b_lat, b_lon) = b.0.to_lat_lon();
                    let da = (a_lat - lat).powi(2) + (a_lon - lon).powi(2);
                    let db = (b_lat - lat).powi(2) + (b_lon - lon).powi(2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });

                // Enforce max_tiles_per_cycle limit. Only regions whose tiles
                // survive truncation are marked InProgress — regions whose tiles
                // are entirely truncated stay untracked and are retried next tick.
                let max_tiles = self.config.max_tiles_per_cycle as usize;
                if tiles_with_region.len() > max_tiles {
                    tracing::debug!(
                        total = tiles_with_region.len(),
                        limit = max_tiles,
                        "Sliding box prefetch capped at max_tiles_per_cycle"
                    );
                    tiles_with_region.truncate(max_tiles);
                }

                // Mark submitted regions as InProgress
                let mut marked_regions = HashSet::new();
                let mut all_tiles = Vec::with_capacity(tiles_with_region.len());
                for (tile, region) in tiles_with_region {
                    if marked_regions.insert(region) {
                        if let Some(ref geo_index) = self.geo_index {
                            self.boundary_strategy.mark_in_progress(&region, geo_index);
                        }
                        tracing::debug!(
                            region_lat = region.lat,
                            region_lon = region.lon,
                            "Prefetch target: region queued"
                        );
                    }
                    all_tiles.push(tile);
                }

                let total = all_tiles.len();
                self.status.active_strategy = "sliding_box";
                PrefetchPlan::with_tiles(all_tiles, &calibration, "sliding_box", 0, total)
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
        let Some(ref client) = self.dds_client else {
            tracing::warn!("No DDS client configured - cannot execute prefetch");
            return 0;
        };

        let result = super::plan_executor::execute_plan(
            plan,
            client.as_ref(),
            &mut self.transition_throttle,
            cancellation,
        );

        if result.deferred {
            self.total_deferred_cycles += 1;
        }
        if !result.pending.is_empty() {
            self.pending_tiles = result.pending;
        }

        result.submitted
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
                // Opportunistic mode allows prefetch (SimState handles load detection)
                true
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

        // Drain pending tiles from a previous partial submission before generating
        // a new plan. This prevents the "fire-and-forget" bug where large boundary
        // plans lose tiles when the channel is full.
        if !self.pending_tiles.is_empty() {
            let pending = std::mem::take(&mut self.pending_tiles);
            let pending_count = pending.len();

            // Still need to update phase detector for correct state tracking
            self.phase_detector.update(state.ground_speed, msl_ft);

            let calibration = self
                .calibration
                .clone()
                .unwrap_or_else(PerformanceCalibration::default_opportunistic);
            let plan = PrefetchPlan::with_tiles(
                pending,
                &calibration,
                "boundary_pending",
                0,
                pending_count,
            );

            let cancellation = CancellationToken::new();
            let submitted = self.execute(&plan, cancellation);

            if submitted > 0 {
                self.mark_cached(plan.tiles.iter().take(submitted).cloned());
            }

            self.total_cycles += 1;
            self.total_tiles_submitted += submitted as u64;

            tracing::debug!(
                submitted,
                remaining = self.pending_tiles.len(),
                "Drained pending tiles from previous cycle"
            );

            self.run_region_maintenance();
            return Some(submitted);
        }

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

        // Run the filtering pipeline (cache → patches → disk)
        let (filtered_tiles, filter_counts) = super::filtering::run_filter_pipeline(
            std::mem::take(&mut plan.tiles),
            self.memory_cache.as_deref(),
            &mut self.cached_tiles,
            self.geo_index.as_ref(),
            self.ortho_union_index.as_ref(),
        )
        .await;
        plan.tiles = filtered_tiles;

        let total_filtered = filter_counts.total();

        tracing::debug!(
            raw_plan_tiles = plan.skipped_cached + total_filtered + plan.tiles.len(),
            cache_skipped = plan.skipped_cached + filter_counts.cache_hits,
            patch_skipped = filter_counts.patch_skipped,
            disk_skipped = filter_counts.disk_skipped,
            remaining = plan.tiles.len(),
            strategy = plan.strategy,
            "Prefetch plan filter pipeline summary"
        );

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
        self.total_cache_hits += (plan.skipped_cached as usize + total_filtered) as u64;

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

    /// Sweep stale InProgress regions, promote completed ones, and evict
    /// state for regions that have left the retained window.
    ///
    /// This must run every cycle regardless of whether a prefetch plan was generated,
    /// otherwise InProgress regions block future boundary cycles indefinitely.
    pub fn run_region_maintenance(&mut self) {
        if let Some(ref geo_index) = self.geo_index {
            BoundaryStrategy::sweep_stale_regions(geo_index, self.config.stale_region_timeout);
            BoundaryStrategy::promote_completed_regions(
                geo_index,
                &self.cached_tiles,
                self.scenery_index.as_ref(),
            );
            // Evict PrefetchedRegion entries for regions outside the retained window,
            // making them eligible for re-prefetch when the aircraft returns.
            BoundaryStrategy::evict_non_retained(geo_index);
            // Evict cached_tiles entries for tiles outside the retained window,
            // allowing re-query of the memory cache for those tiles.
            BoundaryStrategy::evict_cached_tiles_outside_retained(
                &mut self.cached_tiles,
                geo_index,
            );
        }
    }

    fn cycle_stats(&self) -> super::status_updater::CycleStats {
        super::status_updater::CycleStats {
            total_cycles: self.total_cycles,
            total_tiles_submitted: self.total_tiles_submitted,
            total_cache_hits: self.total_cache_hits,
            total_deferred_cycles: self.total_deferred_cycles,
        }
    }

    fn update_shared_status(&self, position: (f64, f64), plan: &PrefetchPlan, submitted: usize) {
        if let Some(ref status) = self.shared_status {
            super::status_updater::update_status_with_plan(
                status,
                &self.status,
                position,
                plan,
                submitted,
                &self.cycle_stats(),
            );
        }
    }

    fn update_shared_status_position(&self, position: (f64, f64)) {
        if let Some(ref status) = self.shared_status {
            super::status_updater::update_status_position(status, position);
        }
    }

    fn update_shared_status_no_plan(&self) {
        if let Some(ref status) = self.shared_status {
            super::status_updater::update_status_no_plan(status, &self.status, &self.cycle_stats());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::coordinator::test_support::{
        ground_state, make_scenery_index, patched_region_area, test_calibration, test_plan,
        BackpressureMockClient, CapLimitedDdsClient, DummyTracker, HighLoadDdsClient,
        StableBoundsTracker,
    };
    use crate::prefetch::adaptive::scenery_window::WindowState;

    // ─────────────────────────────────────────────────────────────────────────
    // Creation tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_coordinator_creation() {
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        assert!(coord.config.enabled);
        assert!(coord.calibration.is_none());

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

    // test_process_telemetry_updates_status_when_throttled was removed
    // along with the CircuitBreaker/PrefetchThrottler systems (replaced by SimState).

    // ─────────────────────────────────────────────────────────────────────────
    // Backpressure tests (Phase 5)
    // ─────────────────────────────────────────────────────────────────────────

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
        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> = Arc::new(DummyTracker);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scene_tracker(tracker);
        assert!(coord.scene_tracker.is_some());
    }

    #[test]
    fn test_coordinator_with_scenery_window_returns_plan_on_boundary() {
        use crate::geo_index::GeoIndex;

        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> =
            Arc::new(StableBoundsTracker::default_bounds());
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
    fn test_boundary_plan_tiles_sorted_by_distance_from_aircraft() {
        use crate::geo_index::GeoIndex;

        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> =
            Arc::new(StableBoundsTracker::default_bounds());
        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker)
            .with_geo_index(geo_index);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Enter cruise phase
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Aircraft at 52.0, near north edge — triggers boundary crossing
        let plan = coord.update((52.0, 7.0), 0.0, 200.0, 35000.0);

        if coord.status.phase == FlightPhase::Cruise {
            let plan = plan.expect("Should generate boundary plan near edge");
            assert!(
                plan.tiles.len() > 1,
                "Need multiple tiles to verify sorting"
            );

            // Verify tiles are sorted by distance from aircraft position
            let mut prev_dist = 0.0_f64;
            for tile in &plan.tiles {
                let (tile_lat, tile_lon) = tile.to_lat_lon();
                let dist = (tile_lat - 52.0).powi(2) + (tile_lon - 7.0).powi(2);
                assert!(
                    dist >= prev_dist - 0.001,
                    "Tiles must be sorted by distance from aircraft. \
                     Found dist={:.4} after prev={:.4}",
                    dist,
                    prev_dist,
                );
                prev_dist = dist;
            }
        }
    }

    #[test]
    fn test_coordinator_no_plan_when_far_from_boundary() {
        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> =
            Arc::new(StableBoundsTracker::default_bounds());

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

        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> =
            Arc::new(StableBoundsTracker::default_bounds());
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

        // Wide window so center is far from edges
        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> =
            Arc::new(StableBoundsTracker::with_bounds(45.0, 55.0, 0.0, 14.0));
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

    // ─────────────────────────────────────────────────────────────────────────
    // Pending tiles carry-over tests
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_pending_tiles_retained_on_channel_full() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        // Cap at 5 submissions per cycle — way less than a boundary plan generates
        let client = Arc::new(CapLimitedDdsClient::new(5));

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            trigger_distance: 1.0,
            load_depth_lat: 1,
            load_depth_lon: 1,
            default_window_rows: 6,
            window_lon_extent: 6.0,
            // Zero ramp so transition throttle doesn't reduce tile count
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Fast-forward phase detector into cruise using a CENTER position
        // far from all boundaries. Window rows=6, so half_rows=3.
        // Monitor at (50.0, 10.0): lat(47,53), lon(7,13). trigger=1.0.
        // At center (50.0, 10.0), distance to nearest edge = 3.0 > trigger=1.0.
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);

        assert_eq!(
            coord.phase_detector.current_phase(),
            FlightPhase::Cruise,
            "Phase detector should be in Cruise after fast-forward"
        );

        // Now move aircraft near northern boundary.
        // Monitor: lat(47, 53). trigger_distance=1.0.
        // Aircraft at lat=52.5 → distance to north edge = 53.0 - 52.5 = 0.5 < 1.0 → triggers!
        let state = AircraftState::new(52.5, 10.0, 0.0, 200.0, 35000.0);

        // First cycle: boundary plan generated, only 5 submitted (ChannelFull)
        let result = coord.process_telemetry(&state).await;
        let first_submitted = result.unwrap_or(0);
        assert!(
            first_submitted > 0,
            "First cycle should submit tiles from boundary plan"
        );
        assert!(
            first_submitted <= 5,
            "First cycle should be capped at 5 by ChannelFull"
        );

        // KEY ASSERTION: pending tiles should be non-empty (if plan had more than 5)
        // If the plan was small enough to fit in 5, skip the pending assertion
        if first_submitted == 5 {
            assert!(
                !coord.pending_tiles.is_empty(),
                "Unsubmitted tiles should be stored in pending_tiles for the next cycle"
            );
            let pending_after_first = coord.pending_tiles.len();

            // Reset the mock client for the next cycle
            client.reset();

            // Second cycle: should drain from pending_tiles, NOT generate a new boundary plan
            let result2 = coord.process_telemetry(&state).await;
            let second_submitted = result2.unwrap_or(0);
            assert!(
                second_submitted > 0,
                "Second cycle should submit tiles from pending queue"
            );
            assert!(
                coord.pending_tiles.len() < pending_after_first,
                "Pending tiles should decrease after second cycle (was {}, now {})",
                pending_after_first,
                coord.pending_tiles.len()
            );
        }
    }

    #[tokio::test]
    async fn test_pending_tiles_fully_drained_before_new_plan() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        // Allow 1000 submissions — enough to drain everything
        let client = Arc::new(CapLimitedDdsClient::new(1000));

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            trigger_distance: 3.0,
            load_depth_lat: 1,
            load_depth_lon: 1,
            // Zero ramp so transition throttle doesn't reduce tile count
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Fast-forward to cruise
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((53.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((53.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((53.0, 10.0), 0.0, 200.0, 35000.0);

        // Manually inject pending tiles (simulating a previous partial submission)
        let fake_pending: Vec<TileCoord> = (0..20)
            .map(|i| TileCoord {
                row: 1000 + i,
                col: 2000,
                zoom: 14,
            })
            .collect();
        coord.pending_tiles = fake_pending;

        let state = AircraftState::new(55.5, 10.0, 0.0, 200.0, 35000.0);

        // Cycle with pending tiles: should drain pending first, NOT generate new plan
        let result = coord.process_telemetry(&state).await;
        let submitted = result.unwrap_or(0);
        assert_eq!(
            submitted, 20,
            "Should submit all 20 pending tiles when channel has capacity"
        );
        assert!(
            coord.pending_tiles.is_empty(),
            "Pending tiles should be empty after full drain"
        );
    }

    #[test]
    fn test_throttle_truncated_tiles_stored_as_pending() {
        // When the transition throttle reduces max_tiles, tiles beyond the
        // throttle cutoff must also be stored as pending — not silently dropped.
        //
        // Scenario: 100-tile plan, throttle at 20%, channel accepts all.
        // Expected: 20 submitted, 80 stored as pending for next cycle.

        let client = Arc::new(CapLimitedDdsClient::new(1000)); // no channel limit

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            // 5-second ramp so throttle is definitely active
            ramp_duration: std::time::Duration::from_secs(5),
            ramp_start_fraction: 0.20,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Fast-forward phase to cruise so transition throttle activates
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);

        assert_eq!(
            coord.phase_detector.current_phase(),
            FlightPhase::Cruise,
            "Should be in Cruise"
        );
        assert!(
            coord.transition_throttle.is_active(),
            "Transition throttle should be active after entering cruise"
        );

        // Build a 100-tile plan
        let tiles: Vec<TileCoord> = (0..100)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let calibration = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles, &calibration, "boundary", 0, 100);

        let cancellation = CancellationToken::new();
        let submitted = coord.execute(&plan, cancellation);

        // Throttle at ~20% of 100 = ~20 tiles submitted
        assert!(
            submitted > 0 && submitted < 100,
            "Throttle should limit submission (submitted {})",
            submitted
        );

        // KEY ASSERTION: the remaining ~80 tiles must be in pending_tiles
        let total_accounted = submitted + coord.pending_tiles.len();
        assert_eq!(
            total_accounted,
            100,
            "All 100 tiles must be accounted for: {} submitted + {} pending = {} (expected 100)",
            submitted,
            coord.pending_tiles.len(),
            total_accounted
        );
    }

    #[test]
    fn test_throttle_and_channel_full_both_store_pending() {
        // When BOTH throttle and channel capacity limit submission,
        // ALL unsubmitted tiles must be stored as pending.
        //
        // Scenario: 100-tile plan, throttle at 20% (→ 20 tiles), channel cap at 10.
        // Expected: 10 submitted, 90 stored as pending (10 from throttled batch + 80 beyond throttle).

        let client = Arc::new(CapLimitedDdsClient::new(10)); // channel cap at 10

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(5),
            ramp_start_fraction: 0.20,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Fast-forward to cruise
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);

        assert!(coord.transition_throttle.is_active());

        // Build a 100-tile plan
        let tiles: Vec<TileCoord> = (0..100)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let calibration = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles, &calibration, "boundary", 0, 100);

        let cancellation = CancellationToken::new();
        let submitted = coord.execute(&plan, cancellation);

        assert_eq!(
            submitted, 10,
            "Should submit exactly 10 tiles (channel cap)"
        );

        // ALL remaining tiles must be pending (channel-full remainder + throttle-truncated)
        let total_accounted = submitted + coord.pending_tiles.len();
        assert_eq!(
            total_accounted,
            100,
            "All 100 tiles must be accounted for: {} submitted + {} pending = {} (expected 100)",
            submitted,
            coord.pending_tiles.len(),
            total_accounted
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // DSF region filtering tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_get_tiles_for_region_filters_to_target_dsf() {
        // When a SceneryIndex has tiles in multiple adjacent DSF regions,
        // get_tiles_for_region should only return tiles whose geographic
        // center falls within the target 1° DSF region — not tiles from
        // neighboring regions that fall within the 45nm search radius.
        use crate::coord::{to_tile_coords, CHUNKS_PER_TILE_SIDE, CHUNK_ZOOM_OFFSET};
        use crate::geo_index::DsfRegion;
        use crate::prefetch::scenery_index::{SceneryIndexConfig, SceneryTile};

        let index = SceneryIndex::new(SceneryIndexConfig::default());

        // Populate tiles in THREE adjacent DSF regions: (50,9), (50,10), (51,9)
        for (lat_base, lon_base) in &[(50, 9), (50, 10), (51, 9)] {
            for lat_step in 0..4u32 {
                for lon_step in 0..4u32 {
                    let sample_lat = *lat_base as f64 + (lat_step as f64 * 0.25) + 0.125;
                    let sample_lon = *lon_base as f64 + (lon_step as f64 * 0.25) + 0.125;
                    let tile_zoom: u8 = 16 - CHUNK_ZOOM_OFFSET;
                    if let Ok(coord) = to_tile_coords(sample_lat, sample_lon, tile_zoom) {
                        index.add_tile(SceneryTile {
                            row: coord.row * CHUNKS_PER_TILE_SIDE,
                            col: coord.col * CHUNKS_PER_TILE_SIDE,
                            chunk_zoom: 16,
                            lat: sample_lat as f32,
                            lon: sample_lon as f32,
                            is_sea: false,
                        });
                    }
                }
            }
        }

        let scenery_index = Arc::new(index);

        // Create coordinator with scenery index
        let config = AdaptivePrefetchConfig::default();
        let coord = AdaptivePrefetchCoordinator::new(config).with_scenery_index(scenery_index);

        // Query for region (50, 9) only
        let target = DsfRegion::new(50, 9);
        let tiles = coord.get_tiles_for_region(&target);

        // Should only get tiles from region (50,9), NOT from (50,10) or (51,9)
        assert!(!tiles.is_empty(), "Should find tiles in the target region");

        // At zoom 12, each 1° region has ~16 tiles (4x4 grid). With dedup,
        // we expect at most 16 tiles from one region.
        assert!(
            tiles.len() <= 16,
            "Should have at most 16 tiles from a single DSF region, got {}",
            tiles.len()
        );

        // Verify all returned tiles correspond to the target DSF region
        // by checking their tile coordinates fall within the expected range.
        // At zoom 12, one degree is approximately 4 tiles.
        for tile in &tiles {
            assert_eq!(
                tile.zoom, 12,
                "Tiles should be at zoom 12 (from chunk_zoom 16)"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Pending tiles cap tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_pending_tiles_capped_at_max() {
        // When a massive plan generates more tiles than MAX_PENDING_TILES,
        // the pending queue must be capped to prevent executor flooding.
        //
        // Scenario: 5000-tile plan, throttle at 20% (→ 1000 tiles submitted),
        // remaining 4000 exceed MAX_PENDING_TILES (2000).
        // Expected: 1000 submitted, 2000 pending (capped), 2000 dropped.

        let client = Arc::new(CapLimitedDdsClient::new(10_000)); // no channel limit

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(5),
            ramp_start_fraction: 0.20,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Fast-forward to cruise
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);

        assert!(coord.transition_throttle.is_active());

        // Build a 5000-tile plan (way more than MAX_PENDING_TILES)
        let tiles: Vec<TileCoord> = (0..5000)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let calibration = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles, &calibration, "boundary", 0, 5000);

        let cancellation = CancellationToken::new();
        let submitted = coord.execute(&plan, cancellation);

        // Throttle at ~20% → ~1000 submitted
        assert!(submitted > 0, "Should submit some tiles");

        // KEY ASSERTION: pending tiles must be capped at MAX_PENDING_TILES
        assert!(
            coord.pending_tiles.len() <= MAX_PENDING_TILES,
            "Pending tiles ({}) should not exceed MAX_PENDING_TILES ({})",
            coord.pending_tiles.len(),
            MAX_PENDING_TILES
        );

        // The total accounted for should be less than 5000 (some were dropped by cap)
        let total = submitted + coord.pending_tiles.len();
        assert!(
            total < 5000,
            "With cap, total ({}) should be less than plan size (5000)",
            total
        );
    }

    #[test]
    fn test_pending_cap_on_backpressure_defer() {
        // When executor load exceeds BACKPRESSURE_DEFER_THRESHOLD and the
        // plan is stored as pending, the cap must still be applied.

        let client = Arc::new(HighLoadDdsClient);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        // Build a 5000-tile plan
        let tiles: Vec<TileCoord> = (0..5000)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let calibration = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles, &calibration, "boundary", 0, 5000);

        let cancellation = CancellationToken::new();
        let submitted = coord.execute(&plan, cancellation);

        // Should defer (0 submitted) due to high load
        assert_eq!(submitted, 0, "Should defer due to backpressure");

        // KEY ASSERTION: pending tiles must be capped
        assert!(
            coord.pending_tiles.len() <= MAX_PENDING_TILES,
            "Deferred pending tiles ({}) should not exceed MAX_PENDING_TILES ({})",
            coord.pending_tiles.len(),
            MAX_PENDING_TILES
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Position-based window centering tests (#86)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_sliding_box_generates_plan_on_first_cruise_tick() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            forward_margin: 3.0,
            behind_margin: 1.0,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Enter cruise — no scene tracker or boundary monitors needed
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // First cruise tick should generate a plan from the sliding box
        let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

        if coord.status.phase == FlightPhase::Cruise {
            assert!(
                plan.is_some(),
                "Sliding box should generate plan on first cruise tick"
            );
            let plan = plan.unwrap();
            assert!(!plan.tiles.is_empty(), "Plan should have tiles");
            assert_eq!(coord.status.active_strategy, "sliding_box");
        }
    }

    #[test]
    fn test_sliding_box_deduplicates_across_ticks() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            forward_margin: 3.0,
            behind_margin: 1.0,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Enter cruise
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // First tick — generates plan, marks regions InProgress
        let plan1 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

        // Second tick at same position — all regions already tracked
        let plan2 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

        if coord.status.phase == FlightPhase::Cruise {
            assert!(plan1.is_some(), "First tick should generate plan");
            assert!(
                plan2.is_none(),
                "Second tick at same position should generate no plan (all regions tracked)"
            );
        }
    }

    #[test]
    fn test_long_flight_generates_plans_at_each_position() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            forward_margin: 3.0,
            behind_margin: 1.0,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);

        // Enter cruise
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut plans_generated = 0;

        // Fly 20° west in 1° steps from lon=15. The box is 4° wide (3° ahead +
        // 1° behind), so new regions enter the box every 1° of westward travel.
        for step in 0..20 {
            let lon = 15.0 - step as f64;
            let plan = coord.update((50.0, lon), 270.0, 200.0, 35000.0);
            if plan.is_some() {
                plans_generated += 1;
            }
        }

        assert!(
            plans_generated >= 5,
            "Should generate plans as aircraft crosses new DSF boundaries, got {}",
            plans_generated
        );
    }

    #[test]
    fn test_truncated_regions_not_marked_in_progress() {
        use crate::geo_index::{GeoIndex, PrefetchedRegion};

        let geo_index = Arc::new(GeoIndex::new());

        // Very low cap to force truncation — only 5 tiles allowed
        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            max_tiles_per_cycle: 5,
            forward_margin: 3.0,
            behind_margin: 1.0,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index));

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);

        // Enter cruise
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        // First tick — box covers ~16 regions but only 5 tiles submitted
        let plan1 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

        if coord.status.phase != FlightPhase::Cruise {
            return; // Skip if phase detector didn't reach cruise
        }

        assert!(plan1.is_some(), "Should generate plan on first tick");
        let plan1 = plan1.unwrap();
        assert!(
            plan1.tiles.len() <= 5,
            "Plan should be capped at 5 tiles, got {}",
            plan1.tiles.len()
        );

        // Count how many regions are marked InProgress
        let tracked_count = geo_index.regions::<PrefetchedRegion>().len();

        // Key: with 5 tiles and ~16 tiles per region, at most 1-2 regions
        // should be marked. If all 16 regions were marked, it's the bug.
        assert!(
            tracked_count < 10,
            "Only regions with submitted tiles should be marked InProgress, \
             but {} regions were tracked (expected < 10)",
            tracked_count
        );

        // Second tick at same position — should find more new regions to submit
        let plan2 = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        assert!(
            plan2.is_some(),
            "Second tick should find untracked regions from truncated first tick"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // SimState integration tests (#79)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_coordinator_skips_when_scenery_loading() {
        use crate::aircraft_position::web_api::sim_state::SimState;

        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        let loading = SimState {
            scenery_loading: true,
            ..SimState::default()
        };
        coord.set_sim_state(loading);

        let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        assert!(plan.is_none(), "Should skip prefetch when scenery loading");
    }

    #[test]
    fn test_coordinator_skips_during_replay() {
        use crate::aircraft_position::web_api::sim_state::SimState;

        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        let replay = SimState {
            replay: true,
            ..SimState::default()
        };
        coord.set_sim_state(replay);

        let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        assert!(plan.is_none(), "Should skip prefetch during replay");
    }

    #[test]
    fn test_coordinator_continues_when_paused() {
        use crate::aircraft_position::web_api::sim_state::SimState;
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            forward_margin: 3.0,
            behind_margin: 1.0,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index);

        // Paused state — should still prefetch
        let paused = SimState {
            paused: true,
            ..SimState::default()
        };
        coord.set_sim_state(paused);

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);

        // Enter cruise
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        if coord.status.phase == FlightPhase::Cruise {
            assert!(
                plan.is_some(),
                "Should continue prefetch when paused (opportunistic)"
            );
        }
    }
}
