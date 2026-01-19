//! Builder for creating prefetcher strategies.
//!
//! This module provides a builder for creating prefetcher instances based on
//! configuration settings. It encapsulates the complexity of configuring
//! different prefetch strategies (radial, heading-aware, tile-based).
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::PrefetcherBuilder;
//!
//! let prefetcher = PrefetcherBuilder::new()
//!     .strategy("auto")
//!     .memory_cache(memory_cache)
//!     .dds_client(client)
//!     .shared_status(status)
//!     .cone_half_angle(45.0)
//!     .outer_radius_nm(105.0)
//!     .radial_fallback_radius(3)
//!     .build();
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::executor::{DdsClient, MemoryCache};
use crate::ortho_union::OrthoUnionIndex;

use super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::config::{FuseInferenceConfig, HeadingAwarePrefetchConfig};
use super::heading_aware::{HeadingAwarePrefetcher, HeadingAwarePrefetcherConfig};
use super::inference::FuseRequestAnalyzer;
use super::load_monitor::FuseLoadMonitor;
use super::radial::{RadialPrefetchConfig, RadialPrefetcher};
use super::scenery_index::SceneryIndex;
use super::state::SharedPrefetchStatus;
use super::strategy::Prefetcher;
use super::throttler::PrefetchThrottler;
use super::tile_based::{DdsAccessEvent, TileBasedConfig, TileBasedPrefetcher};

/// Default telemetry staleness threshold in seconds.
const DEFAULT_TELEMETRY_STALE_SECS: u64 = 5;

/// Default FUSE confidence threshold for fallback.
const DEFAULT_FUSE_CONFIDENCE_THRESHOLD: f32 = 0.3;

/// Default zoom level for prefetch tiles.
const DEFAULT_ZOOM: u8 = 14;

/// Default TTL for failed attempts in seconds.
const DEFAULT_ATTEMPT_TTL_SECS: u64 = 60;

/// Builder for creating prefetcher strategies.
///
/// Uses the builder pattern to configure and construct the appropriate
/// prefetcher based on the selected strategy.
pub struct PrefetcherBuilder<M: MemoryCache> {
    // Required components
    memory_cache: Option<Arc<M>>,
    dds_client: Option<Arc<dyn DdsClient>>,

    // Strategy selection
    strategy: PrefetchStrategy,

    // Shared components
    shared_status: Option<Arc<SharedPrefetchStatus>>,

    // Heading-aware configuration
    cone_half_angle: f32,
    inner_radius_nm: f32,
    outer_radius_nm: f32,
    telemetry_stale_secs: u64,
    fuse_confidence_threshold: f32,

    // Radial configuration (used by radial strategy and as fallback)
    radial_radius: u8,
    zoom: u8,
    attempt_ttl_secs: u64,

    // Cycle timing configuration
    max_tiles_per_cycle: usize,
    cycle_interval_ms: u64,

    // External FUSE analyzer (for wiring callback to services before building)
    fuse_analyzer: Option<Arc<FuseRequestAnalyzer>>,

    // Scenery-aware prefetch (optional)
    scenery_index: Option<Arc<SceneryIndex>>,

    // Throttler for circuit breaker integration (optional)
    // When provided, the prefetcher checks this before each prefetch cycle
    throttler: Option<Arc<dyn PrefetchThrottler>>,

    // Tile-based configuration
    tile_based_rows_ahead: u32,
}

/// Available prefetch strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchStrategy {
    /// Simple radius-based prefetching around aircraft position.
    Radial,
    /// Direction-aware prefetching with forward cone and turn detection.
    HeadingAware,
    /// DSF tile-based prefetching aligned with X-Plane's loading behavior.
    /// Tracks 1° × 1° DSF tiles and prefetches neighboring tiles during quiet periods.
    TileBased,
    /// Automatic strategy selection with graceful degradation.
    /// Uses heading-aware when telemetry available, falls back to radial.
    Auto,
}

impl std::str::FromStr for PrefetchStrategy {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "radial" => Self::Radial,
            "heading-aware" => Self::HeadingAware,
            "tile-based" => Self::TileBased,
            _ => Self::Auto, // Default to auto for "auto" or unknown values
        })
    }
}

