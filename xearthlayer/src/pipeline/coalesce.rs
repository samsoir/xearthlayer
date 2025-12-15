//! Request coalescing for the DDS pipeline.
//!
//! This module provides request coalescing to prevent duplicate tile processing.
//! When multiple FUSE requests arrive for the same tile simultaneously, only one
//! actual processing task runs - all other waiters receive the same result.
//!
//! # Architecture
//!
//! ```text
//! FUSE Request A ─┐
//!                 │                              Pipeline
//! FUSE Request B ─┼──► RequestCoalescer ──────► Processor
//!                 │        │                        │
//! FUSE Request C ─┘        │                        │
//!                          ▼                        ▼
//!                    [A, B, C all                [One task]
//!                     receive same                  │
//!                     result]◄─────────────────────┘
//! ```
//!
//! # Implementation
//!
//! Uses `DashMap` for lock-free concurrent access, allowing high-throughput
//! registration without lock contention. Statistics use atomic counters.

use crate::coord::TileCoord;
use crate::fuse::DdsResponse;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info};

/// Tracks in-flight requests for request coalescing.
///
/// Thread-safe structure that tracks which tiles are currently being processed,
/// allowing duplicate requests to wait for the same result instead of triggering
/// duplicate work.
///
/// Uses `DashMap` for lock-free concurrent access, enabling high-throughput
/// request registration during X-Plane's burst loading patterns.
pub struct RequestCoalescer {
    /// In-flight requests: tile -> broadcast sender for result
    /// Using DashMap for lock-free concurrent access
    in_flight: DashMap<TileCoord, broadcast::Sender<CoalescedResult>>,
    /// Statistics using atomic counters for lock-free updates
    total_requests: AtomicU64,
    coalesced_requests: AtomicU64,
    new_requests: AtomicU64,
}

/// Statistics for monitoring coalescing effectiveness.
#[derive(Debug, Default, Clone)]
pub struct CoalescerStats {
    /// Total requests received
    pub total_requests: u64,
    /// Requests that were coalesced (waited for existing work)
    pub coalesced_requests: u64,
    /// Requests that triggered new work
    pub new_requests: u64,
}

impl CoalescerStats {
    /// Returns the coalescing ratio (0.0 to 1.0)
    pub fn coalescing_ratio(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.coalesced_requests as f64 / self.total_requests as f64
        }
    }
}

/// Result type for coalesced responses
#[derive(Clone, Debug)]
pub(crate) struct CoalescedResult {
    pub(crate) data: Arc<Vec<u8>>,
    pub(crate) cache_hit: bool,
    pub(crate) duration: std::time::Duration,
}

impl From<CoalescedResult> for DdsResponse {
    fn from(result: CoalescedResult) -> Self {
        Self {
            data: (*result.data).clone(),
            cache_hit: result.cache_hit,
            duration: result.duration,
        }
    }
}

impl RequestCoalescer {
    /// Creates a new request coalescer.
    pub fn new() -> Self {
        Self {
            in_flight: DashMap::new(),
            total_requests: AtomicU64::new(0),
            coalesced_requests: AtomicU64::new(0),
            new_requests: AtomicU64::new(0),
        }
    }

