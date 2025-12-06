//! Parallel tile generator with request coalescing and timeout handling.
//!
//! This module provides a wrapper around any `TileGenerator` that:
//! - Uses a thread pool for parallel tile generation
//! - Coalesces duplicate requests (multiple requests for same tile wait for one generation)
//! - Handles timeouts by returning a magenta placeholder tile
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    FUSE Filesystem                          │
//! │         Multiple concurrent read() calls                    │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │               ParallelTileGenerator                         │
//! │  - Thread pool (configurable size, default: num_cpus)       │
//! │  - Request coalescing (HashMap<TileRequest, InFlight>)      │
//! │  - Timeout handling (configurable, default: 10s)            │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                Inner TileGenerator                          │
//! │            (DefaultTileGenerator)                           │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use crate::dds::DdsFormat;
use crate::fuse::generate_magenta_placeholder;
use crate::log::Logger;
use crate::tile::{TileGenerator, TileGeneratorError, TileRequest};
use crate::{log_debug, log_error, log_info, log_warn};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Configuration for the parallel tile generator.
#[derive(Debug, Clone)]
pub struct ParallelConfig {
    /// Number of worker threads (default: number of CPU cores)
    pub threads: usize,
    /// Generation timeout in seconds (default: 10)
    pub timeout_secs: u64,
    /// DDS format for placeholder generation
    pub dds_format: DdsFormat,
    /// Number of mipmap levels
    pub mipmap_count: usize,
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            timeout_secs: 10,
            dds_format: DdsFormat::BC1,
            mipmap_count: 5,
        }
    }
}

impl ParallelConfig {
    /// Create a new configuration with the given number of threads.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// Set the generation timeout in seconds.
    pub fn with_timeout_secs(mut self, timeout: u64) -> Self {
        self.timeout_secs = timeout;
        self
    }

    /// Set the DDS format for placeholder generation.
    pub fn with_dds_format(mut self, format: DdsFormat) -> Self {
        self.dds_format = format;
        self
    }

    /// Set the number of mipmap levels.
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = count;
        self
    }
}

/// In-flight request state for coalescing.
struct InFlight {
    /// Channel to send result to waiting threads
    result_senders: Vec<Sender<Result<Vec<u8>, TileGeneratorError>>>,
}

/// Work item for the thread pool.
struct WorkItem {
    request: TileRequest,
    result_sender: Sender<Result<Vec<u8>, TileGeneratorError>>,
}

/// Parallel tile generator with request coalescing and timeout handling.
///
/// Wraps an inner `TileGenerator` and adds:
/// - Thread pool for parallel generation
/// - Request coalescing (duplicate requests wait for single generation)
/// - Timeout handling (returns magenta placeholder if generation exceeds timeout)
pub struct ParallelTileGenerator {
    /// Inner tile generator
    inner: Arc<dyn TileGenerator>,
    /// In-flight requests map for coalescing
    in_flight: Arc<Mutex<HashMap<TileRequest, InFlight>>>,
    /// Work queue sender
    work_sender: Sender<WorkItem>,
    /// Configuration
    config: ParallelConfig,
    /// Condition variable for signaling waiters
    cond: Arc<Condvar>,
    /// Shutdown flag
    shutdown: Arc<Mutex<bool>>,
    /// Logger for diagnostic output
    logger: Arc<dyn Logger>,
}

impl ParallelTileGenerator {
    /// Create a new parallel tile generator.
    ///
    /// # Arguments
    ///
    /// * `inner` - The inner tile generator to wrap
    /// * `config` - Configuration for parallel generation
    /// * `logger` - Logger for diagnostic output
    pub fn new(
        inner: Arc<dyn TileGenerator>,
        config: ParallelConfig,
        logger: Arc<dyn Logger>,
    ) -> Self {
        let (work_sender, work_receiver) = mpsc::channel::<WorkItem>();
        let work_receiver = Arc::new(Mutex::new(work_receiver));
        let in_flight = Arc::new(Mutex::new(HashMap::new()));
        let cond = Arc::new(Condvar::new());
        let shutdown = Arc::new(Mutex::new(false));

        log_info!(
            logger,
            "Starting parallel tile generator with {} threads, {}s timeout",
            config.threads,
            config.timeout_secs
        );

        // Spawn worker threads
        for i in 0..config.threads {
            let inner = Arc::clone(&inner);
            let work_receiver = Arc::clone(&work_receiver);
            let in_flight = Arc::clone(&in_flight);
            let cond = Arc::clone(&cond);
            let shutdown = Arc::clone(&shutdown);
            let worker_logger = Arc::clone(&logger);

            thread::Builder::new()
                .name(format!("tile-worker-{}", i))
                .spawn(move || {
                    Self::worker_loop(
                        inner,
                        work_receiver,
                        in_flight,
                        cond,
                        shutdown,
                        worker_logger,
                    );
                })
                .expect("Failed to spawn tile worker thread");
        }

        Self {
            inner,
            in_flight,
            work_sender,
            config,
            cond,
            shutdown,
            logger,
        }
    }

