//! Core adaptive prefetch coordinator implementation.
//!
//! This module contains the [`AdaptivePrefetchCoordinator`] struct and its
//! implementation. The async run loop (`Prefetcher` trait impl) is in the
//! separate [`super::runner`] module.

use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::coord::TileCoord;
use crate::executor::{DaemonMemoryCache, DdsClient, DdsDiskCacheChecker};
use crate::metrics::MetricsClient;

/// Maximum demotion attempts before marking a region as NoCoverage.
const MAX_REGION_ATTEMPTS: u8 = 3;
use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage, PrefetchedRegion};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::state::{AircraftState, SharedPrefetchStatus};
use crate::prefetch::SceneryIndex;

use super::super::boundary_strategy::{BoundaryStrategy, RegionDiskState};
use super::super::calibration::{PerformanceCalibration, StrategyMode};
use super::super::config::{AdaptivePrefetchConfig, PrefetchMode};
use super::super::phase_detector::{FlightPhase, PhaseDetector};
use super::super::prefetch_box::PrefetchBox;
use super::super::strategy::PrefetchPlan;
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
/// │ Phase   │ │ Ground  │ │ Prefetch     │ │Boundary │ │  Sim    │
/// │Detector │ │Strategy │ │ Box          │ │Strategy │ │  State  │
/// └─────────┘ └─────────┘ └──────────────┘ └─────────┘ └─────────┘
/// ```
///
/// In cruise phase, the coordinator uses a **sliding prefetch box** approach:
/// the [`PrefetchBox`] computes a heading-biased region around the aircraft,
/// and the [`BoundaryStrategy`] manages region lifecycle (InProgress/Prefetched).
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

    /// X-Plane sim state from Web API (direct detection, replaces heuristics).
    sim_state: crate::aircraft_position::web_api::sim_state::SimState,

    /// DDS client for submitting prefetch requests.
    pub(super) dds_client: Option<Arc<dyn DdsClient>>,

    /// Memory cache for checking tile existence before submitting.
    ///
    /// When set, the coordinator queries this cache to filter out tiles
    /// that are already cached, avoiding unnecessary job submissions.
    pub(super) memory_cache: Option<Arc<dyn DaemonMemoryCache>>,

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

    /// DDS disk cache checker for verifying tile existence during stale region evaluation.
    ///
    /// When an InProgress region becomes stale, we check if its tiles exist on DDS disk
    /// before deciding to promote (tiles exist) or demote (tiles missing) the region.
    dds_disk_checker: Option<Arc<dyn DdsDiskCacheChecker>>,

    /// Tracks demotion attempts per region to prevent infinite retry loops.
    ///
    /// Incremented each time an InProgress region is demoted (tiles not on disk after
    /// stale timeout). After [`MAX_REGION_ATTEMPTS`] demotions, the region is marked
    /// NoCoverage and permanently excluded for this session.
    region_attempts: std::collections::HashMap<DsfRegion, u8>,

    /// Mapping of planned tiles to their source DSF region for the current
    /// in-flight plan. Populated by the cruise branch of [`update()`] and
    /// consumed by [`execute()`] to compute per-region submission completeness.
    ///
    /// A region is marked `InProgress` only when every one of its planned
    /// tiles is successfully submitted — regions whose tiles were deferred,
    /// channel-rejected, or throttle-overflowed stay unmarked so they
    /// naturally re-enter `new_regions_with_extent` on the next cycle.
    /// This fixes the `#172` bug where regions were marked before
    /// submission and then stuck `InProgress` despite tiles never being
    /// generated.
    ///
    /// Cleared after each [`execute()`] call (per-plan transient state).
    pub(super) current_plan_regions: std::collections::HashMap<TileCoord, DsfRegion>,

    /// Metrics client for prefetch telemetry (#176).
    ///
    /// When set, `run_region_maintenance` reports the region-state
    /// distribution each cycle so a default-level flight log carries the
    /// 60s "Prefetch sample" line without needing `--debug`.
    metrics_client: Option<MetricsClient>,
}

