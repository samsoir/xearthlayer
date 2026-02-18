//! Prewarm context with authoritative job tracking.
//!
//! This module provides the core prewarm execution context that owns job lifecycle
//! and provides status via shared state rather than message passing.
//!
//! # Architecture
//!
//! - `PrewarmContext` - Internal context that runs as a self-driving tokio task
//! - `PrewarmStatus` - Shared state updated atomically as jobs complete
//! - `PrewarmHandle` - Lightweight handle for TUI/GUI to query status and cancel
//!
//! # Example
//!
//! ```ignore
//! let handle = PrewarmContext::start(
//!     "KSFO".to_string(),
//!     tiles,
//!     dds_client,
//!     memory_cache,
//!     ortho_index,
//!     &runtime_handle,
//! );
//!
//! // In TUI/GUI loop
//! let status = handle.status();
//! if status.is_complete {
//!     println!("Done: {} completed, {} failed", status.completed, status.failed);
//! }
//!
//! // To cancel
//! handle.cancel();
//! ```

use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::coord::TileCoord;
use crate::executor::{DdsClient, MemoryCache, Priority};
use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::tile_based::DsfTileCoord;
use crate::runtime::{JobRequest, RequestOrigin};

/// Maximum concurrent tile requests in flight.
///
/// This must be less than the executor's job channel capacity (256 by default)
/// to prevent job submission failures. We use 128 to leave headroom for FUSE
/// requests during prewarm.
const MAX_CONCURRENT: usize = 128;

/// Authoritative status of the prewarm operation.
///
/// This struct is the single source of truth for prewarm progress.
/// Updated atomically by the context as jobs complete.
#[derive(Debug, Clone)]
pub struct PrewarmStatus {
    /// Airport ICAO code being prewarmed.
    pub icao: String,
    /// Total tiles to process (including cache hits).
    pub total: usize,
    /// Tiles successfully generated.
    pub completed: usize,
    /// Tiles that failed to generate.
    pub failed: usize,
    /// Tiles that were already in memory cache.
    pub cache_hits: usize,
    /// Tiles skipped because their region is owned by a patch.
    pub patch_skipped: usize,
    /// Tiles that already exist on disk (XEL cache, etc.).
    pub disk_hits: usize,
    /// Whether prewarm has finished (success or failure).
    pub is_complete: bool,
    /// Whether prewarm was cancelled by user.
    pub was_cancelled: bool,
}

impl PrewarmStatus {
    /// Create initial status for a prewarm operation.
    fn new(icao: String, total: usize) -> Self {
        Self {
            icao,
            total,
            completed: 0,
            failed: 0,
            cache_hits: 0,
            patch_skipped: 0,
            disk_hits: 0,
            is_complete: false,
            was_cancelled: false,
        }
    }

    /// Number of tiles currently in flight (submitted but not yet complete).
    pub fn in_flight(&self) -> usize {
        self.total.saturating_sub(
            self.completed + self.failed + self.cache_hits + self.patch_skipped + self.disk_hits,
        )
    }

    /// Progress as a fraction from 0.0 to 1.0.
    pub fn progress_fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0; // Empty prewarm is "complete"
        }
        (self.completed + self.cache_hits + self.patch_skipped + self.disk_hits) as f64
            / self.total as f64
    }

    /// Total tiles that have been processed (completed + failed + cache_hits + patch_skipped + disk_hits).
    pub fn processed(&self) -> usize {
        self.completed + self.failed + self.cache_hits + self.patch_skipped + self.disk_hits
    }
}

/// Handle to a running prewarm operation.
///
/// Provides read-only access to status and ability to cancel.
/// Lightweight and cheap to clone.
#[derive(Clone)]
pub struct PrewarmHandle {
    status: Arc<Mutex<PrewarmStatus>>,
    cancellation: CancellationToken,
}

impl PrewarmHandle {
    /// Get current prewarm status (snapshot).
    ///
    /// Returns a clone of the current status. This is cheap since
    /// PrewarmStatus is small and the lock is held briefly.
    pub fn status(&self) -> PrewarmStatus {
        self.status.lock().clone()
    }

