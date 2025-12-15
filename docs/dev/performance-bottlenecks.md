# Performance Bottlenecks and Optimization Strategy

**Status**: Resolved - See [Async Pipeline Architecture](./async-pipeline-architecture.md) for implementation details
**Created**: 2025-12-07
**Last Updated**: 2025-12-14

## Executive Summary

This document captured the original performance analysis that led to the async pipeline architecture redesign. The issues identified here have been resolved through:

- **Phase 3.1**: Async HTTP with request coalescing (fixed thread pool exhaustion)
- **Phase 3.2**: HTTP concurrency limiting (fixed network stack exhaustion)

### Original Issues (Now Resolved)

- **Observed**: 4-6 MB/s average network throughput, 11 MB/s peak
- **Expected**: Should saturate available bandwidth (50+ MB/s on typical connections)
- **CPU utilization**: Far below capacity during load
- **Critical issue**: Network activity stops completely after ~20 minutes (suspected deadlock)

### Resolution

The async pipeline architecture with HTTP concurrency limiting now:
- Handles X-Plane's concurrent requests efficiently
- Prevents network stack exhaustion through semaphore-based rate limiting
- Automatically tunes concurrency based on CPU count: `min(cpus * 16, 256)`
- Successfully loads scenery in X-Plane without crashes

This document is preserved for historical reference. See [Async Pipeline Architecture](./async-pipeline-architecture.md) for the current implementation.

---

## System Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              X-PLANE APPLICATION                                 │
│                         (Multiple concurrent DDS requests)                       │
└────────────────────────────────────────┬────────────────────────────────────────┘
                                         │ read() syscalls (many concurrent)
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         FUSE LAYER: PassthroughEF                                │
│                                                                                  │
│  Thread Model: SINGLE-THREADED event loop (fuser library)                       │
│  Operations: lookup(), getattr(), read(), readdir()                             │
│                                                                                  │
│  For virtual DDS files:                                                          │
│    read() → generate_dds() → TileGenerator::generate() → BLOCKS                 │
│                                                                                  │
│  Synchronization:                                                                │
│    - Mutex<HashMap> for inode mappings (brief locks, <1ms)                      │
│    - Mutex<HashMap> for virtual DDS metadata (brief locks, <1ms)                │
└────────────────────────────────────────┬────────────────────────────────────────┘
                                         │
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                    TILE GENERATION: ParallelTileGenerator                        │
│                                                                                  │
│  Thread Model: N worker threads (default: num_cpus)                             │
│  Features: Request coalescing, timeout handling, placeholder fallback           │
│                                                                                  │
│  Synchronization:                                                                │
│    - Mutex<HashMap<TileRequest, InFlight>> for request coalescing               │
│    - Mutex<Receiver<WorkItem>> shared by all workers (contention point)         │
│    - mpsc::channel for work queue (unbounded)                                   │
│    - mpsc::channel per request for results                                      │
│                                                                                  │
│  Flow:                                                                           │
│    1. FUSE thread submits work to channel                                       │
│    2. FUSE thread blocks on recv_timeout(10s) waiting for result                │
│    3. Worker picks up work, calls inner generator                               │
│    4. Worker sends result back through channel                                  │
└────────────────────────────────────────┬────────────────────────────────────────┘
                                         │
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                    DOWNLOAD: TileOrchestrator                                    │
│                                                                                  │
│  Thread Model: ThreadPool(32) created per tile download                         │
│  Task: Download 256 chunks (16×16) and assemble into 4096×4096 image           │
│                                                                                  │
│  Synchronization:                                                                │
│    - mpsc::channel for chunk results                                            │
│    - ThreadPool internal work queue                                             │
│                                                                                  │
│  Flow:                                                                           │
│    1. Submit all 256 chunks to thread pool                                      │
│    2. Each chunk: HTTP GET with retries (up to 3)                              │
│    3. Worker blocks on `for result in rx` until ALL chunks complete            │
│    4. Assemble image from successful chunks (requires 80% success)             │
└────────────────────────────────────────┬────────────────────────────────────────┘
                                         │
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                    ENCODING: DdsTextureEncoder                                   │
│                                                                                  │
│  Thread Model: Runs on tile worker thread (blocking)                            │
│  Task: BC1/BC3 compress 4096×4096 image + generate 5 mipmap levels             │
│  Duration: ~0.2-1.0 seconds per tile                                            │
└────────────────────────────────────────┬────────────────────────────────────────┘
                                         │
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                    CACHE: CacheSystem (Two-Tier)                                 │
│                                                                                  │
│  Memory Cache (2GB default):                                                     │
│    - Mutex<HashMap> for entries                                                  │
│    - Mutex for size tracking                                                     │
│    - Mutex for statistics                                                        │
│    - LRU eviction under lock (O(n log n) sort)                                  │
│                                                                                  │
│  Disk Cache (20GB default):                                                      │
│    - Mutex for index                                                             │
│    - Blocking file I/O for reads/writes                                         │
│    - Background daemon for eviction                                              │
└─────────────────────────────────────────────────────────────────────────────────┘
```

---

## Identified Bottlenecks

### Bottleneck #1: FUSE Single-Threaded Event Loop

**Location**: `xearthlayer/src/fuse/passthrough_ef.rs`

**Problem**: The `fuser` library dispatches all filesystem operations to a single thread. When `read()` is called for a virtual DDS file, the FUSE thread blocks waiting for tile generation (up to 10s timeout). During this time, no other filesystem operations can be processed.

**Impact**:
- X-Plane requests multiple tiles concurrently, but only one is processed at a time
- Scene loading is serialized despite having N worker threads available
- Effective parallelism is 1, not N

**Evidence**: CPU utilization remains low during initial load despite available worker threads.

**Severity**: HIGH - This is likely the primary throughput limiter.

---

### Bottleneck #2: Mutex-Protected Work Receiver

**Location**: `xearthlayer/src/tile/parallel.rs:209`

**Problem**: All N worker threads share a single `Mutex<Receiver<WorkItem>>`. Workers must acquire this lock to receive work, creating contention.

```rust
let work = {
    let receiver = work_receiver.lock().unwrap();  // Lock contention here
    receiver.recv_timeout(Duration::from_millis(100))
};
```

**Impact**:
- Lock contention when multiple workers try to get work simultaneously
- 100ms timeout means workers poll frequently, increasing contention
- With N workers, worst case is N-1 workers waiting for lock

**Severity**: MEDIUM - Contributes to inefficiency but not the primary issue.

---

### Bottleneck #3: ThreadPool Created Per Tile

**Location**: `xearthlayer/src/orchestrator/download.rs:154`

**Problem**: Each tile download creates a new `ThreadPool(32)`:

```rust
let pool = ThreadPool::new(self.max_parallel_downloads);
```

**Impact**:
- Thread creation/destruction overhead for every tile
- With 8 worker threads, potentially 8 × 32 = 256 download threads active
- OS scheduler overhead managing hundreds of threads
- No connection reuse benefit across tiles

**Severity**: MEDIUM - Overhead and resource waste, but not blocking.

---

### Bottleneck #4: Blocking Chunk Collection (Potential Deadlock)

**Location**: `xearthlayer/src/orchestrator/download.rs:228`

**Problem**: After submitting all 256 chunks, the worker blocks indefinitely waiting for results:

```rust
drop(tx);  // Signal end of submissions

