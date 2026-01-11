# Job Executor Framework Design

## Status

**Complete** - All phases implemented:
- ✅ Phase 1: Core Framework (executor, job/task traits, resource pools)
- ✅ Phase 2: DDS Integration (DdsGenerateJob, tasks)
- ✅ Phase 3: Tile Prefetch (TilePrefetchJob, factory pattern)
- ✅ Phase 4: Daemon Architecture (XEarthLayerRuntime, DdsClient, channel-based communication)

## Problem Statement

The current XEarthLayer pipeline is tightly coupled to DDS generation workflows. While it works well for its purpose, it has limitations:

1. **Opinionated structure** - Pipeline stages are DDS-specific (download, assemble, encode)
2. **Limited composability** - Cannot easily express "prefetch a tile" as a collection of DDS jobs
3. **No job-level tracking** - Cannot track progress of aggregate operations (e.g., "tile 80% complete")
4. **Rigid error handling** - Error policies are baked into the pipeline, not declarative per-job

As we implement tile-based prefetching, we need to express hierarchical work:
- A tile prefetch job spawns many DDS generation jobs
- Each DDS job has multiple tasks (download, assemble, encode)
- The tile job completes when enough child DDS jobs succeed

This requires a generalized job execution framework.

## Goals

1. **Generic execution** - Framework knows nothing about DDS, tiles, or imagery
2. **Trait-based extensibility** - New job/task types implement traits
3. **Hierarchical composition** - Jobs contain tasks; tasks can spawn child jobs
4. **Declarative policies** - Error handling, retries, completion criteria defined per-job
5. **Efficient execution** - Respect concurrency limits, support cancellation
6. **Observable** - Progress tracking, logging, metrics

## Non-Goals

- Distributed execution (single-process only)
- Persistence/recovery (jobs are in-memory)
- Scheduling/cron (jobs are submitted, not scheduled)

## Design Decisions

### Build vs Buy

**Decision**: Build our own job executor backed by Tokio primitives.

**Alternatives Considered**:

| Crate | Why Not |
|-------|---------|
| [Apalis](https://crates.io/crates/apalis) | Requires external storage (Redis, Postgres, SQLite). Our jobs are transient, in-memory. |
| [Fang](https://crates.io/crates/fang) | PostgreSQL-backed, designed for persistent job queues. |
| [task-exec-queue](https://crates.io/crates/task-exec-queue) | Simple queue without resource pools, priority, or hierarchical jobs. |

**Rationale**:

1. **In-process, transient workloads** - We don't need persistence; jobs are short-lived (seconds to minutes)
2. **Custom resource pools** - Existing crates don't support declaring "this task needs network" vs "this task needs CPU"
3. **X-Plane coalescing** - Domain-specific requirement: same DDS path → share work, not duplicate
4. **Hierarchical composition** - Jobs spawning child jobs is uncommon in job queue crates
5. **Implementation cost** - Estimated ~500-800 lines of focused code on Tokio foundation

### Tokio Primitives Used

| Primitive | Purpose |
|-----------|---------|
| `Semaphore` | Resource pool capacity limits |
| `mpsc::channel` | Task/job queues with backpressure |
| `CancellationToken` | Signal propagation (stop/kill) |
| `spawn_blocking` | CPU-bound work (encode, assemble) |
| `watch::channel` | Job status broadcast to handles |
| `select!` | Multi-source event loop |

## Design

### Core Concepts

| Concept | Description |
|---------|-------------|
| **Job** | A named unit of work with tasks, error policy, and completion criteria |
| **Task** | A single async operation that may spawn child jobs |
| **JobExecutor** | Runs jobs respecting dependencies and concurrency limits |
| **JobHandle** | Reference to a submitted job for status/cancellation |
| **TaskContext** | Execution context passed to tasks for spawning children and accessing resources |
| **ResourcePool** | Semaphore-backed capacity limit for a resource type (network, disk, CPU) |

### Resource Pools

Tasks declare which resource type they require. The scheduler only dispatches tasks when their required resource pool has capacity. This replaces job-level concurrency limits with task-level resource awareness.

```rust
/// Resource types that tasks can require.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum ResourceType {
    /// Network I/O (HTTP connections).
    /// Default capacity: min(num_cpus * 16, 256)
    Network,

    /// Disk I/O (file read/write).
    /// Capacity varies by storage type (HDD: 4, SSD: 64, NVMe: 256)
    DiskIO,

    /// CPU-bound work (encoding, assembly).
    /// Default capacity: max(num_cpus * 1.25, num_cpus + 2)
    CPU,
}

/// Pool of permits for a resource type.
pub struct ResourcePool {
    resource_type: ResourceType,
    semaphore: Arc<Semaphore>,
    capacity: usize,
}

impl ResourcePool {
    pub fn new(resource_type: ResourceType, capacity: usize) -> Self {
        Self {
            resource_type,
            semaphore: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Acquire a permit, waiting if none available.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed")
    }

    /// Try to acquire a permit without waiting.
    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }

    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// Collection of all resource pools.
pub struct ResourcePools {
    pools: HashMap<ResourceType, ResourcePool>,
}

impl ResourcePools {
    pub fn new(config: &ResourcePoolConfig) -> Self {
        let mut pools = HashMap::new();
        pools.insert(ResourceType::Network, ResourcePool::new(ResourceType::Network, config.network));
        pools.insert(ResourceType::DiskIO, ResourcePool::new(ResourceType::DiskIO, config.disk_io));
        pools.insert(ResourceType::CPU, ResourcePool::new(ResourceType::CPU, config.cpu));
        Self { pools }
    }

    pub fn get(&self, resource_type: ResourceType) -> &ResourcePool {
        self.pools.get(&resource_type).expect("pool exists")
    }
}

/// Configuration for resource pool capacities.
pub struct ResourcePoolConfig {
    pub network: usize,  // Default: 256
    pub disk_io: usize,  // Default: 64 (SSD profile)
    pub cpu: usize,      // Default: num_cpus + 2
}

impl Default for ResourcePoolConfig {
    fn default() -> Self {
        let cpus = num_cpus::get();
        Self {
            network: (cpus * 16).min(256),
            disk_io: 64,  // SSD default, configurable via cache.disk_io_profile
            cpu: cpus + 2,
        }
    }
}
```

**Design Rationale**:

- Tasks from different jobs can run concurrently if they use different resource pools
- A CPU-bound encode task doesn't block network downloads
- Prevents resource exhaustion: can't spawn 1000 HTTP requests while encoding is starved
- Aligns with existing `StorageConcurrencyLimiter` and `CPUConcurrencyLimiter` patterns

### Relationship Model

```
┌─────────────────────────────────────────────────────────────────────┐
│                       Cardinality Model                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│   Job ─────── 1:n ───────► Task ─────── 1:1 ───────► Child Job      │
│    │                         │                           │          │
│    │                         │                           │          │
│    │                         │ (optional)                │          │
│    │                         │                           │          │
│    │                         └── Task may or may not     │          │
│    │                             spawn a child job       │          │
│    │                                                     │          │
│    └── Job completion depends                            │          │
│        on all tasks completing                           │          │
│        (including waiting for                            │          │
│        any spawned children)             ┌───────────────┘          │
│                                          │                          │
│                                          ▼                          │
│                                   Child Job ──── 1:n ──► Child Task │
│                                          │                          │
│                                          └── Recursive: child tasks │
│                                              can spawn grandchildren│
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Trait Definitions

#### Job Trait

```rust
/// A unit of work composed of tasks with error handling and completion criteria.
pub trait Job: Send + Sync + 'static {
    /// Unique identifier for this job instance.
    fn id(&self) -> JobId;

    /// Human-readable name for logging/display.
    fn name(&self) -> &str;

    /// Error handling policy for this job.
    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    /// IDs of jobs that must complete before this job can start.
    fn dependencies(&self) -> Vec<JobId> {
        vec![]
    }

    /// Priority for this job's tasks. Default: PREFETCH (0).
    fn priority(&self) -> Priority {
        Priority::PREFETCH
    }

    /// Create the tasks for this job.
    /// Called when the job is ready to execute.
    fn create_tasks(&self) -> Vec<Box<dyn Task>>;

    /// Called when all tasks and child jobs complete.
    /// Allows job to inspect results and determine final status.
    fn on_complete(&self, result: &JobResult) -> JobStatus {
        match result.failed_tasks.is_empty() && result.failed_children.is_empty() {
            true => JobStatus::Succeeded,
            false => JobStatus::Failed,
        }
    }
}

/// Unique job identifier.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub struct JobId(pub String);

impl JobId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}
```

#### Task Trait

```rust
/// A single async operation within a job.
pub trait Task: Send + Sync + 'static {
    /// Human-readable name for logging/display.
    fn name(&self) -> &str;

    /// Resource type required by this task.
    /// Scheduler acquires a permit from this pool before executing.
    fn resource_type(&self) -> ResourceType;

    /// Retry policy for this task.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::None
    }

    /// Execute the task.
    ///
    /// # Arguments
    /// * `ctx` - Context for spawning child jobs and accessing resources
    ///
    /// # Returns
    /// * `TaskResult` indicating success, failure, or retry
    fn execute(
        &self,
        ctx: &mut TaskContext,
    ) -> impl Future<Output = TaskResult> + Send;
}