impl<M: MemoryCache + 'static> PrefetcherBuilder<M> {
    /// Create a new prefetcher builder with default settings.
    pub fn new() -> Self {
        Self {
            memory_cache: None,
            dds_client: None,
            strategy: PrefetchStrategy::Auto,
            shared_status: None,
            cone_half_angle: 30.0,
            inner_radius_nm: 85.0,
            outer_radius_nm: 95.0, // Reduced from 105nm for less aggressive prefetch
            telemetry_stale_secs: DEFAULT_TELEMETRY_STALE_SECS,
            fuse_confidence_threshold: DEFAULT_FUSE_CONFIDENCE_THRESHOLD,
            radial_radius: 3,
            zoom: DEFAULT_ZOOM,
            attempt_ttl_secs: DEFAULT_ATTEMPT_TTL_SECS,
            max_tiles_per_cycle: 50, // Reduced from 100 for better bandwidth sharing
            cycle_interval_ms: 2000, // Increased from 1000ms for less aggressive prefetch
            fuse_analyzer: None,
            scenery_index: None,
            throttler: None,
            tile_based_rows_ahead: 1,
        }
    }

    /// Set the memory cache adapter (required).
    pub fn memory_cache(mut self, cache: Arc<M>) -> Self {
        self.memory_cache = Some(cache);
        self
    }

    /// Set the DDS client for submitting prefetch requests (required).
    pub fn dds_client(mut self, client: Arc<dyn DdsClient>) -> Self {
        self.dds_client = Some(client);
        self
    }

    /// Set the prefetch strategy.
    pub fn strategy(mut self, strategy: &str) -> Self {
        // FromStr never fails for PrefetchStrategy (returns Auto for unknown values)
        self.strategy = strategy.parse().unwrap_or(PrefetchStrategy::Auto);
        self
    }

    /// Set the shared prefetch status for UI updates.
    pub fn shared_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.shared_status = Some(status);
        self
    }

    /// Set the cone half-angle in degrees (for heading-aware strategy).
    pub fn cone_half_angle(mut self, degrees: f32) -> Self {
        self.cone_half_angle = degrees;
        self
    }

    /// Set the inner radius in nautical miles (where prefetch zone starts).
    pub fn inner_radius_nm(mut self, nm: f32) -> Self {
        self.inner_radius_nm = nm;
        self
    }

    /// Set the outer radius in nautical miles (where prefetch zone ends).
    pub fn outer_radius_nm(mut self, nm: f32) -> Self {
        self.outer_radius_nm = nm;
        self
    }

    /// Set telemetry staleness threshold in seconds.
    pub fn telemetry_stale_secs(mut self, secs: u64) -> Self {
        self.telemetry_stale_secs = secs;
        self
    }

    /// Set FUSE confidence threshold for fallback activation.
    pub fn fuse_confidence_threshold(mut self, threshold: f32) -> Self {
        self.fuse_confidence_threshold = threshold;
        self
    }

    /// Set the radial radius in tiles (for radial strategy and fallback).
    pub fn radial_radius(mut self, radius: u8) -> Self {
        self.radial_radius = radius;
        self
    }

    /// Set the zoom level for prefetch tiles.
    pub fn zoom(mut self, zoom: u8) -> Self {
        self.zoom = zoom;
        self
    }

    /// Set the TTL for failed attempt tracking in seconds.
    pub fn attempt_ttl_secs(mut self, secs: u64) -> Self {
        self.attempt_ttl_secs = secs;
        self
    }

    /// Set the maximum tiles to submit per prefetch cycle.
    pub fn max_tiles_per_cycle(mut self, count: usize) -> Self {
        self.max_tiles_per_cycle = count;
        self
    }

    /// Set the interval between prefetch cycles in milliseconds.
    pub fn cycle_interval_ms(mut self, ms: u64) -> Self {
        self.cycle_interval_ms = ms;
        self
    }

    /// Set the FUSE request analyzer for position inference.
    ///
    /// When provided, this analyzer is used instead of creating a new one.
    /// This allows wiring the analyzer's callback to services before building
    /// the prefetcher.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
    /// let callback = analyzer.callback();
    ///
    /// // Wire callback to services...
    ///
    /// let prefetcher = PrefetcherBuilder::new()
    ///     .memory_cache(cache)
    ///     .dds_client(client)
    ///     .with_fuse_analyzer(analyzer)
    ///     .build();
    /// ```
    pub fn with_fuse_analyzer(mut self, analyzer: Arc<FuseRequestAnalyzer>) -> Self {
        self.fuse_analyzer = Some(analyzer);
        self
    }

    /// Set the scenery index for scenery-aware prefetching.
    ///
    /// When provided, the prefetcher uses the index to find tiles that exist
    /// in the scenery package, rather than calculating coordinates. This ensures:
    ///
    /// - Exact zoom levels per tile (from .ter files)
    /// - Only tiles that exist in the package are prefetched
    /// - Sea tiles can be deprioritized
    ///
    /// # Example
    ///
    /// ```ignore
    /// let index = Arc::new(SceneryIndex::with_defaults());
    /// index.build_from_package(Path::new("/path/to/scenery"))?;
    ///
    /// let prefetcher = PrefetcherBuilder::new()
    ///     .memory_cache(cache)
    ///     .dds_client(client)
    ///     .with_scenery_index(index)
    ///     .build();
    /// ```
    pub fn with_scenery_index(mut self, index: Arc<SceneryIndex>) -> Self {
        self.scenery_index = Some(index);
        self
    }

    /// Set a custom throttler for controlling when prefetching pauses.
    ///
    /// Use this when you have a custom `PrefetchThrottler` implementation.
    /// For the standard circuit breaker, use `with_circuit_breaker_throttler` instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::prefetch::NeverThrottle;
    ///
    /// let prefetcher = PrefetcherBuilder::new()
    ///     .memory_cache(cache)
    ///     .dds_client(client)
    ///     .with_throttler(Arc::new(NeverThrottle)) // For testing
    ///     .build();
    /// ```
    pub fn with_throttler(mut self, throttler: Arc<dyn PrefetchThrottler>) -> Self {
        self.throttler = Some(throttler);
        self
    }

    /// Create and set a circuit breaker throttler.
    ///
    /// This is the standard way to wire up prefetch throttling. The circuit
    /// breaker monitors FUSE request rate via the load monitor and pauses
    /// prefetching when X-Plane is under heavy load.
    ///
    /// # Arguments
    ///
    /// * `load_monitor` - Shared load monitor that tracks FUSE requests
    /// * `config` - Circuit breaker configuration (threshold, durations)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::prefetch::CircuitBreakerConfig;
    /// use std::time::Duration;
    ///
    /// let config = CircuitBreakerConfig {
    ///     threshold_jobs_per_sec: 50.0,
    ///     open_duration: Duration::from_millis(500),
    ///     half_open_duration: Duration::from_secs(2),
    /// };
    ///
    /// let prefetcher = PrefetcherBuilder::new()
    ///     .memory_cache(cache)
    ///     .dds_client(client)
    ///     .with_circuit_breaker_throttler(load_monitor, config)
    ///     .build();
    /// ```
    pub fn with_circuit_breaker_throttler(
        mut self,
        load_monitor: Arc<dyn FuseLoadMonitor>,
        config: CircuitBreakerConfig,
    ) -> Self {
        let circuit_breaker = CircuitBreaker::new(config, load_monitor);
        self.throttler = Some(Arc::new(circuit_breaker));
        self
    }

    /// Set the number of DSF tile rows to prefetch ahead (for tile-based strategy).
    ///
    /// Higher values prefetch more aggressively but use more bandwidth.
    /// Default: 1 (prefetch the immediate next row of tiles).
    pub fn tile_based_rows_ahead(mut self, rows: u32) -> Self {
        self.tile_based_rows_ahead = rows;
        self
    }

    /// Build a tile-based prefetcher instance.
    ///
    /// This method requires additional components specific to tile-based prefetching:
    /// - `ortho_index`: Index for enumerating DDS files in DSF tiles
    /// - `access_rx`: Channel receiving DDS access events from FUSE
    ///
    /// # Panics
    ///
    /// Panics if required components (memory_cache, dds_client, throttler) are not set.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (tx, rx) = mpsc::unbounded_channel();
    ///
    /// let prefetcher = PrefetcherBuilder::new()
    ///     .memory_cache(cache)
    ///     .dds_client(client)
    ///     .with_throttler(circuit_breaker)
    ///     .tile_based_rows_ahead(1)
    ///     .build_tile_based(ortho_index, rx);
    /// ```
    pub fn build_tile_based(
        self,
        ortho_index: Arc<OrthoUnionIndex>,
        access_rx: mpsc::UnboundedReceiver<DdsAccessEvent>,
    ) -> Box<dyn Prefetcher> {
        let memory_cache = self
            .memory_cache
            .expect("memory_cache is required for build_tile_based");
        let dds_client = self
            .dds_client
            .expect("dds_client is required for build_tile_based");
        let throttler = self
            .throttler
            .expect("throttler is required for build_tile_based (use with_throttler or with_circuit_breaker_throttler)");

        let config = TileBasedConfig {
            rows_ahead: self.tile_based_rows_ahead,
            ..TileBasedConfig::default()
        };

        let prefetcher = TileBasedPrefetcher::new(
            ortho_index,
            dds_client,
            memory_cache,
            access_rx,
            throttler,
            config,
        );

        Box::new(prefetcher)
    }

    /// Build the prefetcher instance.
    ///
    /// # Panics
    ///
    /// Panics if required components (memory_cache, dds_client) are not set.
    pub fn build(self) -> Box<dyn Prefetcher> {
        let memory_cache = self
            .memory_cache
            .expect("memory_cache is required for PrefetcherBuilder");
        let dds_client = self
            .dds_client
            .expect("dds_client is required for PrefetcherBuilder");

        match self.strategy {
            PrefetchStrategy::Radial => {
                // Ring-based radial prefetcher (uses nautical mile annulus)
                let config = RadialPrefetchConfig {
                    inner_radius_nm: self.inner_radius_nm,
                    outer_radius_nm: self.outer_radius_nm,
                    zoom: self.zoom,
                    attempt_ttl: Duration::from_secs(self.attempt_ttl_secs),
                };

                let mut prefetcher = RadialPrefetcher::new(memory_cache, dds_client, config);

                if let Some(status) = self.shared_status {
                    prefetcher = prefetcher.with_shared_status(status);
                }

                if let Some(throttler) = self.throttler {
                    prefetcher = prefetcher.with_throttler(throttler);
                }

                Box::new(prefetcher)
            }
            PrefetchStrategy::HeadingAware | PrefetchStrategy::Auto => {
                // Heading-aware prefetcher with graceful degradation
                let heading_config = HeadingAwarePrefetchConfig {
                    cone_half_angle: self.cone_half_angle,
                    inner_radius_nm: self.inner_radius_nm,
                    outer_radius_nm: self.outer_radius_nm,
                    zoom: self.zoom,
                    max_tiles_per_cycle: self.max_tiles_per_cycle,
                    cycle_interval_ms: self.cycle_interval_ms,
                    ..HeadingAwarePrefetchConfig::default()
                };

                let fuse_config = FuseInferenceConfig::default();

                let config = HeadingAwarePrefetcherConfig {
                    heading: heading_config,
                    fuse_inference: fuse_config.clone(),
                    telemetry_stale_threshold: Duration::from_secs(self.telemetry_stale_secs),
                    fuse_confidence_threshold: self.fuse_confidence_threshold,
                    radial_fallback_radius: self.radial_radius,
                };

                // Use provided analyzer or create a new one
                // Providing an external analyzer allows wiring its callback to services
                let fuse_analyzer = self
                    .fuse_analyzer
                    .unwrap_or_else(|| Arc::new(FuseRequestAnalyzer::new(fuse_config)));

                let mut prefetcher =
                    HeadingAwarePrefetcher::new(memory_cache, dds_client, config, fuse_analyzer);

                if let Some(status) = self.shared_status {
                    prefetcher = prefetcher.with_shared_status(status);
                }

                if let Some(index) = self.scenery_index {
                    prefetcher = prefetcher.with_scenery_index(index);
                }

                if let Some(throttler) = self.throttler {
                    prefetcher = prefetcher.with_throttler(throttler);
                }

                Box::new(prefetcher)
            }
            PrefetchStrategy::TileBased => {
                panic!(
                    "TileBased strategy requires build_tile_based() instead of build(). \
                     TileBased needs OrthoUnionIndex and DDS access channel from FUSE."
                );
            }
        }
    }
}

