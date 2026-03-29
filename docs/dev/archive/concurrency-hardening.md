# Concurrency Hardening Specification

**Status**: Complete
**Priority**: Critical
**Estimated Effort**: ~12 hours
**Branch**: `feature/pre-caching`
**Completed**: 2025-12-23

## Overview

This document captures findings from a comprehensive concurrency review of XEarthLayer and proposes hardening measures to improve robustness under heavy load. The review was conducted as part of the `feature/pre-caching` branch work.

**Update (2025-12-23)**: During testing, deadlock situations were observed with jobs stuck across all pipeline stages:
- 55 download jobs stuck
- 12 assemble jobs stuck
- 5 encode jobs stuck
- 32 jobs failed
- Sim crashed

This prompted escalation from "Medium" to "Critical" priority and addition of H-005/H-006 measures.

## Current Concurrency Architecture

XEarthLayer uses a multi-layered concurrency control system designed to prevent resource exhaustion:

### Concurrency Limiters

| Component | Mechanism | Limit Formula | Location |
|-----------|-----------|---------------|----------|
| HTTP Requests | `HttpConcurrencyLimiter` | `min(num_cpus * 16, 256)` | `pipeline/http_limiter.rs` |
| CPU Work | Shared `CPUConcurrencyLimiter` | `num_cpus * 1.25` | `pipeline/runner.rs` |
| Disk I/O | Profile-based `StorageConcurrencyLimiter` | NVMe: 128, SSD: 64, HDD: 4 | `pipeline/adapters/disk_cache_parallel.rs` |
| Prefetch Jobs | `Semaphore` | `max(num_cpus / 4, 2)` | `pipeline/runner.rs` |
| FUSE Disk I/O | `StorageConcurrencyLimiter` | `min(num_cpus * 16, 256)` | `fuse/fuse3/filesystem.rs` |
| **Job-Level** | **None (GAP)** | **Unlimited** | **N/A** |

### Key Design Patterns

1. **Owned Semaphore Permits**: All limiters use `OwnedSemaphorePermit` held through job lifetime, preventing resource starvation.

2. **Cancellation Token Propagation**: `CancellationToken` flows through entire pipeline, enabling clean abort on timeout.

3. **Request Coalescing**: `DashMap` with broadcast channels prevents duplicate processing of same tile.

4. **Validation Before Return**: DDS data is validated before returning to X-Plane, preventing simulator crashes from malformed data.

### Pipeline Flow

```
FUSE Read Request
    │
    ├─▶ Request Coalescer (DashMap + broadcast)
    │       │
    │       ├─▶ [Prefetch] try_acquire_owned on prefetch semaphore
    │       │       │
    │       │       └─▶ Fails? Skip silently (non-blocking)
    │       │
    │       ▼
    │   Memory Cache Check
    │       │
    │       ├─▶ Hit? Return immediately
    │       │
    │       ▼
    │   Download Stage
    │       │
    │       ├─▶ HTTP limiter.acquire() per chunk (256 chunks)
    │       ├─▶ Disk cache check (disk_io_limiter.acquire())
    │       └─▶ HTTP request with timeout
    │       │
    │       ▼
    │   Assembly Stage
    │       │
    │       └─▶ cpu_limiter.acquire() → spawn_blocking
    │       │
    │       ▼
    │   Encode Stage
    │       │
    │       └─▶ cpu_limiter.acquire() → spawn_blocking
    │       │
    │       ▼
    │   Cache Stage
    │       │
    │       └─▶ Memory cache put + disk cache put
    │
    └─▶ Return to FUSE (validated)
```

## Review Findings

### Strengths (No Changes Needed)

1. **HTTP Concurrency**: Well-bounded with proper permit lifecycle.

2. **CPU Concurrency**: Shared limiter between assemble/encode solves previous deadlock (documented in `async-pipeline-architecture.md`).

3. **Disk I/O Profiles**: Auto-detection of storage type with appropriate limits.

4. **FUSE Timeouts**: 30-second generation timeout with cancellation propagation.

5. **Request Coalescing**: Lock-free DashMap prevents contention.

