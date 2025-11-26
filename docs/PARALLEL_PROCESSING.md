# Parallel Tile Processing Design

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

## Future: Async FUSE Model

The current implementation uses blocking threads. A future enhancement (tracked as TD1 in TODO.md) would migrate to `fuser`'s async model:

### Current (Blocking)

```rust
impl Filesystem for PassthroughFS {
    fn read(&mut self, ..., reply: ReplyData) {
        // Blocks thread until tile generated
        let data = self.generator.generate(&request);
        reply.data(&data);
    }
}
```

### Future (Async)

```rust
impl AsyncFilesystem for PassthroughFS {
    async fn read(&self, ...) -> Result<Vec<u8>> {
        // Yields thread while waiting
        self.generator.generate_async(&request).await
    }
}
```

Benefits of async model:
- Better thread utilization under high concurrency
- More efficient I/O waiting (no blocked threads)
- Native integration with tokio ecosystem

Risks:
- Significant refactoring required
- Need to verify FUSE async compatibility
- May require changes to provider/orchestrator layers

## Related Files

- `xearthlayer/src/tile/parallel.rs` - ParallelTileGenerator implementation
- `xearthlayer/src/tile/mod.rs` - Module exports
- `xearthlayer/src/service/facade.rs` - Integration with service layer
- `xearthlayer/src/config/file.rs` - Configuration parsing
- `xearthlayer/src/fuse/placeholder.rs` - Magenta placeholder generation
