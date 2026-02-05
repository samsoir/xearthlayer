//! XEarthLayer runtime orchestrator.
//!
//! This module provides the central runtime that coordinates all daemons
//! and manages their lifecycles. The runtime owns the job channel and
//! provides DdsClient access to producers (FUSE, Prefetch).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      XEarthLayerRuntime                         │
//! │                                                                  │
//! │  ┌───────────────┐                    ┌──────────────────────┐  │
//! │  │ DdsClient     │──────────────────► │  ExecutorDaemon      │  │
//! │  │ (for FUSE,    │    JobRequest      │  (background task)   │  │
//! │  │  Prefetch)    │    channel         │                      │  │
//! │  └───────────────┘                    └──────────────────────┘  │
//! │                                                                  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Request coalescing is handled by the FUSE layer before requests reach
//! the runtime. The daemon receives already-deduplicated requests.
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::runtime::XEarthLayerRuntime;
//!
//! // Create runtime with dependencies
//! let runtime = XEarthLayerRuntime::new(factory, memory_cache, config);
//!
//! // Get client for producers
//! let client = runtime.dds_client();
//!
//! // When shutting down
//! runtime.shutdown().await;
//! ```

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::executor::{
    ChannelDdsClient, DaemonMemoryCache, DdsClient, ExecutorDaemon, ExecutorDaemonConfig,
    JobSubmitter, MultiplexTelemetrySink, TelemetrySink, TracingTelemetrySink,
};
use crate::jobs::DdsJobFactory;
use crate::metrics::MetricsClient;
use crate::runtime::{SharedTileProgressTracker, TileProgressSink, TileProgressTracker};

/// Configuration for the XEarthLayer runtime.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// Configuration for the executor daemon.
    pub daemon_config: ExecutorDaemonConfig,
}

impl RuntimeConfig {
    /// Create a new runtime configuration with custom settings.
    pub fn new(daemon_config: ExecutorDaemonConfig) -> Self {
        Self { daemon_config }
    }

    /// Set the channel capacity for job requests.
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.daemon_config.channel_capacity = capacity;
        self
    }
}

/// The main XEarthLayer runtime.
///
/// This struct orchestrates the daemon architecture, owning the job channel
/// and managing daemon lifecycles. It provides:
///
/// - A `DdsClient` for producers (FUSE, Prefetch) to submit tile requests
/// - Background execution of the `ExecutorDaemon`
/// - Graceful shutdown coordination
///
/// # Lifecycle
///
/// 1. **Creation**: `new()` creates the channel and spawns the daemon task
/// 2. **Operation**: Producers use `dds_client()` to submit requests
/// 3. **Shutdown**: `shutdown()` cancels the daemon and waits for completion
pub struct XEarthLayerRuntime {
    /// DDS client for producers to submit requests.
    dds_client: Arc<ChannelDdsClient>,

    /// Job submitter for generic job submission (e.g., GC jobs).
    job_submitter: JobSubmitter,

    /// Handle to the daemon background task.
    daemon_handle: Option<JoinHandle<()>>,

    /// Shutdown token for graceful termination.
    shutdown_token: CancellationToken,

    /// Tile progress tracker for TUI display.
    tile_progress_tracker: SharedTileProgressTracker,
}

impl XEarthLayerRuntime {
    /// Create a new runtime with the given dependencies.
    ///
    /// This starts the executor daemon in a background task immediately.
    /// Request coalescing is handled by the FUSE layer, not the daemon.
    ///
    /// # Arguments
    ///
    /// * `factory` - Factory for creating DDS generation jobs
    /// * `memory_cache` - Memory cache for fast-path lookups
    /// * `config` - Runtime configuration
    /// * `handle` - Handle to the Tokio runtime for spawning tasks
    ///
    /// # Type Parameters
    ///
    /// * `F` - DDS job factory type
    /// * `M` - Memory cache type
    pub fn new<F, M>(
        factory: Arc<F>,
        memory_cache: Arc<M>,
        config: RuntimeConfig,
        handle: tokio::runtime::Handle,
    ) -> Self
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
    {
        // Default to tracing-only telemetry, no metrics
        Self::with_metrics_client(factory, memory_cache, config, handle, None)
    }

