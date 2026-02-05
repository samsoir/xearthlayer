//! Metrics collection and reporting system.
//!
//! This module provides a 3-layer architecture for metrics:
//!
//! 1. **Emission Layer** ([`MetricsClient`]) - Fire-and-forget event emission
//! 2. **Aggregation Layer** ([`MetricsDaemon`]) - Independent event processing
//! 3. **Reporting Layer** ([`MetricsReporter`]) - Transform data for presentation
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │  EMISSION LAYER                                                      │
//! │  MetricsClient (cloneable, cheap, fire-and-forget)                  │
//! │  - Used by: Tasks, CacheAdapters, FUSE handlers, Prefetcher         │
//! └──────────────────────────────┬──────────────────────────────────────┘
//!                                │ MetricEvent (mpsc channel)
//!                                ▼
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │  AGGREGATION LAYER                                                   │
//! │  MetricsDaemon (independent async task)                              │
//! │  - Receives events from channel                                      │
//! │  - Updates counters/gauges in AggregatedState                       │
//! │  - Samples time-series at 100ms intervals for sparklines            │
//! └──────────────────────────────┬──────────────────────────────────────┘
//!                                │ read-only access to state
//!                                ▼
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │  REPORTING LAYER                                                     │
//! │  MetricsReporter trait + TuiReporter implementation                 │
//! │  - Reads AggregatedState + TimeSeriesHistory                        │
//! │  - Transforms into TelemetrySnapshot for TUI                        │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::metrics::{MetricsSystem, TuiReporter};
//!
//! // Create the metrics system
//! let system = MetricsSystem::new(&tokio_runtime.handle());
//!
//! // Get a client to emit events
//! let client = system.client();
//! client.download_completed(1024, 5000);
//!
//! // Get a snapshot for display
//! let reporter = TuiReporter::new();
//! let snapshot = system.snapshot(&reporter);
//!
//! // Shutdown gracefully
//! system.shutdown().await;
//! ```

mod client;
mod daemon;
mod event;
mod reporter;
mod snapshot;
mod state;

pub use client::MetricsClient;
pub use daemon::{MetricsDaemon, MetricsStateSnapshot, SharedMetricsState};
pub use event::MetricEvent;
pub use reporter::{MetricsReporter, TuiReporter};
pub use snapshot::TelemetrySnapshot;
pub use state::{AggregatedState, RingBuffer, TimeSeriesHistory, DEFAULT_HISTORY_CAPACITY};

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// =============================================================================
// Metrics System
// =============================================================================

/// The complete metrics system.
///
/// This is the top-level factory that creates and manages all metrics
/// components. It provides:
///
/// - A [`MetricsClient`] for emitting events
/// - Access to state snapshots for reporting
/// - Graceful shutdown coordination
///
/// # Example
///
/// ```ignore
/// let system = MetricsSystem::new(&runtime.handle());
/// let client = system.client();
///
/// // Use client in various components
/// client.download_started();
///
/// // Get snapshot for TUI
/// let reporter = TuiReporter::new();
/// let snapshot = system.snapshot(&reporter);
/// ```
pub struct MetricsSystem {
    /// Client for emitting events.
    client: MetricsClient,

    /// Handle to the shared state for reporters.
    state_handle: SharedMetricsState,

    /// Handle to the daemon task.
    daemon_handle: Option<JoinHandle<()>>,

    /// Shutdown signal for the daemon.
    shutdown: CancellationToken,
}

impl MetricsSystem {
    /// Creates a new metrics system and starts the daemon.
    ///
    /// The daemon runs as an async task on the provided runtime and will
    /// continue processing events until [`shutdown`](Self::shutdown) is called.
    ///
    /// # Arguments
    ///
    /// * `runtime_handle` - Handle to the Tokio runtime for spawning the daemon
    pub fn new(runtime_handle: &tokio::runtime::Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = MetricsClient::new(tx);

        let daemon = MetricsDaemon::new(rx);
        let state_handle = daemon.state_handle();
        let shutdown = CancellationToken::new();

        let daemon_shutdown = shutdown.clone();
        let daemon_handle = Some(runtime_handle.spawn(async move {
            daemon.run(daemon_shutdown).await;
        }));

        Self {
            client,
            state_handle,
            daemon_handle,
            shutdown,
        }
    }