6. **Prefetch Limiting**: Fixed in this PR - uses `try_acquire_owned()` to hold permit through job lifetime.

### Critical Issues Identified

#### Issue 1: No Job-Level Concurrency Limit (CRITICAL)

**Risk Level**: Critical

**Description**: There is no limit on how many tiles can start processing simultaneously. When X-Plane loads scenery, it can request 50+ tiles at once. Each tile spawns 256 chunk downloads, overwhelming the HTTP limiter (256 permits) with potentially thousands of waiting tasks.

**Evidence**: Observed deadlock with 55 download jobs, 12 assemble jobs, 5 encode jobs stuck simultaneously.

**Root Cause**: Without job-level limiting, all tiles start their download stages concurrently, each acquiring HTTP permits. With 50 tiles × 256 chunks = 12,800 download tasks competing for 256 HTTP permits, most tasks wait indefinitely while holding other resources.

**Fix**: H-005 - Add job-level semaphore (`num_cpus × 2` default).

#### Issue 2: No Stall Detection or Recovery

**Risk Level**: High

**Description**: No mechanism exists to detect if jobs are stalled or to recover from stuck states. Jobs that acquire permits and stall hold those permits indefinitely.

**Evidence**: System remained stuck until manually killed, with no automatic recovery.

**Fix**: H-003 (Health Check) + H-006 (Pipeline Control Plane with active recovery).

#### Issue 3: Blocking Thread Pool Exhaustion

**Risk Level**: Medium

**Description**: The default Tokio blocking thread pool (512 threads) is shared between:
- Disk cache reads (`spawn_blocking`)
- Disk cache writes (`spawn_blocking`)
- CPU-bound assembly (`spawn_blocking`)
- CPU-bound encoding (`spawn_blocking`)

While semaphores limit concurrent operations, under sustained heavy load the pool could become contended, leading to increased latency.

**Evidence**: None observed in production, but theoretical concern based on architecture review.

**Mitigation**: H-001 - Explicit pool sizing coordinated with disk I/O profile.

#### Issue 4: Unbounded Semaphore Wait Time

**Risk Level**: Low

**Description**: Semaphore acquisitions do not have explicit timeouts. If a job acquires a permit and stalls (e.g., unexpectedly long network operation), other jobs wait indefinitely.

**Evidence**: Not observed due to existing HTTP/FUSE timeouts, but represents a gap in defense-in-depth.

**Mitigation**: H-002 - Add timeout on semaphore acquisitions.

### End-User Log Analysis

Analyzed logs from user experiencing crashes:

**First Load Log**:
- Normal operation: 08:51:58 to 08:53:34 (2 minutes)
- Gap: 08:53:34 → 09:07:52 (14 minutes, no DDS processing)
- After gap: Only FUSE_FORGET operations (cleanup)

**Restart Log**:
- Normal operation with 97% cache hits
- Gap: 09:11:59 → 09:48:29 (36 minutes)
- Activity resumes normally at 09:48:29

**Conclusion**: These gaps are NOT XEL deadlocks. If XEL were deadlocked, logging would stop permanently. The logs resume normally, suggesting:
1. X-Plane pauses/reduces DDS requests during certain flight phases
2. User's crashes likely stem from memory pressure (2GB XEL cache + X-Plane) or X-Plane bugs
3. "Killed XEL" report may be Linux OOM killer

---

## Proposed Hardening Measures

### H-001: Explicit Blocking Thread Pool Configuration

**Priority**: Medium
**Effort**: Low (~1 hour)
**Status**: Complete

Configure the Tokio runtime's blocking thread pool size based on system resources:

```rust
// In run.rs or service initialization
let storage_profile = detect_storage_profile(&cache_dir);
let blocking_threads = match storage_profile {
    DiskIoProfile::Nvme => num_cpus * 4,
    DiskIoProfile::Ssd => num_cpus * 2,
    DiskIoProfile::Hdd => num_cpus,
    DiskIoProfile::Auto => num_cpus * 2, // Default to SSD-like
};

let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(num_cpus)
    .max_blocking_threads(blocking_threads)
    .enable_all()
    .build()?;
```