/// Result of task execution.
pub enum TaskResult {
    /// Task completed successfully.
    Success,

    /// Task completed successfully with output data.
    SuccessWithOutput(TaskOutput),

    /// Task failed with error.
    Failed(TaskError),

    /// Task requests retry (respects retry policy).
    Retry(TaskError),

    /// Task was cancelled.
    Cancelled,
}

/// Arbitrary output data from a task.
pub struct TaskOutput {
    data: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl TaskOutput {
    pub fn new() -> Self {
        Self { data: HashMap::new() }
    }

    pub fn set<T: Any + Send + Sync>(&mut self, key: &str, value: T) {
        self.data.insert(key.to_string(), Box::new(value));
    }

    pub fn get<T: Any + Send + Sync>(&self, key: &str) -> Option<&T> {
        self.data.get(key).and_then(|v| v.downcast_ref())
    }
}
```

#### TaskContext

```rust
/// Execution context provided to tasks.
pub struct TaskContext {
    /// Job ID of the parent job.
    job_id: JobId,

    /// Sender for spawning child jobs.
    child_job_sender: mpsc::Sender<Box<dyn Job>>,

    /// Handles to child jobs spawned by this task.
    child_handles: Vec<JobHandle>,

    /// Output from previous tasks in this job.
    previous_outputs: HashMap<String, TaskOutput>,

    /// Shared resources (caches, indices, etc.).
    resources: Arc<Resources>,

    /// Cancellation token.
    cancel_token: CancellationToken,
}

impl TaskContext {
    /// Spawn a child job. Returns immediately; child executes asynchronously.
    ///
    /// The parent job will not complete until all spawned children complete.
    pub fn spawn_child_job(&mut self, job: impl Job) -> JobHandle {
        let handle = JobHandle::new(job.id());
        self.child_job_sender
            .send(Box::new(job))
            .expect("executor channel closed");
        self.child_handles.push(handle.clone());
        handle
    }

    /// Get output from a previous task in this job.
    pub fn get_output<T: Any + Send + Sync>(&self, task_name: &str, key: &str) -> Option<&T> {
        self.previous_outputs
            .get(task_name)
            .and_then(|o| o.get(key))
    }

    /// Access shared resources.
    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// Check if cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Get cancellation token for async operations.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}
```

### Policy Types

#### Error Policy

```rust
/// How a job handles task/child failures.
#[derive(Clone, Debug)]
pub enum ErrorPolicy {
    /// Stop job immediately on first failure.
    FailFast,

    /// Continue executing remaining tasks despite failures.
    ContinueOnError,

    /// Job succeeds if at least `threshold` fraction of work succeeds.
    /// Useful for prefetching where partial success is acceptable.
    PartialSuccess {
        /// Minimum success ratio (0.0 - 1.0).
        threshold: f64,
    },

    /// Custom completion logic (defer to `Job::on_complete`).
    Custom,
}
```

#### Retry Policy

```rust
/// How a task handles transient failures.
#[derive(Clone, Debug)]
pub enum RetryPolicy {
    /// No retries.
    None,

    /// Fixed number of retries with constant delay.
    Fixed {
        max_attempts: u32,
        delay: Duration,
    },