    /// Worker thread loop.
    fn worker_loop(
        inner: Arc<dyn TileGenerator>,
        work_receiver: Arc<Mutex<Receiver<WorkItem>>>,
        in_flight: Arc<Mutex<HashMap<TileRequest, InFlight>>>,
        cond: Arc<Condvar>,
        shutdown: Arc<Mutex<bool>>,
        logger: Arc<dyn Logger>,
    ) {
        loop {
            // Check shutdown
            if *shutdown.lock().unwrap() {
                break;
            }

            // Try to get work
            let work = {
                let receiver = work_receiver.lock().unwrap();
                receiver.recv_timeout(Duration::from_millis(100))
            };

            let work_item = match work {
                Ok(item) => item,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };

            // Generate tile
            log_debug!(logger, "Worker generating tile {:?}", work_item.request);
            let result = inner.generate(&work_item.request);

            // Send result and notify waiters
            let mut map = in_flight.lock().unwrap();
            if let Some(flight) = map.remove(&work_item.request) {
                // Send result to all waiters
                let result_clone = result.clone();
                for sender in flight.result_senders {
                    let _ = sender.send(result_clone.clone());
                }
            }
            // Also send to the original requester if they're not in the list
            let _ = work_item.result_sender.send(result);
            cond.notify_all();
        }
    }

    /// Generate a tile with coalescing and timeout handling.
    fn generate_with_timeout(&self, request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
        // Create result channel
        let (result_sender, result_receiver) = mpsc::channel();

        // Check if request is already in-flight (coalescing)
        {
            let mut map = self.in_flight.lock().unwrap();
            if let Some(flight) = map.get_mut(request) {
                log_debug!(self.logger, "Coalescing request for tile {:?}", request);
                flight.result_senders.push(result_sender.clone());
                // Wait for result with timeout
                drop(map);
                return self.wait_for_result(result_receiver);
            }

            // Create new in-flight entry
            map.insert(
                *request,
                InFlight {
                    result_senders: vec![],
                },
            );
        }

        // Submit work
        let work_item = WorkItem {
            request: *request,
            result_sender,
        };

        if self.work_sender.send(work_item).is_err() {
            // Worker threads have shut down
            return Err(TileGeneratorError::DownloadFailed(
                "Tile generation worker shutdown".to_string(),
            ));
        }

        // Wait for result with timeout
        self.wait_for_result(result_receiver)
    }

    /// Wait for result with timeout, returning placeholder on timeout.
    fn wait_for_result(
        &self,
        receiver: Receiver<Result<Vec<u8>, TileGeneratorError>>,
    ) -> Result<Vec<u8>, TileGeneratorError> {
        let timeout = Duration::from_secs(self.config.timeout_secs);

        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                log_warn!(
                    self.logger,
                    "Tile generation timed out after {}s, returning placeholder",
                    self.config.timeout_secs
                );
                self.generate_placeholder()
            }
            Err(RecvTimeoutError::Disconnected) => {
                log_error!(self.logger, "Tile generation worker disconnected");
                self.generate_placeholder()
            }
        }
    }

    /// Generate a magenta placeholder tile.
    fn generate_placeholder(&self) -> Result<Vec<u8>, TileGeneratorError> {
        generate_magenta_placeholder(4096, 4096, self.config.dds_format, self.config.mipmap_count)
            .map_err(|e| TileGeneratorError::EncodingFailed(e.to_string()))
    }
}

impl TileGenerator for ParallelTileGenerator {
    fn generate(&self, request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
        self.generate_with_timeout(request)
    }

    fn expected_size(&self) -> usize {
        self.inner.expected_size()
    }
}

impl Drop for ParallelTileGenerator {
    fn drop(&mut self) {
        // Signal shutdown to workers
        *self.shutdown.lock().unwrap() = true;
        self.cond.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::NoOpLogger;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_logger() -> Arc<dyn Logger> {
        Arc::new(NoOpLogger)
    }

    /// Mock generator that tracks calls and can simulate delays.
    struct MockGenerator {
        call_count: AtomicUsize,
        delay_ms: u64,
        should_fail: bool,
    }

    impl MockGenerator {
        fn new() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                delay_ms: 0,
                should_fail: false,
            }
        }