for result in rx {  // BLOCKS FOREVER until all 256 chunks complete
    match result {
        Ok(chunk) => results.push(chunk),
        Err(_) => failed += 1,
    }
}
```

**Impact**:
- If any chunk download hangs beyond HTTP timeout, the entire tile hangs
- If a download thread panics, its result is never sent, channel never closes
- No timeout on the collection loop itself
- After enough stuck workers, all N workers are blocked, system deadlocks

**Evidence**: Network activity stops completely after ~20 minutes.

**Severity**: CRITICAL - This is the likely cause of the observed deadlock.

---

### Bottleneck #5: HTTP Connection Pool Isolation

**Location**: `xearthlayer/src/provider/http.rs`

**Problem**: Each `XEarthLayerService` creates its own `ReqwestClient` with its own connection pool. While we increased `pool_max_idle_per_host` to 64, pools are not shared.

**Impact**:
- Connection establishment overhead not amortized across services
- With multiple mounted packages, each has separate connection pool
- DNS resolution and TLS handshakes repeated

**Severity**: LOW - Improved with pool size increase, but could be better.

---

### Bottleneck #6: Synchronous Texture Encoding

**Location**: `xearthlayer/src/tile/default.rs:134`

**Problem**: BC1/BC3 texture encoding is CPU-intensive (~0.2-1s) and runs synchronously on the worker thread.

```rust
let dds_data = self.encoder.encode(&image)?;  // Blocks worker for ~1s
```

**Impact**:
- Worker thread blocked during encoding, can't process other tiles
- With N workers and 1s encoding time, max throughput is N tiles/second
- CPU-bound work competes with I/O-bound download workers

**Severity**: MEDIUM - Limits peak throughput but encoding is necessary work.

---

### Bottleneck #7: Memory Cache Eviction Under Lock

**Location**: `xearthlayer/src/cache/memory.rs:175-213`

**Problem**: LRU eviction sorts all cache entries while holding the mutex:

```rust
fn evict_lru_until_size(&self, required_size: usize) {
    let mut cache = self.cache.lock().unwrap();
    let mut current_size = self.current_size_bytes.lock().unwrap();

    // Sort ALL entries by timestamp - O(n log n)
    let mut entries: Vec<_> = cache.iter().map(...).collect();
    entries.sort_by_key(...);
    // ... eviction loop
}
```

**Impact**:
- With thousands of cached tiles, sorting takes milliseconds
- All cache operations blocked during eviction
- Spiky latency when cache is full

**Severity**: LOW - Only affects steady-state after cache fills.

---

## Observability Gaps

Current telemetry provides:
- Network: bytes downloaded, chunks success/fail, average throughput
- Tiles: pending, active, completed, failed, timeouts
- Cache: hit rate, memory usage
- System: memory footprint, CPU (limited)

Missing instrumentation needed to diagnose issues:

| Metric | Purpose | Priority |
|--------|---------|----------|
| FUSE queue depth | See how many requests waiting for FUSE thread | HIGH |
| Worker thread state | Identify stuck/blocked workers | HIGH |
| Work queue depth | See if work is piling up | MEDIUM |
| Per-chunk latency | Identify slow chunk downloads | MEDIUM |
| ThreadPool utilization | See download thread activity | MEDIUM |
| Lock contention time | Measure mutex wait times | LOW |

---

## Mitigation Strategies

### Strategy A: Fix the Deadlock (Critical)

**Approach**: Add timeout to chunk collection loop to prevent infinite blocking.

```rust
// Current (dangerous):
for result in rx { ... }

