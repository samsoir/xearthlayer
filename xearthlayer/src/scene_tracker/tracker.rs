//! Scene Tracker trait and implementation.
//!
//! The Scene Tracker maintains an empirical model of what X-Plane has requested,
//! providing both query APIs (pull) and event subscriptions (push).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, trace};

use super::model::{DdsTileCoord, FuseAccessEvent, GeoBounds, GeoRegion};

/// Trait for querying scene loading state (pull API).
///
/// The Scene Tracker maintains empirical data about what X-Plane has requested.
/// It does NOT interpret the data - consumers derive meaning from it.
pub trait SceneTracker: Send + Sync {
    /// Get all DDS tiles X-Plane has requested this session.
    fn requested_tiles(&self) -> HashSet<DdsTileCoord>;

    /// Check if a specific DDS tile has been requested.
    fn is_tile_requested(&self, tile: &DdsTileCoord) -> bool;

    /// Get total number of tile requests this session.
    fn total_requests(&self) -> u64;

    // === Derived queries (calculated from empirical data) ===

    /// Derive which 1x1 regions have been loaded.
    ///
    /// This is calculated from the requested tiles, not stored directly.
    fn loaded_regions(&self) -> HashSet<GeoRegion>;

    /// Check if a 1x1 region has any requested tiles.
    fn is_region_loaded(&self, region: &GeoRegion) -> bool;

    /// Derive the geographic bounding box of all requested tiles.
    ///
    /// Returns `None` if no tiles have been requested.
    fn loaded_bounds(&self) -> Option<GeoBounds>;
}

/// Internal state for the scene tracker.
struct TrackerState {
    /// All tiles requested this session.
    requested_tiles: HashSet<DdsTileCoord>,

    /// Total requests counter.
    total_requests: u64,
}

impl TrackerState {
    fn new() -> Self {
        Self {
            requested_tiles: HashSet::new(),
            total_requests: 0,
        }
    }
}

/// Configuration for the DefaultSceneTracker.
#[derive(Debug, Clone)]
pub struct SceneTrackerConfig {
    /// Channel capacity for tile access broadcasts.
    pub tile_channel_capacity: usize,
}

impl Default for SceneTrackerConfig {
    fn default() -> Self {
        Self {
            tile_channel_capacity: 256,
        }
    }
}

/// Default implementation of the Scene Tracker.
///
/// Receives events from FUSE via an unbounded channel and maintains
/// the empirical model of X-Plane's requests.
pub struct DefaultSceneTracker {
    /// Thread-safe state for detailed tracking.
    state: Arc<RwLock<TrackerState>>,

    /// Broadcast channel for tile access events.
    tile_tx: broadcast::Sender<DdsTileCoord>,
}

impl DefaultSceneTracker {
    /// Create a new scene tracker with the given configuration.
    pub fn new(config: SceneTrackerConfig) -> Self {
        let (tile_tx, _) = broadcast::channel(config.tile_channel_capacity);

        Self {
            state: Arc::new(RwLock::new(TrackerState::new())),
            tile_tx,
        }
    }

    /// Create a scene tracker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SceneTrackerConfig::default())
    }

    /// Subscribe to individual tile access events.
    ///
    /// Note: This is high-volume during scene loading.
    pub fn subscribe_tile_access(&self) -> broadcast::Receiver<DdsTileCoord> {
        self.tile_tx.subscribe()
    }

    /// Start the scene tracker's event processing loop.
    ///
    /// This spawns an async task that processes events from the FUSE layer.
    /// The task runs until the receiver is closed.
    ///
    /// # Arguments
    ///
    /// * `rx` - Unbounded receiver for FUSE access events
    ///
    /// # Returns
    ///
    /// A handle to the spawned task.
    pub fn start(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<FuseAccessEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            debug!("Scene tracker started, waiting for FUSE events");

            while let Some(event) = rx.recv().await {
                self.process_event(event);
            }

            debug!("Scene tracker stopped (channel closed)");
        })
    }

    /// Start the scene tracker on a specific runtime handle.
    ///
    /// Like [`start`], but uses the provided runtime handle instead of
    /// `tokio::spawn()`, which requires an active tokio runtime context.
    pub fn start_on(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<FuseAccessEvent>,
        handle: &tokio::runtime::Handle,
    ) -> tokio::task::JoinHandle<()> {
        let tracker = self;
        handle.spawn(async move {
            debug!("Scene tracker started, waiting for FUSE events");
            while let Some(event) = rx.recv().await {
                tracker.process_event(event);
            }
            debug!("Scene tracker stopped (channel closed)");
        })
    }

    /// Process a single FUSE access event.
    fn process_event(&self, event: FuseAccessEvent) {
        trace!(
            tile = %event.tile,
            "Scene tracker received tile access"
        );

        // Broadcast the tile access (ignore errors - no subscribers is OK)
        let _ = self.tile_tx.send(event.tile);

        // Update state
        if let Ok(mut state) = self.state.write() {
            state.requested_tiles.insert(event.tile);
            state.total_requests += 1;
        }
    }
}

