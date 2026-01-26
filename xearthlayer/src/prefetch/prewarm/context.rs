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
    /// Tiles that were already in cache.
    pub cache_hits: usize,
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
            is_complete: false,
            was_cancelled: false,
        }
    }

    /// Number of tiles currently in flight (submitted but not yet complete).
    pub fn in_flight(&self) -> usize {
        self.total
            .saturating_sub(self.completed + self.failed + self.cache_hits)
    }

    /// Progress as a fraction from 0.0 to 1.0.
    pub fn progress_fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0; // Empty prewarm is "complete"
        }
        (self.completed + self.cache_hits) as f64 / self.total as f64
    }

    /// Total tiles that have been processed (completed + failed + cache_hits).
    pub fn processed(&self) -> usize {
        self.completed + self.failed + self.cache_hits
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
    /// * `runtime` - Tokio runtime handle to spawn the task on
    pub fn start(
        icao: String,
        tiles: Vec<TileCoord>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
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

        let cache_hits = {
            let s = self.status.lock();
            s.cache_hits
        };

        info!(
            total = self.tiles.len(),
            cache_hits,
            to_generate = to_generate.len(),
            "Cache check complete"
        );

        if to_generate.is_empty() {
            // All tiles were cached
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
/// * `runtime` - Tokio runtime handle
pub fn start_prewarm<M: MemoryCache + Send + Sync + 'static>(
    icao: String,
    tiles: Vec<TileCoord>,
    dds_client: Arc<dyn DdsClient>,
    memory_cache: Arc<M>,
    runtime: &Handle,
) -> PrewarmHandle {
    PrewarmContext::start(icao, tiles, dds_client, memory_cache, runtime)
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
        assert!(!status.is_complete);
        assert!(!status.was_cancelled);
    }

    #[test]
    fn test_status_in_flight() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.in_flight(), 100);

        status.completed = 30;
        status.failed = 5;
        status.cache_hits = 15;
        assert_eq!(status.in_flight(), 50); // 100 - 30 - 5 - 15
    }

    #[test]
    fn test_status_progress_fraction() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.progress_fraction(), 0.0);

        status.completed = 40;
        status.cache_hits = 10;
        assert!((status.progress_fraction() - 0.5).abs() < 0.001);

        // Empty prewarm should show as complete
        let empty = PrewarmStatus::new("TEST".to_string(), 0);
        assert_eq!(empty.progress_fraction(), 1.0);
    }

    #[test]
    fn test_status_processed() {
        let mut status = PrewarmStatus::new("TEST".to_string(), 100);
        assert_eq!(status.processed(), 0);

        status.completed = 30;
        status.failed = 5;
        status.cache_hits = 15;
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
}