    /// Attempts to register a request for the given tile.
    ///
    /// Returns `CoalesceResult::NewRequest` if this is the first request for the tile,
    /// meaning the caller should process it and call `complete()` when done.
    ///
    /// Returns `CoalesceResult::Coalesced` with a receiver if another request is already
    /// in flight, meaning the caller should wait on the receiver for the result.
    ///
    /// This method is lock-free and can handle high-throughput concurrent registration.
    pub(crate) fn register(&self, tile: TileCoord) -> CoalesceResult {
        self.total_requests.fetch_add(1, Ordering::Relaxed);

        // Use entry API for atomic check-and-insert
        // This avoids the race condition of check-then-insert
        match self.in_flight.entry(tile) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                // Request already in-flight, subscribe to result
                let rx = entry.get().subscribe();
                self.coalesced_requests.fetch_add(1, Ordering::Relaxed);
                debug!(
                    tile = ?tile,
                    coalesced = self.coalesced_requests.load(Ordering::Relaxed),
                    "Coalescing request - waiting for in-flight processing"
                );
                CoalesceResult::Coalesced(rx)
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // New request, create broadcast channel
                // Use capacity of 16 - typical case is 1-4 concurrent requests for same tile
                let (tx, _rx) = broadcast::channel(16);
                entry.insert(tx.clone());
                self.new_requests.fetch_add(1, Ordering::Relaxed);
                debug!(
                    tile = ?tile,
                    in_flight_count = self.in_flight.len(),
                    "New request - starting processing"
                );
                CoalesceResult::NewRequest { tile, sender: tx }
            }
        }
    }

    /// Completes a request, broadcasting the result to all waiters.
    ///
    /// This should be called by the original processor when it finishes.
    pub fn complete(&self, tile: TileCoord, response: DdsResponse) {
        if let Some((_, tx)) = self.in_flight.remove(&tile) {
            let result = CoalescedResult {
                data: Arc::new(response.data),
                cache_hit: response.cache_hit,
                duration: response.duration,
            };

            // Send to all waiters - ignore errors (receivers may have been dropped)
            let subscriber_count = tx.receiver_count();
            let _ = tx.send(result);

            if subscriber_count > 0 {
                debug!(
                    tile = ?tile,
                    waiters = subscriber_count,
                    "Broadcast result to {} coalesced waiters",
                    subscriber_count
                );
            }
        }
    }

    /// Cancels a request, removing it from in-flight and notifying waiters.
    ///
    /// This should be called when processing is cancelled (e.g., due to FUSE timeout).
    /// Dropping the broadcast sender will cause all waiters to receive an error,
    /// which they should handle gracefully.
    pub fn cancel(&self, tile: TileCoord) {
        if let Some((_, _tx)) = self.in_flight.remove(&tile) {
            debug!(
                tile = ?tile,
                "Cancelled in-flight request - waiters will receive error"
            );
            // Dropping tx will close the channel, causing waiters to get RecvError
        }
    }

    /// Returns a snapshot of the current statistics.
    pub fn stats(&self) -> CoalescerStats {
        CoalescerStats {
            total_requests: self.total_requests.load(Ordering::Relaxed),
            coalesced_requests: self.coalesced_requests.load(Ordering::Relaxed),
            new_requests: self.new_requests.load(Ordering::Relaxed),
        }
    }

    /// Returns the number of currently in-flight requests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Logs current statistics.
    pub fn log_stats(&self) {
        let stats = self.stats();
        let in_flight_count = self.in_flight_count();

        info!(
            total_requests = stats.total_requests,
            coalesced = stats.coalesced_requests,
            new_requests = stats.new_requests,
            in_flight = in_flight_count,
            coalescing_ratio = format!("{:.1}%", stats.coalescing_ratio() * 100.0),
            "Request coalescing statistics"
        );
    }
}

impl Default for RequestCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of attempting to register a request.
#[allow(dead_code)]
pub(crate) enum CoalesceResult {
    /// This is a new request - caller should process and call complete()
    NewRequest {
        tile: TileCoord,
        /// The broadcast sender is kept internally for potential future use
        /// (e.g., progress updates or cancellation)
        sender: broadcast::Sender<CoalescedResult>,
    },
    /// Request is coalesced - wait on this receiver for the result
    Coalesced(broadcast::Receiver<CoalescedResult>),
}

#[allow(dead_code)]
impl CoalesceResult {
    /// Returns true if this is a new request that needs processing.
    pub fn is_new_request(&self) -> bool {
        matches!(self, Self::NewRequest { .. })
    }

    /// Waits for the coalesced result if this is a coalesced request.
    ///
    /// Returns `None` if this is a new request (caller should process it).
    /// Returns `Some(response)` with the result if coalesced.
    pub async fn wait_for_coalesced(self) -> Option<DdsResponse> {
        match self {
            Self::Coalesced(mut rx) => {
                // Wait for the result from the broadcast channel
                match rx.recv().await {
                    Ok(result) => Some(result.into()),
                    Err(_) => {
                        // Channel closed without sending - shouldn't happen normally
                        // Return None to indicate caller should handle this case
                        None
                    }
                }
            }
            Self::NewRequest { .. } => None,
        }
    }

