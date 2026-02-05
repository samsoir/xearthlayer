//! Job Executor Framework
//!
//! This module provides a job-based execution framework for managing concurrent
//! work with resource limits, priorities, and hierarchical job relationships.
//!
//! # Architecture
//!
//! The executor framework follows a layered design:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                       JobSubmitter                           │
//! │  Submit jobs, get handles for status/signalling             │
//! ├─────────────────────────────────────────────────────────────┤
//! │                       JobExecutor                            │
//! │  Main event loop: dispatch tasks, handle completions        │
//! ├─────────────────────────────────────────────────────────────┤
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
//! │  │ Priority    │  │ Resource    │  │ Telemetry           │  │
//! │  │ Queue       │  │ Pools       │  │ Sink                │  │
//! │  └─────────────┘  └─────────────┘  └─────────────────────┘  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Core Concepts
//!
//! - **Job**: A unit of work that produces tasks. Jobs have an error policy,
//!   priority, and can spawn child jobs.
//!
//! - **Task**: An async operation within a job. Tasks declare their resource
//!   type and can have retry policies.
//!
//! - **Resource Pool**: Semaphore-based limits on concurrent operations by
//!   type (Network, DiskIO, CPU).
//!
//! - **Priority**: Tasks are scheduled by priority (ON_DEMAND > PREFETCH >
//!   HOUSEKEEPING), with FIFO ordering within the same priority level.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{
//!     Job, Task, JobExecutor, ExecutorConfig, TaskContext, TaskResult,
//!     ResourceType, Priority, JobId, JobStatus,
//! };
//!
//! // Define a task
//! struct DownloadTask { url: String }
//!
//! impl Task for DownloadTask {
//!     fn name(&self) -> &str { "Download" }
//!     fn resource_type(&self) -> ResourceType { ResourceType::Network }
//!
//!     fn execute<'a>(&'a self, ctx: &'a mut TaskContext)
//!         -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>>
//!     {
//!         Box::pin(async move {
//!             if ctx.is_cancelled() {
//!                 return TaskResult::Cancelled;
//!             }
//!             // ... download logic ...
//!             TaskResult::Success
//!         })
//!     }
//! }
//!
//! // Define a job
//! struct MyJob { id: String }
//!
//! impl Job for MyJob {
//!     fn id(&self) -> JobId { JobId::new(&self.id) }
//!     fn name(&self) -> &str { "MyJob" }
//!     fn create_tasks(&self) -> Vec<Box<dyn Task>> {
//!         vec![Box::new(DownloadTask { url: "...".into() })]
//!     }
//!     // ... other methods ...
//! }
//!
//! // Run the executor
//! let config = ExecutorConfig::default();
//! let (executor, submitter) = JobExecutor::new(config);
//!
//! let handle = submitter.submit(MyJob { id: "job-1".into() });
//!
//! tokio::spawn(async move {
//!     executor.run(shutdown_token).await;
//! });
//!
//! let result = handle.wait().await;
//! ```
//!
//! # Telemetry
//!
//! The executor emits structured events via the [`TelemetrySink`] trait.
//! Events include job lifecycle (submitted, started, completed), task
//! lifecycle (started, completed, retrying), and resource pool status.
//!
//! # Signals
//!
//! Jobs can be controlled via signals sent through the [`JobHandle`]:
//! - `pause()`: Stop dispatching new tasks (in-flight continue)
//! - `resume()`: Resume from paused state
//! - `stop()`: Finish current tasks, don't start new ones
//! - `kill()`: Cancel all tasks immediately

// Module declarations
pub mod adapters;
mod cache_adapter;
mod chunk_results;
mod client;
mod concurrency;
mod context;
mod daemon;
mod download_config;
mod handle;
mod job;
mod policy;
mod queue;
mod resource_pool;
mod storage_limiter;
mod task;
mod telemetry;
mod traits;