    /// Request cancellation of the prewarm operation.
    ///
    /// Cancellation is cooperative - in-flight jobs may still complete.
    pub fn cancel(&self) {
        info!("Prewarm cancellation requested");
        self.cancellation.cancel();
    }

    /// Check if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Get the cancellation token (for child operations).
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Internal context that drives the prewarm operation.
///
/// Runs as a self-driving tokio task. Updates shared status as jobs complete.
struct PrewarmContext<M: MemoryCache> {
    tiles: Vec<TileCoord>,
    dds_client: Arc<dyn DdsClient>,
    memory_cache: Arc<M>,
    ortho_index: Arc<OrthoUnionIndex>,
    geo_index: Option<Arc<GeoIndex>>,
    status: Arc<Mutex<PrewarmStatus>>,
    cancellation: CancellationToken,
}

impl<M: MemoryCache + Send + Sync + 'static> PrewarmContext<M> {
    /// Start a prewarm operation and return a handle.
    ///
    /// Spawns a self-driving task on the provided runtime that:
    /// 1. Filters out already-cached tiles
    /// 2. Submits jobs with a sliding window of MAX_CONCURRENT
    /// 3. Updates shared status as jobs complete
    /// 4. Marks complete when all jobs finish or cancelled
    ///
    /// # Arguments
    ///
    /// * `icao` - Airport ICAO code (for display)
    /// * `tiles` - Tiles to prewarm
    /// * `dds_client` - Client for submitting DDS generation jobs
    /// * `memory_cache` - Cache to check for already-cached tiles
    /// * `ortho_index` - Index for checking disk-resident tiles (patches, cache)
    /// * `geo_index` - Geospatial index for patched region filtering
    /// * `runtime` - Tokio runtime handle to spawn the task on
    pub fn start(
        icao: String,
        tiles: Vec<TileCoord>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        ortho_index: Arc<OrthoUnionIndex>,
        geo_index: Option<Arc<GeoIndex>>,
        runtime: &Handle,
    ) -> PrewarmHandle {
        let total = tiles.len();
        let status = Arc::new(Mutex::new(PrewarmStatus::new(icao.clone(), total)));
        let cancellation = CancellationToken::new();

        let handle = PrewarmHandle {
            status: Arc::clone(&status),
            cancellation: cancellation.clone(),
        };

        if tiles.is_empty() {
            // No tiles to process - mark complete immediately
            let mut s = status.lock();
            s.is_complete = true;
            info!(icao = %icao, "Prewarm started with no tiles");
            return handle;
        }

        let context = Self {
            tiles,
            dds_client,
            memory_cache,
            ortho_index,
            geo_index,
            status,
            cancellation,
        };

        info!(icao = %icao, total, "Starting prewarm context");
        runtime.spawn(context.run());

        handle
    }

