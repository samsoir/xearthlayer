# Parallel Tile Processing Design

> **⚠️ DEPRECATED (v0.3.0)**: This document describes the legacy pipeline architecture. The async pipeline section at the bottom has been superseded by the **Job Executor Framework** (`docs/dev/job-executor-design.md`). The new architecture uses a channel-based job executor daemon with explicit job/task separation. See [Job Executor Design](job-executor-design.md) for the current implementation.

This document describes the design of XEarthLayer's parallel tile generation system, which enables concurrent processing of multiple tile requests from X-Plane.

## Overview

X-Plane uses multiple cores to load scenery tiles during flight loading and background streaming. Without parallel processing on our side, tile generation becomes a bottleneck, resulting in 4-5 minute initial load times.

The `ParallelTileGenerator` addresses this by:
1. Using a thread pool for concurrent tile generation
2. Coalescing duplicate requests to avoid redundant work
3. Enforcing timeouts to prevent indefinite blocking

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         X-Plane Flight Simulator                         │
│               (Multiple threads requesting tiles concurrently)           │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                           FUSE Filesystem                                │
│                  (PassthroughFS / XEarthLayerFS)                         │
│                    Receives read() calls for DDS files                   │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       ParallelTileGenerator                              │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                    Request Coalescing                            │    │
│  │         HashMap<TileRequest, Vec<ResultSender>>                  │    │
│  │    (Duplicate requests wait for single generation)               │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                     │                                    │
│                                     ▼                                    │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                      Work Queue                                  │    │
│  │               mpsc::channel<WorkItem>                            │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                     │                                    │
│         ┌───────────────────────────┼───────────────────────────┐       │
│         ▼                           ▼                           ▼       │
│  ┌─────────────┐            ┌─────────────┐            ┌─────────────┐  │
│  │  Worker 0   │            │  Worker 1   │            │  Worker N   │  │
│  │  (thread)   │            │  (thread)   │    ...     │  (thread)   │  │
│  └─────────────┘            └─────────────┘            └─────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       DefaultTileGenerator                               │
│                  (TileOrchestrator + DdsEncoder)                         │
│        Downloads 256 chunks in parallel, composites, encodes             │
└─────────────────────────────────────────────────────────────────────────┘
```

## Components

### ParallelTileGenerator

Location: `xearthlayer/src/tile/parallel.rs`

The main wrapper that adds parallelism to any `TileGenerator` implementation.

```rust
pub struct ParallelTileGenerator {
    inner: Arc<dyn TileGenerator>,      // Wrapped generator
    in_flight: Arc<Mutex<HashMap<...>>>, // Request coalescing map
    work_sender: Sender<WorkItem>,       // Work queue
    config: ParallelConfig,              // Thread count, timeout
    shutdown: Arc<Mutex<bool>>,          // Shutdown signal
}
```

### ParallelConfig

Configuration for the parallel generator:

```rust
pub struct ParallelConfig {
    pub threads: usize,        // Worker thread count (default: num_cpus)
    pub timeout_secs: u64,     // Generation timeout (default: 10s)
    pub dds_format: DdsFormat, // For placeholder generation
    pub mipmap_count: usize,   // For placeholder generation
}
```

## Request Flow

### Normal Request

1. FUSE `read()` call arrives for a DDS file
2. `ParallelTileGenerator::generate()` is called
3. Check if request is already in-flight (coalescing)
   - If yes: add sender to waiters list, wait for result
   - If no: create new in-flight entry, submit to work queue
4. Worker thread picks up work item
5. Inner generator downloads and encodes tile
6. Result sent to all waiting senders
7. FUSE returns data to X-Plane

### Coalesced Request

When X-Plane requests the same tile from multiple threads:

```
Thread A: Request tile (100, 200, 16)  →  Creates in-flight entry, submits work
Thread B: Request tile (100, 200, 16)  →  Finds in-flight, joins waiters
Thread C: Request tile (100, 200, 16)  →  Finds in-flight, joins waiters
                                          │
                                          ▼
                    Worker generates tile ONCE
                                          │
                                          ▼
                    Result broadcast to A, B, C
```

### Timeout Handling

If generation exceeds the configured timeout:

1. Waiting thread receives `RecvTimeoutError::Timeout`
2. Magenta placeholder DDS is generated and returned
3. X-Plane displays visible error indicator (better than hanging)
4. Background generation may still complete (result cached)

## Configuration

Settings in `~/.xearthlayer/config.ini`:

```ini
[generation]
; Number of threads for parallel tile generation (default: number of CPU cores)
; WARNING: Do not set this higher than your CPU core count
threads = 8