    /// Exponential backoff.
    ExponentialBackoff {
        max_attempts: u32,
        initial_delay: Duration,
        max_delay: Duration,
        multiplier: f64,
    },
}

impl RetryPolicy {
    /// Convenience constructor for common case.
    pub fn exponential(max_attempts: u32) -> Self {
        Self::ExponentialBackoff {
            max_attempts,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}
```

#### Priority

Tasks are queued by priority, then FIFO within the same priority level. Higher values execute first.

```rust
/// Task priority (higher = more important).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(pub i32);

impl Priority {
    /// On-demand requests from X-Plane FUSE layer.
    /// These must be served immediately - X-Plane is waiting.
    pub const ON_DEMAND: Priority = Priority(100);

    /// Background prefetch work.
    /// Lower priority, runs when X-Plane is idle.
    pub const PREFETCH: Priority = Priority(0);

    /// Housekeeping tasks (cache cleanup, index rebuilding).
    /// Lowest priority, runs when nothing else needs resources.
    pub const HOUSEKEEPING: Priority = Priority(-50);
}

impl Default for Priority {
    fn default() -> Self {
        Self::PREFETCH
    }
}
```

**Queue Behavior**:

1. Tasks enter queue with their job's priority
2. Scheduler pulls highest priority task first
3. Within same priority, tasks execute in FIFO order (by `enqueued_at`)
4. If highest-priority task's resource pool is full, scheduler waits (doesn't skip to lower priority)

**Design Rationale**:

- On-demand requests naturally preempt prefetch work without explicit signalling
- X-Plane's FUSE requests (Priority 100) always get served before prefetch (Priority 0)
- Prefetch tasks still make progress during quiet periods
- Housekeeping (cache eviction) runs in gaps when system is truly idle

### JobExecutor

```rust
/// Executes jobs respecting dependencies, resource pools, and policies.
pub struct JobExecutor {
    /// Channel for receiving new jobs (including child jobs).
    job_receiver: mpsc::Receiver<Box<dyn Job>>,

    /// Sender for submitting jobs.
    job_sender: mpsc::Sender<Box<dyn Job>>,

    /// Priority queue of ready tasks (ordered by priority, then FIFO).
    task_queue: PriorityQueue<QueuedTask>,

    /// Jobs waiting for dependencies.
    pending: HashMap<JobId, PendingJob>,

    /// Jobs currently executing.
    running: HashMap<JobId, RunningJob>,

    /// Completed jobs (kept for dependency resolution).
    completed: HashMap<JobId, JobResult>,

    /// Resource pools (network, disk, CPU).
    resource_pools: Arc<ResourcePools>,

    /// Shared resources for all jobs.
    resources: Arc<Resources>,

    /// Telemetry sink for emitting events.
    telemetry: Arc<dyn TelemetrySink>,
}

/// A task queued for execution with its priority.
struct QueuedTask {
    task: Box<dyn Task>,
    job_id: JobId,
    priority: Priority,
    enqueued_at: Instant,  // For FIFO ordering within same priority
}

impl JobExecutor {
    /// Create a new executor with the given configuration.
    pub fn new(
        pool_config: ResourcePoolConfig,
        resources: Resources,
        telemetry: Arc<dyn TelemetrySink>,
    ) -> Self;

    /// Submit a job for execution. Returns handle for status/cancellation.
    pub fn submit(&self, job: impl Job) -> JobHandle {
        let handle = JobHandle::new(job.id());
        self.job_sender.send(Box::new(job)).expect("executor running");
        handle
    }

    /// Run the executor (blocks until shutdown).
    pub async fn run(&mut self, shutdown: CancellationToken) -> Result<()> {
        loop {
            tokio::select! {
                // Receive new jobs (submitted or child jobs)
                Some(job) = self.job_receiver.recv() => {
                    self.enqueue_job(job);
                }

                // Dispatch ready tasks to available resource pools
                _ = self.dispatch_tasks() => {}

                // Check for completed tasks
                _ = self.poll_task_completion() => {}

                // Shutdown requested
                _ = shutdown.cancelled() => {
                    self.cancel_all_jobs().await;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Dispatch tasks from queue when their resource pool has capacity.
    async fn dispatch_tasks(&mut self) {
        // Iterate queue in priority order (highest first, then FIFO)
        while let Some(queued) = self.task_queue.peek() {
            let pool = self.resource_pools.get(queued.task.resource_type());

            // Try to acquire permit without blocking
            if let Some(permit) = pool.try_acquire() {
                let queued = self.task_queue.pop().unwrap();
                self.spawn_task(queued, permit).await;
            } else {
                // No capacity for highest-priority task; wait for completion
                break;
            }
        }
    }

    /// Spawn a task with its acquired resource permit.
    async fn spawn_task(&self, queued: QueuedTask, permit: OwnedSemaphorePermit) {
        let job_id = queued.job_id.clone();
        let task_name = queued.task.name().to_string();

        self.telemetry.emit(TelemetryEvent::TaskStarted {
            job_id: job_id.clone(),
            task_name: task_name.clone(),
        });

        let start = Instant::now();

        // Spawn task execution
        tokio::spawn(async move {
            let result = queued.task.execute(&mut ctx).await;
            drop(permit);  // Release resource permit
            // ... handle result, notify job ...
        });
    }

    fn dependencies_satisfied(&self, job: &PendingJob) -> bool {
        job.dependencies
            .iter()
            .all(|dep| self.completed.contains_key(dep))
    }
}
```

### JobHandle

```rust
/// Handle to a submitted job for status queries and signalling.
#[derive(Clone)]
pub struct JobHandle {
    job_id: JobId,
    status: watch::Receiver<JobStatus>,
    signal_sender: mpsc::Sender<Signal>,
}

impl JobHandle {
    /// Get current job status.
    pub fn status(&self) -> JobStatus {
        *self.status.borrow()
    }

    /// Wait for job completion.
    pub async fn wait(&mut self) -> JobResult {
        loop {
            if self.status().is_terminal() {
                break;
            }
            self.status.changed().await.ok();
        }
        // Return full result...
    }

    /// Get the job ID.
    pub fn id(&self) -> &JobId {
        &self.job_id
    }

    // --- Signalling API ---

    /// Pause the job: no new tasks start, in-flight tasks continue.
    pub fn pause(&self) {
        let _ = self.signal_sender.try_send(Signal::Pause);
    }

    /// Resume from paused state.
    pub fn resume(&self) {
        let _ = self.signal_sender.try_send(Signal::Resume);
    }

    /// Graceful stop: finish current tasks, don't start new ones.
    pub fn stop(&self) {
        let _ = self.signal_sender.try_send(Signal::Stop);
    }

    /// Immediate cancellation: cancel all in-flight tasks.
    pub fn kill(&self) {
        let _ = self.signal_sender.try_send(Signal::Kill);
    }
}

/// Job execution status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    /// Waiting for dependencies.
    Pending,

    /// Currently executing.
    Running,

    /// Paused (no new tasks dispatched).
    Paused,

    /// Completed successfully.
    Succeeded,

    /// Completed with failures.
    Failed,

    /// Cancelled before completion.
    Cancelled,

    /// Stopped gracefully (finished current work).
    Stopped,
}

impl JobStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled | Self::Stopped)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused)
    }
}
```

### Signalling

Jobs can be controlled via signals sent through the `JobHandle`. This enables clean shutdown, burst detection pause, and task cancellation.

```rust
/// Signal that can be sent to a running job.
#[derive(Clone, Copy, Debug)]
pub enum Signal {
    /// Pause execution: no new tasks start, in-flight tasks continue.
    /// Status changes to `Paused`.
    Pause,