    /// Main execution loop.
    async fn run(self) {
        // Step 1: Filter out cached tiles
        let mut to_generate = Vec::with_capacity(self.tiles.len());

        for tile in &self.tiles {
            if self
                .memory_cache
                .get(tile.row, tile.col, tile.zoom)
                .await
                .is_some()
            {
                self.increment_cache_hits();
            } else {
                to_generate.push(*tile);
            }
        }

        // Step 1b: Filter out tiles in patched regions (region-level ownership)
        let before_patch = to_generate.len();
        if let Some(ref geo_index) = self.geo_index {
            let gi = Arc::clone(geo_index);
            to_generate.retain(|tile| {
                let (lat, lon) = tile.to_lat_lon();
                let dsf = DsfTileCoord::from_lat_lon(lat, lon);
                !gi.contains::<PatchCoverage>(&DsfRegion::new(dsf.lat, dsf.lon))
            });
        }
        let patch_skipped = before_patch - to_generate.len();
        if patch_skipped > 0 {
            let mut s = self.status.lock();
            s.patch_skipped = patch_skipped;
        }

        // Step 1c: Filter out tiles already on disk (XEL cache, etc.)
        // Use chunk_origin() for the canonical tile → chunk coordinate conversion
        let before_disk = to_generate.len();
        to_generate.retain(|tile| {
            let (chunk_row, chunk_col, chunk_zoom) = tile.chunk_origin();
            !self
                .ortho_index
                .dds_tile_exists(chunk_row, chunk_col, chunk_zoom)
        });
        let disk_hits = before_disk - to_generate.len();
        if disk_hits > 0 {
            let mut s = self.status.lock();
            s.disk_hits = disk_hits;
        }

        let cache_hits = {
            let s = self.status.lock();
            s.cache_hits
        };

        info!(
            total = self.tiles.len(),
            cache_hits,
            patch_skipped,
            disk_hits,
            to_generate = to_generate.len(),
            "Prewarm tile filtering complete"
        );

        if to_generate.is_empty() {
            // All tiles were cached or already on disk
            self.mark_complete();
            return;
        }

        // Check for early cancellation
        if self.cancellation.is_cancelled() {
            self.mark_cancelled(to_generate.len());
            return;
        }

        // Step 2: Get sender for job submission
        let sender = match self.dds_client.sender() {
            Some(s) => s,
            None => {
                warn!("DdsClient does not support async submission");
                self.mark_all_failed(to_generate.len());
                return;
            }
        };

        // Step 3: Submit initial batch and track completions
        let mut pending = FuturesUnordered::new();
        let mut tiles_iter = to_generate.into_iter();

        // Submit initial batch
        for tile in tiles_iter.by_ref().take(MAX_CONCURRENT) {
            if let Some(rx) = self.submit_tile(&sender, tile).await {
                pending.push(rx);
            }
        }

        debug!(in_flight = pending.len(), "Initial batch submitted");

        // Step 4: Process completions with sliding window
        while !pending.is_empty() {
            tokio::select! {
                biased;

                // Check cancellation first
                _ = self.cancellation.cancelled() => {
                    let remaining = pending.len() + tiles_iter.len();
                    info!(
                        completed = self.status.lock().completed,
                        remaining,
                        "Prewarm cancelled"
                    );
                    self.mark_cancelled(remaining);
                    return;
                }

                // Process next completion
                Some(result) = pending.next() => {
                    self.handle_completion(result);

                    // Submit another tile if available
                    if let Some(tile) = tiles_iter.next() {
                        if let Some(rx) = self.submit_tile(&sender, tile).await {
                            pending.push(rx);
                        }
                    }
                }
            }
        }

        // Step 5: Mark complete
        self.mark_complete();
    }

    /// Submit a single tile for generation.
    async fn submit_tile(
        &self,
        sender: &tokio::sync::mpsc::Sender<JobRequest>,
        tile: TileCoord,
    ) -> Option<oneshot::Receiver<crate::runtime::DdsResponse>> {
        let (tx, rx) = oneshot::channel();
        let request = JobRequest {
            tile,
            priority: Priority::PREFETCH,
            cancellation: self.cancellation.child_token(),
            response_tx: Some(tx),
            origin: RequestOrigin::Prewarm,
        };

        match sender.send(request).await {
            Ok(()) => Some(rx),
            Err(_) => {
                warn!(?tile, "Failed to submit tile - executor may be shutdown");
                self.increment_failed();
                None
            }
        }
    }

    /// Handle a completed job.
    fn handle_completion(
        &self,
        result: Result<crate::runtime::DdsResponse, oneshot::error::RecvError>,
    ) {
        match result {
            Ok(response) if response.is_success() => {
                self.increment_completed();
            }
            Ok(_) => {
                // Response received but indicated failure
                self.increment_failed();
            }
            Err(_) => {
                // Sender dropped without response (executor crashed/cancelled)
                self.increment_failed();
            }
        }
    }

    /// Increment completed count.
    fn increment_completed(&self) {
        let mut s = self.status.lock();
        s.completed += 1;
    }

    /// Increment failed count.
    fn increment_failed(&self) {
        let mut s = self.status.lock();
        s.failed += 1;
    }

    /// Increment cache hits count.
    fn increment_cache_hits(&self) {
        let mut s = self.status.lock();
        s.cache_hits += 1;
    }

    /// Mark prewarm as complete.
    fn mark_complete(&self) {
        let mut s = self.status.lock();
        s.is_complete = true;
        info!(
            completed = s.completed,
            failed = s.failed,
            cache_hits = s.cache_hits,
            patch_skipped = s.patch_skipped,
            disk_hits = s.disk_hits,
            total = s.total,
            "Prewarm complete"
        );
    }