    /// Extracts the tile for a new request, or None if coalesced.
    pub fn tile(&self) -> Option<TileCoord> {
        match self {
            Self::NewRequest { tile, .. } => Some(*tile),
            Self::Coalesced(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    fn test_tile(row: u32, col: u32) -> TileCoord {
        TileCoord { row, col, zoom: 16 }
    }

    fn test_response() -> DdsResponse {
        DdsResponse {
            data: vec![0xDD, 0x53, 0x20],
            cache_hit: false,
            duration: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn test_first_request_is_new() {
        let coalescer = RequestCoalescer::new();
        let tile = test_tile(100, 200);

        let result = coalescer.register(tile);

        assert!(result.is_new_request());
        assert_eq!(result.tile(), Some(tile));
    }

    #[tokio::test]
    async fn test_second_request_is_coalesced() {
        let coalescer = RequestCoalescer::new();
        let tile = test_tile(100, 200);

        // First request
        let first = coalescer.register(tile);
        assert!(first.is_new_request());

        // Second request for same tile should be coalesced
        let second = coalescer.register(tile);
        assert!(!second.is_new_request());
        assert_eq!(second.tile(), None);
    }

    #[tokio::test]
    async fn test_different_tiles_not_coalesced() {
        let coalescer = RequestCoalescer::new();
        let tile1 = test_tile(100, 200);
        let tile2 = test_tile(100, 201);

        let first = coalescer.register(tile1);
        let second = coalescer.register(tile2);

        assert!(first.is_new_request());
        assert!(second.is_new_request());
    }

    #[tokio::test]
    async fn test_coalesced_request_receives_result() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let tile = test_tile(100, 200);

        // First request
        let _first = coalescer.register(tile);

        // Second request for same tile
        let second = coalescer.register(tile);

        // Complete the request
        let response = test_response();
        coalescer.complete(tile, response.clone());

        // Second request should receive the result
        if let CoalesceResult::Coalesced(mut rx) = second {
            let result = rx.recv().await.unwrap();
            let response: DdsResponse = result.into();
            assert_eq!(response.data, vec![0xDD, 0x53, 0x20]);
            assert!(!response.cache_hit);
        } else {
            panic!("Expected coalesced result");
        }
    }

    #[tokio::test]
    async fn test_multiple_waiters_all_receive_result() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let tile = test_tile(100, 200);

        // First request (will process)
        let _first = coalescer.register(tile);

        // Multiple coalesced requests
        let waiter1 = coalescer.register(tile);
        let waiter2 = coalescer.register(tile);
        let waiter3 = coalescer.register(tile);

        // Spawn waiters
        let coalescer_clone = Arc::clone(&coalescer);
        let handles: Vec<_> = vec![waiter1, waiter2, waiter3]
            .into_iter()
            .map(|w| {
                tokio::spawn(async move {
                    if let CoalesceResult::Coalesced(mut rx) = w {
                        rx.recv().await.ok()
                    } else {
                        None
                    }
                })
            })
            .collect();

        // Complete the request
        let response = test_response();
        coalescer_clone.complete(tile, response);

        // All waiters should receive the result
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_some());
        }
    }

    #[tokio::test]
    async fn test_completion_removes_from_in_flight() {
        let coalescer = RequestCoalescer::new();
        let tile = test_tile(100, 200);

        // Register and complete
        let _first = coalescer.register(tile);
        assert_eq!(coalescer.in_flight_count(), 1);

        coalescer.complete(tile, test_response());
        assert_eq!(coalescer.in_flight_count(), 0);

        // New request for same tile should be new (not coalesced)
        let second = coalescer.register(tile);
        assert!(second.is_new_request());
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        let coalescer = RequestCoalescer::new();
        let tile = test_tile(100, 200);

        // First request
        let _first = coalescer.register(tile);

        // Three coalesced requests
        let _c1 = coalescer.register(tile);
        let _c2 = coalescer.register(tile);
        let _c3 = coalescer.register(tile);

        let stats = coalescer.stats();
        assert_eq!(stats.total_requests, 4);
        assert_eq!(stats.new_requests, 1);
        assert_eq!(stats.coalesced_requests, 3);
        assert!((stats.coalescing_ratio() - 0.75).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_concurrent_registration() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let tile = test_tile(100, 200);

        // Spawn multiple concurrent registrations
        let mut handles = vec![];
        for _ in 0..10 {
            let c = Arc::clone(&coalescer);
            handles.push(tokio::spawn(async move { c.register(tile) }));
        }

        // Wait for all to complete
        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Exactly one should be a new request
        let new_count = results.iter().filter(|r| r.is_new_request()).count();
        let coalesced_count = results.iter().filter(|r| !r.is_new_request()).count();

        assert_eq!(new_count, 1, "Exactly one request should be new");
        assert_eq!(coalesced_count, 9, "Nine requests should be coalesced");
    }

    #[tokio::test]
    async fn test_wait_for_coalesced_returns_result() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let tile = test_tile(100, 200);

        // First request
        let _first = coalescer.register(tile);

        // Coalesced request
        let second = coalescer.register(tile);

        // Complete in background
        let c = Arc::clone(&coalescer);
        tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            c.complete(tile, test_response());
        });

        // Wait for result
        let response = second.wait_for_coalesced().await;
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.data, vec![0xDD, 0x53, 0x20]);
    }

    #[tokio::test]
    async fn test_wait_for_new_request_returns_none() {
        let coalescer = RequestCoalescer::new();
        let tile = test_tile(100, 200);

        let first = coalescer.register(tile);

        // wait_for_coalesced on a new request should return None
        let result = first.wait_for_coalesced().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_high_throughput_registration() {
        // Test that registration is truly lock-free under high contention
        let coalescer = Arc::new(RequestCoalescer::new());
        let num_tiles = 100;
        let requests_per_tile = 10;

        let mut handles = vec![];

        // Spawn many concurrent registrations for different tiles
        for tile_id in 0..num_tiles {
            for _ in 0..requests_per_tile {
                let c = Arc::clone(&coalescer);
                handles.push(tokio::spawn(async move {
                    let tile = test_tile(tile_id, 0);
                    c.register(tile)
                }));
            }
        }

        // Wait for all
        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Should have num_tiles new requests (one per tile)
        let new_count = results.iter().filter(|r| r.is_new_request()).count();
        assert_eq!(new_count, num_tiles as usize);

        // Stats should reflect total
        let stats = coalescer.stats();
        assert_eq!(stats.total_requests, (num_tiles * requests_per_tile) as u64);
        assert_eq!(stats.new_requests, num_tiles as u64);
    }
}