    /// Create a new runtime with the event-based metrics client.
    ///
    /// This starts the executor daemon in a background task immediately.
    /// When metrics are provided, per-chunk and job events flow to the
    /// metrics daemon for real-time dashboard updates.
    ///
    /// # Arguments
    ///
    /// * `factory` - Factory for creating DDS generation jobs
    /// * `memory_cache` - Memory cache for fast-path lookups
    /// * `config` - Runtime configuration
    /// * `handle` - Handle to the Tokio runtime for spawning tasks
    /// * `metrics_client` - Optional metrics client for event emission
    ///
    /// # Type Parameters
    ///
    /// * `F` - DDS job factory type
    /// * `M` - Memory cache type
    pub fn with_metrics_client<F, M>(
        factory: Arc<F>,
        memory_cache: Arc<M>,
        config: RuntimeConfig,
        handle: tokio::runtime::Handle,
        metrics_client: Option<MetricsClient>,
    ) -> Self
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
    {
        info!("Starting XEarthLayer runtime");

        // Create tile progress tracker for TUI display
        let tile_progress_tracker = TileProgressTracker::new();
        let tile_progress_sink = TileProgressSink::new(Arc::clone(&tile_progress_tracker));

        // Combine tracing sink with tile progress sink
        let telemetry: Arc<dyn TelemetrySink> = Arc::new(MultiplexTelemetrySink::new(vec![
            Arc::new(TracingTelemetrySink) as Arc<dyn TelemetrySink>,
            tile_progress_sink as Arc<dyn TelemetrySink>,
        ]));

        // Create the daemon with or without metrics
        let (daemon, job_tx) = match metrics_client {
            Some(client) => ExecutorDaemon::with_metrics_client(
                config.daemon_config,
                factory,
                memory_cache,
                telemetry,
                client,
            ),
            None => ExecutorDaemon::with_telemetry(
                config.daemon_config,
                factory,
                memory_cache,
                telemetry,
            ),
        };

        // Create the DDS client for producers
        let dds_client = Arc::new(ChannelDdsClient::new(job_tx));

        // Extract the job submitter before spawning the daemon
        // This allows external components (like the GC scheduler) to submit jobs
        let job_submitter = daemon.job_submitter();

        // Create shutdown token for coordinating shutdown
        let shutdown_token = CancellationToken::new();

        // Spawn the daemon as a background task with shutdown coordination
        // Use handle.spawn() instead of tokio::spawn() so we don't require
        // the calling thread to be in a runtime context.
        let daemon_shutdown = shutdown_token.clone();
        let daemon_handle = Some(handle.spawn(async move {
            daemon.run(daemon_shutdown).await;
        }));

        info!("XEarthLayer runtime started");

        Self {
            dds_client,
            job_submitter,
            daemon_handle,
            shutdown_token,
            tile_progress_tracker,
        }
    }

    /// Get the DDS client for producers to submit tile requests.
    ///
    /// This client can be cloned and shared with FUSE handlers,
    /// prefetchers, and other components that need to request tiles.
    ///
    /// # Returns
    ///
    /// An `Arc<dyn DdsClient>` that can be used to submit requests.
    pub fn dds_client(&self) -> Arc<dyn DdsClient> {
        Arc::clone(&self.dds_client) as Arc<dyn DdsClient>
    }

    /// Get a concrete reference to the channel-based DDS client.
    ///
    /// This is useful when you need the concrete type rather than
    /// the trait object.
    pub fn channel_dds_client(&self) -> Arc<ChannelDdsClient> {
        Arc::clone(&self.dds_client)
    }

    /// Get a job submitter for submitting arbitrary jobs to the executor.
    ///
    /// This is used by the GC scheduler to submit garbage collection jobs.
    /// The submitter is cloneable and can be passed to background tasks.
    ///
    /// # Returns
    ///
    /// A clone of the internal `JobSubmitter` that can submit any `Job` to the executor.
    pub fn job_submitter(&self) -> JobSubmitter {
        self.job_submitter.clone()
    }

    /// Check if the runtime is still running.
    ///
    /// Returns `false` if the daemon has been shut down or crashed.
    pub fn is_running(&self) -> bool {
        self.dds_client.is_connected()
    }