    /// Mark prewarm as cancelled.
    fn mark_cancelled(&self, remaining: usize) {
        let mut s = self.status.lock();
        s.is_complete = true;
        s.was_cancelled = true;
        info!(
            completed = s.completed,
            remaining, "Prewarm marked cancelled"
        );
    }

    /// Mark all remaining tiles as failed.
    fn mark_all_failed(&self, count: usize) {
        let mut s = self.status.lock();
        s.failed += count;
        s.is_complete = true;
        warn!(failed = s.failed, "Prewarm failed - no sender available");
    }
}

/// Start a prewarm operation.
///
/// This is the public API for starting prewarm. Returns a handle that can be
/// used to query status and cancel the operation.
///
/// # Arguments
///
/// * `icao` - Airport ICAO code (for display)
/// * `tiles` - Tiles to prewarm
/// * `dds_client` - Client for submitting DDS generation jobs
/// * `memory_cache` - Cache to check for already-cached tiles
/// * `ortho_index` - Index for checking disk-resident tiles (patches, cache)
/// * `geo_index` - Geospatial index for patched region filtering
/// * `runtime` - Tokio runtime handle
pub fn start_prewarm<M: MemoryCache + Send + Sync + 'static>(
    icao: String,
    tiles: Vec<TileCoord>,
    dds_client: Arc<dyn DdsClient>,
    memory_cache: Arc<M>,
    ortho_index: Arc<OrthoUnionIndex>,
    geo_index: Option<Arc<GeoIndex>>,
    runtime: &Handle,
) -> PrewarmHandle {
    PrewarmContext::start(
        icao,
        tiles,
        dds_client,
        memory_cache,
        ortho_index,
        geo_index,
        runtime,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_new() {
        let status = PrewarmStatus::new("KSFO".to_string(), 100);
        assert_eq!(status.icao, "KSFO");
        assert_eq!(status.total, 100);
        assert_eq!(status.completed, 0);
        assert_eq!(status.failed, 0);
        assert_eq!(status.cache_hits, 0);
        assert_eq!(status.disk_hits, 0);
        assert!(!status.is_complete);
        assert!(!status.was_cancelled);
    }

    #[test]
    fn test_status_in_flight() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.in_flight(), 100);

        status.completed = 30;
        status.failed = 5;
        status.cache_hits = 10;
        status.disk_hits = 5;
        assert_eq!(status.in_flight(), 50); // 100 - 30 - 5 - 10 - 5
    }

    #[test]
    fn test_status_progress_fraction() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.progress_fraction(), 0.0);

        status.completed = 30;
        status.cache_hits = 10;
        status.disk_hits = 10;
        assert!((status.progress_fraction() - 0.5).abs() < 0.001);

        // Empty prewarm should show as complete
        let empty = PrewarmStatus::new("TEST".to_string(), 0);
        assert_eq!(empty.progress_fraction(), 1.0);
    }

    #[test]
    fn test_status_processed() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.processed(), 0);

        status.completed = 25;
        status.failed = 5;
        status.cache_hits = 10;
        status.disk_hits = 10;
        assert_eq!(status.processed(), 50);
    }

    #[test]
    fn test_handle_cancel() {
        let status = Arc::new(Mutex::new(PrewarmStatus::new("TEST".to_string(), 10)));
        let cancellation = CancellationToken::new();

        let handle = PrewarmHandle {
            status,
            cancellation,
        };

        assert!(!handle.is_cancelled());
        handle.cancel();
        assert!(handle.is_cancelled());
    }

    #[test]
    fn test_handle_status_clone() {
        let status = Arc::new(Mutex::new(PrewarmStatus::new("KJFK".to_string(), 50)));
        let cancellation = CancellationToken::new();

        let handle = PrewarmHandle {
            status: Arc::clone(&status),
            cancellation,
        };

        // Modify underlying status
        {
            let mut s = status.lock();
            s.completed = 25;
        }

        // Handle should see updated status
        let snapshot = handle.status();
        assert_eq!(snapshot.completed, 25);
        assert_eq!(snapshot.icao, "KJFK");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Integration tests: disk-existence filtering via OrthoUnionIndex
    // ─────────────────────────────────────────────────────────────────────────

    use crate::ortho_union::{OrthoSource, OrthoUnionIndex};
    use crate::runtime::DdsResponse;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    /// Mock DdsClient that provides a sender for async job submission.
    struct TestDdsClient {
        sender: mpsc::Sender<JobRequest>,
    }

    impl TestDdsClient {
        fn new() -> (Arc<Self>, mpsc::Receiver<JobRequest>) {
            let (tx, rx) = mpsc::channel(256);
            (Arc::new(Self { sender: tx }), rx)
        }
    }

    impl DdsClient for TestDdsClient {
        fn submit(&self, request: JobRequest) -> Result<(), crate::executor::DdsClientError> {
            self.sender
                .try_send(request)
                .map_err(|_| crate::executor::DdsClientError::ChannelFull)
        }

        fn request_dds(
            &self,
            tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (tx, rx) = oneshot::channel();
            drop(tx);
            let _ = self.sender.try_send(JobRequest::prefetch(tile));
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            _priority: crate::executor::Priority,
            _origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.request_dds(tile, cancellation)
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn sender(&self) -> Option<mpsc::Sender<JobRequest>> {
            Some(self.sender.clone())
        }
    }

    /// Mock MemoryCache that always returns None (empty cache).
    struct EmptyMemoryCache;

    impl MemoryCache for EmptyMemoryCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
            async { None }
        }

        fn put(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
            _data: Vec<u8>,
        ) -> impl std::future::Future<Output = ()> + Send {
            async {}
        }

        fn size_bytes(&self) -> usize {
            0
        }

        fn entry_count(&self) -> usize {
            0
        }
    }

    /// Create an OrthoUnionIndex with DDS files for specific tiles.
    fn create_index_with_dds(temp: &TempDir, dds_files: &[&str]) -> Arc<OrthoUnionIndex> {
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();

        for filename in dds_files {
            std::fs::write(pkg_dir.join("textures").join(filename), b"dds content").unwrap();
        }

        let source = OrthoSource::new_package("test", &pkg_dir);
        Arc::new(OrthoUnionIndex::with_sources(vec![source]))
    }

    #[tokio::test]
    async fn test_prewarm_skips_tiles_on_disk() {
        let temp = TempDir::new().unwrap();

        // Prewarm uses tile-level coords (row, col, zoom), but dds_tile_exists()
        // needs chunk-level coords (row*16, col*16, zoom+4). DDS filenames on
        // disk use chunk coords. Tile (6, 12, 12) → chunk (96, 192, 16).
        let index = create_index_with_dds(
            &temp,
            &["96_192_BI16.dds", "96_208_BI16.dds"], // chunk coords for tiles (6,12) and (6,13)
        );

        // Verify the index finds these as chunk coords
        assert!(index.dds_tile_exists(96, 192, 16));
        assert!(index.dds_tile_exists(96, 208, 16));
        assert!(!index.dds_tile_exists(96, 224, 16));

        // Create tiles at tile-level coords: 2 on disk + 1 not on disk
        let tiles = vec![
            TileCoord {
                row: 6,
                col: 12,
                zoom: 12,
            }, // chunk (96, 192, 16) — on disk
            TileCoord {
                row: 6,
                col: 13,
                zoom: 12,
            }, // chunk (96, 208, 16) — on disk
            TileCoord {
                row: 6,
                col: 14,
                zoom: 12,
            }, // chunk (96, 224, 16) — NOT on disk
        ];

        let (client, mut rx) = TestDdsClient::new();
        let memory_cache = Arc::new(EmptyMemoryCache);

        let handle = start_prewarm(
            "TEST".to_string(),
            tiles,
            client,
            memory_cache,
            index,
            None,
            &tokio::runtime::Handle::current(),
        );

        // Respond to the ONE job that should be submitted (tile col=14)
        if let Some(request) = rx.recv().await {
            assert_eq!(
                request.tile.col, 14,
                "Only non-disk tile should be submitted"
            );
            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::success(
                    vec![0u8; 10],
                    std::time::Duration::from_millis(1),
                ));
            }
        }

        // Wait for completion
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = handle.status();
        assert!(status.is_complete);
        assert_eq!(status.disk_hits, 2, "Two tiles should be disk hits");
        assert_eq!(status.completed, 1, "One tile should be generated");
        assert_eq!(status.cache_hits, 0, "No memory cache hits");
    }

    /// Build a test GeoIndex with patched regions for the given coordinates.
    fn build_test_geo_index(regions: &[(i32, i32)]) -> Arc<GeoIndex> {
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
        geo_index
    }

    /// Build a test OrthoUnionIndex fixture (no patched regions — use GeoIndex).
    fn build_test_index(temp: &TempDir) -> Arc<OrthoUnionIndex> {
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();

        let source = OrthoSource::new_package("test", &pkg_dir);
        let index = OrthoUnionIndex::with_sources(vec![source]);
        Arc::new(index)
    }

    #[tokio::test]
    async fn test_prewarm_skips_patched_regions() {
        use crate::prefetch::tile_based::DsfTileCoord;

        let temp = TempDir::new().unwrap();

        // Build test index (no patched regions on the index itself)
        let index = build_test_index(&temp);

        // Build GeoIndex with patched region (33, -119)
        let geo_index = build_test_geo_index(&[(33, -119)]);

        // Create tiles: 2 in patched region + 1 outside
        // Use to_tile_coords to get valid chunk coords in DSF region (33, -119)
        let tc_patched = crate::coord::to_tile_coords(33.5, -118.5, 16).unwrap();
        let tc_patched2 = crate::coord::TileCoord {
            row: tc_patched.row + 1,
            col: tc_patched.col,
            zoom: 16,
        };
        // A tile clearly outside the patched region
        let tc_outside = crate::coord::to_tile_coords(60.0, 10.0, 16).unwrap();

        // Verify our coordinate expectations
        let dsf = DsfTileCoord::from_lat_lon(33.5, -118.5);
        assert_eq!(dsf.lat, 33);
        assert_eq!(dsf.lon, -119);

        let tiles = vec![tc_patched, tc_patched2, tc_outside];

        let (client, mut rx) = TestDdsClient::new();
        let memory_cache = Arc::new(EmptyMemoryCache);

        let handle = start_prewarm(
            "PATCH".to_string(),
            tiles,
            client,
            memory_cache,
            index,
            Some(geo_index),
            &tokio::runtime::Handle::current(),
        );

        // Only the non-patched tile should be submitted
        if let Some(request) = rx.recv().await {
            assert_eq!(
                request.tile.row, tc_outside.row,
                "Only non-patched tile should be submitted"
            );
            if let Some(tx) = request.response_tx {
                let _ = tx.send(DdsResponse::success(
                    vec![0u8; 10],
                    std::time::Duration::from_millis(1),
                ));
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = handle.status();
        assert!(status.is_complete);
        assert_eq!(status.patch_skipped, 2, "Two tiles should be patch-skipped");
        assert_eq!(status.completed, 1, "One tile should be generated");
    }

    #[tokio::test]
    async fn test_prewarm_all_tiles_on_disk() {
        let temp = TempDir::new().unwrap();

        // Tile (3, 4, 12) → chunk (48, 64, 16), tile (3, 5, 12) → chunk (48, 80, 16)
        let index = create_index_with_dds(&temp, &["48_64_BI16.dds", "48_80_BI16.dds"]);

        let tiles = vec![
            TileCoord {
                row: 3,
                col: 4,
                zoom: 12,
            },
            TileCoord {
                row: 3,
                col: 5,
                zoom: 12,
            },
        ];

        let (client, _rx) = TestDdsClient::new();
        let memory_cache = Arc::new(EmptyMemoryCache);

        let handle = start_prewarm(
            "DISK".to_string(),
            tiles,
            client,
            memory_cache,
            index,
            None,
            &tokio::runtime::Handle::current(),
        );

        // Wait briefly for the async task to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = handle.status();
        assert!(
            status.is_complete,
            "Should complete immediately when all tiles on disk"
        );
        assert_eq!(status.disk_hits, 2);
        assert_eq!(status.completed, 0, "No tiles should need generation");
        assert_eq!(status.total, 2);
    }
}
