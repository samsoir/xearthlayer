# Concurrency Hardening Specification

**Status**: Proposed
**Priority**: Medium
**Estimated Effort**: 1-2 days

## Overview

This document captures findings from a comprehensive concurrency review of XEarthLayer and proposes hardening measures to improve robustness under heavy load. The review was conducted as part of the `feature/pre-caching` branch work.

## Current Concurrency Architecture

XEarthLayer uses a multi-layered concurrency control system designed to prevent resource exhaustion:

### Concurrency Limiters

| Component | Mechanism | Limit Formula | Location |
|-----------|-----------|---------------|----------|
| HTTP Requests | `HttpConcurrencyLimiter` | `min(num_cpus * 16, 256)` | `pipeline/http_limiter.rs` |
| CPU Work | Shared `ConcurrencyLimiter` | `num_cpus * 1.25` | `pipeline/runner.rs` |
| Disk I/O | Profile-based `ConcurrencyLimiter` | NVMe: 128, SSD: 64, HDD: 4 | `pipeline/adapters/disk_cache_parallel.rs` |
| Prefetch Jobs | `Semaphore` | `max(num_cpus / 4, 2)` | `pipeline/runner.rs` |
| FUSE Disk I/O | `ConcurrencyLimiter` | `min(num_cpus * 16, 256)` | `fuse/fuse3/filesystem.rs` |

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

### Potential Issues Identified

#### Issue 1: Blocking Thread Pool Exhaustion

**Risk Level**: Medium

**Description**: The default Tokio blocking thread pool (512 threads) is shared between:
- Disk cache reads (`spawn_blocking`)
- Disk cache writes (`spawn_blocking`)
- CPU-bound assembly (`spawn_blocking`)
- CPU-bound encoding (`spawn_blocking`)

While semaphores limit concurrent operations, under sustained heavy load the pool could become contended, leading to increased latency.

**Evidence**: None observed in production, but theoretical concern based on architecture review.

**Mitigation**: Existing semaphores provide protection, but explicit pool sizing would add defense-in-depth.

#### Issue 2: Unbounded Semaphore Wait Time

**Risk Level**: Low

**Description**: Semaphore acquisitions do not have explicit timeouts. If a job acquires a permit and stalls (e.g., unexpectedly long network operation), other jobs wait indefinitely.

**Evidence**: Not observed due to existing HTTP/FUSE timeouts, but represents a gap in defense-in-depth.

**Mitigation**: Existing cancellation tokens and timeouts provide protection at other layers.

#### Issue 3: No Stall Detection

**Risk Level**: Low

**Description**: No mechanism to detect if the system is stalled without examining logs. A health check would aid debugging and operational monitoring.

**Evidence**: End-user logs showed 14-36 minute gaps with no DDS processing (likely X-Plane pausing requests, not XEL stall).

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

## Proposed Hardening Measures

### H-001: Explicit Blocking Thread Pool Configuration

**Priority**: Medium
**Effort**: Low (~2 hours)

Configure the Tokio runtime's blocking thread pool size based on system resources:

```rust
// In run.rs or service initialization
let storage_profile = detect_storage_profile(&cache_dir);
let blocking_threads = match storage_profile {
    StorageProfile::Nvme => num_cpus * 4,
    StorageProfile::Ssd => num_cpus * 2,
    StorageProfile::Hdd => num_cpus,
};

let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(config.worker_threads)
    .max_blocking_threads(blocking_threads)
    .enable_all()
    .build()?;
```

**Rationale**: Provides explicit control over blocking thread pool, preventing unbounded growth and coordinating with disk I/O profile.

### H-002: Timeout on Semaphore Acquisitions

**Priority**: Low
**Effort**: Low (~1 hour)

Add optional timeout on CPU limiter acquisition as defense-in-depth:

```rust
// In runner.rs, during assembly/encode stages
match tokio::time::timeout(
    Duration::from_secs(60),
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

**Rationale**: Safety net for unexpected stalls. Existing timeouts at HTTP/FUSE layer should prevent this, but adds defense-in-depth.

### H-003: Health Check / Stall Detection

**Priority**: Medium
**Effort**: Medium (~4 hours)

Add periodic health check that monitors system state:

```rust
pub struct HealthMonitor {
    last_successful_dds: AtomicInstant,
    stall_threshold: Duration,
}

impl HealthMonitor {
    pub async fn run(&self, cancellation_token: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => break,
                _ = interval.tick() => {
                    let elapsed = self.last_successful_dds.elapsed();
                    if elapsed > self.stall_threshold {
                        warn!(
                            elapsed_secs = elapsed.as_secs(),
                            "No successful DDS completions for {} seconds",
                            elapsed.as_secs()
                        );

                        // Log current system state
                        self.log_diagnostics();
                    }
                }
            }
        }
    }

    fn log_diagnostics(&self) {
        // Log semaphore permit availability
        // Log in-flight request count
        // Log memory cache stats
    }
}
```

**Rationale**: Aids debugging by detecting and logging stall conditions. Does not prevent issues but provides visibility.

### H-004: Memory Pressure Monitoring

**Priority**: Low
**Effort**: Medium (~3 hours)

Monitor system memory and adjust cache behavior under pressure:

```rust
pub fn check_memory_pressure() -> MemoryPressure {
    let sys = sysinfo::System::new_all();
    let available = sys.available_memory();
    let total = sys.total_memory();
    let available_pct = (available as f64 / total as f64) * 100.0;

    if available_pct < 10.0 {
        MemoryPressure::Critical
    } else if available_pct < 20.0 {
        MemoryPressure::High
    } else {
        MemoryPressure::Normal
    }
}
```

**Rationale**: End-user crashes may be caused by OOM killer. Proactive monitoring could trigger cache eviction or reduced prefetch activity.

## Implementation Plan

### Phase 1: Documentation (Complete)
- [x] Document current architecture
- [x] Analyze end-user logs
- [x] Identify potential issues
- [x] Propose hardening measures

### Phase 2: Quick Wins (Future PR)
- [ ] H-001: Explicit blocking thread pool configuration
- [ ] H-002: Timeout on semaphore acquisitions

### Phase 3: Monitoring (Future PR)
- [ ] H-003: Health check / stall detection
- [ ] H-004: Memory pressure monitoring

## Testing Strategy

1. **Load Testing**: Simulate heavy concurrent tile requests with multiple packages mounted
2. **Memory Stress**: Run with reduced memory cache to trigger eviction
3. **Network Failure**: Simulate provider timeouts and failures
4. **Long-Duration**: 2+ hour flight sessions monitoring for degradation

## References

- `docs/dev/async-pipeline-architecture.md` - Pipeline design and deadlock fixes
- `docs/dev/predictive-caching.md` - Prefetch system design
- `xearthlayer/src/pipeline/concurrency_limiter.rs` - ConcurrencyLimiter implementation
- `xearthlayer/src/pipeline/runner.rs` - DDS handler with coalescing

---

## Changelog

| Date | Author | Changes |
|------|--------|---------|
| 2025-12-23 | Claude | Initial specification from concurrency review |