        fn with_delay(delay_ms: u64) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                delay_ms,
                should_fail: false,
            }
        }

        fn with_failure() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                delay_ms: 0,
                should_fail: true,
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl TileGenerator for MockGenerator {
        fn generate(&self, request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);

            if self.delay_ms > 0 {
                thread::sleep(Duration::from_millis(self.delay_ms));
            }

            if self.should_fail {
                Err(TileGeneratorError::DownloadFailed(
                    "mock failure".to_string(),
                ))
            } else {
                // Return unique data based on request
                let data = format!("{}-{}-{}", request.row(), request.col(), request.zoom());
                Ok(data.into_bytes())
            }
        }

        fn expected_size(&self) -> usize {
            4096
        }
    }

    #[test]
    fn test_parallel_generation() {
        let inner = Arc::new(MockGenerator::new());
        let config = ParallelConfig::default().with_threads(4);
        let generator = ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        );

        let request = TileRequest::new(100, 200, 16);
        let result = generator.generate(&request);

        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1);
    }

    #[test]
    fn test_request_coalescing() {
        let inner = Arc::new(MockGenerator::with_delay(100));
        let config = ParallelConfig::default()
            .with_threads(2)
            .with_timeout_secs(5);
        let generator = Arc::new(ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        ));

        let request = TileRequest::new(100, 200, 16);

        // Spawn multiple threads requesting same tile
        let mut handles = vec![];
        for _ in 0..5 {
            let gen = Arc::clone(&generator);
            let req = request;
            handles.push(thread::spawn(move || gen.generate(&req)));
        }

        // Wait for all
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All should succeed
        for result in &results {
            assert!(result.is_ok());
        }

        // But inner should only be called once (or twice due to race at start)
        // Allow up to 2 because of startup race condition
        assert!(
            inner.call_count() <= 2,
            "Expected coalescing, got {} calls",
            inner.call_count()
        );
    }

    #[test]
    fn test_timeout_returns_placeholder() {
        // Use a channel to control when the mock completes - it will block until signaled
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        struct BlockingMock {
            receiver: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            call_count: std::sync::atomic::AtomicUsize,
        }

        impl TileGenerator for BlockingMock {
            fn generate(&self, _request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
                self.call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Block until signaled (or channel dropped)
                let _ = self.receiver.lock().unwrap().recv();
                Ok(b"mock-data".to_vec())
            }

            fn expected_size(&self) -> usize {
                4096
            }
        }

        let inner = Arc::new(BlockingMock {
            receiver: std::sync::Mutex::new(rx),
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let config = ParallelConfig::default()
            .with_threads(1)
            .with_timeout_secs(1); // 1 second timeout
        let generator = ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        );

        let request = TileRequest::new(100, 200, 16);
        let result = generator.generate(&request);

        // Should timeout and return placeholder (not the mock's "mock-data" response)
        assert!(result.is_ok(), "Should return placeholder on timeout");

        // Verify it's a DDS placeholder (starts with "DDS "), not the mock response
        let data = result.unwrap();
        assert_eq!(&data[0..4], b"DDS ", "Should return valid DDS placeholder");
        assert_ne!(&data[..], b"mock-data", "Should not return mock data");

        // Verify the inner generator was called (work was submitted)
        assert_eq!(
            inner.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Inner generator should have been called"
        );

        // Unblock the worker thread so it can clean up
        drop(tx);
    }

    #[test]
    fn test_different_tiles_processed_in_parallel() {
        // Track peak concurrent executions to verify parallelism
        use std::sync::atomic::AtomicUsize;

        struct ConcurrencyTrackingMock {
            current_count: AtomicUsize,
            peak_count: AtomicUsize,
            call_count: AtomicUsize,
            barrier: std::sync::Barrier,
        }

        impl TileGenerator for ConcurrencyTrackingMock {
            fn generate(&self, _request: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
                self.call_count.fetch_add(1, Ordering::SeqCst);

                // Increment current count and update peak
                let current = self.current_count.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak_count.fetch_max(current, Ordering::SeqCst);

                // Wait at barrier - all threads must reach here before any can proceed
                // This ensures we measure true concurrency
                self.barrier.wait();

                // Decrement current count
                self.current_count.fetch_sub(1, Ordering::SeqCst);

                Ok(b"tile-data".to_vec())
            }

            fn expected_size(&self) -> usize {
                4096
            }
        }

        let inner = Arc::new(ConcurrencyTrackingMock {
            current_count: AtomicUsize::new(0),
            peak_count: AtomicUsize::new(0),
            call_count: AtomicUsize::new(0),
            barrier: std::sync::Barrier::new(4), // Wait for all 4 to be concurrent
        });

        let config = ParallelConfig::default()
            .with_threads(4)
            .with_timeout_secs(10);
        let generator = Arc::new(ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        ));

        // Request 4 different tiles in parallel
        let mut handles = vec![];
        for i in 0..4 {
            let gen = Arc::clone(&generator);
            handles.push(thread::spawn(move || {
                let request = TileRequest::new(100 + i, 200, 16);
                gen.generate(&request)
            }));
        }

        // Wait for all
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }

        // Verify parallelism: peak concurrent count should be 4 (all running simultaneously)
        assert_eq!(
            inner.peak_count.load(Ordering::SeqCst),
            4,
            "All 4 tiles should be processed concurrently"
        );
        assert_eq!(
            inner.call_count.load(Ordering::SeqCst),
            4,
            "Each tile should be generated once"
        );
    }

    #[test]
    fn test_error_propagation() {
        let inner = Arc::new(MockGenerator::with_failure());
        let config = ParallelConfig::default().with_threads(1);
        let generator = ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        );

        let request = TileRequest::new(100, 200, 16);
        let result = generator.generate(&request);

        assert!(result.is_err());
    }

    #[test]
    fn test_expected_size_delegation() {
        let inner = Arc::new(MockGenerator::new());
        let config = ParallelConfig::default();
        let generator = ParallelTileGenerator::new(
            Arc::clone(&inner) as Arc<dyn TileGenerator>,
            config,
            test_logger(),
        );

        assert_eq!(generator.expected_size(), 4096);
    }
}