**Rationale**: Provides explicit control over blocking thread pool, preventing unbounded growth and coordinating with disk I/O profile.

### H-002: Timeout on Semaphore Acquisitions

**Priority**: Medium
**Effort**: Low (~30 mins)
**Status**: Complete

Add timeout variant to `StorageConcurrencyLimiter`:

```rust
impl StorageConcurrencyLimiter {
    pub async fn acquire_timeout(&self, timeout: Duration)
        -> Result<StoragePermit<'_>, AcquireTimeoutError>;
}
```

Usage in pipeline stages:
```rust
match tokio::time::timeout(
    Duration::from_secs(30),
    cpu_limiter.acquire()
).await {
    Ok(permit) => {
        // Proceed with operation
    }
    Err(_) => {
        warn!(
            job_id = %job_id,
            "CPU limiter acquisition timed out - potential stall detected"
        );
        return Err(PipelineError::ResourceTimeout);
    }
}
```

**Rationale**: Safety net for unexpected stalls. Matches FUSE timeout (30s).

### H-003: Health Check / Stall Detection

**Priority**: High
**Effort**: Medium (~1.5 hours)
**Status**: Complete

Integrated into Pipeline Control Plane (H-006). Periodic health check that monitors system state:

```rust
pub struct ControlPlaneHealth {
    last_successful_job: AtomicU64,  // timestamp
    jobs_in_progress: AtomicUsize,
    jobs_recovered: AtomicU64,
    peak_concurrent_jobs: AtomicUsize,
    health_status: AtomicU8,  // Healthy=0, Degraded=1, Recovering=2, Critical=3
}
```

Health check runs every 5 seconds, logs diagnostics when stalls detected.

### H-004: Memory Pressure Monitoring

**Priority**: Low
**Effort**: Medium (~3 hours)
**Status**: Deferred (post-MVP)

Monitor system memory and adjust cache behavior under pressure. Deferred as it requires adding `sysinfo` dependency.

### H-005: Job-Level Concurrency Limiter (NEW)

**Priority**: Critical
**Effort**: Medium (~2 hours)
**Status**: Complete

Add a semaphore that limits how many tiles can be processing simultaneously:

```rust
// In PipelineControlPlane
job_limiter: Arc<Semaphore>,  // Capacity: num_cpus × 2

// Acquisition behavior:
// - On-demand requests: block until slot available
// - Prefetch requests: try_acquire, skip if unavailable
```

**Default**: `num_cpus × 2` (configurable via `control_plane.max_concurrent_jobs`)

**Rationale**: Prevents unbounded tile starts that overwhelm downstream limiters. With 8 CPUs, limits to 16 concurrent tiles = 4,096 max concurrent chunk downloads (well under HTTP limiter capacity).

### H-006: Pipeline Control Plane (NEW)

**Priority**: Critical
**Effort**: High (~6 hours)
**Status**: Complete

Central control system that provides job tracking, health monitoring, and active recovery:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Pipeline Control Plane                        │
├─────────────────────────────────────────────────────────────────┤
│  Job Registry          │  Health Monitor    │  Recovery Engine  │
│  ────────────────      │  ──────────────    │  ───────────────  │
│  • DashMap tracking    │  • 5s check interval│  • Cancel token  │
│  • Stage progression   │  • Stall detection │  • Coalescer cleanup│
│  • Start timestamps    │  • Health status   │  • Return placeholder│
└─────────────────────────────────────────────────────────────────┘
                              │
           ┌──────────────────┼──────────────────┐
           ▼                  ▼                  ▼
    ┌────────────┐     ┌────────────┐     ┌────────────┐
    │Job Limiter │     │HTTP Limiter│     │CPU Limiter │
    │ (H-005)    │     │ (existing) │     │ (existing) │
    │ cpus × 2   │     │ cpus × 16  │     │ cpus × 1.25│
    └────────────┘     └────────────┘     └────────────┘