// Proposed (safe):
let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
loop {
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => { /* process */ },
        Err(RecvTimeoutError::Timeout) => break,  // Give up on remaining chunks
        Err(RecvTimeoutError::Disconnected) => break,
    }
}
```

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Prevents deadlock | May return incomplete tiles more often |
| Simple fix, low risk | Need to handle partial results gracefully |
| No architecture change | Doesn't fix root cause (slow chunks) |

**Effort**: Low (1-2 hours)

---

### Strategy B: Multi-Threaded FUSE Dispatch

**Approach**: Replace single-threaded FUSE with multi-threaded dispatch.

**Option B1: Use fuse-mt crate**
- Wraps fuser with thread pool for operation dispatch
- Already attempted, caused different deadlock issues

**Option B2: Async FUSE with fuser's async support**
- Use `fuser`'s `MountOption::AutoUnmount` with async runtime
- Dispatch operations to async tasks

**Option B3: Manual thread pool dispatch**
- Keep single FUSE thread but immediately dispatch work to pool
- Return placeholder/loading indicator, update asynchronously

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Enables true parallelism | Significant complexity increase |
| Utilizes all worker threads | Previous fuse-mt attempt caused deadlocks |
| Matches X-Plane's concurrent requests | Need to handle FUSE reply from different thread |

**Effort**: High (1-2 weeks)

---

### Strategy C: Shared Download ThreadPool

**Approach**: Create a single global ThreadPool for all chunk downloads instead of per-tile pools.

```rust
// Current: ThreadPool created per tile
let pool = ThreadPool::new(32);

// Proposed: Shared pool across all downloads
lazy_static! {
    static ref DOWNLOAD_POOL: ThreadPool = ThreadPool::new(64);
}
```

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Eliminates thread creation overhead | Need to manage pool lifecycle |
| Better resource utilization | Shared pool may have different contention patterns |
| Consistent thread count | Configuration becomes global |

**Effort**: Low-Medium (2-4 hours)

---

### Strategy D: Async Downloads with Tokio

**Approach**: Replace blocking HTTP with async reqwest and tokio runtime.

```rust
// Current: Blocking HTTP in thread pool
provider.download_chunk(row, col, zoom)  // Blocks thread

// Proposed: Async HTTP with runtime
async fn download_chunk(&self, row, col, zoom) -> Result<Vec<u8>> {
    self.client.get(&url).send().await?.bytes().await
}
```

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Efficient I/O multiplexing | Major architectural change |
| Thousands of concurrent requests possible | Async/await complexity throughout |
| Lower memory per connection | Need to integrate with sync FUSE layer |
| Natural backpressure | Learning curve, debugging difficulty |

**Effort**: Very High (2-4 weeks)

---

### Strategy E: Request Prefetching

**Approach**: Predict and prefetch adjacent tiles before X-Plane requests them.

```rust
// When tile (row, col, zoom) is requested, also queue:
// (row-1, col, zoom), (row+1, col, zoom), (row, col-1, zoom), (row, col+1, zoom)
```

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Reduces perceived latency | Wastes bandwidth on unused tiles |
| Tiles ready when needed | Increases memory pressure |
| Hides download latency | Complex prediction logic |

**Effort**: Medium (3-5 days)

---

### Strategy F: Separate Download and Encode Thread Pools

**Approach**: Use dedicated pools for I/O-bound downloads and CPU-bound encoding.

```rust
// I/O pool: Many threads, mostly waiting on network
static DOWNLOAD_POOL: ThreadPool = ThreadPool::new(128);

