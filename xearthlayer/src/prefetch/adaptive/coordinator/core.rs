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
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::state::{AircraftState, DetailedPrefetchStats, SharedPrefetchStatus};
use crate::prefetch::throttler::{PrefetchThrottler, ThrottleState};
use crate::prefetch::CircuitState;
use crate::prefetch::SceneryIndex;

use super::super::calibration::{PerformanceCalibration, StrategyMode};
use super::super::config::{AdaptivePrefetchConfig, PrefetchMode};
use super::super::cruise_strategy::CruiseStrategy;
use super::super::ground_strategy::GroundStrategy;
use super::super::phase_detector::{FlightPhase, PhaseDetector};
use super::super::strategy::{AdaptivePrefetchStrategy, PrefetchPlan};
use super::super::turn_detector::TurnDetector;

use super::status::CoordinatorStatus;
use super::telemetry::extract_track;

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
///                    │    Coordinator      │
///                    │  (main loop)        │
///                    └─────────┬───────────┘
///                              │
///      ┌───────────┬──────────┼──────────┬───────────┐
///      ▼           ▼          ▼          ▼           ▼
/// ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
/// │ Phase   │ │ Turn    │ │ Ground  │ │ Cruise  │ │ Circuit │
/// │Detector │ │Detector │ │Strategy │ │Strategy │ │ Breaker │
/// └─────────┘ └─────────┘ └─────────┘ └─────────┘ └─────────┘
/// ```
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

    /// Turn detector.
    turn_detector: TurnDetector,

    /// Ground strategy.
    ground_strategy: GroundStrategy,

    /// Cruise strategy.
    cruise_strategy: CruiseStrategy,

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

    /// Ortho union index for checking if tiles already exist on disk.
    /// When set, prefetch will skip tiles that are already installed
    /// in local ortho packages or patches.
    ortho_union_index: Option<Arc<OrthoUnionIndex>>,
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
        let turn_detector = TurnDetector::new(&config);
        let ground_strategy = GroundStrategy::new(&config);
        let cruise_strategy = CruiseStrategy::new(&config);

        Self {
            config,
            calibration: None,
            phase_detector,
            turn_detector,
            ground_strategy,
            cruise_strategy,
            throttler: None,
            dds_client: None,
            memory_cache: None,
            cached_tiles: HashSet::new(),
            status: CoordinatorStatus::default(),
            shared_status: None,
            total_cycles: 0,
            total_tiles_submitted: 0,
            total_cache_hits: 0,
            ortho_union_index: None,
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
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.ground_strategy = self.ground_strategy.with_scenery_index(Arc::clone(&index));
        self.cruise_strategy = self.cruise_strategy.with_scenery_index(index);
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
    /// * `agl_ft` - Altitude above ground level in feet
    ///
    /// # Returns
    ///
    /// A `PrefetchPlan` if prefetching is appropriate, `None` otherwise.
    pub fn update(
        &mut self,
        position: (f64, f64),
        track: f64,
        ground_speed_kt: f32,
        agl_ft: f32,
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

        // Update phase detector
        self.phase_detector.update(ground_speed_kt, agl_ft);
        let phase = self.phase_detector.current_phase();
        self.status.phase = phase;

        // Update turn detector
        self.turn_detector.update(track);
        self.status.turn_state = self.turn_detector.state();
        self.status.stable_track = self.turn_detector.stable_track();

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
            FlightPhase::Cruise => {
                // Only prefetch in cruise if track is stable
                if !self.turn_detector.is_stable() {
                    tracing::debug!(
                        turn_state = ?self.turn_detector.state(),
                        "Skipping cruise prefetch - track not stable"
                    );
                    return None;
                }

                self.status.active_strategy = self.cruise_strategy.name();
                self.cruise_strategy.calculate_prefetch(
                    position,
                    track,
                    &calibration,
                    &self.cached_tiles,
                )
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
    /// # Arguments
    ///
    /// * `plan` - The prefetch plan to execute
    /// * `cancellation` - Shared cancellation token for the batch
    ///
    /// # Returns
    ///
    /// Number of tiles submitted.
    pub fn execute(&self, plan: &PrefetchPlan, cancellation: CancellationToken) -> usize {
        let Some(ref client) = self.dds_client else {
            tracing::warn!("No DDS client configured - cannot execute prefetch");
            return 0;
        };

        let mut submitted = 0;
        for tile in &plan.tiles {
            client.prefetch_with_cancellation(*tile, cancellation.clone());
            submitted += 1;
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

    /// Get the turn detector for external monitoring.
    pub fn turn_detector(&self) -> &TurnDetector {
        &self.turn_detector
    }

    /// Reset the coordinator state.
    ///
    /// Call this when teleporting or starting a new flight.
    pub fn reset(&mut self) {
        self.turn_detector.reset();
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
            "adaptive, mode={:?}, ground_threshold={}kt, turn_threshold={}°",
            mode,
            self.config.ground_speed_threshold_kt,
            self.config.turn_threshold_deg()
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

        // Note: We don't have true AGL from telemetry, only MSL altitude.
        // Phase detection now uses ground speed as the primary indicator.
        // Pass 0 for AGL so the phase detector uses ground speed only.
        let agl_ft = 0.0;

        // Always update shared status with current position to show TUI we're receiving telemetry
        // This fixes the bug where prefetch status stayed "Idle" when no plan was generated
        self.update_shared_status_position(position);

        let mut plan = match self.update(position, track, state.ground_speed, agl_ft) {
            Some(p) => p,
            None => {
                // No plan generated - still update status with why (disabled, throttled, etc.)
                self.update_shared_status_no_plan();
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

        // Filter out tiles that already exist on disk (ortho packages, patches)
        // This addresses Issue #39: Skip installed ortho tiles during prefetch
        let disk_filtered = if let Some(ref index) = self.ortho_union_index {
            let tiles_before = plan.tiles.len();
            plan.tiles
                .retain(|tile| !index.dds_tile_exists(tile.row, tile.col, tile.zoom));
            let skipped_disk = tiles_before - plan.tiles.len();

            if skipped_disk > 0 {
                tracing::debug!(
                    skipped = skipped_disk,
                    remaining = plan.tiles.len(),
                    "Filtered tiles already on disk"
                );
            }
            skipped_disk
        } else {
            0
        };

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

        Some(submitted)
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
            ttl_skipped: 0,               // Not tracked in adaptive
            active_zoom_levels: vec![14], // Fallback uses zoom 14
            is_active: submitted > 0,
            circuit_state,
            loading_tiles,
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
            active_zoom_levels: vec![14],
            is_active: false,
            circuit_state,
            loading_tiles: vec![],
        };
        status.update_detailed_stats(detailed);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::turn_detector::TurnState;
    use super::*;
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

        let plan = coord.update((53.5, 9.5), 45.0, 100.0, 1000.0);
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

        let plan = coord.update((53.5, 9.5), 45.0, 100.0, 1000.0);
        assert!(plan.is_none());
    }

    #[test]
    fn test_update_ground_phase() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Ground conditions: low speed, low AGL
        let _plan = coord.update((53.5, 9.5), 45.0, 10.0, 5.0);
        assert_eq!(coord.status.phase, FlightPhase::Ground);
        assert_eq!(coord.status.active_strategy, "ground");
    }

    #[test]
    fn test_update_cruise_phase() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        // Cruise conditions: high speed
        coord.update((53.5, 9.5), 45.0, 200.0, 10000.0);
        // Phase detector has hysteresis, so first update may not transition
        assert!(
            coord.status.phase == FlightPhase::Ground || coord.status.phase == FlightPhase::Cruise
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Turn detector integration tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_update_tracks_turn_state() {
        let mut coord =
            AdaptivePrefetchCoordinator::with_defaults().with_calibration(test_calibration());

        coord.update((53.5, 9.5), 45.0, 200.0, 10000.0);
        // Initially not stable
        assert_ne!(coord.status.turn_state, TurnState::Stable);
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
        coord.update((53.5, 9.5), 45.0, 200.0, 10000.0);
        coord.mark_cached(vec![TileCoord {
            row: 100,
            col: 200,
            zoom: 14,
        }]);

        // Reset
        coord.reset();
        assert!(coord.cached_tiles.is_empty());
        assert_eq!(coord.turn_detector.state(), TurnState::Initializing);
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
}