```

**Components**:

1. **Job Registry**: Tracks all active jobs with DashMap
   - Job ID, tile coordinates, start time
   - Current stage (Queued, Downloading, Assembling, Encoding, Caching, Completed, Recovered)
   - Cancellation token for each job
   - Prefetch vs on-demand flag

2. **Health Monitor**: Periodic health checks
   - 5-second interval
   - Detects jobs exceeding stall threshold (60s default)
   - Updates health status atomics

3. **Recovery Engine**: Active recovery of stuck jobs
   - Cancels stalled job's CancellationToken
   - Removes from coalescer (notifies waiters)
   - Waiters receive error, return placeholder to X-Plane
   - Logs recovery action

**Design Decision**: Control plane is **always enabled** (not optional). The overhead is minimal (atomic operations) and the safety benefits are critical.

---

## Implementation Plan

### Phase 1: Core Data Structures (COMPLETE)
**Files created**:
- [x] `xearthlayer/src/pipeline/control_plane/mod.rs` - Module exports
- [x] `xearthlayer/src/pipeline/control_plane/job_registry.rs` - Job tracking with DashMap (11 tests)
- [x] `xearthlayer/src/pipeline/control_plane/health.rs` - Health metrics with atomics (8 tests)

**Key types**:
```rust
pub enum JobStage {
    Queued,
    Downloading,
    Assembling,
    Encoding,
    Caching,
    Completed,
    Recovered,
}

pub struct JobEntry {
    job_id: JobId,
    tile: TileCoord,
    started_at: Instant,
    stage: AtomicU8,
    cancellation_token: CancellationToken,
    is_prefetch: bool,
}

pub struct JobRegistry {
    jobs: DashMap<JobId, Arc<JobEntry>>,
    jobs_by_tile: DashMap<TileCoord, JobId>,
    total_jobs: AtomicU64,
    completed_jobs: AtomicU64,
    recovered_jobs: AtomicU64,
}
```

### Phase 2: Control Plane Service (COMPLETE)
**Files created**:
- [x] `xearthlayer/src/pipeline/control_plane/service.rs` - Control plane service (10 tests)

**Key API**:
```rust
pub struct PipelineControlPlane {
    config: ControlPlaneConfig,
    registry: Arc<JobRegistry>,
    job_limiter: Arc<Semaphore>,  // H-005
    health: Arc<ControlPlaneHealth>,
    coalescer: Arc<RequestCoalescer>,
    shutdown: CancellationToken,
}