    /// Returns a clone of the metrics client.
    ///
    /// The client is cheaply cloneable and can be distributed to multiple
    /// components.
    pub fn client(&self) -> MetricsClient {
        self.client.clone()
    }

    /// Returns a handle to the shared metrics state.
    ///
    /// This can be used for direct state access without a reporter.
    pub fn state_handle(&self) -> SharedMetricsState {
        Arc::clone(&self.state_handle)
    }

    /// Generates a snapshot using the provided reporter.
    ///
    /// # Type Parameters
    ///
    /// * `R` - The reporter type
    /// * `O` - The output type (inferred from the reporter)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let reporter = TuiReporter::new();
    /// let snapshot: TelemetrySnapshot = system.snapshot(&reporter);
    /// ```
    pub fn snapshot<R, O>(&self, reporter: &R) -> O
    where
        R: MetricsReporter<Output = O>,
    {
        let guard = self.state_handle.read().unwrap();
        reporter.report(&guard.state, &guard.history)
    }

    /// Returns a snapshot of the current state without a reporter.
    ///
    /// This provides direct access to the raw state for custom processing.
    pub fn state_snapshot(&self) -> MetricsStateSnapshot {
        self.state_handle.read().unwrap().clone()
    }

    /// Shuts down the metrics system gracefully.
    ///
    /// This signals the daemon to stop and waits for it to complete.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(handle) = self.daemon_handle.take() {
            let _ = handle.await;
        }
    }

    /// Returns true if the daemon is still running.
    pub fn is_running(&self) -> bool {
        self.daemon_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }
}

impl std::fmt::Debug for MetricsSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsSystem")
            .field("running", &self.is_running())
            .finish()
    }
}

// =============================================================================
// Optional Metrics Client
// =============================================================================

/// Extension trait for optional metrics client usage.
///
/// This allows components to work with `Option<MetricsClient>` without
/// verbose match statements.
pub trait OptionalMetrics {
    fn download_started(&self);
    fn download_completed(&self, bytes: u64, duration_us: u64);
    fn download_failed(&self);
    fn download_retried(&self);
    fn disk_cache_hit(&self, bytes: u64);
    fn disk_cache_miss(&self);
    fn disk_write_started(&self);
    fn disk_write_completed(&self, bytes: u64, duration_us: u64);
    fn disk_cache_initial_size(&self, bytes: u64);
    fn disk_cache_size(&self, bytes: u64);
    fn memory_cache_hit(&self);
    fn memory_cache_miss(&self);
    fn memory_cache_size(&self, bytes: u64);
    fn job_submitted(&self, is_fuse: bool);
    fn job_started(&self);
    fn job_completed(&self, success: bool, duration_us: u64);
    fn job_coalesced(&self);
    fn job_timed_out(&self);
    fn encode_started(&self);
    fn encode_completed(&self, bytes: u64, duration_us: u64);
    fn assembly_completed(&self, duration_us: u64);
    fn fuse_request_started(&self);
    fn fuse_request_completed(&self);
}

impl OptionalMetrics for Option<MetricsClient> {
    #[inline]
    fn download_started(&self) {
        if let Some(client) = self {
            client.download_started();
        }
    }