    /// Resume from paused state.
    /// Status changes back to `Running`.
    Resume,

    /// Graceful stop: finish current tasks, don't start new ones.
    /// Status changes to `Stopped` when complete.
    Stop,

    /// Immediate cancellation: cancel all in-flight tasks via CancellationToken.
    /// Status changes to `Cancelled`.
    Kill,
}
```

**Signal Propagation**:

```
┌─────────────────────────────────────────────────────────────────────┐
│                      Signal Propagation                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  JobHandle.pause()                                                  │
│       │                                                             │
│       ▼                                                             │
│  Parent Job receives Signal::Pause                                  │
│       │                                                             │
│       ├── Sets status = Paused                                      │
│       ├── Stops dispatching new tasks from its queue                │
│       └── Propagates pause to all child jobs                        │
│              │                                                      │
│              ▼                                                      │
│       Child jobs pause recursively                                  │
│                                                                     │
│  In-flight tasks: Continue until completion (graceful)              │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

**Use Cases**:

| Signal | Use Case |
|--------|----------|
| `Pause` | TileBasedPrefetcher detects X-Plane burst, pauses prefetch jobs |
| `Resume` | Quiet period detected, resume prefetching |
| `Stop` | Clean application shutdown |
| `Kill` | Timeout exceeded, abort immediately |

### Telemetry

Jobs and tasks emit telemetry events via a sink abstraction. The executor doesn't know how events are consumed—this follows the "emit, don't present" pattern.

```rust
/// Events emitted during job/task execution.
#[derive(Clone, Debug)]
pub enum TelemetryEvent {
    // --- Job Events ---
    JobSubmitted {
        job_id: JobId,
        name: String,
        priority: Priority,
    },
    JobStarted {
        job_id: JobId,
    },
    JobCompleted {
        job_id: JobId,
        status: JobStatus,
        duration: Duration,
        tasks_succeeded: usize,
        tasks_failed: usize,
    },
    JobSignalled {
        job_id: JobId,
        signal: Signal,
    },

    // --- Task Events ---
    TaskStarted {
        job_id: JobId,
        task_name: String,
        resource_type: ResourceType,
    },
    TaskCompleted {
        job_id: JobId,
        task_name: String,
        result: TaskResultKind,
        duration: Duration,
    },
    TaskRetrying {
        job_id: JobId,
        task_name: String,
        attempt: u32,
        delay: Duration,
    },

    // --- Resource Pool Events ---
    ResourcePoolExhausted {
        resource_type: ResourceType,
        waiting_tasks: usize,
    },
    ResourcePoolAvailable {
        resource_type: ResourceType,
        available_permits: usize,
    },
}

/// Simplified result kind for telemetry (no error details).
#[derive(Clone, Copy, Debug)]
pub enum TaskResultKind {
    Success,
    Failed,
    Cancelled,
}

/// Sink for telemetry events.
pub trait TelemetrySink: Send + Sync {
    fn emit(&self, event: TelemetryEvent);
}

/// No-op sink for when telemetry is disabled.
pub struct NullTelemetrySink;

impl TelemetrySink for NullTelemetrySink {
    fn emit(&self, _event: TelemetryEvent) {}
}
```

**Telemetry Architecture**:

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Telemetry Flow                                   │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│   Job/Task Execution                                                │
│        │                                                            │
│        │ emit(TelemetryEvent)                                       │
│        ▼                                                            │
│   ┌─────────────────┐                                               │
│   │ TelemetrySink   │ ◄── Trait (injected at startup)               │
│   └────────┬────────┘                                               │
│            │                                                        │
│            ▼                                                        │
│   ┌─────────────────┐     ┌─────────────────┐                       │
│   │   Aggregator    │────►│    Metrics      │                       │
│   │   (Optional)    │     │  (Prometheus)   │                       │
│   └────────┬────────┘     └─────────────────┘                       │
│            │                                                        │
│            ▼                                                        │
│   ┌─────────────────┐     ┌─────────────────┐                       │
│   │   Status UI     │     │   Log Output    │                       │
│   │   (TUI widget)  │     │   (tracing)     │                       │
│   └─────────────────┘     └─────────────────┘                       │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

**Design Rationale**:

- Jobs/tasks are decoupled from presentation concerns
- Same events can feed multiple consumers (TUI, logging, Prometheus)
- `NullTelemetrySink` for tests and minimal overhead when disabled
- Events are structured for efficient aggregation (job_id, durations)

### Shared Resources

```rust
/// Shared resources available to all jobs/tasks.
pub struct Resources {
    /// Ortho union index for tile lookups.
    pub ortho_index: Arc<OrthoUnionIndex>,

    /// Memory cache.
    pub memory_cache: Arc<MemoryCache>,

    /// Disk cache.
    pub disk_cache: Arc<DiskCache>,

    /// HTTP client.
    pub http_client: Arc<dyn HttpClient>,

    /// Imagery provider.
    pub imagery_provider: Arc<dyn ImageryProvider>,

    /// Extensible: additional resources.
    pub extensions: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Resources {
    /// Get a typed extension resource.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.extensions
            .get(&TypeId::of::<T>())
            .and_then(|r| r.clone().downcast().ok())
    }
}
```

### Coalescing

X-Plane often requests the same DDS file multiple times in rapid succession (during initial load and when panning the map). The job executor integrates with coalescing to prevent duplicate work.

```rust
/// Coalescer for DDS generation jobs.
///
/// When multiple callers request the same DDS path, they share the same job
/// rather than creating duplicates.
pub struct DdsJobCoalescer {
    /// In-flight jobs by DDS path.
    in_flight: RwLock<HashMap<PathBuf, JobHandle>>,

    /// Job executor for submitting new jobs.
    executor: Arc<JobExecutor>,
}

impl DdsJobCoalescer {
    pub fn new(executor: Arc<JobExecutor>) -> Self {
        Self {
            in_flight: RwLock::new(HashMap::new()),
            executor,
        }
    }

    /// Submit a DDS generation request, coalescing with in-flight jobs.
    ///
    /// If a job for the same path is already running, returns the existing handle.
    /// Otherwise, creates a new job and returns its handle.
    pub async fn submit_or_join(&self, path: PathBuf, priority: Priority) -> JobHandle {
        // Fast path: check if already in-flight
        {
            let in_flight = self.in_flight.read().await;
            if let Some(handle) = in_flight.get(&path) {
                if !handle.status().is_terminal() {
                    return handle.clone();
                }
            }
        }

        // Slow path: acquire write lock and submit new job
        let mut in_flight = self.in_flight.write().await;

        // Double-check after acquiring write lock
        if let Some(handle) = in_flight.get(&path) {
            if !handle.status().is_terminal() {
                return handle.clone();
            }
        }

        // Create and submit new job
        let job = DdsGenerateJob::with_priority(path.clone(), priority);
        let handle = self.executor.submit(job);

        in_flight.insert(path, handle.clone());
        handle
    }

    /// Clean up completed jobs from the in-flight map.
    pub async fn cleanup(&self) {
        let mut in_flight = self.in_flight.write().await;
        in_flight.retain(|_, handle| !handle.status().is_terminal());
    }
}
```

**Integration with FUSE Layer**:

```rust
impl Fuse3OrthoUnionFS {
    async fn handle_dds_request(&self, path: &Path) -> Result<Bytes> {
        // Submit with ON_DEMAND priority (immediate)
        let handle = self.coalescer
            .submit_or_join(path.to_path_buf(), Priority::ON_DEMAND)
            .await;

        // Wait for completion
        let result = handle.wait().await;

        // ... return DDS data from cache ...
    }
}
```

**Coalescing with Prefetch**:

```rust
impl TileBasedPrefetcher {
    async fn prefetch_dds(&self, path: PathBuf) {
        // Submit with PREFETCH priority (lower)
        // If an ON_DEMAND job is already running for this path,
        // we'll join it rather than create duplicate work
        let handle = self.coalescer
            .submit_or_join(path, Priority::PREFETCH)
            .await;

        // Don't wait - prefetch is fire-and-forget
        // Job runs in background, cache populated when done
    }
}
```

**Design Rationale**:

- Extends existing `RequestCoalescer` pattern from the async pipeline
- Multiple callers share work: X-Plane request + prefetch don't duplicate
- Priority is respected: ON_DEMAND jobs run before PREFETCH, but they can coalesce
- Lock granularity: read lock for check, write lock only for new jobs
- Cleanup prevents unbounded memory growth

## Example: Tile Prefetch Implementation

### TilePrefetchJob

```rust
/// Job to prefetch all DDS files within a 1° tile.
pub struct TilePrefetchJob {
    id: JobId,
    lat: i32,
    lon: i32,
}

impl TilePrefetchJob {
    pub fn new(lat: i32, lon: i32) -> Self {
        Self {
            id: JobId::new(format!("tile-prefetch-{}-{}", lat, lon)),
            lat,
            lon,
        }
    }
}

impl Job for TilePrefetchJob {
    fn id(&self) -> JobId {
        self.id.clone()
    }

    fn name(&self) -> &str {
        "TilePrefetch"
    }

    fn error_policy(&self) -> ErrorPolicy {
        // 80% success = tile is usable
        ErrorPolicy::PartialSuccess { threshold: 0.8 }
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        vec![
            Box::new(EnumerateDdsFilesTask::new(self.lat, self.lon)),
            Box::new(WaitForChildrenTask::new()),
        ]
    }
}
```

### EnumerateDdsFilesTask

```rust
/// Task that enumerates DDS files in a tile and spawns child jobs.
pub struct EnumerateDdsFilesTask {
    lat: i32,
    lon: i32,
}

impl EnumerateDdsFilesTask {
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }
}

impl Task for EnumerateDdsFilesTask {
    fn name(&self) -> &str {
        "EnumerateDdsFiles"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU  // Index lookup is CPU-bound
    }

    async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
        let ortho_index = &ctx.resources().ortho_index;

        // Get all DDS files in this 1° tile
        let dds_files = ortho_index.dds_files_in_tile(self.lat, self.lon);

        if dds_files.is_empty() {
            return TaskResult::Success;
        }

        // Spawn a child DdsGenerateJob for each file
        for dds_path in dds_files {
            if ctx.is_cancelled() {
                return TaskResult::Cancelled;
            }

            ctx.spawn_child_job(DdsGenerateJob::new(dds_path));
        }

        TaskResult::Success
    }
}
```

### DdsGenerateJob

```rust
/// Job to generate a single DDS file.
pub struct DdsGenerateJob {
    id: JobId,
    path: PathBuf,
    priority: Priority,
}

impl DdsGenerateJob {
    pub fn new(path: PathBuf) -> Self {
        Self::with_priority(path, Priority::PREFETCH)
    }

    pub fn with_priority(path: PathBuf, priority: Priority) -> Self {
        let id = JobId::new(format!("dds-{}", path.display()));
        Self { id, path, priority }
    }
}

impl Job for DdsGenerateJob {
    fn id(&self) -> JobId {
        self.id.clone()
    }

    fn name(&self) -> &str {
        "DdsGenerate"
    }

    fn error_policy(&self) -> ErrorPolicy {
        ErrorPolicy::FailFast
    }

    fn priority(&self) -> Priority {
        self.priority
    }

    fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        vec![
            Box::new(DownloadChunksTask::new(self.path.clone())),
            Box::new(AssembleImageTask::new()),
            Box::new(EncodeDdsTask::new(self.path.clone())),
        ]
    }
}
```

### DownloadChunksTask

```rust
/// Task to download imagery chunks for a DDS file.
pub struct DownloadChunksTask {
    path: PathBuf,
}

impl Task for DownloadChunksTask {
    fn name(&self) -> &str {
        "DownloadChunks"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::Network  // HTTP downloads
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::exponential(3)
    }

    async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
        let provider = &ctx.resources().imagery_provider;
        let coords = parse_dds_coords(&self.path)?;

        // Download all 256 chunks (16x16)
        let chunks = provider
            .download_chunks(coords, ctx.cancel_token())
            .await;

        match chunks {
            Ok(data) => {
                let mut output = TaskOutput::new();
                output.set("chunks", data);
                TaskResult::SuccessWithOutput(output)
            }
            Err(e) if e.is_transient() => TaskResult::Retry(e.into()),
            Err(e) => TaskResult::Failed(e.into()),
        }
    }
}
```

### AssembleImageTask

```rust
/// Task to assemble chunks into a full image.
pub struct AssembleImageTask;

impl Task for AssembleImageTask {
    fn name(&self) -> &str {
        "AssembleImage"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU  // Image assembly is CPU-bound
    }

    async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
        // Get chunks from previous task
        let chunks: &Vec<Bytes> = ctx
            .get_output("DownloadChunks", "chunks")
            .ok_or_else(|| TaskError::missing_input("chunks"))?;

        // Assemble into 4096x4096 image
        let image = assemble_chunks(chunks)?;

        let mut output = TaskOutput::new();
        output.set("image", image);
        TaskResult::SuccessWithOutput(output)
    }
}
```

### EncodeDdsTask

```rust
/// Task to encode image as DDS and cache it.
pub struct EncodeDdsTask {
    path: PathBuf,
}

impl Task for EncodeDdsTask {
    fn name(&self) -> &str {
        "EncodeDds"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::CPU  // BC1 encoding is CPU-bound
    }

    async fn execute(&self, ctx: &mut TaskContext) -> TaskResult {
        let image: &DynamicImage = ctx
            .get_output("AssembleImage", "image")
            .ok_or_else(|| TaskError::missing_input("image"))?;

        // Encode to DDS
        let dds_data = encode_bc1_dds(image)?;

        // Store in caches
        let cache = &ctx.resources().memory_cache;
        cache.put(&self.path, dds_data.clone()).await;

        let disk_cache = &ctx.resources().disk_cache;
        disk_cache.put(&self.path, &dds_data).await?;

        TaskResult::Success
    }
}
```

## Execution Flow

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Execution Flow Example                           │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  1. Submit TilePrefetchJob(lat=60, lon=-146)                       │
│     │                                                               │
│     ▼                                                               │
│  2. Executor starts TilePrefetchJob                                │
│     │                                                               │
│     ▼                                                               │
│  3. EnumerateDdsFilesTask executes                                 │
│     │  - Queries OrthoUnionIndex                                   │
│     │  - Finds 847 DDS files                                       │
│     │  - Spawns 847 DdsGenerateJob children                        │
│     │                                                               │
│     ▼                                                               │
│  4. Executor receives 847 child jobs                               │
│     │  - Queues them (respects max_jobs limit)                     │
│     │                                                               │
│     ▼                                                               │
│  5. DdsGenerateJobs execute in parallel                            │
│     │  - Each runs: Download → Assemble → Encode                   │
│     │  - Some may fail, retry, or succeed                          │
│     │                                                               │
│     ▼                                                               │
│  6. WaitForChildrenTask waits for all children                     │
│     │                                                               │
│     ▼                                                               │
│  7. TilePrefetchJob::on_complete() evaluates results               │
│     │  - 812/847 succeeded (95.9%)                                 │
│     │  - Threshold 80% met → JobStatus::Succeeded                  │
│     │                                                               │
│     ▼                                                               │
│  8. Job complete, handle notified                                  │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Integration with Tile-Based Prefetcher

The `TileBasedPrefetcher` uses the job executor:

```rust
impl TileBasedPrefetcher {
    async fn prefetch_tile(&self, tile: TileCoord) {
        // Submit tile prefetch job
        let job = TilePrefetchJob::new(tile.lat, tile.lon);
        let handle = self.executor.submit(job);

        // Track active prefetch
        self.active_tiles.insert(tile, handle.clone());

        // Optionally wait or let it run in background
        // handle.wait().await;
    }

    fn cancel_active_prefetch(&self) {
        for (_, handle) in self.active_tiles.drain() {
            handle.cancel();
        }
    }
}
```

## Module Structure

```
xearthlayer/src/
├── executor/                   # Core executor framework
│   ├── mod.rs                  # Module exports
│   ├── job.rs                  # Job trait, JobId, JobResult
│   ├── task.rs                 # Task trait, TaskResult, TaskOutput
│   ├── context.rs              # TaskContext for task execution
│   ├── executor.rs             # JobExecutor and ExecutorSubmitter
│   ├── handle.rs               # JobHandle and JobStatus
│   ├── policy.rs               # ErrorPolicy, RetryPolicy, Priority
│   ├── client.rs               # DdsClient trait, ChannelDdsClient
│   └── daemon.rs               # ExecutorDaemon, DaemonMemoryCache
│
├── runtime/                    # Daemon orchestration
│   ├── mod.rs                  # Module exports
│   ├── orchestrator.rs         # XEarthLayerRuntime
│   └── request.rs              # JobRequest, DdsResponse, RequestOrigin
│
├── jobs/                       # Job implementations
│   ├── mod.rs
│   ├── dds_generate.rs         # DdsGenerateJob
│   ├── tile_prefetch.rs        # TilePrefetchJob
│   └── factory.rs              # DdsJobFactory trait
│
├── tasks/                      # Task implementations
│   ├── mod.rs
│   ├── download_chunks.rs      # DownloadChunksTask
│   ├── assemble_image.rs       # AssembleImageTask
│   ├── encode_dds.rs           # EncodeDdsTask
│   ├── cache_write.rs          # CacheWriteTask
│   └── generate_tile_list.rs   # GenerateTileListTask
│
├── fuse/fuse3/
│   └── shared.rs               # DdsRequestor trait (supports DdsClient)
│
└── prefetch/
    └── radial.rs               # RadialPrefetcher (supports DdsClient)
```

## Migration Path

### Phase 1: Core Framework ✅

1. ✅ Implement `executor/` module with traits and executor
2. ✅ Add basic job/task implementations for testing
3. ✅ Unit tests for executor logic

### Phase 2: DDS Pipeline Migration ✅

1. ✅ Implement `DdsGenerateJob` and its tasks
2. ✅ Wire up to existing pipeline infrastructure
3. ✅ Ensure feature parity with current DDS generation

### Phase 3: Tile Prefetch Integration ✅

1. ✅ Implement `TilePrefetchJob`
2. ✅ Implement `DdsJobFactory` trait and `DefaultDdsJobFactory`
3. ✅ Implement `GenerateTileListTask` for child job spawning

### Phase 4: Daemon Architecture ✅

1. ✅ Implement `runtime/` module (XEarthLayerRuntime, JobRequest, DdsResponse)
2. ✅ Implement `DdsClient` trait and `ChannelDdsClient`
3. ✅ Implement `ExecutorDaemon` with channel-based communication
4. ✅ Update FUSE `DdsRequestor` trait with optional DdsClient support
5. ✅ Update `RadialPrefetcher` with optional DdsClient support

### Phase 5: Wire Up & Remove Legacy (Next)

1. Wire up `XEarthLayerRuntime` in the service layer
2. Decouple traits from pipeline module (see Technical Debt below)
3. Remove legacy pipeline stages and DdsHandler
4. Integration testing with X-Plane

### Technical Debt: Pipeline Module Dependencies

The executor/jobs/tasks modules currently depend on traits defined in `pipeline/`:

| Module | Dependencies |
|--------|--------------|
| `executor/daemon.rs` | `CoalesceResult`, `RequestCoalescer`, `MemoryCache` (blanket impl) |
| `executor/resource_pool.rs` | `DiskIoProfile` |
| `jobs/factory.rs` | `ChunkProvider`, `TextureEncoderAsync`, `MemoryCache`, `DiskCache`, `BlockingExecutor` |
| `jobs/dds_generate.rs` | Same as factory |
| `tasks/*.rs` | `ChunkProvider`, `DiskCache`, `BlockingExecutor`, `TextureEncoderAsync`, `ChunkResults`, `PipelineConfig` |
| `runtime/orchestrator.rs` | `RequestCoalescer` |

**Resolution Plan**:

1. Move core traits to `executor/traits.rs`:
   - `ChunkProvider`, `DiskCache`, `MemoryCache`, `TextureEncoderAsync`, `BlockingExecutor`
   - `ChunkResults`, `PipelineConfig` (or inline where used)

2. Move `RequestCoalescer` to `executor/coalescer.rs` or `runtime/coalescer.rs`

3. Move `DiskIoProfile` to `executor/resource_pool.rs` or a config module

4. Keep adapters (`AsyncProviderAdapter`, `ParallelDiskCache`, etc.) in standalone `adapters/` module

5. Delete remaining `pipeline/` module (stages, runner, control_plane)

**Already Decoupled**:
- `ExecutorCacheAdapter` in `executor/cache_adapter.rs` (replaces `MemoryCacheAdapter` dependency)

## Success Criteria

- [x] Jobs and Tasks are trait-based, allowing new implementations
- [x] Child job spawning works correctly (1:1 task→child relationship)
- [x] Error policies (FailFast, ContinueOnError, PartialSuccess) work correctly
- [x] Retry policies work with exponential backoff
- [x] Resource pools (Network, DiskIO, CPU) limit concurrent tasks by type
- [x] Priority queue orders tasks correctly (ON_DEMAND > PREFETCH > HOUSEKEEPING)
- [x] Signalling works (pause/resume/stop/kill)
- [x] Telemetry events emitted via TelemetrySink
- [x] DDS job coalescing prevents duplicate work (via RequestCoalescer)
- [x] Progress can be tracked via JobHandle
- [x] Existing DDS generation works via new framework
- [x] Tile prefetch jobs work end-to-end
- [x] Daemon architecture with channel-based communication
- [x] DdsClient trait for producer abstraction (FUSE, Prefetch)

## Future Considerations

1. **Prometheus metrics** - Export telemetry events to Prometheus for dashboards
2. **Job persistence** - Optional persistence for crash recovery (not in scope)
3. **Visualization** - Debug UI showing job graph and status (TUI widget)
4. **Adaptive pools** - Auto-tune resource pool capacities based on observed performance
5. **Work stealing** - Balance load across resource pools when one is idle

## Implementation Notes

This section captures key decisions made during implementation.

### Phase 2: Direct Implementation vs Wrapping (January 2026)

**Decision**: Tasks directly implement business logic rather than wrapping existing pipeline stages.

**Context**: The original plan called for tasks to wrap existing pipeline stage functions (e.g., `download_stage_with_limiter`) to minimize regression risk. During implementation, we discovered this created unnecessary indirection:

```rust
// REJECTED: Wrapper approach
// Task delegates to stage, bypassing most functionality (limiters, metrics)
let result = encode_stage(job_id, image, encoder, executor, None, None, false).await;

// IMPLEMENTED: Direct approach
// Task contains the actual business logic
let dds_data = executor.execute_blocking(move || encoder.encode(&image)).await;
```

**Rationale**:
- Wrapping created double abstraction (Task → Stage → Work)
- Stage parameters for limiters/metrics were always `None` (dead code paths)
- Direct implementation makes the job executor the replacement, not a wrapper
- Business logic is preserved through "lift and shift" from battle-tested stages

### Resource Injection Pattern (January 2026)

**Decision**: Use hybrid injection pattern - explicit dependencies for leaf tasks, factory pattern for spawning tasks.

**Leaf Tasks (Phase 2)**:
```rust
// Explicit Arc<T> fields - clear contracts, easy testing
pub struct DownloadChunksTask<P, D> {
    provider: Arc<P>,
    disk_cache: Arc<D>,
}
```

**Spawning Tasks (Phase 3)**:
```rust
// Factory injection - encapsulates complex wiring
pub struct EnumerateDdsFilesTask {
    job_factory: Arc<dyn DdsJobFactory>,  // Creates child DdsGenerateJobs
}

pub trait DdsJobFactory: Send + Sync {
    fn create_job(&self, tile: TileCoord, priority: Priority) -> Box<dyn Job>;
}
```

**Rationale**:
- Leaf tasks have clear, testable dependency contracts
- Spawning tasks don't need knowledge of child job dependencies
- Factory can be shared across multiple jobs
- Dependency wiring happens once at application startup

### TaskOutput Ownership (January 2026)

**Decision**: Add `Clone` derive to `ChunkResults` to enable ownership transfer between tasks.

**Context**: `TaskOutput::get()` returns `Option<&T>` (borrowed), but CPU-bound work requires `'static` closures for `spawn_blocking`. Solution:

```rust
// ChunkResults now derives Clone (pipeline/error.rs)
#[derive(Debug, Clone)]
pub struct ChunkResults { ... }

// In AssembleImageTask
let chunks = ctx.get_output::<ChunkResults>("DownloadChunks", "chunks")?.clone();
executor.execute_blocking(move || assemble_chunks(chunks)).await
```

While cloning ~16MB of chunk data isn't free, it's a one-time cost per tile and maintains clean API boundaries.

### Phase 4: Daemon Architecture (January 2026)

**Decision**: FUSE Handler and Job Executor are peer daemons communicating via channels, not hierarchically coupled.

**Context**: The original integration plan embedded the Job Executor inside the FUSE handler's request path via a `DdsHandler` function pointer. This created implicit ownership and tight coupling. Analysis revealed that:

- FUSE Handler is a **daemon** providing X-Plane with a filesystem interface
- Job Executor is a **daemon** managing all DDS generation work
- Prefetcher is a **daemon** proactively warming the cache
- Prewarmer is a **one-shot context** that runs to completion

These are independent processes that should exist in isolation with minimal knowledge of each other.

**Architecture**:

```
┌─────────────────────────────────────────────────────────────────┐
│                      CLI / XEarthLayerRuntime                    │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
│  │ FUSE Daemon  │  │  Prefetch    │  │  Prewarm     │           │
│  │              │  │  Daemon      │  │  Context     │           │
│  │ (on-demand)  │  │ (continuous) │  │ (one-shot)   │           │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘           │
│         │                 │                 │                    │
│         │    Job Producers (submit work)    │                    │
│         ▼                 ▼                 ▼                    │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │              JobRequest Channel (mpsc)                   │    │
│  │              Owned by CLI, bounded                       │    │
│  └─────────────────────────┬───────────────────────────────┘    │
│                            │                                     │
│                            │    Job Consumer                     │
│                            ▼                                     │
│                 ┌──────────────────────┐                        │
│                 │  Job Executor Daemon │                        │
│                 │                      │                        │
│                 │  • Receives requests │                        │
│                 │  • Schedules jobs    │                        │
│                 │  • Manages resources │                        │
│                 │  • Sends responses   │                        │
│                 └──────────────────────┘                        │
└─────────────────────────────────────────────────────────────────┘
```

**Key Design Principles**:

| Principle | Implementation |
|-----------|----------------|
| **Channel as Mediator** | Use bounded `mpsc` channel instead of Mediator object. Producers send to channel, consumer receives. Neither knows about the other. |
| **Trait-Based Producer API** | Producers receive `Arc<dyn JobSubmitter>`, not raw channel sender. Enables mocking and hides implementation. |
| **Optional Response Channels** | FUSE includes `oneshot::Sender` in request (must wait). Prefetch omits it (fire-and-forget). |
| **Peer Daemons** | Neither daemon owns the other. CLI creates, starts, and shuts down all daemons. Independent lifecycles. |

**Message Types**:

```rust
/// Request to generate a DDS tile
pub struct JobRequest {
    pub tile: TileCoord,
    pub priority: Priority,
    pub cancellation: CancellationToken,
    pub response_tx: Option<oneshot::Sender<JobResponse>>,  // FUSE needs this
    pub origin: RequestOrigin,  // Fuse, Prefetch, Prewarm, Manual
}

/// Response from job completion
pub struct JobResponse {
    pub data: Vec<u8>,
    pub cache_hit: bool,
    pub duration: Duration,
}
```

**Producer Trait**:

```rust
/// Trait for submitting job requests to the executor
pub trait JobSubmitter: Send + Sync + 'static {
    fn submit(&self, request: JobRequest) -> Result<(), JobSubmitError>;
    fn submit_with_response(&self, tile: TileCoord, priority: Priority, ...)
        -> oneshot::Receiver<JobResponse>;
    fn is_connected(&self) -> bool;
}

/// Implementation wraps channel sender
pub struct ChannelJobSubmitter {
    tx: mpsc::Sender<JobRequest>,
}
```

**SOLID Analysis**:

| Principle | How It's Satisfied |
|-----------|-------------------|
| **Single Responsibility** | FUSE handles filesystem, Executor handles jobs, Channel handles transport |
| **Open/Closed** | New producers (e.g., CLI commands) can submit without modifying executor |
| **Liskov Substitution** | Any `JobSubmitter` implementation is interchangeable |
| **Interface Segregation** | Producers only know `JobSubmitter` trait, not executor internals |
| **Dependency Inversion** | All components depend on abstractions (traits, channels), not concrete types |

**Rationale**:

1. **Maximum decoupling** - Producers don't know how jobs are executed, executor doesn't know who's asking
2. **Testable in isolation** - Mock the trait for FUSE tests, no executor needed
3. **Independent lifecycles** - Each daemon can be started/stopped independently
4. **Natural backpressure** - Bounded channel prevents overwhelming the executor
5. **Clean shutdown** - CLI coordinates graceful shutdown of all daemons

**Comparison with Original Design**:

| Aspect | Original (Embedded) | New (Peer Daemons) |
|--------|---------------------|-------------------|
| FUSE-Executor relationship | FUSE owns `DdsHandler` function | FUSE holds `JobSubmitter` client |
| Coupling | FUSE embeds pipeline logic | FUSE only knows request/response |
| Testing | Need executor for FUSE tests | Mock `JobSubmitter` trait |
| Lifecycle | Tied together | Independent |
| Adding producers | Modify DdsHandler signature | Clone channel sender |

**Files to Create (Phase 4)**:

```
xearthlayer/src/
├── executor/
│   ├── daemon.rs     # JobExecutorDaemon - receives from channel, runs jobs
│   └── client.rs     # JobSubmitter trait + ChannelJobSubmitter
│
└── runtime/
    ├── mod.rs        # Module exports
    ├── runtime.rs    # XEarthLayerRuntime - owns channel & daemons
    └── request.rs    # JobRequest, JobResponse, RequestOrigin
```

## References

- [Tile-Based Prefetch Design](tile-based-prefetch-design.md) - Consumer of this framework
- [Async Pipeline Architecture](async-pipeline-architecture.md) - Current pipeline (to be migrated)
- [Predictive Caching](predictive-caching.md) - Prefetcher integration