impl PipelineControlPlane {
    pub async fn acquire_job_slot(&self, is_prefetch: bool) -> Result<OwnedSemaphorePermit, JobSlotError>;
    pub fn register_job(&self, job_id, tile, token, is_prefetch) -> Arc<JobEntry>;
    pub fn update_stage(&self, job_id: JobId, stage: JobStage);
    pub fn complete_job(&self, job_id: JobId);
    pub fn start_health_monitor(self: Arc<Self>) -> JoinHandle<()>;
}
```

### Phase 3: Semaphore Timeout (H-002) (COMPLETE)
**Files modified**:
- [x] `xearthlayer/src/pipeline/concurrency_limiter.rs` - Added `acquire_timeout()` method and `AcquireTimeoutError` type (3 new tests)

### Phase 4: Pipeline Integration (COMPLETE)
**Files modified**:
- [x] `xearthlayer/src/pipeline/runner.rs` - Added `create_dds_handler_with_control_plane()` function
- [x] `xearthlayer/src/pipeline/processor.rs` - Added `process_tile_with_observer()` accepting `StageObserver`
- [x] `xearthlayer/src/pipeline/mod.rs` - Export control_plane module and StageObserver trait

**Design Decision - Stage Observer Pattern**:
Instead of having the processor directly call control plane methods, we introduced a `StageObserver` trait:

```rust
pub trait StageObserver: Send + Sync {
    fn on_stage_change(&self, job_id: JobId, stage: JobStage);
}
```

This allows workers to emit stage events without knowing the listener, maintaining clean separation of concerns. The control plane implements `StageObserver` and tracks stage progression.

**Integration via submit() API**:
```rust
control_plane.submit(job_id, tile, is_prefetch, token, |observer| async {
    process_tile_with_observer(job_id, tile, provider, encoder, ..., observer).await
}).await
```

### Phase 5: Configuration (COMPLETE)
**Files modified**:
- [x] `xearthlayer/src/config/file.rs` - Added `ControlPlaneSettings` struct
- [x] `xearthlayer/src/config/keys.rs` - Added ConfigKey variants for control plane settings
- [x] `xearthlayer/src/config/mod.rs` - Export new types and default functions
- [x] `xearthlayer/src/service/config.rs` - Added `ControlPlaneSettings` to `ServiceConfig`

**New config section**:
```ini
[control_plane]
max_concurrent_jobs = 16      # default: num_cpus × 2
stall_threshold_secs = 60     # default: 60
health_check_interval_secs = 5 # default: 5
semaphore_timeout_secs = 30   # default: 30
```

### Phase 6: Metrics & Dashboard (COMPLETE)
**Files modified**:
- [x] `xearthlayer-cli/src/ui/widgets/control_plane.rs` - New control plane widget
- [x] `xearthlayer-cli/src/ui/widgets/mod.rs` - Export ControlPlaneWidget
- [x] `xearthlayer-cli/src/ui/dashboard.rs` - Added control plane health display

**Metrics tracked in ControlPlaneHealth**:
- `jobs_in_progress: AtomicUsize`
- `peak_concurrent_jobs: AtomicUsize`
- `jobs_recovered: AtomicU64`
- `jobs_rejected_capacity: AtomicU64`
- `semaphore_timeouts: AtomicU64`
- `health_status: AtomicU8` (Healthy, Degraded, Recovering, Critical)

### Phase 7: Blocking Thread Pool (H-001) (COMPLETE)
**Files modified**:
- [x] `xearthlayer/src/pipeline/storage.rs` - Added named constants and `max_blocking_threads()` method
- [x] `xearthlayer/src/service/facade.rs` - Added `with_disk_profile()` constructor

**Design Decision**:
Instead of configuring the Tokio runtime in the CLI's run command, blocking thread pool sizing is coordinated with the disk I/O profile in the service layer. Added constants:

```rust
// Blocking thread pool parameters by storage profile
pub const HDD_BLOCKING_SCALING_FACTOR: usize = 2;
pub const HDD_BLOCKING_CEILING: usize = 16;
pub const SSD_BLOCKING_SCALING_FACTOR: usize = 4;
pub const SSD_BLOCKING_CEILING: usize = 64;
pub const NVME_BLOCKING_SCALING_FACTOR: usize = 8;
pub const NVME_BLOCKING_CEILING: usize = 128;
```

### Phase 8: Service Integration (COMPLETE)
**Files modified**:
- [x] `xearthlayer/src/service/facade.rs` - Control plane created and health monitor started
- [x] `xearthlayer/src/service/dds_handler.rs` - Added `with_control_plane()` builder method
- [x] `xearthlayer-cli/src/commands/run.rs` - Wired dashboard with control plane health

**Integration**:
- Control plane is created in `XEarthLayerService::build()` using config settings
- Health monitor spawned automatically as background task
- `control_plane_health()` and `max_concurrent_jobs()` accessors added for dashboard
- DdsHandlerBuilder accepts optional control plane via `with_control_plane()`

### Phase 9: Testing (COMPLETE)
**35 unit tests covering**:
- [x] JobRegistry operations (register, get, complete, find_stalled)
- [x] ControlPlaneHealth state transitions and atomic operations
- [x] Control plane service lifecycle (creation, shutdown)
- [x] Job slot acquisition (on-demand blocking, prefetch non-blocking)
- [x] Stall detection and recovery
- [x] Submit API variations (success, failure, no capacity, shutdown, cancelled)
- [x] Stage observer pattern
- [x] Health monitor lifecycle

---

## Default Configuration Values

| Setting | Default | Rationale |
|---------|---------|-----------|
| `max_concurrent_jobs` | `num_cpus × 2` | Balance throughput vs resource contention |
| `stall_threshold_secs` | `60` | Longer than FUSE timeout (30s) to allow retries |
| `health_check_interval_secs` | `5` | Responsive without overhead |
| `semaphore_timeout_secs` | `30` | Match FUSE timeout |
| `blocking_threads` | `num_cpus × 2` (SSD default) | Coordinated with disk I/O profile |

---

## Files Summary

### Files Created

| File | Purpose |
|------|---------|
| `xearthlayer/src/pipeline/control_plane/mod.rs` | Module exports + StageObserver trait |
| `xearthlayer/src/pipeline/control_plane/job_registry.rs` | Job tracking with DashMap (11 tests) |
| `xearthlayer/src/pipeline/control_plane/health.rs` | Health metrics with atomics (9 tests) |
| `xearthlayer/src/pipeline/control_plane/service.rs` | Control plane service (15 tests) |
| `xearthlayer-cli/src/ui/widgets/control_plane.rs` | Control plane dashboard widget |

### Files Modified

| File | Changes |
|------|---------|
| `xearthlayer/src/pipeline/mod.rs` | Export control_plane module |
| `xearthlayer/src/pipeline/runner.rs` | Added `create_dds_handler_with_control_plane()` |
| `xearthlayer/src/pipeline/processor.rs` | Added `process_tile_with_observer()` |
| `xearthlayer/src/pipeline/concurrency_limiter.rs` | Added `acquire_timeout()` method |
| `xearthlayer/src/pipeline/storage.rs` | Added blocking thread constants and methods |
| `xearthlayer/src/config/file.rs` | Added ControlPlaneSettings |
| `xearthlayer/src/config/keys.rs` | Added ConfigKey variants for control plane |
| `xearthlayer/src/config/mod.rs` | Export new types and default functions |
| `xearthlayer/src/service/config.rs` | Added ControlPlaneSettings to ServiceConfig |
| `xearthlayer/src/service/facade.rs` | Wire control plane, add health accessor |
| `xearthlayer/src/service/dds_handler.rs` | Added `with_control_plane()` builder method |
| `xearthlayer-cli/src/ui/dashboard.rs` | Added control plane health display |
| `xearthlayer-cli/src/ui/widgets/mod.rs` | Export ControlPlaneWidget |
| `xearthlayer-cli/src/commands/run.rs` | Wire dashboard with control plane health |
| `docs/configuration.md` | Document new settings |

---

## Success Criteria

1. No more deadlock situations under heavy load
2. X-Plane always receives valid DDS data (actual or placeholder)
3. Stalled jobs are detected and recovered automatically
4. Dashboard shows control plane health status
5. All existing tests pass
6. New unit tests for control plane components

---

## Testing Strategy

1. **Load Testing**: Simulate heavy concurrent tile requests with multiple packages mounted
2. **Memory Stress**: Run with reduced memory cache to trigger eviction
3. **Network Failure**: Simulate provider timeouts and failures
4. **Long-Duration**: 2+ hour flight sessions monitoring for degradation
5. **Stall Recovery**: Artificially stall jobs and verify recovery

---

## References

- `docs/dev/async-pipeline-architecture.md` - Pipeline design and deadlock fixes
- `docs/dev/predictive-caching.md` - Prefetch system design
- `xearthlayer/src/pipeline/storage_limiter.rs` - StorageConcurrencyLimiter (disk I/O)
- `xearthlayer/src/pipeline/cpu_limiter.rs` - CPUConcurrencyLimiter (CPU-bound work)
- `xearthlayer/src/pipeline/runner.rs` - DDS handler with coalescing
- `xearthlayer/src/pipeline/coalesce.rs` - RequestCoalescer with cancel() method

---

## Changelog

| Date | Author | Changes |
|------|--------|---------|
| 2025-12-23 | Claude | Initial specification from concurrency review |
| 2025-12-23 | Claude | Escalated to Critical after deadlock observed in testing |
| 2025-12-23 | Claude | Added H-005 (Job-Level Limiter) and H-006 (Pipeline Control Plane) |
| 2025-12-23 | Claude | Added detailed implementation plan with 9 phases |
| 2025-12-23 | Claude | Completed all 9 phases, marked spec as Complete |
| 2025-12-23 | Claude | Added design decisions: StageObserver pattern, submit() API, blocking pool integration |