    /// Get the shutdown token for external coordination.
    ///
    /// Other components can listen to this token to know when
    /// the runtime is shutting down.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Get the tile progress tracker for TUI display.
    ///
    /// The tracker provides a snapshot of active tile generation
    /// progress for rendering in the dashboard.
    pub fn tile_progress_tracker(&self) -> SharedTileProgressTracker {
        Arc::clone(&self.tile_progress_tracker)
    }

    /// Shutdown the runtime gracefully.
    ///
    /// This cancels the daemon and waits for it to complete any
    /// in-flight work before returning.
    pub async fn shutdown(mut self) {
        info!("Shutting down XEarthLayer runtime");

        // Signal shutdown to the daemon
        self.shutdown_token.cancel();

        // Wait for the daemon to finish
        if let Some(handle) = self.daemon_handle.take() {
            match handle.await {
                Ok(()) => info!("Executor daemon shut down cleanly"),
                Err(e) => tracing::error!("Executor daemon task panicked: {}", e),
            }
        }

        info!("XEarthLayer runtime stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::{Job, JobId, Priority, Task};
    use std::time::Duration;

    /// Mock job factory for testing.
    struct MockJobFactory;

    impl DdsJobFactory for MockJobFactory {
        fn create_job(&self, _tile: TileCoord, _priority: Priority) -> Box<dyn Job> {
            // Return a minimal mock job
            Box::new(MockJob)
        }
    }

    /// Mock job for testing.
    struct MockJob;

    impl Job for MockJob {
        fn id(&self) -> JobId {
            JobId::new("mock-job")
        }

        fn name(&self) -> &str {
            "MockJob"
        }

        fn priority(&self) -> Priority {
            Priority::PREFETCH
        }

        fn create_tasks(&self) -> Vec<Box<dyn Task>> {
            vec![]
        }
    }

    /// Mock memory cache for testing.
    /// Always returns cache miss - used for testing the runtime without real caching.
    struct MockMemoryCache;

    impl DaemonMemoryCache for MockMemoryCache {
        fn get(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + '_>>
        {
            Box::pin(async { None }) // Always miss for testing
        }
    }

    #[tokio::test]
    async fn test_runtime_creation_and_shutdown() {
        let factory = Arc::new(MockJobFactory);
        let memory_cache = Arc::new(MockMemoryCache);
        let config = RuntimeConfig::default();
        let handle = tokio::runtime::Handle::current();

        // Use new() which creates its own coalescer
        let runtime = XEarthLayerRuntime::new(factory, memory_cache, config, handle);

        // Runtime should be running
        assert!(runtime.is_running());

        // Shutdown should complete without hanging
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
            .await
            .expect("Shutdown should complete within 5 seconds");
    }

    #[tokio::test]
    async fn test_runtime_multiple_instances() {
        // Test that multiple runtimes can be created and shut down independently
        let handle = tokio::runtime::Handle::current();

        let factory1 = Arc::new(MockJobFactory);
        let memory_cache1 = Arc::new(MockMemoryCache);
        let config1 = RuntimeConfig::default();
        let runtime1 = XEarthLayerRuntime::new(factory1, memory_cache1, config1, handle.clone());

        let factory2 = Arc::new(MockJobFactory);
        let memory_cache2 = Arc::new(MockMemoryCache);
        let config2 = RuntimeConfig::default();
        let runtime2 = XEarthLayerRuntime::new(factory2, memory_cache2, config2, handle);

        // Both should be running
        assert!(runtime1.is_running());
        assert!(runtime2.is_running());

        // Shutdown in reverse order
        runtime2.shutdown().await;
        runtime1.shutdown().await;
    }

    #[tokio::test]
    async fn test_dds_client_is_connected() {
        let factory = Arc::new(MockJobFactory);
        let memory_cache = Arc::new(MockMemoryCache);
        let config = RuntimeConfig::default();
        let handle = tokio::runtime::Handle::current();

        let runtime = XEarthLayerRuntime::new(factory, memory_cache, config, handle);

        // Client should be connected initially
        let client = runtime.dds_client();
        assert!(client.is_connected());

        runtime.shutdown().await;
    }

    #[test]
    fn test_runtime_config_default() {
        let config = RuntimeConfig::default();
        assert_eq!(config.daemon_config.channel_capacity, 1000);
    }

    #[test]
    fn test_runtime_config_builder() {
        let config = RuntimeConfig::default().with_channel_capacity(500);
        assert_eq!(config.daemon_config.channel_capacity, 500);
    }
}