impl<M: MemoryCache> Default for PrefetcherBuilder<M> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    /// Mock memory cache for testing.
    struct MockCache {
        entries: RwLock<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MemoryCache for MockCache {
        fn get(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
            let result = self.entries.read().unwrap().get(&(row, col, zoom)).cloned();
            async move { result }
        }

        fn put(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
            data: Vec<u8>,
        ) -> impl std::future::Future<Output = ()> + Send {
            self.entries.write().unwrap().insert((row, col, zoom), data);
            async {}
        }

        fn size_bytes(&self) -> usize {
            self.entries.read().unwrap().values().map(|v| v.len()).sum()
        }

        fn entry_count(&self) -> usize {
            self.entries.read().unwrap().len()
        }
    }

    #[test]
    fn test_strategy_parsing() {
        assert_eq!(
            "radial".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::Radial
        );
        assert_eq!(
            "heading-aware".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::HeadingAware
        );
        assert_eq!(
            "tile-based".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::TileBased
        );
        assert_eq!(
            "auto".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::Auto
        );
        assert_eq!(
            "RADIAL".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::Radial
        );
        assert_eq!(
            "TILE-BASED".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::TileBased
        );
        assert_eq!(
            "unknown".parse::<PrefetchStrategy>().unwrap(),
            PrefetchStrategy::Auto
        );
    }

    #[test]
    fn test_builder_defaults() {
        let builder: PrefetcherBuilder<MockCache> = PrefetcherBuilder::new();
        assert_eq!(builder.strategy, PrefetchStrategy::Auto);
        assert_eq!(builder.radial_radius, 3);
        assert_eq!(builder.zoom, 14);
    }

    #[test]
    fn test_builder_chain() {
        let builder: PrefetcherBuilder<MockCache> = PrefetcherBuilder::new()
            .strategy("radial")
            .radial_radius(5)
            .cone_half_angle(45.0)
            .outer_radius_nm(110.0);

        assert_eq!(builder.strategy, PrefetchStrategy::Radial);
        assert_eq!(builder.radial_radius, 5);
        assert_eq!(builder.cone_half_angle, 45.0);
        assert_eq!(builder.outer_radius_nm, 110.0);
    }
}