    #[inline]
    fn download_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.download_completed(bytes, duration_us);
        }
    }

    #[inline]
    fn download_failed(&self) {
        if let Some(client) = self {
            client.download_failed();
        }
    }

    #[inline]
    fn download_retried(&self) {
        if let Some(client) = self {
            client.download_retried();
        }
    }

    #[inline]
    fn disk_cache_hit(&self, bytes: u64) {
        if let Some(client) = self {
            client.disk_cache_hit(bytes);
        }
    }

    #[inline]
    fn disk_cache_miss(&self) {
        if let Some(client) = self {
            client.disk_cache_miss();
        }
    }

    #[inline]
    fn disk_write_started(&self) {
        if let Some(client) = self {
            client.disk_write_started();
        }
    }

    #[inline]
    fn disk_write_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.disk_write_completed(bytes, duration_us);
        }
    }

    #[inline]
    fn disk_cache_initial_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.disk_cache_initial_size(bytes);
        }
    }

    #[inline]
    fn disk_cache_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.disk_cache_size(bytes);
        }
    }

    #[inline]
    fn memory_cache_hit(&self) {
        if let Some(client) = self {
            client.memory_cache_hit();
        }
    }

    #[inline]
    fn memory_cache_miss(&self) {
        if let Some(client) = self {
            client.memory_cache_miss();
        }
    }

    #[inline]
    fn memory_cache_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.memory_cache_size(bytes);
        }
    }

    #[inline]
    fn job_submitted(&self, is_fuse: bool) {
        if let Some(client) = self {
            client.job_submitted(is_fuse);
        }
    }

    #[inline]
    fn job_started(&self) {
        if let Some(client) = self {
            client.job_started();
        }
    }

    #[inline]
    fn job_completed(&self, success: bool, duration_us: u64) {
        if let Some(client) = self {
            client.job_completed(success, duration_us);
        }
    }

    #[inline]
    fn job_coalesced(&self) {
        if let Some(client) = self {
            client.job_coalesced();
        }
    }

    #[inline]
    fn job_timed_out(&self) {
        if let Some(client) = self {
            client.job_timed_out();
        }
    }

    #[inline]
    fn encode_started(&self) {
        if let Some(client) = self {
            client.encode_started();
        }
    }

    #[inline]
    fn encode_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.encode_completed(bytes, duration_us);
        }
    }

    #[inline]
    fn assembly_completed(&self, duration_us: u64) {
        if let Some(client) = self {
            client.assembly_completed(duration_us);
        }
    }

    #[inline]
    fn fuse_request_started(&self) {
        if let Some(client) = self {
            client.fuse_request_started();
        }
    }

    #[inline]
    fn fuse_request_completed(&self) {
        if let Some(client) = self {
            client.fuse_request_completed();
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_metrics_system_lifecycle() {
        let runtime = tokio::runtime::Handle::current();
        let system = MetricsSystem::new(&runtime);

        assert!(system.is_running());

        let client = system.client();
        client.download_started();
        client.download_completed(1024, 5000);

        // Allow some time for events to be processed
        tokio::time::sleep(Duration::from_millis(150)).await;

        let reporter = TuiReporter::new();
        let snapshot = system.snapshot(&reporter);
        assert_eq!(snapshot.chunks_downloaded, 1);
        assert_eq!(snapshot.bytes_downloaded, 1024);

        system.shutdown().await;
    }

    #[tokio::test]
    async fn test_state_snapshot() {
        let runtime = tokio::runtime::Handle::current();
        let system = MetricsSystem::new(&runtime);

        let client = system.client();
        client.job_submitted(true);
        client.job_started();

        tokio::time::sleep(Duration::from_millis(150)).await;

        let snapshot = system.state_snapshot();
        assert_eq!(snapshot.state.jobs_submitted, 1);
        assert_eq!(snapshot.state.fuse_jobs_submitted, 1);
        assert_eq!(snapshot.state.jobs_active, 1);

        system.shutdown().await;
    }

    #[tokio::test]
    async fn test_state_handle_access() {
        let runtime = tokio::runtime::Handle::current();
        let system = MetricsSystem::new(&runtime);

        let handle = system.state_handle();

        // Multiple accesses should work
        {
            let _guard1 = handle.read().unwrap();
        }
        {
            let _guard2 = handle.read().unwrap();
        }

        system.shutdown().await;
    }

    #[test]
    fn test_optional_metrics_some() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = MetricsClient::new(tx);
        let optional: Option<MetricsClient> = Some(client);

        // Should not panic
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
    }

    #[test]
    fn test_optional_metrics_none() {
        let optional: Option<MetricsClient> = None;

        // Should be no-ops
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
    }

    #[tokio::test]
    async fn test_debug_output() {
        let runtime = tokio::runtime::Handle::current();
        let system = MetricsSystem::new(&runtime);

        let debug = format!("{:?}", system);
        assert!(debug.contains("MetricsSystem"));
        assert!(debug.contains("running"));

        system.shutdown().await;
    }
}