// CPU pool: Few threads, fully utilized
static ENCODE_POOL: ThreadPool = ThreadPool::new(num_cpus);
```

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| I/O doesn't block CPU work | More complex work routing |
| Better resource utilization | Need to coordinate between pools |
| Can tune each pool independently | May not address main bottleneck |

**Effort**: Medium (3-5 days)

---

### Strategy G: Improve Observability First

**Approach**: Add comprehensive instrumentation before making changes.

Add metrics for:
1. Worker thread state (idle/downloading/encoding/blocked)
2. Work queue depth over time
3. Per-chunk download latency histogram
4. FUSE operation latency breakdown
5. Thread pool utilization

**Tradeoffs**:
| Pro | Con |
|-----|-----|
| Data-driven decisions | Delays actual fixes |
| Identifies real bottlenecks | Instrumentation has overhead |
| Validates fix effectiveness | May not find smoking gun |

**Effort**: Medium (2-3 days)

---

## Recommended Approach

### Phase 1: Critical Fixes (Immediate)
1. **Fix deadlock** (Strategy A) - Add timeout to chunk collection
2. **Add basic instrumentation** - Worker state, queue depth

### Phase 2: Quick Wins (This Week)
3. **Shared download pool** (Strategy C) - Reduce thread overhead
4. **Validate with instrumentation** - Confirm improvements

### Phase 3: Architecture (If Needed)
5. **Multi-threaded FUSE** (Strategy B) - Only if Phase 1-2 insufficient
6. **Async downloads** (Strategy D) - Major rewrite, last resort

---

## Open Questions

1. Why did the previous `fuse-mt` implementation cause deadlocks?
2. Is the 80% chunk success requirement too strict for flaky networks?
3. Should we implement request cancellation when FUSE times out?
4. What is X-Plane's actual concurrent request pattern during scene load?

---

## Testing Strategy

Any changes should be validated with:

1. **Synthetic benchmark**: Measure tiles/second with controlled workload
2. **Real-world test**: 20+ minute X-Plane session monitoring for deadlock
3. **Stress test**: Multiple regions loading simultaneously
4. **Metrics comparison**: Before/after throughput, latency distribution

---

## Appendix: Thread Model Diagram

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              THREAD INVENTORY                                    │
├─────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│  FUSE Thread (1)                                                                │
│  └─ Handles ALL filesystem operations sequentially                              │
│  └─ Blocks during tile generation (up to 10s)                                  │
│                                                                                  │
│  Tile Worker Threads (N = num_cpus, e.g., 8)                                   │
│  └─ tile-worker-0 through tile-worker-7                                        │
│  └─ Each processes one tile at a time                                          │
│  └─ Blocks during download + encode                                            │
│                                                                                  │
│  Download Threads (32 per active tile, transient)                              │
│  └─ Created by ThreadPool::new(32) for each tile                               │
│  └─ With 8 workers: up to 256 download threads                                 │
│  └─ Each blocks on HTTP I/O                                                    │
│                                                                                  │
│  Cache Daemon Thread (1)                                                        │
│  └─ Periodic disk cache cleanup                                                │
│  └─ Runs every 60 seconds                                                      │
│                                                                                  │
│  Stats Logger Thread (1)                                                        │
│  └─ Periodic stats output                                                      │
│  └─ Runs every 60 seconds                                                      │
│                                                                                  │
│  Network Stats Logger Thread (1)                                               │
│  └─ Periodic network stats output                                              │
│  └─ Runs every 60 seconds                                                      │
│                                                                                  │
│  Telemetry Display Thread (1)                                                  │
│  └─ Updates status bar                                                         │
│  └─ Runs every 1 second                                                        │
│                                                                                  │
│  TOTAL: ~12 persistent + up to 256 transient = ~268 threads peak               │
│                                                                                  │
└─────────────────────────────────────────────────────────────────────────────────┘
```

---

## Revision History

| Date | Change |
|------|--------|
| 2025-12-07 | Initial document created with architecture analysis |
| 2025-12-14 | Marked as resolved; issues fixed by async pipeline phases 3.1 and 3.2 |
