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
};
use crate::jobs::DdsJobFactory;

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

    /// Handle to the daemon background task.
    daemon_handle: Option<JoinHandle<()>>,

    /// Shutdown token for graceful termination.
    shutdown_token: CancellationToken,
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
    ///
    /// # Type Parameters
    ///
    /// * `F` - DDS job factory type
    /// * `M` - Memory cache type
    pub fn new<F, M>(factory: Arc<F>, memory_cache: Arc<M>, config: RuntimeConfig) -> Self
    where
        F: DdsJobFactory + 'static,
        M: DaemonMemoryCache + 'static,
    {
        info!("Starting XEarthLayer runtime");

        // Create the daemon and its channel
        let (daemon, job_tx) = ExecutorDaemon::new(config.daemon_config, factory, memory_cache);

        // Create the DDS client for producers
        let dds_client = Arc::new(ChannelDdsClient::new(job_tx));

        // Create shutdown token for coordinating shutdown
        let shutdown_token = CancellationToken::new();

        // Spawn the daemon as a background task with shutdown coordination
        let daemon_shutdown = shutdown_token.clone();
        let daemon_handle = Some(tokio::spawn(async move {
            daemon.run(daemon_shutdown).await;
        }));

        info!("XEarthLayer runtime started");

        Self {
            dds_client,
            daemon_handle,
            shutdown_token,
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

        // Use new() which creates its own coalescer
        let runtime = XEarthLayerRuntime::new(factory, memory_cache, config);

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
        let factory1 = Arc::new(MockJobFactory);
        let memory_cache1 = Arc::new(MockMemoryCache);
        let config1 = RuntimeConfig::default();
        let runtime1 = XEarthLayerRuntime::new(factory1, memory_cache1, config1);

        let factory2 = Arc::new(MockJobFactory);
        let memory_cache2 = Arc::new(MockMemoryCache);
        let config2 = RuntimeConfig::default();
        let runtime2 = XEarthLayerRuntime::new(factory2, memory_cache2, config2);

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

        let runtime = XEarthLayerRuntime::new(factory, memory_cache, config);

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