; Timeout in seconds for generating a single tile (default: 10)
; If exceeded, returns a magenta placeholder texture
timeout = 10
```

## Thread Safety

All components are thread-safe:

- `ParallelTileGenerator` implements `Send + Sync`
- `TileGenerator` trait requires `Send + Sync`
- In-flight map protected by `Mutex`
- Result broadcasting via `mpsc` channels

## Backpressure

The thread pool size provides natural backpressure:

- If all workers are busy, new requests queue in the channel
- Channel is unbounded, but worker count limits concurrency
- Prevents memory exhaustion from unlimited parallel downloads

## Performance Characteristics

| Metric | Sequential | Parallel (8 threads) |
|--------|-----------|---------------------|
| Tiles/second | ~0.5 | ~4 (theoretical max) |
| Initial load (KOAK) | 4-5 min | ~1 min (target) |
| Memory per tile | ~50MB | ~50MB × threads |

## Async Pipeline with Request Coalescing

The async pipeline (`xearthlayer/src/pipeline/`) provides an alternative processing path
that uses fully asynchronous I/O, avoiding thread pool exhaustion issues.

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         X-Plane Flight Simulator                         │
│               (Multiple threads requesting tiles concurrently)           │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     AsyncPassthroughFS (FUSE)                            │
│                 Receives read() calls for DDS files                      │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       DdsHandler (runner.rs)                             │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                   RequestCoalescer                               │    │
│  │     HashMap<TileCoord, broadcast::Sender<Result>>                │    │
│  │    (Duplicate tile requests wait for single processing)          │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                     │                                    │
│                          If new request                                  │
│                                     ▼                                    │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │               Tokio Runtime (async tasks)                        │    │
│  │          256 concurrent chunk downloads via async I/O            │    │
│  │                 No thread pool exhaustion                        │    │
│  └─────────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                      AsyncProvider (async HTTP)                          │
│           Non-blocking downloads using async reqwest                     │
│         AsyncBingMapsProvider / AsyncGo2Provider / etc.                  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key Components

#### RequestCoalescer

Location: `xearthlayer/src/pipeline/coalesce.rs`

Prevents duplicate tile processing when multiple FUSE requests arrive for the same tile:

```rust
pub struct RequestCoalescer {
    in_flight: Mutex<HashMap<TileCoord, broadcast::Sender<Result>>>,
    stats: Mutex<CoalescerStats>,
}
```

When a request arrives:
1. Check if tile is already being processed
2. If yes: subscribe to broadcast channel, wait for result
3. If no: register as new request, process tile, broadcast result

Benefits:
- Prevents 5× duplicate work when X-Plane opens multiple file handles
- All waiters receive the same result
- Statistics tracking for monitoring coalescing effectiveness

#### AsyncProvider Trait

Location: `xearthlayer/src/provider/types.rs`

Async version of the Provider trait for non-blocking HTTP:

```rust
pub trait AsyncProvider: Send + Sync {
    async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError>;
    fn name(&self) -> &str;
    fn min_zoom(&self) -> u8;
    fn max_zoom(&self) -> u8;
}
```

Implementations:
- `AsyncBingMapsProvider` - Bing Maps with async reqwest
- `AsyncGo2Provider` - Google GO2 with async reqwest
- `AsyncGoogleMapsProvider` - Google Maps API with async session creation

#### AsyncHttpClient

Location: `xearthlayer/src/provider/http.rs`

Non-blocking HTTP client using async reqwest:

```rust
pub trait AsyncHttpClient: Send + Sync {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;
}
```

### Why Async I/O Matters

The blocking I/O approach had a critical flaw:

```
Problem: spawn_blocking for 256 chunks per tile
         × multiple concurrent tile requests
         = thread pool exhaustion → deadlock

With 10 tiles requested simultaneously:
  10 tiles × 256 chunks = 2,560 spawn_blocking calls
  Tokio's default blocking pool = 512 threads
  Result: Deadlock after ~181,000 requests
```

The async approach eliminates this:
- All chunk downloads use non-blocking I/O
- Single connection pool shared across all downloads
- Natural backpressure through semaphores
- No thread pool exhaustion possible

### Performance Comparison

| Aspect | Blocking (spawn_blocking) | Async (async reqwest) |
|--------|--------------------------|----------------------|
| Threads per tile | 256 (blocking) | 0 (uses event loop) |
| Max concurrent tiles | ~2 (before pool exhaustion) | Limited only by bandwidth |
| Deadlock risk | High under load | None |
| Connection reuse | Per-call client | Shared connection pool |
| Memory per tile | ~2MB (thread stacks) | ~256KB (futures) |

### Configuration

The async pipeline is used automatically when the service is configured with
an async provider. No additional configuration required.

## Related Files

- `xearthlayer/src/tile/parallel.rs` - ParallelTileGenerator implementation
- `xearthlayer/src/tile/mod.rs` - Module exports
- `xearthlayer/src/service/facade.rs` - Integration with service layer
- `xearthlayer/src/config/file.rs` - Configuration parsing
- `xearthlayer/src/fuse/placeholder.rs` - Magenta placeholder generation