// Executor core - split into focused modules
mod active_job;
mod config;
mod core;
mod dispatch;
mod lifecycle;
mod signals;
mod submitter;
mod watchdog;

// Re-export public types

// Policy types
pub use policy::{
    ErrorPolicy, Priority, RetryPolicy, DEFAULT_BACKOFF_MULTIPLIER, DEFAULT_INITIAL_DELAY_MS,
    DEFAULT_MAX_DELAY_SECS, PRIORITY_HOUSEKEEPING, PRIORITY_ON_DEMAND, PRIORITY_PREFETCH,
};

// Job types
pub use job::{Job, JobId, JobResult};

// Task types
pub use task::{Task, TaskError, TaskOutput, TaskResult, TaskResultKind};

// Context
pub use context::{SpawnedChildJob, TaskContext};

// Handle and status
pub use handle::{JobHandle, JobStatus, Signal};

// Resource pools
pub use resource_pool::{
    ResourcePermit, ResourcePool, ResourcePoolConfig, ResourcePools, ResourceType,
    DEFAULT_CPU_CAPACITY_MULTIPLIER, DEFAULT_DISK_IO_CAPACITY, DEFAULT_NETWORK_CAPACITY,
    DISK_IO_CAPACITY_HDD, DISK_IO_CAPACITY_NVME, DISK_IO_CAPACITY_SSD, MIN_CPU_CAPACITY_ADDITION,
};

// Telemetry
pub use telemetry::{
    MultiplexTelemetrySink, NullTelemetrySink, TelemetryEvent, TelemetrySink, TracingTelemetrySink,
};

// Queue
pub use queue::{PriorityQueue, QueuedTask};

// Job concurrency limits
pub use concurrency::{
    default_tile_generation_limit, JobConcurrencyLimits, FALLBACK_CPU_COUNT,
    TILE_GENERATION_CPU_MULTIPLIER, TILE_GENERATION_GROUP, TILE_GENERATION_MIN_LIMIT,
};

// Executor (split into focused modules)
pub use config::{
    ExecutorConfig, DEFAULT_JOB_CHANNEL_CAPACITY, DEFAULT_MAX_CONCURRENT_TASKS,
    DEFAULT_SIGNAL_CHANNEL_CAPACITY,
};
pub use core::JobExecutor;
pub use submitter::JobSubmitter;

// Client (for daemon architecture)
pub use client::{ChannelDdsClient, DdsClient, DdsClientError};

// Daemon
pub use daemon::{
    DaemonMemoryCache, ExecutorDaemon, ExecutorDaemonConfig, DEFAULT_REQUEST_CHANNEL_CAPACITY,
};

// Cache adapter (decoupled from pipeline module)
pub use cache_adapter::ExecutorCacheAdapter;

// Core traits (decoupled from pipeline module)
pub use traits::{
    BlockingExecutor, ChunkDownloadError, ChunkProvider, ConcurrentResults, ConcurrentRunner,
    DiskCache, ExecutorError, MemoryCache, TextureEncodeError, TextureEncoderAsync, Timer,
    TokioExecutor,
};

// Chunk results (for download/assemble stages)
pub use chunk_results::{ChunkFailure, ChunkResults, ChunkSuccess};

// Download configuration
pub use download_config::{DownloadConfig, DEFAULT_MAX_CONCURRENT_HTTP};

// Adapters (bridge external implementations to executor traits)
pub use adapters::{
    AsyncProviderAdapter, DiskCacheAdapter, MemoryCacheAdapter, NullDiskCache, ProviderAdapter,
    TextureEncoderAdapter,
};

// Storage concurrency limiter (semaphore-based I/O limiting)
pub use storage_limiter::{
    AcquireTimeoutError, StorageConcurrencyLimiter, StoragePermit, DEFAULT_CEILING,
    DEFAULT_SCALING_FACTOR, DISK_IO_CEILING, DISK_IO_SCALING_FACTOR,
};