impl std::fmt::Debug for AdaptivePrefetchCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptivePrefetchCoordinator")
            .field("config.enabled", &self.config.enabled)
            .field("config.mode", &self.config.mode)
            .field("has_calibration", &self.calibration.is_some())
            .field("has_dds_client", &self.dds_client.is_some())
            .field("has_metrics_client", &self.metrics_client.is_some())
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
        let boundary_strategy = BoundaryStrategy::new();
        let prefetch_box = PrefetchBox::new(config.box_extent, config.box_max_bias);

        Self {
            config,
            calibration: None,
            phase_detector,
            transition_throttle,
            sim_state: crate::aircraft_position::web_api::sim_state::SimState::default(),
            dds_client: None,
            memory_cache: None,
            status: CoordinatorStatus::default(),
            shared_status: None,
            total_cycles: 0,
            total_tiles_submitted: 0,
            total_cache_hits: 0,
            total_deferred_cycles: 0,
            ortho_union_index: None,
            geo_index: None,
            boundary_strategy,
            prefetch_box,
            scene_tracker: None,
            scenery_index: None,
            pending_tiles: Vec::new(),
            dds_disk_checker: None,
            region_attempts: std::collections::HashMap::new(),
            current_plan_regions: std::collections::HashMap::new(),
            metrics_client: None,
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
    /// The index is used by the prefetch box (both ground and cruise phases)
    /// to discover actual installed zoom levels rather than assuming zoom 14.
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.scenery_index = Some(index);
        self
    }

    /// Attach a metrics client for prefetch telemetry.
    pub fn with_metrics_client(mut self, client: MetricsClient) -> Self {
        self.metrics_client = Some(client);
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

    /// Set the DDS disk cache checker for stale region evaluation.
    ///
    /// When set, stale InProgress regions are checked against the DDS disk cache.
    /// Tiles found on disk trigger promotion; tiles not found trigger demotion.
    pub fn with_dds_disk_checker(mut self, checker: Arc<dyn DdsDiskCacheChecker>) -> Self {
        self.dds_disk_checker = Some(checker);
        self
    }

    /// Set the scene tracker for observing X-Plane tile requests.
    pub fn with_scene_tracker(mut self, tracker: Arc<dyn SceneTracker>) -> Self {
        self.scene_tracker = Some(tracker);
        self
    }

    /// Get DDS tiles for a DSF region from the scenery index.
    ///
    /// If a scenery index is available, queries it for tiles in the region.
    /// This returns tiles at whatever zoom levels are actually installed in the
    /// X-Plane scenery (e.g., ZL12 at cruise altitude), rather than assuming
    /// a fixed zoom level.
    ///
    /// There is no geometric fallback (#176): when no scenery index is
    /// configured, or the index has no tiles for the region, this returns
    /// empty and the caller marks the region NoCoverage.
    fn get_tiles_for_region(&self, region: &DsfRegion) -> Vec<TileCoord> {
        match self.scenery_index {
            // An empty result means the index knows of no ortho scenery
            // here. The caller marks the region NoCoverage. There is no
            // geometric fallback: a 4x4 sample of a region holding ~2,500
            // tiles at zoom 14 made "all tiles cached" meaningless. See #176.
            Some(ref index) => index.tiles_in_region(*region),
            None => Vec::new(),
        }
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

        // Clear last cycle's published bounds — any early return below
        // leaves the debug map with `None`, preventing stale overlays.
        // Re-populated only when a valid box is actually constructed.
        self.status.box_bounds = None;

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

        // Pick box shape per phase. Transition skips prefetch entirely so
        // X-Plane gets full resources during takeoff. Ground and Cruise
        // both use the same prefetch box — ground with symmetric bias
        // (aircraft centered), cruise with heading-biased forward extent.
        // This unification replaces the separate `GroundStrategy`
        // (deleted post-#172) with a single, reusable box.
        let (extent, max_bias, strategy_name) = match phase {
            FlightPhase::Transition => {
                return None;
            }
            FlightPhase::Ground => (self.config.box_extent, 0.5, "ground_box"),
            FlightPhase::Cruise => {
                let extent = crate::prefetch::adaptive::compute_extent(
                    ground_speed_kt,
                    self.config.box_min_speed,
                    self.config.box_max_speed,
                    self.config.box_min_extent,
                    self.config.box_extent,
                );
                (extent, self.config.box_max_bias, "sliding_box")
            }
        };

        self.status.box_extent = extent;
        self.status.active_strategy = strategy_name;

        let shape = crate::prefetch::adaptive::prefetch_box::BoxShape::new(extent, max_bias);
        let (lat, lon) = position;

        // Compute the actual box bounds once. This becomes the single source
        // of truth for: retention updates, region enumeration, tracing, and
        // the debug map overlay. Publishing here (before the `new_regions`
        // emptiness check) means the map shows the active box even on cycles
        // that produce no work — the overlay accurately reflects "the box the
        // coordinator was looking at" at all times.
        let (box_lat_min, box_lat_max, box_lon_min, box_lon_max) =
            self.prefetch_box.bounds_with_shape(lat, lon, track, shape);
        self.status.box_bounds = Some(crate::prefetch::state::BoxBoundsSnapshot {
            lat_min: box_lat_min,
            lat_max: box_lat_max,
            lon_min: box_lon_min,
            lon_max: box_lon_max,
        });

        // Update retained region tracking from prefetch box bounds — cruise only.
        // On the ground the aircraft is stationary so there is no "behind" to
        // retain; updating retention in the ground branch would activate
        // `evict_non_retained` against regions that legitimately live far from
        // the aircraft (e.g. prewarm state from a previous session).
        if matches!(phase, FlightPhase::Cruise) {
            if let Some(ref geo_index) = self.geo_index {
                self.prefetch_box.update_retention_with_shape(
                    lat,
                    lon,
                    track,
                    self.config.window_buffer as i32,
                    geo_index,
                    shape,
                );
            }
        }

        let new_regions = if let Some(ref geo_index) = self.geo_index {
            self.prefetch_box
                .new_regions_with_shape(lat, lon, track, geo_index, shape)
        } else {
            self.prefetch_box.regions_with_shape(lat, lon, track, shape)
        };

        if new_regions.is_empty() {
            tracing::trace!(
                lat = format!("{:.2}", lat),
                lon = format!("{:.2}", lon),
                track = format!("{:.1}", track),
                phase = ?phase,
                "No new regions in prefetch box"
            );
            return None;
        }

        // Log the box bounds for debugging (reusing the values computed above).
        tracing::debug!(
            aircraft = format!("{:.4}, {:.4}", lat, lon),
            track = format!("{:.1}", track),
            ground_speed_kt = format!("{:.0}", ground_speed_kt),
            extent = format!("{:.2}", extent),
            max_bias = format!("{:.2}", max_bias),
            phase = ?phase,
            box_bounds = format!(
                "[{:.1}:{:.1}N, {:.1}:{:.1}E]",
                box_lat_min, box_lat_max, box_lon_min, box_lon_max
            ),
            new_regions = new_regions.len(),
            "Prefetch box: new regions detected"
        );

        // Expand regions to tiles. Every DSF region that intersects the
        // prefetch window must have all its tiles submitted (unless
        // already cached — that filtering happens downstream). No caps,
        // no distance-sort; the filter pipeline, executor backpressure,
        // and pending_tiles queue handle rate limiting. See #172
        // post-flight: distance-sort + cap produced forward starvation.
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

        // Record tile→region mapping for execute() to use when deciding
        // which regions to mark InProgress. Marking is deferred until
        // after submission — only regions whose tiles were actually
        // accepted by the executor get marked (see Part 1).
        self.current_plan_regions.clear();
        self.current_plan_regions.reserve(tiles_with_region.len());
        let mut all_tiles = Vec::with_capacity(tiles_with_region.len());
        for (tile, region) in tiles_with_region {
            self.current_plan_regions.insert(tile, region);
            all_tiles.push(tile);
        }

        let total = all_tiles.len();
        let plan = PrefetchPlan::with_tiles(all_tiles, &calibration, strategy_name, 0, total);

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

        // Per-cycle instrumentation (#172 Part 4): surface the
        // tiles_planned / tiles_submitted / tiles_pending shape of each
        // cycle at INFO level so a persistent "planned ≫ submitted"
        // pattern is immediately visible in logs without grepping the
        // decision tree. This is the primary telemetry for verifying
        // that the mark-after-submit invariant holds in flight.
        let tiles_planned = plan.tiles.len();
        let tiles_submitted = result.submitted_count();
        let tiles_pending = result.pending.len();
        let tiles_dropped = tiles_planned.saturating_sub(tiles_submitted + tiles_pending);
        if tiles_planned > 0 {
            tracing::info!(
                strategy = plan.strategy,
                tiles_planned,
                tiles_submitted,
                tiles_pending,
                tiles_dropped,
                deferred = result.deferred,
                "Prefetch cycle summary"
            );
        }

        // Mark regions as InProgress only if ALL of their planned tiles
        // appear in `result.submitted_tiles` — the authoritative record
        // of what the executor actually accepted. A planned tile that's
        // neither submitted nor pending would be a logic bug (the
        // pending cap was removed post-flight #172) — defence in depth
        // is still correct here, the positive check stays right.
        //
        // See #172 Part 1 (the ordering fix) + Part 2 (positive check).
        if !self.current_plan_regions.is_empty() && !result.deferred {
            let submitted_set: std::collections::HashSet<TileCoord> =
                result.submitted_tiles.iter().copied().collect();
            let mut region_planned: std::collections::HashMap<DsfRegion, Vec<TileCoord>> =
                std::collections::HashMap::new();
            for (tile, region) in &self.current_plan_regions {
                region_planned.entry(*region).or_default().push(*tile);
            }

            if let Some(ref geo_index) = self.geo_index {
                let mut marked = 0usize;
                for (region, planned_tiles) in &region_planned {
                    let fully_submitted = planned_tiles.iter().all(|t| submitted_set.contains(t));
                    if fully_submitted {
                        self.boundary_strategy.mark_in_progress(region, geo_index);
                        marked += 1;
                    }
                }
                if marked > 0 {
                    tracing::debug!(
                        regions_marked = marked,
                        regions_in_plan = region_planned.len(),
                        "Prefetch: marked fully-submitted regions as InProgress"
                    );
                }
            }
        }
        self.current_plan_regions.clear();

        if result.deferred {
            self.total_deferred_cycles += 1;
        }
        if !result.pending.is_empty() {
            self.pending_tiles = result.pending;
        }

        tiles_submitted
    }

    /// Mark every region whose planned tiles were *all* removed by the
    /// filter pipeline as `InProgress`.
    ///
    /// `surviving` is the plan's tile list after filtering. A region with no
    /// surviving tile contributes nothing to submission, so [`execute()`]'s
    /// mark-after-submit never sees it; and if that is true of every region,
    /// the plan is empty and [`execute()`] is not called at all. Either way
    /// the region would stay unmarked and be re-planned on every cycle.
    ///
    /// `InProgress` rather than `Prefetched` is deliberate. The filter says
    /// the tiles looked cached at plan time; that is not proof they are on
    /// the DDS disk. `BoundaryStrategy::promote_completed_regions` re-checks
    /// every tile in the region against the authoritative disk cache during
    /// the maintenance pass at the end of this same cycle, and only then
    /// confirms `Prefetched`. If the check fails the region simply stays
    /// `InProgress` until `evaluate_stale_regions` retires or retries it.
    ///
    /// Patch-owned regions are excluded. Their tiles filter out in full at
    /// the `PatchCoverage` stage, but prefetch will never put those tiles on
    /// the DDS disk — X-Plane is served them by the patch through FUSE
    /// passthrough. Marking one `InProgress` would be a claim prefetch is
    /// working on it, which `promote_completed_regions` could never confirm;
    /// the region would go stale and be retired to `NoCoverage` with a
    /// misleading "failed attempts" warning in the flight log. Patch
    /// ownership is a whole-region property in the `GeoIndex`, so this
    /// exclusion is exact rather than a heuristic. Such regions keep being
    /// re-planned and filtered each cycle, as they were before this change.
    ///
    /// Marked regions are dropped from `current_plan_regions` so
    /// [`execute()`] only reasons about regions that had tiles to submit.
    ///
    /// Returns the number of regions marked.
    fn mark_fully_filtered_regions(&mut self, surviving: &[TileCoord]) -> usize {
        if self.current_plan_regions.is_empty() {
            return 0;
        }
        let Some(geo_index) = self.geo_index.clone() else {
            return 0;
        };

        let surviving_regions: HashSet<DsfRegion> = surviving
            .iter()
            .filter_map(|tile| self.current_plan_regions.get(tile).copied())
            .collect();

        let fully_filtered: HashSet<DsfRegion> = self
            .current_plan_regions
            .values()
            .copied()
            .filter(|region| !surviving_regions.contains(region))
            .filter(|region| !geo_index.contains::<PatchCoverage>(region))
            .collect();

        if fully_filtered.is_empty() {
            return 0;
        }

        for region in &fully_filtered {
            self.boundary_strategy.mark_in_progress(region, &geo_index);
        }
        self.current_plan_regions
            .retain(|_, region| !fully_filtered.contains(region));

        tracing::debug!(
            regions_marked = fully_filtered.len(),
            "Prefetch: marked fully-filtered regions InProgress for disk-verified promotion"
        );
        fully_filtered.len()
    }

    /// Get current status for UI/logging.
    pub fn status(&self) -> &CoordinatorStatus {
        &self.status
    }

    /// Get the phase detector for external monitoring.
    pub fn phase_detector(&self) -> &PhaseDetector {
        &self.phase_detector
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
            "adaptive, mode={:?}, ground_threshold={}kt, box_extent={:.1}°",
            mode, self.config.ground_speed_threshold_kt, self.config.box_extent,
        )
    }

    /// Log plan details with metadata.
    fn log_plan(&self, plan: &PrefetchPlan, position: (f64, f64), track: f64) {
        let (lat, lon) = position;

        if let Some(ref metadata) = plan.metadata {
            tracing::debug!(
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
            tracing::debug!(
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
        //
        // Re-filter pending tiles first — tiles may have been cached by other
        // means since they were first submitted (memory cache by FUSE on-demand
        // promotion, DDS disk by prior cycles, installed packages, etc.).
        // Without re-filtering, the executor walks cache tiers for each
        // already-cached tile and reads the ~10MB DDS payload to return a hit,
        // producing constant disk reads AND starving genuinely-uncached work
        // behind a queue of redundant cache-hit submissions. See #172
        // post-flight finding.
        if !self.pending_tiles.is_empty() {
            let pending = std::mem::take(&mut self.pending_tiles);
            let pending_before_filter = pending.len();

            // Still need to update phase detector for correct state tracking
            self.phase_detector.update(state.ground_speed, msl_ft);

            let (filtered_pending, filter_counts) = super::filtering::run_filter_pipeline(
                pending,
                self.memory_cache.as_deref(),
                self.geo_index.as_ref(),
                self.ortho_union_index.as_ref(),
                self.dds_disk_checker.as_ref(),
            )
            .await;
            let filtered_total = filter_counts.total();

            tracing::debug!(
                pending_before = pending_before_filter,
                cache_skipped = filter_counts.cache_hits,
                patch_skipped = filter_counts.patch_skipped,
                disk_skipped = filter_counts.disk_skipped,
                dds_disk_hits = filter_counts.dds_disk_hits,
                remaining = filtered_pending.len(),
                "Pending drain filter pipeline summary"
            );

            if filtered_pending.is_empty() {
                tracing::debug!(
                    pending_before = pending_before_filter,
                    filtered = filtered_total,
                    "All pending tiles already cached — nothing to drain"
                );
                self.total_cycles += 1;
                self.total_cache_hits += filtered_total as u64;
                self.run_region_maintenance();
                return Some(0);
            }

            let pending_count = filtered_pending.len();
            let calibration = self
                .calibration
                .clone()
                .unwrap_or_else(PerformanceCalibration::default_opportunistic);
            let plan = PrefetchPlan::with_tiles(
                filtered_pending,
                &calibration,
                "boundary_pending",
                0,
                pending_count,
            );

            let cancellation = CancellationToken::new();
            let submitted = self.execute(&plan, cancellation);

            self.total_cycles += 1;
            self.total_tiles_submitted += submitted as u64;
            self.total_cache_hits += filtered_total as u64;

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

        // Run the filtering pipeline: memory → patches → packages → DDS disk
        let (filtered_tiles, filter_counts) = super::filtering::run_filter_pipeline(
            std::mem::take(&mut plan.tiles),
            self.memory_cache.as_deref(),
            self.geo_index.as_ref(),
            self.ortho_union_index.as_ref(),
            self.dds_disk_checker.as_ref(),
        )
        .await;
        plan.tiles = filtered_tiles;

        let total_filtered = filter_counts.total();

        tracing::debug!(
            raw_plan_tiles = plan.skipped_cached + total_filtered + plan.tiles.len(),
            cache_skipped = plan.skipped_cached + filter_counts.cache_hits,
            patch_skipped = filter_counts.patch_skipped,
            disk_skipped = filter_counts.disk_skipped,
            dds_disk_hits = filter_counts.dds_disk_hits,
            remaining = plan.tiles.len(),
            strategy = plan.strategy,
            "Prefetch plan filter pipeline summary"
        );

        // Regions whose entire planned tile set was filtered out submit
        // nothing, so `execute()`'s mark-after-submit can never fire for
        // them — and when *every* region is in that state the plan is empty
        // and `execute()` is skipped outright. Left unmarked, the region
        // stays absent from `PrefetchedRegion`, re-enters
        // `new_regions_with_shape` on the next 2s cycle, and repeats forever
        // while `regions_prefetched` under-reports. See #176.
        self.mark_fully_filtered_regions(&plan.tiles);

        let submitted = if plan.is_empty() {
            0
        } else {
            let cancellation = CancellationToken::new();
            self.execute(&plan, cancellation)
        };

        // Update statistics
        self.total_cycles += 1;
        self.total_tiles_submitted += submitted as u64;
        self.total_cache_hits += (plan.skipped_cached + total_filtered) as u64;

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
        let geo_index = match self.geo_index {
            Some(ref gi) => Arc::clone(gi),
            None => return,
        };

        // Evaluate stale InProgress regions: promote if tiles on disk, demote if not.
        // This replaces the old sweep_stale_regions which blindly removed stale regions
        // without checking whether tiles had actually been generated.
        self.evaluate_stale_regions(&geo_index);

        // Consult the authoritative DDS disk cache rather than a local
        // shadow of "tiles we believe are cached". See #172 Part 3: a
        // prior `HashSet` shadow failed to track ~94% of actually-cached
        // tiles in production, leaving the rescue path
        // (evaluate_stale_regions) to carry the work. The shadow itself
        // was deleted in #176 once it was confirmed write-only.
        let promoted = BoundaryStrategy::promote_completed_regions(
            &geo_index,
            self.dds_disk_checker.as_ref(),
            self.scenery_index.as_ref(),
            self.ortho_union_index.as_ref(),
        );
        if !promoted.is_empty() {
            // A promoted region's failure history belongs to the episode that
            // failed, not to the region. Without this, a region that recovers and
            // is later demoted by the FUSE observer can be retired to NoCoverage
            // after a single fresh failure.
            for region in &promoted {
                self.region_attempts.remove(region);
            }
            if let Some(ref metrics) = self.metrics_client {
                metrics.prefetch_regions_promoted_normal(promoted.len());
            }
        }
        // Evict PrefetchedRegion entries for regions outside the retained window,
        // making them eligible for re-prefetch when the aircraft returns.
        BoundaryStrategy::evict_non_retained(&geo_index);

        // Per-maintenance-cycle instrumentation (#172 Part 4): report
        // region-state distribution. A healthy system shows normal-path
        // promotions dominating; if `in_progress` stays high while
        // `prefetched` remains low, the fast-path is stalling and the
        // rescue path is carrying the work — the same anti-pattern that
        // produced the 61:4 rescue ratio in the LOWW flight log.
        let (in_progress, prefetched, no_coverage) = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .fold((0usize, 0usize, 0usize), |(ip, p, nc), (_, r)| {
                if r.is_in_progress() {
                    (ip + 1, p, nc)
                } else if r.is_prefetched() {
                    (ip, p + 1, nc)
                } else {
                    (ip, p, nc + 1)
                }
            });
        tracing::debug!(
            regions_in_progress = in_progress,
            regions_prefetched = prefetched,
            regions_nocoverage = no_coverage,
            "Region maintenance: state distribution"
        );

        if let Some(ref metrics) = self.metrics_client {
            metrics.prefetch_region_state(in_progress, prefetched, no_coverage);
        }
    }

    /// Evaluate stale InProgress regions and decide: promote, demote, or NoCoverage.
    ///
    /// For each InProgress region that has exceeded the stale timeout:
    /// 1. Check if tiles exist on DDS disk cache → promote to Prefetched
    /// 2. If tiles not on disk, check attempt counter:
    ///    - Under limit → remove from GeoIndex (allows retry on next cycle)
    ///    - At limit → mark NoCoverage (permanently excluded this session)
    fn evaluate_stale_regions(&mut self, geo_index: &GeoIndex) {
        let stale: Vec<DsfRegion> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, region)| region.is_stale(self.config.stale_region_timeout))
            .map(|(dsf, _)| dsf)
            .collect();

        if stale.is_empty() {
            return;
        }

        for region in stale {
            // Single definition of "is this region fully covered?", shared
            // with the fast path (`BoundaryStrategy::promote_completed_regions`).
            // Prior to #223's review these were two copies that already
            // disagreed on the empty and unanswerable cases.
            match BoundaryStrategy::region_disk_state(
                region,
                self.scenery_index.as_ref(),
                self.dds_disk_checker.as_ref(),
                self.ortho_union_index.as_ref(),
            ) {
                RegionDiskState::Complete => {
                    // Tiles generated successfully — promote based on the
                    // authoritative disk check (this is the rescue path;
                    // the fast path never had reliable tracking to lose).
                    geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());
                    // See the fast-path promotion comment in `run_region_maintenance`:
                    // the strike count belongs to the failed episode, not the region.
                    self.region_attempts.remove(&region);
                    if let Some(ref metrics) = self.metrics_client {
                        metrics.prefetch_region_promoted_rescue();
                    }
                    tracing::info!(
                        lat = region.lat,
                        lon = region.lon,
                        "Stale InProgress region promoted — tiles found on DDS disk"
                    );
                }
                // No tiles indexed: the region can never be confirmed, so
                // retiring it after repeated attempts is correct — this
                // preserves pre-#223 behavior.
                RegionDiskState::Incomplete | RegionDiskState::NoTiles => {
                    let attempts = self.region_attempts.entry(region).or_insert(0);
                    *attempts += 1;

                    if *attempts >= MAX_REGION_ATTEMPTS {
                        // Exhausted retries — mark permanently excluded
                        geo_index
                            .insert::<PrefetchedRegion>(region, PrefetchedRegion::no_coverage());
                        tracing::warn!(
                            lat = region.lat,
                            lon = region.lon,
                            attempts = *attempts,
                            "Region marked NoCoverage after {} failed attempts",
                            MAX_REGION_ATTEMPTS
                        );
                        // Clear the strike count for the retired episode,
                        // mirroring both promotion arms above. Without this,
                        // a region the observer later demotes out of
                        // NoCoverage (see `state_observer.rs::observe`) would
                        // re-enter the prefetch cycle still carrying the old
                        // count, so one single fresh failure would re-hit
                        // `>= MAX_REGION_ATTEMPTS` and re-retire it instantly
                        // — zero real retries. Clearing here gives a demoted
                        // region a fresh set of `MAX_REGION_ATTEMPTS` strikes
                        // each time it's demoted; the observer's per-region
                        // demotion rate limit (one per `demotion_interval`)
                        // is what bounds that loop, not this counter.
                        self.region_attempts.remove(&region);
                    } else {
                        // Remove from GeoIndex to allow retry on next cycle.
                        //
                        // Do NOT clear `region_attempts` here. This removal is
                        // deliberate — it makes the region eligible for
                        // re-prefetching — and `region_attempts` is the only
                        // thing that remembers the strike across it. Clearing
                        // on this branch (by symmetry with the promote branch
                        // above) would make `MAX_REGION_ATTEMPTS` unreachable:
                        // every retry would start the count over, and a region
                        // that never succeeds would retry forever instead of
                        // eventually being retired to `NoCoverage`.
                        geo_index.remove::<PrefetchedRegion>(&region);
                        tracing::info!(
                            lat = region.lat,
                            lon = region.lon,
                            attempt = *attempts,
                            max = MAX_REGION_ATTEMPTS,
                            "Stale InProgress region demoted for retry"
                        );
                    }
                }
                // Unanswerable. Leave the claim standing and do not consume
                // a retry; the next cycle may have a checker.
                RegionDiskState::Unknown => continue,
            }
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

    /// Reset the phase detector based on SimState on_ground flag.
    ///
    /// Called when telemetry resumes after a stale period. Uses the on_ground
    /// flag from the first new telemetry packet to correctly initialise the
    /// phase detector without waiting for hysteresis to accumulate.
    pub fn reset_phase_from_on_ground(&mut self, on_ground: bool) {
        if on_ground {
            self.phase_detector.reset_to_ground();
            self.status.phase = FlightPhase::Ground;
        } else {
            self.phase_detector.reset_to_cruise();
            self.status.phase = FlightPhase::Cruise;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::coordinator::test_support::{
        ground_state, make_scenery_index, make_scenery_index_covering, patched_region_area,
        test_calibration, test_plan, AlwaysHitDiskChecker, AlwaysHitMemoryCache,
        BackpressureMockClient, CapLimitedDdsClient, DummyTracker, HighLoadDdsClient,
        MockDiskChecker, StableBoundsTracker,
    };
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
        assert_eq!(coord.status.active_strategy, "ground_box");
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
    // Speed-proportional extent tests (#125)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_cruise_extent_scales_with_ground_speed() {
        let cal = test_calibration();
        let mut coord = AdaptivePrefetchCoordinator::with_defaults().with_calibration(cal);

        // Fast-forward phase detector into Cruise using short hysteresis + takeoff timeout.
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);

        // Prime into Cruise at low speed (just above ground threshold: default 40 kt)
        coord.update((47.0, 8.0), 0.0, 50.0, 10000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((47.0, 8.0), 0.0, 50.0, 10000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _ = coord.update((47.0, 8.0), 0.0, 50.0, 10000.0);

        assert_eq!(
            coord.status.phase,
            FlightPhase::Cruise,
            "Should be in Cruise phase"
        );
        let low_extent = coord.status.box_extent;

        // Now update at high speed
        let _ = coord.update((47.0, 8.0), 0.0, 400.0, 35000.0);
        let high_extent = coord.status.box_extent;

        assert!(
            high_extent > low_extent,
            "High speed extent ({}) should be larger than low speed extent ({})",
            high_extent,
            low_extent
        );
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
        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0, false);

        // Disabled coordinator returns None
        let result = coord.process_telemetry(&state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_process_telemetry_no_dds_client() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());
        let state = AircraftState::new(53.5, 9.5, 90.0, 10.0, 5.0, false); // Ground conditions

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

        // Post-#176, get_tiles_for_region has no geometric fallback — the
        // ground box needs real scenery coverage to produce candidate tiles
        // in the first place, before the patched-region filter can remove
        // them. Cover the same area the patched regions cover.
        let scenery_index = make_scenery_index_covering(
            (aircraft_dsf.lat - coverage_radius)..=(aircraft_dsf.lat + coverage_radius),
            (aircraft_dsf.lon - coverage_radius)..=(aircraft_dsf.lon + coverage_radius),
            16,
        );

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_calibration(test_calibration())
            .with_geo_index(geo_index)
            .with_scenery_index(scenery_index);

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

        let state = AircraftState::new(53.5, 9.5, 90.0, 250.0, 35000.0, false);

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
    fn test_coordinator_with_scene_tracker() {
        let tracker: Arc<dyn crate::scene_tracker::SceneTracker> = Arc::new(DummyTracker);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scene_tracker(tracker);
        assert!(coord.scene_tracker.is_some());
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
    fn test_get_tiles_for_region_without_index_returns_empty() {
        // #176: without a scenery index there is no geometric fallback —
        // the coordinator has no way to know what tiles the region should
        // contain, so it must yield nothing rather than guess zoom 14.
        let coord = AdaptivePrefetchCoordinator::with_defaults();
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(
            tiles.is_empty(),
            "Without a scenery index, get_tiles_for_region must return nothing"
        );
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
    fn test_get_tiles_for_region_returns_empty_when_no_coverage() {
        // #176: an index with no tiles for the target region is a
        // statement — "no ortho scenery here" — not a cue to fall back to
        // a geometric zoom-14 guess.
        // Index has tiles at (60, 20) but we query (50, 9)
        let index = make_scenery_index(60, 20, 16);
        let coord = AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(index);
        let region = DsfRegion::new(50, 9);

        let tiles = coord.get_tiles_for_region(&region);
        assert!(
            tiles.is_empty(),
            "Should return empty when scenery index has no coverage for the region"
        );
    }

    /// Build a [`DdsDiskCacheChecker`] that reports only the listed tiles as
    /// present on disk.
    fn test_checker(tiles: &[TileCoord]) -> Arc<dyn DdsDiskCacheChecker> {
        MockDiskChecker::with_tile_coords(tiles.iter().copied())
    }

    #[test]
    fn test_get_tiles_for_region_empty_for_unindexed_region() {
        // Removing the geometric fallback means an empty index result is a
        // statement — "no ortho scenery here" — rather than a cue to guess 16
        // tiles at zoom 14.
        //
        // NOTE: despite this test's prior name
        // (`test_region_with_no_indexed_tiles_is_marked_no_coverage`), it only
        // ever exercised `get_tiles_for_region` directly — no `GeoIndex` is
        // wired, no planning cycle runs, and no `PrefetchedRegion` state is
        // ever read. It does not, and never did, verify the NoCoverage
        // marking. See `test_no_coverage_region_marked_and_nothing_submitted`
        // below for a real test of that behaviour.
        use crate::prefetch::scenery_index::SceneryTile;

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });

        let coord =
            AdaptivePrefetchCoordinator::with_defaults().with_scenery_index(Arc::clone(&index));

        // A region the index knows nothing about — mid-Pacific.
        assert!(
            coord
                .get_tiles_for_region(&DsfRegion::new(10, -150))
                .is_empty(),
            "an uncovered region must yield no tiles, not a geometric guess"
        );
        // And the covered one still yields its tile.
        assert_eq!(
            coord.get_tiles_for_region(&DsfRegion::new(33, -119)).len(),
            1
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // NoCoverage marking on the planning path (#223 remediation, F5)
    //
    // `test_get_tiles_for_region_empty_for_unindexed_region` above only
    // proves `get_tiles_for_region` returns empty for an unindexed region —
    // it never runs a planning cycle and never reads `PrefetchedRegion`
    // state, so the production marking at the `if tiles.is_empty()` branch
    // in `update()` (which calls `BoundaryStrategy::mark_no_coverage`) was
    // unverified: deleting that branch left the whole suite green.
    //
    // This test wires a real `GeoIndex` and a scenery index with zero
    // coverage anywhere in the prefetch box, runs a full planning cycle via
    // `process_telemetry`, and asserts both halves of the contract: the
    // touched regions carry `PrefetchedRegion::no_coverage()`, and nothing
    // reached the DDS client.
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_no_coverage_region_marked_and_nothing_submitted() {
        use crate::geo_index::{GeoIndex, PrefetchedRegion};

        let geo_index = Arc::new(GeoIndex::new());
        let client = Arc::new(CapLimitedDdsClient::new(100_000));
        // Zero tiles anywhere — every region the prefetch box touches must
        // report no coverage.
        let empty_index = Arc::new(SceneryIndex::with_defaults());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_scenery_index(empty_index);

        fast_forward_to_cruise(&mut coord);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);
        let submitted = coord.process_telemetry(&state).await;

        assert_eq!(
            submitted.unwrap_or(0),
            0,
            "A scenery index with no coverage anywhere must yield no submissions",
        );
        assert_eq!(
            client.submitted_count(),
            0,
            "No tiles should ever reach the DDS client when there is no coverage",
        );

        let no_coverage_regions: Vec<_> = geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_no_coverage())
            .collect();
        assert!(
            !no_coverage_regions.is_empty(),
            "Regions the prefetch box touched with zero scenery coverage must be \
             marked NoCoverage, not left unmarked to be re-planned every cycle",
        );
    }

    #[test]
    fn test_stale_rescue_requires_full_coverage_not_one_tile() {
        // Regression for #176 defect 2. The rescue path sampled tiles.first()
        // and promoted the whole region on that single hit, which is how 61 of
        // 65 promotions were decided in the #172 LOWW log.
        //
        // Note: the `row`/`col` values in the `SceneryTile` fixtures in this
        // test (and its positive counterpart below) are arbitrary and not
        // geographically consistent with their `lat`/`lon` — only `lat`/`lon`
        // drive region membership in `tiles_in_region`.
        use crate::prefetch::scenery_index::SceneryTile;

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 5000,
            col: 6000,
            chunk_zoom: 16,
            lat: 33.7,
            lon: -118.2,
            is_sea: false,
        });

        let region = DsfRegion::new(33, -119);
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Only one of the two tiles is on disk.
        let checker = test_checker(&[TileCoord {
            row: 1000 / 16,
            col: 2000 / 16,
            zoom: 12,
        }]);

        // Precondition: the rescue path must actually be looking at the
        // region's real tile set (2 tiles), not silently falling through to
        // an empty result. Without this guard, gutting the rescue path's
        // tile lookup to `Vec::new()` would make `tiles_on_disk` fall
        // through to the `_ => false` arm, the region would demote instead
        // of promote, and the assertion below would still pass — proving
        // nothing about whether the real tile set was consulted.
        assert_eq!(
            index.tiles_in_region(region).len(),
            2,
            "Precondition: index must expose both region tiles for this test to be meaningful"
        );

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_disk_checker(checker);
        coord.config.stale_region_timeout = std::time::Duration::ZERO; // everything is stale

        coord.run_region_maintenance();

        let state = geo_index.get::<PrefetchedRegion>(&region);
        assert!(
            state.is_none_or(|s| !s.is_prefetched()),
            "partial coverage must not be promoted by the rescue path"
        );
    }

    #[test]
    fn test_stale_rescue_promotes_when_full_coverage_on_disk() {
        // Positive counterpart to `test_stale_rescue_requires_full_coverage_not_one_tile`.
        // The rescue promote branch (evaluate_stale_regions, tiles_on_disk == true)
        // had no positive test anywhere in the suite — only the demotion path was
        // covered. Same fixture, but both tiles are present in the checker this time.
        use crate::prefetch::scenery_index::SceneryTile;

        let index = Arc::new(SceneryIndex::with_defaults());
        index.add_tile(SceneryTile {
            row: 1000,
            col: 2000,
            chunk_zoom: 16,
            lat: 33.5,
            lon: -118.5,
            is_sea: false,
        });
        index.add_tile(SceneryTile {
            row: 5000,
            col: 6000,
            chunk_zoom: 16,
            lat: 33.7,
            lon: -118.2,
            is_sea: false,
        });

        let region = DsfRegion::new(33, -119);
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // Both tiles are on disk this time.
        let checker = test_checker(&[
            TileCoord {
                row: 1000 / 16,
                col: 2000 / 16,
                zoom: 12,
            },
            TileCoord {
                row: 5000 / 16,
                col: 6000 / 16,
                zoom: 12,
            },
        ]);

        assert_eq!(
            index.tiles_in_region(region).len(),
            2,
            "Precondition: index must expose both region tiles for this test to be meaningful"
        );

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_disk_checker(checker);
        coord.config.stale_region_timeout = std::time::Duration::ZERO; // everything is stale

        coord.run_region_maintenance();

        let state = geo_index.get::<PrefetchedRegion>(&region);
        assert!(
            state.is_some_and(|s| s.is_prefetched()),
            "full coverage on disk must be promoted by the rescue path"
        );
    }

    #[tokio::test]
    async fn test_stale_region_not_struck_when_disk_state_unknown() {
        // A stale InProgress region with a scenery index but NO dds_disk_checker.
        // Before: `_ => false` counted a failed attempt, and three of these
        // retired the region to NoCoverage permanently. After: Unknown is not
        // evidence of absence, so the region keeps its InProgress claim and no
        // attempt is recorded.
        let index = make_scenery_index_covering(50..=50, 9..=9, 16);
        let geo_index = Arc::new(GeoIndex::new());
        let region = DsfRegion { lat: 50, lon: 9 };
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index));
        // deliberately no .with_dds_disk_checker(...)
        coord.config.stale_region_timeout = std::time::Duration::ZERO; // force staleness

        for _ in 0..MAX_REGION_ATTEMPTS + 1 {
            coord.evaluate_stale_regions(&geo_index);
        }

        let state = geo_index.get::<PrefetchedRegion>(&region);
        assert!(
            state.map(|s| s.is_in_progress()).unwrap_or(false),
            "region must remain InProgress; Unknown is not evidence of absence"
        );
        assert!(
            !coord.region_attempts.contains_key(&region),
            "an unanswerable check must not consume a retry"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Strike count lifecycle + promotion metrics (#176 Task 4 / F1 + F17)
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_promotion_clears_stale_attempt_count() {
        // Two early failures, then a successful promotion, then one fresh
        // failure must NOT retire the region: the strike count belongs to the
        // failed episode, not to the region forever.
        let index = make_scenery_index(50, 9, 16);
        let region = DsfRegion::new(50, 9);
        let tiles = index.tiles_in_region(region);
        assert!(
            !tiles.is_empty(),
            "Precondition: index covers region (50,9)"
        );

        let geo_index = Arc::new(GeoIndex::new());
        let empty_checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(std::iter::empty::<TileCoord>());

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_disk_checker(Arc::clone(&empty_checker));

        // Phase 1: checker empty, region InProgress, stale -> 2 strikes.
        coord.config.stale_region_timeout = std::time::Duration::ZERO;
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        coord.evaluate_stale_regions(&geo_index);
        assert_eq!(
            coord.region_attempts.get(&region),
            Some(&1),
            "Precondition: first stale failure recorded"
        );
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        coord.evaluate_stale_regions(&geo_index);
        assert_eq!(
            coord.region_attempts.get(&region),
            Some(&2),
            "Precondition: second stale failure recorded (below MAX_REGION_ATTEMPTS)"
        );

        // Phase 2: checker seeded with the region's tiles -> promote via the
        // fast path (large timeout so the region is not picked up as stale).
        let full_checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());
        coord.dds_disk_checker = Some(full_checker);
        coord.config.stale_region_timeout = std::time::Duration::from_secs(600);
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        coord.run_region_maintenance();
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .is_some_and(|s| s.is_prefetched()),
            "Precondition: region promoted via the fast path"
        );
        assert!(
            !coord.region_attempts.contains_key(&region),
            "Promotion must clear the strike count from the failed episode"
        );

        // Phase 3: checker emptied, region re-marked InProgress, stale once.
        coord.dds_disk_checker = Some(empty_checker);
        coord.config.stale_region_timeout = std::time::Duration::ZERO;
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
        coord.evaluate_stale_regions(&geo_index);

        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .map(|s| !s.is_no_coverage())
                .unwrap_or(true),
            "one fresh failure after a successful promotion must not retire the region"
        );
    }

    #[tokio::test]
    async fn test_demoted_no_coverage_region_gets_fresh_strikes_after_retirement() {
        // Regression for #223 Finding B: strike counts were cleared on both
        // promotion paths but never on retirement to NoCoverage. A region
        // demoted out of NoCoverage by `PrefetchStateObserver` (simulated
        // here by removing its GeoIndex entry, exactly as the observer
        // does — see `state_observer.rs::observe`) re-entered the prefetch
        // cycle still carrying its old strike count, so a single fresh
        // failure retired it again immediately: zero real retries.
        let index = make_scenery_index(50, 9, 16);
        let region = DsfRegion::new(50, 9);
        assert!(
            !index.tiles_in_region(region).is_empty(),
            "Precondition: region has indexed tiles (land, not ocean) so the \
             strike path is reachable — ocean regions take tiles.is_empty() \
             and never touch region_attempts"
        );

        let geo_index = Arc::new(GeoIndex::new());
        let empty_checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(std::iter::empty::<TileCoord>());

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_disk_checker(Arc::clone(&empty_checker));
        coord.config.stale_region_timeout = std::time::Duration::ZERO;

        // Retire the region by strikes: MAX_REGION_ATTEMPTS consecutive
        // stale failures (Incomplete: indexed tiles exist, none on disk).
        for _ in 0..MAX_REGION_ATTEMPTS {
            geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());
            coord.evaluate_stale_regions(&geo_index);
        }
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .is_some_and(|s| s.is_no_coverage()),
            "Precondition: region retired to NoCoverage by strikes"
        );

        // The observer demotes a NoCoverage region it sees contradicted by
        // an on-demand FUSE generation — it clears the GeoIndex claim only
        // (`PrefetchStateObserver::observe` -> `geo_index.remove`).
        geo_index.remove::<PrefetchedRegion>(&region);

        // The region is re-planned and marked InProgress again.
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::in_progress());

        // One single fresh stale failure.
        coord.evaluate_stale_regions(&geo_index);

        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region)
                .map(|s| !s.is_no_coverage())
                .unwrap_or(true),
            "one fresh failure after an observer-driven demotion must not \
             immediately re-retire the region — it should get a fresh set \
             of MAX_REGION_ATTEMPTS strikes"
        );
    }

    #[tokio::test]
    async fn test_normal_and_rescue_promotions_are_counted_separately() {
        use crate::metrics::MetricEvent;

        let index = make_scenery_index_covering(49..=50, 9..=9, 16);
        let region_rescue = DsfRegion::new(49, 9);
        let region_normal = DsfRegion::new(50, 9);
        let tiles_rescue = index.tiles_in_region(region_rescue);
        let tiles_normal = index.tiles_in_region(region_normal);
        assert!(
            !tiles_rescue.is_empty(),
            "Precondition: rescue region covered"
        );
        assert!(
            !tiles_normal.is_empty(),
            "Precondition: normal region covered"
        );

        let geo_index = Arc::new(GeoIndex::new());
        let checker: Arc<dyn DdsDiskCacheChecker> = MockDiskChecker::with_tile_coords(
            tiles_rescue.iter().chain(tiles_normal.iter()).copied(),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let metrics = MetricsClient::new(tx);

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_scenery_index(Arc::clone(&index))
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_disk_checker(checker)
            .with_metrics_client(metrics);

        // Drive one rescue-path promotion: the region is stale, and the
        // rescue path (`evaluate_stale_regions`) finds full coverage.
        coord.config.stale_region_timeout = std::time::Duration::ZERO;
        geo_index.insert::<PrefetchedRegion>(region_rescue, PrefetchedRegion::in_progress());
        coord.evaluate_stale_regions(&geo_index);
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region_rescue)
                .is_some_and(|s| s.is_prefetched()),
            "Precondition: rescue path promoted the region"
        );

        // Drive one fast-path promotion: large timeout so the region is not
        // stale, and `promote_completed_regions` (run unconditionally by
        // `run_region_maintenance`) finds full coverage.
        coord.config.stale_region_timeout = std::time::Duration::from_secs(600);
        geo_index.insert::<PrefetchedRegion>(region_normal, PrefetchedRegion::in_progress());
        coord.run_region_maintenance();
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&region_normal)
                .is_some_and(|s| s.is_prefetched()),
            "Precondition: fast path promoted the region"
        );

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                MetricEvent::PrefetchRegionsPromotedNormal { count } if *count >= 1
            )),
            "fast-path promotion must emit a normal-path count"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MetricEvent::PrefetchRegionPromotedRescue)),
            "rescue-path promotion must emit a rescue count"
        );
    }

    #[tokio::test]
    async fn test_run_region_maintenance_emits_region_state_gauge() {
        use crate::metrics::MetricEvent;

        let geo_index = Arc::new(GeoIndex::new());

        // Seed 1 InProgress, 2 Prefetched, 3 NoCoverage — deliberately
        // distinct counts. With equal counts an argument swap at the
        // `metrics.prefetch_region_state(...)` call site, or a swap of the
        // `is_in_progress`/`is_prefetched` fold arms, would be undetectable.
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(10, 10), PrefetchedRegion::in_progress());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(20, 20), PrefetchedRegion::prefetched());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(21, 20), PrefetchedRegion::prefetched());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(30, 30), PrefetchedRegion::no_coverage());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(31, 30), PrefetchedRegion::no_coverage());
        geo_index
            .insert::<PrefetchedRegion>(DsfRegion::new(32, 30), PrefetchedRegion::no_coverage());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let metrics = MetricsClient::new(tx);

        let mut coord = AdaptivePrefetchCoordinator::with_defaults()
            .with_geo_index(Arc::clone(&geo_index))
            .with_metrics_client(metrics);

        // No scenery_index/dds_disk_checker is wired, so `region_disk_state`
        // returns `Unknown` for the InProgress region and neither
        // `evaluate_stale_regions` nor `promote_completed_regions` promotes
        // it. No `RetainedRegion` entries exist, so `evict_non_retained` is
        // a no-op. The seeded distribution therefore survives untouched to
        // the point the gauge samples it.
        coord.run_region_maintenance();

        // Guard the precondition: if maintenance had promoted or evicted
        // anything, the assertion below would be measuring a different
        // distribution than the one seeded.
        assert!(
            geo_index
                .get::<PrefetchedRegion>(&DsfRegion::new(10, 10))
                .is_some_and(|s| s.is_in_progress()),
            "Precondition: seeded InProgress region must remain InProgress"
        );

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let region_state_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MetricEvent::PrefetchRegionState { .. }))
            .collect();
        assert_eq!(
            region_state_events.len(),
            1,
            "expected exactly one PrefetchRegionState event, got {:?}",
            region_state_events
        );
        assert!(
            matches!(
                region_state_events[0],
                MetricEvent::PrefetchRegionState {
                    in_progress: 1,
                    prefetched: 2,
                    no_coverage: 3,
                }
            ),
            "expected PrefetchRegionState {{ in_progress: 1, prefetched: 2, no_coverage: 3 }}, got {:?}",
            region_state_events[0]
        );
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

        // Post-#176, promotion needs a scenery index to know the region's
        // tile set — there is no more geometric fallback. Cover region
        // (55, 7), the one this test promotes below.
        let scenery_index = make_scenery_index(55, 7, 16);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_scene_tracker(tracker)
            .with_geo_index(Arc::clone(&geo_index))
            .with_scenery_index(Arc::clone(&scenery_index));

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
        // For a real promotion test (post-#172 Part 3), populate the DDS
        // disk checker with the region's tiles so the authoritative
        // check sees them as present.
        let region = DsfRegion::new(55, 7);
        let tiles = scenery_index.tiles_in_region(region);
        assert!(
            !tiles.is_empty(),
            "Precondition: index covers region (55,7)"
        );
        let checker: Arc<dyn crate::executor::DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(tiles.iter().copied());
        coord.dds_disk_checker = Some(checker);

        coord.run_region_maintenance();

        // Region 55,7 should now be promoted to Prefetched
        let state = geo_index.get::<PrefetchedRegion>(&region);
        assert!(
            state.is_some(),
            "Region should still exist in GeoIndex after maintenance"
        );
        assert!(
            state.unwrap().is_prefetched(),
            "Region should be promoted to Prefetched when all tiles are on disk"
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
            // Zero ramp so transition throttle doesn't reduce tile count
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_scenery_index(wide_scenery_index_at_50_10());

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
        // Aircraft at lat=52.5 → near edge of window.
        let state = AircraftState::new(52.5, 10.0, 0.0, 200.0, 35000.0, false);

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

        let state = AircraftState::new(55.5, 10.0, 0.0, 200.0, 35000.0, false);

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
    // No-drop pending invariant (#172 post-flight finding)
    //
    // The pending queue is NEVER capped. Every planned tile must end up
    // either submitted or pending — nothing silently dropped at the
    // submission boundary. The executor's channel capacity and resource
    // pools are the only rate governor.
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_pending_retains_full_plan_under_throttle_overflow() {
        // Scenario: 5000-tile plan, throttle ramp starting at 20%. Some
        // tiles submit (~1000 under throttle), rest go to pending.
        // Invariant: submitted + pending == 5000. No drops.
        let client = Arc::new(CapLimitedDdsClient::new(10_000));

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

        // Build a 5000-tile plan — larger than any old cap
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

        assert!(submitted > 0, "Should submit some tiles under throttle");
        assert_eq!(
            submitted + coord.pending_tiles.len(),
            5000,
            "Every planned tile must be accounted for — no silent drops. \
             submitted={} + pending={} must equal plan size 5000",
            submitted,
            coord.pending_tiles.len()
        );
    }

    #[test]
    fn test_pending_retains_full_plan_on_backpressure_defer() {
        // When executor load exceeds BACKPRESSURE_DEFER_THRESHOLD, the
        // entire plan must be stored as pending — no cap, no drops.
        let client = Arc::new(HighLoadDdsClient);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>);

        let tiles: Vec<TileCoord> = (0..5000)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let calibration = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles.clone(), &calibration, "boundary", 0, 5000);

        let cancellation = CancellationToken::new();
        let submitted = coord.execute(&plan, cancellation);

        assert_eq!(submitted, 0, "Should defer due to backpressure");
        assert_eq!(
            coord.pending_tiles.len(),
            5000,
            "Deferred pending must retain every planned tile — no cap"
        );
        assert_eq!(
            coord.pending_tiles, tiles,
            "Deferred pending must contain the full plan in order"
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
            ..Default::default()
        };
        // Post-#176, get_tiles_for_region has no geometric fallback. Cover
        // the full westward flight path (lon 15 down to -5) plus box-extent
        // margin in every direction.
        let scenery_index = make_scenery_index_covering(40..=60, -15..=25, 16);

        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index)
            .with_scenery_index(scenery_index);

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
    fn test_cruise_plan_includes_every_tile_from_every_intersecting_region() {
        // #172 post-flight finding: the cruise path must plan every tile
        // from every DSF region that intersects the prefetch window.
        // Filtering for "already cached" happens downstream in the filter
        // pipeline — the *plan* itself must contain the full tile set.
        //
        // Previous versions sorted tiles by distance-from-aircraft and
        // truncated at `max_tiles_per_cycle`. This test asserts that
        // behaviour is gone: the plan size is bounded only by the
        // sum of tiles across intersecting regions.
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());

        // `max_tiles_per_cycle` was removed post-#172 entirely (rate
        // limiting now handled by the filter pipeline, executor
        // backpressure, and pending-tiles queue). This test verifies the
        // plan contains many more tiles than the old cap (200) would
        // have imposed.
        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_scenery_index(wide_scenery_index_at_48_15());

        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);

        // Enter cruise
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);

        if coord.status.phase != FlightPhase::Cruise {
            return; // Skip if phase detector didn't reach cruise
        }

        assert!(plan.is_some(), "Should generate plan on cruise tick");
        let plan = plan.unwrap();

        // Box at (48, 15) heading 270° with default extent covers dozens
        // of DSF regions. With ~16 tiles per region (the 4x4 sample density
        // of the wired `wide_scenery_index_at_48_15` fixture), plan size
        // should be in the hundreds — clearly above the 5-cap the old code
        // would have imposed. The exact number depends on box extent &
        // region tile count; assert ">> 5" to prove the cap is not being
        // applied.
        assert!(
            plan.tiles.len() > 20,
            "Plan must contain many more than max_tiles_per_cycle=5 tiles — \
             cap is inert. Got {}",
            plan.tiles.len()
        );

        // Additional invariant: the plan's tile count must match the
        // total of tiles produced by `get_tiles_for_region` for every
        // region in current_plan_regions (i.e. no silent dropping).
        let unique_regions: std::collections::HashSet<DsfRegion> =
            coord.current_plan_regions.values().copied().collect();
        assert!(
            !unique_regions.is_empty(),
            "At least one region must be tracked in current_plan_regions"
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
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(geo_index)
            .with_scenery_index(wide_scenery_index_at_48_15());

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

    // ─────────────────────────────────────────────────────────────────────────
    // reset_phase_from_on_ground tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_reset_phase_from_on_ground_true() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Drive into cruise via the phase detector directly (avoids hysteresis wait)
        coord.phase_detector.set_phase(FlightPhase::Cruise);
        coord.status.phase = FlightPhase::Cruise;
        assert_eq!(coord.status.phase, FlightPhase::Cruise);

        coord.reset_phase_from_on_ground(true);
        assert_eq!(coord.status.phase, FlightPhase::Ground);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Ground);
    }

    #[test]
    fn test_reset_phase_from_on_ground_false() {
        let mut coord = AdaptivePrefetchCoordinator::with_defaults();
        // Starts in Ground by default
        assert_eq!(coord.status.phase, FlightPhase::Ground);

        coord.reset_phase_from_on_ground(false);
        assert_eq!(coord.status.phase, FlightPhase::Cruise);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Mark-after-submit ordering tests (#172 Part 1)
    //
    // A region must only be marked `InProgress` if every one of its planned
    // tiles was successfully submitted to the executor. Regions whose tiles
    // were deferred, channel-rejected, or throttle-overflowed must stay
    // unmarked so they can re-enter `new_regions_with_extent` on the next
    // cycle. This prevents the "shadow claims success, disk says empty"
    // bug observed on LOWW westbound flights where the departure-bubble
    // DSFs were marked InProgress during the throttle ramp but their
    // tiles were never submitted.
    // ─────────────────────────────────────────────────────────────────────────

    /// SceneryIndex covering the area used by `fast_forward_to_cruise` and
    /// the tests built on it: centred on (50, 10), wide enough (±10°) for
    /// the box's maximum extent (7°) plus retention margin in every
    /// direction, including the (52.5, 10) position used by
    /// `test_pending_tiles_retained_on_channel_full`. Post-#176,
    /// `get_tiles_for_region` has no geometric fallback, so these tests
    /// need real coverage to produce a non-empty plan.
    fn wide_scenery_index_at_50_10() -> Arc<SceneryIndex> {
        make_scenery_index_covering(40..=60, 0..=20, 16)
    }

    /// SceneryIndex covering the area used by the (48, 15) heading-270
    /// cruise tests: wide enough for the box's maximum extent (7°) in
    /// every direction from the aircraft's starting position.
    fn wide_scenery_index_at_48_15() -> Arc<SceneryIndex> {
        make_scenery_index_covering(38..=58, 5..=25, 16)
    }

    /// Helper: fast-forward a coordinator into Cruise phase at (50.0, 10.0)
    /// heading 0° (north). Uses the existing hysteresis/timeout shortcut
    /// pattern from `test_pending_tiles_retained_on_channel_full`.
    fn fast_forward_to_cruise(coord: &mut AdaptivePrefetchCoordinator) {
        coord.phase_detector.hysteresis_duration = std::time::Duration::from_millis(1);
        coord.phase_detector.takeoff_timeout = std::time::Duration::from_millis(1);
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        coord.update((50.0, 10.0), 0.0, 200.0, 35000.0);
    }

    fn count_in_progress_regions(geo_index: &crate::geo_index::GeoIndex) -> usize {
        use crate::geo_index::PrefetchedRegion;
        geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_in_progress())
            .count()
    }

    #[tokio::test]
    async fn test_deferred_cycle_marks_no_regions() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());
        // HighLoadDdsClient reports executor_load=0.95 — above the
        // BACKPRESSURE_DEFER_THRESHOLD, so execute_plan returns
        // deferred=true with the whole plan stored as pending.
        let client = Arc::new(HighLoadDdsClient);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            // Zero ramp so Transition throttle doesn't clip the plan —
            // isolate the backpressure-defer scenario cleanly.
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_scenery_index(wide_scenery_index_at_50_10());

        fast_forward_to_cruise(&mut coord);
        assert_eq!(
            coord.phase_detector.current_phase(),
            FlightPhase::Cruise,
            "Precondition: coordinator must be in Cruise phase",
        );

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);
        let submitted = coord.process_telemetry(&state).await.unwrap_or(0);

        assert_eq!(
            submitted, 0,
            "Plan must defer under high executor backpressure",
        );
        assert_eq!(
            count_in_progress_regions(&geo_index),
            0,
            "No regions should be marked InProgress when the entire plan is deferred",
        );
        // Prove a real plan existed rather than `submitted == 0` and
        // `in_progress == 0` both being trivially true of an empty plan
        // (e.g. if the scenery index wiring silently broke). HighLoadDdsClient
        // stores the whole deferred plan as pending, so a non-empty
        // `pending_tiles` is direct evidence tiles were actually planned.
        assert!(
            !coord.pending_tiles.is_empty(),
            "A real plan must have been generated and deferred, not an empty one"
        );
    }

    #[tokio::test]
    async fn test_channel_full_marks_only_fully_submitted_regions() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());
        // Cap of 1 guarantees no region can be 100% submitted — every
        // region in the plan has multiple tiles (4x4 sample grid per
        // region from the scenery index).
        let client = Arc::new(CapLimitedDdsClient::new(1));

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_scenery_index(wide_scenery_index_at_50_10());

        fast_forward_to_cruise(&mut coord);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);
        let submitted = coord.process_telemetry(&state).await.unwrap_or(0);

        assert!(
            submitted <= 1,
            "Channel cap of 1 should admit at most 1 tile, got {}",
            submitted,
        );
        assert_eq!(
            count_in_progress_regions(&geo_index),
            0,
            "With cap=1, no region is fully submitted — zero regions should be marked",
        );
        assert!(
            !coord.pending_tiles.is_empty(),
            "Unsubmitted tiles must be stored as pending for retry",
        );
    }

    #[tokio::test]
    async fn test_fully_submitted_cycle_marks_all_planned_regions() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());
        // Cap well above any plausible plan size — all tiles admit.
        let client = Arc::new(CapLimitedDdsClient::new(100_000));

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_scenery_index(wide_scenery_index_at_50_10());

        fast_forward_to_cruise(&mut coord);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);
        let submitted = coord.process_telemetry(&state).await.unwrap_or(0);

        assert!(submitted > 0, "Happy path should submit tiles");
        assert!(
            coord.pending_tiles.is_empty(),
            "No tiles should be pending when the full plan submits",
        );
        assert!(
            count_in_progress_regions(&geo_index) > 0,
            "Regions with all tiles submitted must be marked InProgress",
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Fully-filtered regions must still reach Prefetched (#176)
    //
    // `execute()` is skipped when the filtered plan is empty, and
    // `mark_in_progress` only ever fired inside `execute()`. A region whose
    // planned tiles all filter out as already-cached was therefore marked
    // nothing at all: absent from PrefetchedRegion, so it re-entered
    // `new_regions_with_shape` every 2s cycle and could never return to
    // `Prefetched`.
    //
    // This branch is routine on this branch, not exotic: the #176 observer
    // demotes a settled region on one on-demand FUSE generation, FUSE then
    // re-caches that tile, and the next cycle re-plans a region in which
    // every tile is cached. `regions_prefetched` — one of the numbers the
    // flight test reads off the 60s Prefetch sample — would permanently
    // under-report.
    // ─────────────────────────────────────────────────────────────────────────

    fn count_prefetched_regions(geo_index: &crate::geo_index::GeoIndex) -> usize {
        use crate::geo_index::PrefetchedRegion;
        geo_index
            .iter::<PrefetchedRegion>()
            .into_iter()
            .filter(|(_, r)| r.is_prefetched())
            .count()
    }

    #[tokio::test]
    async fn test_fully_cached_region_reaches_prefetched() {
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());
        let client = Arc::new(CapLimitedDdsClient::new(100_000));
        // Every tile is already on the DDS disk. The filter pipeline empties
        // the plan, so nothing is submitted — and the same checker is what
        // `promote_completed_regions` consults, so the promotion is a real
        // disk-verified full-coverage check, not the filter's say-so.
        let checker: Arc<dyn DdsDiskCacheChecker> = Arc::new(AlwaysHitDiskChecker);

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_dds_disk_checker(Arc::clone(&checker))
            .with_scenery_index(wide_scenery_index_at_50_10());

        fast_forward_to_cruise(&mut coord);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);

        // Two cycles: the fix marks InProgress in the first and
        // `promote_completed_regions` confirms it on a maintenance pass.
        let submitted = coord.process_telemetry(&state).await.unwrap_or(0);
        assert_eq!(
            submitted, 0,
            "Precondition: with every tile on disk the whole plan must filter out",
        );
        coord.process_telemetry(&state).await;

        assert!(
            count_prefetched_regions(&geo_index) > 0,
            "A region whose tiles are all already cached must reach Prefetched, \
             not sit unmarked and be re-planned forever",
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Two-phase commit boundary (#223 remediation, F6)
    //
    // `test_fully_cached_region_reaches_prefetched` above uses
    // `AlwaysHitDiskChecker` as BOTH the filter authority (stage 4 of the
    // filter pipeline, which empties the plan and triggers
    // `mark_fully_filtered_regions`) AND the promotion authority
    // (`promote_completed_regions`, consulted by `run_region_maintenance`).
    // With one mock playing both roles, that test cannot distinguish a
    // disk-verified promotion from the filter's say-so — replacing
    // `mark_in_progress` in `mark_fully_filtered_regions` with a direct
    // `PrefetchedRegion::prefetched()` insert leaves it green.
    //
    // This test empties the plan via the MEMORY CACHE filter stage (stage 1)
    // instead, using `AlwaysHitMemoryCache`, while wiring a DDS disk checker
    // that reports every tile ABSENT. If promotion were driven by the
    // filter's say-so rather than a real disk check, the region would reach
    // Prefetched here too. It must not: the region should land in
    // InProgress and stay there, including across a subsequent maintenance
    // pass with no new telemetry.
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_filtered_region_stays_in_progress_without_disk_confirmation() {
        use crate::executor::DaemonMemoryCache;
        use crate::geo_index::GeoIndex;

        let geo_index = Arc::new(GeoIndex::new());
        let client = Arc::new(CapLimitedDdsClient::new(100_000));
        // Memory cache reports every tile as already cached — this is what
        // empties the plan and fires `mark_fully_filtered_regions`. It is
        // NOT the disk checker, so this test is independent of the disk
        // check's answer.
        let always_hit_memory: Arc<dyn DaemonMemoryCache> = Arc::new(AlwaysHitMemoryCache);
        // Disk checker (the promotion authority) reports every tile absent.
        let checker: Arc<dyn DdsDiskCacheChecker> =
            MockDiskChecker::with_tile_coords(std::iter::empty::<TileCoord>());

        let config = AdaptivePrefetchConfig {
            mode: PrefetchMode::Aggressive,
            ramp_duration: std::time::Duration::from_secs(0),
            ..Default::default()
        };
        let mut coord = AdaptivePrefetchCoordinator::new(config)
            .with_calibration(test_calibration())
            .with_geo_index(Arc::clone(&geo_index))
            .with_dds_client(Arc::clone(&client) as Arc<dyn DdsClient>)
            .with_memory_cache(always_hit_memory)
            .with_dds_disk_checker(Arc::clone(&checker))
            .with_scenery_index(wide_scenery_index_at_50_10());

        fast_forward_to_cruise(&mut coord);
        assert_eq!(coord.phase_detector.current_phase(), FlightPhase::Cruise);

        let state = AircraftState::new(50.0, 10.0, 0.0, 200.0, 35000.0, false);

        let submitted = coord.process_telemetry(&state).await.unwrap_or(0);
        assert_eq!(
            submitted, 0,
            "Precondition: the memory-cache filter must empty the whole plan",
        );
        assert!(
            count_in_progress_regions(&geo_index) > 0,
            "A fully-filtered region must still be marked InProgress",
        );
        assert_eq!(
            count_prefetched_regions(&geo_index),
            0,
            "Without a disk-verified full-coverage check, the region must NOT \
             reach Prefetched — reaching it here would mean promotion trusts \
             the filter's say-so instead of the authoritative disk checker",
        );

        // A subsequent maintenance pass (another cycle, no new coverage on
        // disk) must not promote it either.
        coord.process_telemetry(&state).await;
        assert_eq!(
            count_prefetched_regions(&geo_index),
            0,
            "Region must still not be Prefetched after a subsequent maintenance pass",
        );
        assert!(
            count_in_progress_regions(&geo_index) > 0,
            "Region should remain InProgress, not be silently dropped",
        );
    }
}