impl SceneTracker for DefaultSceneTracker {
    fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
        self.state
            .read()
            .map(|s| s.requested_tiles.clone())
            .unwrap_or_default()
    }

    fn is_tile_requested(&self, tile: &DdsTileCoord) -> bool {
        self.state
            .read()
            .map(|s| s.requested_tiles.contains(tile))
            .unwrap_or(false)
    }

    fn total_requests(&self) -> u64 {
        self.state.read().map(|s| s.total_requests).unwrap_or(0)
    }

    fn loaded_regions(&self) -> HashSet<GeoRegion> {
        self.state
            .read()
            .map(|s| {
                s.requested_tiles
                    .iter()
                    .map(|t| t.to_geo_region())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_region_loaded(&self, region: &GeoRegion) -> bool {
        self.state
            .read()
            .map(|s| {
                s.requested_tiles
                    .iter()
                    .any(|t| t.to_geo_region() == *region)
            })
            .unwrap_or(false)
    }

    fn loaded_bounds(&self) -> Option<GeoBounds> {
        self.state.read().ok().and_then(|s| {
            let mut iter = s.requested_tiles.iter();
            let first = iter.next()?;
            let (lat, lon) = first.to_lat_lon();
            let mut bounds = GeoBounds::from_point(lat, lon);

            for tile in iter {
                let (lat, lon) = tile.to_lat_lon();
                bounds.expand(lat, lon);
            }

            Some(bounds)
        })
    }
}

// Allow Arc<DefaultSceneTracker> to be used as SceneTracker
impl SceneTracker for Arc<DefaultSceneTracker> {
    fn requested_tiles(&self) -> HashSet<DdsTileCoord> {
        (**self).requested_tiles()
    }

    fn is_tile_requested(&self, tile: &DdsTileCoord) -> bool {
        (**self).is_tile_requested(tile)
    }

    fn total_requests(&self) -> u64 {
        SceneTracker::total_requests(&**self)
    }

    fn loaded_regions(&self) -> HashSet<GeoRegion> {
        (**self).loaded_regions()
    }

    fn is_region_loaded(&self, region: &GeoRegion) -> bool {
        (**self).is_region_loaded(region)
    }

    fn loaded_bounds(&self) -> Option<GeoBounds> {
        (**self).loaded_bounds()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tile(row: u32, col: u32) -> DdsTileCoord {
        DdsTileCoord::new(row, col, 18)
    }

    fn make_event(row: u32, col: u32) -> FuseAccessEvent {
        FuseAccessEvent::new(make_tile(row, col))
    }

    #[test]
    fn test_default_scene_tracker_creation() {
        let tracker = DefaultSceneTracker::with_defaults();
        assert!(tracker.requested_tiles().is_empty());
        assert_eq!(SceneTracker::total_requests(&tracker), 0);
    }

    #[test]
    fn test_process_event() {
        let tracker = DefaultSceneTracker::with_defaults();

        tracker.process_event(make_event(100000, 125184));

        assert_eq!(SceneTracker::total_requests(&tracker), 1);
        assert!(tracker.is_tile_requested(&make_tile(100000, 125184)));
        assert!(!tracker.is_tile_requested(&make_tile(100001, 125184)));
    }

    #[test]
    fn test_loaded_regions() {
        let tracker = DefaultSceneTracker::with_defaults();

        // Add tiles in the Hamburg area (ZL18 tiles)
        tracker.process_event(make_event(83776, 138240)); // ~53.5N, 10.0E
        tracker.process_event(make_event(83777, 138241));

        let regions = tracker.loaded_regions();
        assert!(!regions.is_empty());

        // All tiles should be in approximately the same region
        // (may span multiple regions depending on exact coordinates)
    }

    #[test]
    fn test_loaded_bounds() {
        let tracker = DefaultSceneTracker::with_defaults();

        assert!(tracker.loaded_bounds().is_none());

        tracker.process_event(make_event(83776, 138240));
        tracker.process_event(make_event(83778, 138242));

        let bounds = tracker.loaded_bounds().unwrap();

        // Bounds should encompass both tiles
        let (lat1, lon1) = make_tile(83776, 138240).to_lat_lon();
        let (lat2, lon2) = make_tile(83778, 138242).to_lat_lon();

        assert!(bounds.min_lat <= lat1.min(lat2));
        assert!(bounds.max_lat >= lat1.max(lat2));
        assert!(bounds.min_lon <= lon1.min(lon2));
        assert!(bounds.max_lon >= lon1.max(lon2));
    }

    #[test]
    fn test_is_region_loaded() {
        let tracker = DefaultSceneTracker::with_defaults();

        // Hamburg area tile
        tracker.process_event(make_event(83776, 138240));

        let tile = make_tile(83776, 138240);
        let region = tile.to_geo_region();

        assert!(tracker.is_region_loaded(&region));

        // Far away region should not be loaded
        let far_region = GeoRegion::new(-50, -100);
        assert!(!tracker.is_region_loaded(&far_region));
    }

    #[tokio::test]
    async fn test_start_and_process() {
        let tracker = Arc::new(DefaultSceneTracker::with_defaults());
        let (tx, rx) = mpsc::unbounded_channel();

        // Start the tracker
        let handle = tracker.clone().start(rx);

        // Send some events
        tx.send(make_event(100000, 125184)).unwrap();
        tx.send(make_event(100001, 125185)).unwrap();

        // Give time for processing
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(SceneTracker::total_requests(&*tracker), 2);
        assert!(tracker.is_tile_requested(&make_tile(100000, 125184)));
        assert!(tracker.is_tile_requested(&make_tile(100001, 125185)));

        // Close channel and wait for task
        drop(tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_tile_subscription() {
        let tracker = Arc::new(DefaultSceneTracker::with_defaults());
        let mut tile_rx = tracker.subscribe_tile_access();

        // Process an event
        tracker.process_event(make_event(100000, 125184));

        // Should receive the tile
        let received = tile_rx.try_recv().unwrap();
        assert_eq!(received, make_tile(100000, 125184));
    }
}
