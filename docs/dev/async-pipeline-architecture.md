# Async Pipeline Architecture

**Status**: Phase 1 complete, Phase 2 complete, Phase 3 complete, Phase 3.1 complete (async HTTP + coalescing), Phase 3.2 complete (HTTP concurrency limiting)
**Created**: 2025-12-07
**Last Updated**: 2025-12-14

## Overview

This document describes the redesigned architecture for XEarthLayer's tile generation pipeline. The new design replaces the single-threaded FUSE handler with a multi-threaded dispatch layer backed by a fully asynchronous Tokio-based processing pipeline.

### Goals

1. **Match X-Plane's concurrency** - Handle N concurrent filesystem requests without blocking
2. **Maximize throughput** - Saturate available network and CPU resources
3. **Single responsibility** - Each component does one thing well
4. **Testability** - Components testable in isolation
5. **Observability** - Clear metrics at each stage of the pipeline
6. **Idiomatic Rust** - Align with Tokio conventions for contributor familiarity

### Non-Goals

- Async FUSE interface (we use sync threads at the FUSE boundary)
- Custom state machine framework (we use Tokio's Future combinators)
- Complex retry strategies in v1 (simple max-retry, swappable later)

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              X-PLANE APPLICATION                                 │
│                                                                                  │
│  Scenery Loader Thread Pool (N threads, e.g., 32)                               │
│  Each thread independently calls read() for DDS files                           │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        │ N concurrent read() syscalls
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              LINUX KERNEL / FUSE                                 │
│                                                                                  │
│  Dispatches each request to userspace FUSE daemon                               │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        │ N concurrent FUSE requests
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         FUSE DISPATCH LAYER (Sync)                               │
│                                                                                  │
│  Thread Pool: N handler threads (matching X-Plane's concurrency)                │
│                                                                                  │
│  Each handler thread:                                                           │
│    1. Receives FUSE request                                                     │
│    2. Creates Job                                                               │
│    3. Submits to async pipeline                                                 │
│    4. Blocks on oneshot channel waiting for result                             │
│    5. Replies to kernel with DDS data                                          │
│                                                                                  │
│  Threads block but don't do CPU work - they're parked waiting                  │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        │ Jobs submitted via channel
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         TOKIO ASYNC RUNTIME                                      │
│                                                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐   │
│  │                         JOB PROCESSOR                                    │   │
│  │                                                                          │   │
│  │  async fn process_job(job: Job) -> Result<JobResult>                    │   │
│  │                                                                          │   │
│  │    ┌──────────────┐                                                     │   │
│  │    │   DOWNLOAD   │  Fan-out: 256 async HTTP tasks via JoinSet         │   │
│  │    │    STAGE     │  Fan-in: Collect results as they complete          │   │
│  │    └──────┬───────┘                                                     │   │
│  │           │                                                              │   │
│  │           ▼                                                              │   │
│  │    ┌──────────────┐                                                     │   │
│  │    │   ASSEMBLY   │  Combine chunks into 4096×4096 image               │   │
│  │    │    STAGE     │  spawn_blocking for CPU work                       │   │
│  │    └──────┬───────┘                                                     │   │
│  │           │                                                              │   │
│  │           ▼                                                              │   │
│  │    ┌──────────────┐                                                     │   │
│  │    │   ENCODE     │  BC1/BC3 DDS compression + mipmaps                 │   │
│  │    │    STAGE     │  spawn_blocking for CPU work                       │   │
│  │    └──────┬───────┘                                                     │   │
│  │           │                                                              │   │
│  │           ▼                                                              │   │
│  │    ┌──────────────┐                                                     │   │
│  │    │    CACHE     │  Async write to memory + disk cache                │   │
│  │    │    STAGE     │  tokio::fs for disk I/O                            │   │
│  │    └──────┬───────┘                                                     │   │
│  │           │                                                              │   │
│  │           ▼                                                              │   │
│  │       JobResult                                                          │   │
│  │                                                                          │   │
│  └─────────────────────────────────────────────────────────────────────────┘   │
│                                                                                  │
└───────────────────────────────────────┬─────────────────────────────────────────┘
                                        │
                                        │ Result via oneshot channel
                                        ▼
                              FUSE handler thread unblocks
                              Replies to kernel
                              X-Plane's read() returns
```

---

## Core Models

### Job

A Job represents a request from X-Plane for a DDS file. It is the unit of work from the filesystem's perspective.

```rust
/// A request from X-Plane for a DDS tile
pub struct Job {
    /// Unique identifier for this job
    pub id: JobId,

    /// Tile coordinates being requested
    pub tile_coords: TileCoord,

    /// Priority (for future scheduling optimization)
    pub priority: Priority,

    /// When the job was created
    pub created_at: Instant,

    /// Channel to send result back to FUSE handler
    pub result_sender: oneshot::Sender<JobResult>,
}

/// Result of job processing
pub struct JobResult {
    pub job_id: JobId,
    pub dds_data: Vec<u8>,
    pub duration: Duration,
}

/// Unique job identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);
```

### Task

A Task represents a unit of work within a job. Tasks are executed by specialized workers and have single responsibility.

```rust
/// A unit of work to be executed
pub struct Task<T> {
    /// Unique identifier
    pub id: TaskId,

    /// Parent job (opaque to workers)
    pub job_id: JobId,

    /// The work to be done
    pub payload: T,

    /// Current attempt number (for retry tracking)
    pub attempt: u32,

    /// Maximum attempts before permanent failure
    pub max_attempts: u32,

    /// Callback invoked on completion
    pub on_complete: Box<dyn FnOnce(TaskResult<T>) + Send>,
}

/// Task-specific payloads
pub struct DownloadChunkPayload {
    pub chunk_row: u8,
    pub chunk_col: u8,
    pub global_row: u32,
    pub global_col: u32,
    pub zoom: u8,
}

pub struct AssembleImagePayload {
    pub chunks: HashMap<(u8, u8), Vec<u8>>,
}

pub struct EncodeDdsPayload {
    pub image: RgbaImage,
    pub format: DdsFormat,
    pub mipmap_count: usize,
}

pub struct CachePayload {
    pub key: CacheKey,
    pub data: Vec<u8>,
}
```

---

## State Machine (Implicit in Async Flow)

The job state machine is expressed implicitly through Tokio's async function flow rather than an explicit state machine structure. This aligns with Tokio idioms and is familiar to Rust async developers.

### Logical States

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   PENDING   │────►│ DOWNLOADING │────►│ ASSEMBLING  │────►│  ENCODING   │────►│   CACHING   │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
                                                                                       │
                                                                                       ▼
                                                                               ┌─────────────┐
                                                                               │  COMPLETE   │
                                                                               └─────────────┘
```

### Implementation

```rust
/// Process a job through all stages
async fn process_job(job: Job, ctx: &PipelineContext) -> Result<Vec<u8>, JobError> {
    // DOWNLOADING: Fan-out to download tasks, fan-in results
    let chunks = download_stage(&job, ctx).await?;

    // ASSEMBLING: Combine chunks into image (CPU-bound)
    let image = assembly_stage(&job, chunks, ctx).await?;

    // ENCODING: Compress to DDS format (CPU-bound)
    let dds_data = encode_stage(&job, image, ctx).await?;

    // CACHING: Persist to cache
    cache_stage(&job, &dds_data, ctx).await?;

    Ok(dds_data)
}
```

Each stage function handles its own fan-out/fan-in logic using Tokio primitives.

---

## Pipeline Stages

### Stage 1: Download

Downloads 256 chunks (16x16 grid) that compose a single tile.

**Characteristics:**
- I/O bound (network)
- High parallelism (256 concurrent HTTP requests per job)
- Uses async reqwest

**Implementation:**

```rust
async fn download_stage(job: &Job, ctx: &PipelineContext) -> Result<ChunkResults, JobError> {
    let mut downloads = JoinSet::new();

    // Fan-out: spawn all 256 download tasks
    for (row, col) in chunk_coordinates() {
        let task = DownloadTask::new(
            job.id,
            row, col,
            job.tile_coords,
            ctx.http_client.clone(),
        );
        downloads.spawn(task.execute());
    }

    // Fan-in: collect results as they complete
    let mut results = ChunkResults::new();
    let mut retry_queue = Vec::new();

    while let Some(result) = downloads.join_next().await {
        match result {
            Ok(Ok(chunk)) => {
                results.add_success(chunk);
            }
            Ok(Err(failed)) => {
                // Check if retry is possible
                if failed.attempt < failed.max_attempts {
                    retry_queue.push(failed.into_retry());
                } else {
                    results.add_failure(failed.coords);
                }
            }
            Err(join_error) => {
                // Task panicked - treat as failure
                log::error!("Download task panicked: {}", join_error);
            }
        }
    }

    // Re-queue failed tasks for retry
    for task in retry_queue {
        downloads.spawn(task.execute());
    }

    // Collect retry results...

    Ok(results)
}
```

### Stage 2: Assembly

Combines downloaded chunks into a single 4096x4096 RGBA image.

**Characteristics:**
- CPU bound (image manipulation)
- Single task per job
- Uses `spawn_blocking`

**Implementation:**

```rust
async fn assembly_stage(
    job: &Job,
    chunks: ChunkResults,
    ctx: &PipelineContext,
) -> Result<RgbaImage, JobError> {
    let job_id = job.id;

    tokio::task::spawn_blocking(move || {
        let mut canvas = RgbaImage::new(4096, 4096);

        for row in 0..16u8 {
            for col in 0..16u8 {
                let chunk_image = match chunks.get(row, col) {
                    Some(data) => decode_jpeg(data)?,
                    None => generate_magenta_chunk(), // Failed chunk
                };

                let x = col as u32 * 256;
                let y = row as u32 * 256;
                image::imageops::replace(&mut canvas, &chunk_image, x.into(), y.into());
            }
        }

        Ok(canvas)
    }).await?
}
```

### Stage 3: Encode

Compresses the assembled image to DDS format with mipmaps.

**Characteristics:**
- CPU bound (BC1/BC3 compression)
- Single task per job
- Uses `spawn_blocking`

**Implementation:**

```rust
async fn encode_stage(
    job: &Job,
    image: RgbaImage,
    ctx: &PipelineContext,
) -> Result<Vec<u8>, JobError> {
    let format = ctx.dds_format;
    let mipmap_count = ctx.mipmap_count;

    tokio::task::spawn_blocking(move || {
        let encoder = DdsTextureEncoder::new(format)
            .with_mipmap_count(mipmap_count);
        encoder.encode(&image)
    }).await?
}
```

### Stage 4: Cache

Persists the encoded DDS data to memory and disk cache.

**Characteristics:**
- I/O bound (disk writes)
- Uses async file I/O via tokio::fs

**Implementation:**

```rust
async fn cache_stage(
    job: &Job,
    dds_data: &[u8],
    ctx: &PipelineContext,
) -> Result<(), JobError> {
    let cache_key = CacheKey::from_tile(&job.tile_coords);

    // Memory cache (fast, sync is fine)
    ctx.memory_cache.put(&cache_key, dds_data.to_vec());

    // Disk cache (async I/O)
    ctx.disk_cache.put_async(&cache_key, dds_data).await?;

    Ok(())
}
```

---

## FUSE Integration

### Multi-Threaded Dispatch

The FUSE layer uses a thread pool to handle concurrent requests from X-Plane.

```rust
/// FUSE filesystem with multi-threaded dispatch
pub struct AsyncPassthroughFS {
    /// Channel to submit jobs to the async pipeline
    job_sender: mpsc::Sender<Job>,

    /// Thread pool size (matches X-Plane's concurrency)
    handler_threads: usize,

    // ... other fields for path resolution, etc.
}

impl Filesystem for AsyncPassthroughFS {
    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        // This runs on one of N handler threads

        if let Some(dds_info) = self.get_virtual_dds(ino) {
            // Virtual DDS file - need to generate
            let (result_tx, result_rx) = oneshot::channel();

            let job = Job {
                id: JobId::new(),
                tile_coords: dds_info.tile_coords,
                priority: Priority::Normal,
                created_at: Instant::now(),
                result_sender: result_tx,
            };

            // Submit to async pipeline
            if self.job_sender.blocking_send(job).is_err() {
                reply.error(libc::EIO);
                return;
            }

            // Block waiting for result (this thread only)
            match result_rx.blocking_recv() {
                Ok(result) => {
                    let data = &result.dds_data[offset as usize..];
                    let data = &data[..std::cmp::min(size as usize, data.len())];
                    reply.data(data);
                }
                Err(_) => {
                    reply.error(libc::EIO);
                }
            }
        } else {
            // Real file - passthrough to underlying filesystem
            self.read_real_file(ino, offset, size, reply);
        }
    }
}
```

### Thread Pool Configuration

```rust
/// Mount the filesystem with multi-threaded FUSE
pub fn mount(
    mountpoint: &Path,
    source: &Path,
    handler_threads: usize,
) -> Result<BackgroundSession, MountError> {
    let fs = AsyncPassthroughFS::new(source, ...)?;

    let options = vec![
        MountOption::FSName("xearthlayer".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowOther,
    ];

    // fuser spawns handler_threads to process FUSE operations
    fuser::spawn_mount2(fs, mountpoint, &options)
}
```

---

## Error Handling

### Strategy: Optimistic with Per-Chunk Fallback

The pipeline always returns a complete tile. Failed chunks are replaced with magenta placeholders, making failures visible but not blocking.

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         TILE WITH PARTIAL FAILURES                               │
│                                                                                  │
│  ┌─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┬───  │
│  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │     │
│  ├─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼───  │
│  │ OK  │ OK  │█████│ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │█████│ OK  │     │
│  ├─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼───  │
│  │ OK  │ OK  │ OK  │ OK  │ OK  │ OK  │█████│ OK  │ OK  │ OK  │ OK  │ OK  │     │
│  ├─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼─────┼───  │
│                                                                                  │
│  █ = Magenta chunk (download failed after max retries)                          │
│                                                                                  │
│  User sees: Mostly correct tile with visible pink squares where failures        │
│  occurred. Better than entire tile failing or blocking.                         │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Retry Strategy: Re-queue, Don't Retry In-Place

Failed tasks are re-queued rather than retried within the current execution context. This prevents hung tasks from clogging workers.

```rust
/// Retry policy (simple v1, swappable for smarter strategies later)
pub struct RetryPolicy {
    /// Maximum attempts per task
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3 }
    }
}

/// When a download task fails
fn handle_download_failure(task: DownloadTask, results: &mut ChunkResults, retry_set: &mut JoinSet) {
    if task.attempt < task.max_attempts {
        // Re-queue with incremented attempt counter
        let retry_task = task.into_retry();
        retry_set.spawn(retry_task.execute());
    } else {
        // Max retries exhausted - mark as permanent failure
        results.add_failure(task.chunk_coords);
    }
}
```

### Error Propagation

| Error Type | Handling |
|------------|----------|
| Individual chunk download fails | Retry up to max_attempts, then magenta placeholder |
| All chunks fail | Return full magenta tile (still valid DDS) |
| Assembly fails | Return magenta placeholder tile |
| Encoding fails | Return magenta placeholder tile |
| Cache write fails | Log warning, return data anyway (cache is optimization) |
| Job timeout | Return magenta placeholder tile |

---

## Caching Architecture

The caching system uses a two-tier architecture with different granularities at each level, optimized for their respective access patterns and persistence requirements.

### Design Principles

1. **Different granularity per tier** - Memory caches complete tiles, disk caches individual chunks
2. **Check early, write late** - Memory cache checked before job creation, disk cache checked during download
3. **Partial progress preservation** - Chunk-level disk cache preserves successful downloads even if tile fails
4. **Request coalescing** - Multiple requests for the same tile share a single job

### Two-Tier Cache Architecture

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         CACHE ARCHITECTURE                                       │
│                                                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐    │
│  │                     MEMORY CACHE (Tile Level)                            │    │
│  │                                                                          │    │
│  │  Purpose: Fast access to recently generated tiles                       │    │
│  │  Granularity: Complete DDS tiles (~11MB each)                           │    │
│  │  Capacity: ~180 tiles in 2GB (configurable)                             │    │
│  │  Access: Synchronous (checked before job creation)                      │    │
│  │  Eviction: LRU                                                          │    │
│  │  Persistence: None (volatile, regenerated on restart)                   │    │
│  │                                                                          │    │
│  │  Key: TileCoord { row, col, zoom }                                      │    │
│  │  Value: Vec<u8> (complete DDS file)                                     │    │
│  └─────────────────────────────────────────────────────────────────────────┘    │
│                                                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐    │
│  │                     DISK CACHE (Chunk Level)                             │    │
│  │                                                                          │    │
│  │  Purpose: Persist downloaded chunks across sessions                     │    │
│  │  Granularity: Individual JPEG chunks (~3KB each)                        │    │
│  │  Capacity: ~6.8M chunks in 20GB (~26,500 tiles worth)                  │    │
│  │  Access: Asynchronous via tokio::fs                                     │    │
│  │  Eviction: LRU                                                          │    │
│  │  Persistence: Survives restarts                                         │    │
│  │                                                                          │    │
│  │  Key: ChunkCoord { tile_row, tile_col, zoom, chunk_row, chunk_col }    │    │
│  │  Value: Vec<u8> (JPEG image data)                                       │    │
│  └─────────────────────────────────────────────────────────────────────────┘    │
│                                                                                  │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Why Different Granularities?

| Aspect | Memory (Tiles) | Disk (Chunks) |
|--------|----------------|---------------|
| **Purpose** | Fast repeated access to hot tiles | Persist partial progress, survive restarts |
| **Failure resilience** | Not needed (in-memory, regenerate) | High (250/256 chunks preserved on crash) |
| **Storage efficiency** | Lower (~11MB per tile) | Higher (~768KB per tile as JPEGs) |
| **Cold start** | Empty, warms up during flight | Populated from previous sessions |
| **Effective capacity** | ~180 tiles | ~26,500 tiles |

### Cache Lookup Flow

```
FUSE read() for tile (row=100, col=200, zoom=16)
        │
        ▼
┌─────────────────────────┐
│ 1. Memory Cache Check   │ ──Hit──► Return immediately (sync, <1ms)
│    (Tile level)         │
└───────────┬─────────────┘
            │ Miss
            ▼
┌─────────────────────────┐
│ 2. In-Flight Job Check  │ ──Found──► Subscribe to existing job, await result
│    (Request coalescing) │
└───────────┬─────────────┘
            │ Not found
            ▼
┌─────────────────────────┐
│ 3. Create Job           │
│    Add to in-flight map │
│    Enter pipeline       │
└───────────┬─────────────┘
            │
            ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. Download Stage (for each of 256 chunks)                  │
│                                                              │
│    ┌─────────────────────────┐                              │
│    │ Disk Cache Check        │ ──Hit──► Use cached chunk    │
│    │ (Chunk level, async)    │                              │
│    └───────────┬─────────────┘                              │
│                │ Miss                                        │
│                ▼                                             │
│    ┌─────────────────────────┐                              │
│    │ HTTP Download           │                              │
│    │ Write to Disk Cache     │                              │
│    └─────────────────────────┘                              │
└─────────────────────────────────────────────────────────────┘
            │
            ▼
      Assembly Stage
            │
            ▼
       Encode Stage
            │
            ▼
┌─────────────────────────┐
│ 5. Write Memory Cache   │  Store complete DDS tile
│    (Tile level, sync)   │
└───────────┬─────────────┘
            │
            ▼
      Notify all waiters
      (coalesced requests)
```

### Request Coalescing

Multiple FUSE threads may request the same tile simultaneously. Rather than generating the tile multiple times, we coalesce these requests into a single job.

```rust
/// Tracks in-flight jobs for request coalescing
struct InFlightJobs {
    jobs: Mutex<HashMap<TileCoord, InFlightJob>>,
}

struct InFlightJob {
    /// Channels waiting for this job's result
    waiters: Vec<oneshot::Sender<Arc<JobResult>>>,
}

impl InFlightJobs {
    /// Submit a job, coalescing with existing in-flight job if present
    async fn submit(&self, tile: TileCoord) -> oneshot::Receiver<Arc<JobResult>> {
        let (tx, rx) = oneshot::channel();

        let mut jobs = self.jobs.lock().await;

        if let Some(existing) = jobs.get_mut(&tile) {
            // Coalesce: subscribe to existing job
            existing.waiters.push(tx);
        } else {
            // New job: create entry and spawn processing
            jobs.insert(tile, InFlightJob { waiters: vec![tx] });

            let jobs_handle = self.clone();
            tokio::spawn(async move {
                let result = process_tile(tile).await;
                let result = Arc::new(result);

                // Notify all waiters
                let mut jobs = jobs_handle.jobs.lock().await;
                if let Some(job) = jobs.remove(&tile) {
                    for waiter in job.waiters {
                        let _ = waiter.send(result.clone());
                    }
                }
            });
        }

        rx
    }
}
```

### Disk Cache Structure

```
~/.cache/xearthlayer/
└── chunks/
    └── {provider}/
        └── {zoom}/
            └── {tile_row}/
                └── {tile_col}/
                    ├── 00_00.jpg
                    ├── 00_01.jpg
                    ├── ...
                    └── 15_15.jpg
```

### Cache Configuration

```rust
pub struct CacheConfig {
    /// Memory cache settings
    pub memory: MemoryCacheConfig,

    /// Disk cache settings
    pub disk: DiskCacheConfig,
}

pub struct MemoryCacheConfig {
    /// Maximum memory cache size in bytes (default: 2GB)
    pub max_size_bytes: usize,

    /// Enable/disable memory cache
    pub enabled: bool,
}

pub struct DiskCacheConfig {
    /// Cache directory (default: ~/.cache/xearthlayer/chunks)
    pub directory: PathBuf,

    /// Maximum disk cache size in bytes (default: 20GB)
    pub max_size_bytes: u64,

    /// Enable/disable disk cache
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory: MemoryCacheConfig {
                max_size_bytes: 2 * 1024 * 1024 * 1024, // 2GB
                enabled: true,
            },
            disk: DiskCacheConfig {
                directory: dirs::cache_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("xearthlayer")
                    .join("chunks"),
                max_size_bytes: 20 * 1024 * 1024 * 1024, // 20GB
                enabled: true,
            },
        }
    }
}
```

### Future Considerations: Cache Warming

The current design does not implement cache warming (preloading disk cache to memory on startup), but the architecture supports adding it later:

- On startup, scan disk cache for chunks
- Optionally assemble and encode frequently-accessed tiles into memory cache
- Could use access timestamps or flight plan to prioritize warming

This optimization is deferred until we have telemetry data showing cold start performance is a bottleneck.

---

## Configuration

```rust
/// Pipeline configuration
pub struct PipelineConfig {
    /// Number of FUSE handler threads (should match X-Plane's loader threads)
    pub fuse_handler_threads: usize,

    /// Maximum concurrent jobs in the pipeline
    pub max_concurrent_jobs: usize,

    /// Download settings
    pub download: DownloadConfig,

    /// Encoding settings
    pub encoding: EncodingConfig,

    /// Cache settings
    pub cache: CacheConfig,

    /// Retry policy
    pub retry_policy: RetryPolicy,
}

pub struct DownloadConfig {
    /// HTTP request timeout
    pub request_timeout: Duration,

    /// Maximum concurrent downloads per job (256 chunks)
    pub max_concurrent_per_job: usize,

    /// Global maximum concurrent downloads (across all jobs)
    pub max_concurrent_global: usize,
}

pub struct EncodingConfig {
    /// DDS format (BC1 or BC3)
    pub format: DdsFormat,

    /// Number of mipmap levels
    pub mipmap_count: usize,

    /// Maximum concurrent encoding tasks (CPU-bound, ~num_cpus)
    pub max_concurrent: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);

        Self {
            fuse_handler_threads: num_cpus,
            max_concurrent_jobs: num_cpus * 4,
            download: DownloadConfig {
                request_timeout: Duration::from_secs(10),
                max_concurrent_per_job: 256,
                max_concurrent_global: 1024,
            },
            encoding: EncodingConfig {
                format: DdsFormat::BC1,
                mipmap_count: 5,
                max_concurrent: num_cpus,
            },
            cache: CacheConfig::default(),
            retry_policy: RetryPolicy::default(),
        }
    }
}
```

---

## Observability

### Metrics to Track

| Metric | Description | Granularity |
|--------|-------------|-------------|
| `jobs_submitted` | Total jobs submitted to pipeline | Counter |
| `jobs_completed` | Jobs completed successfully | Counter |
| `jobs_failed` | Jobs that failed entirely | Counter |
| `job_duration_seconds` | Time from submission to completion | Histogram |
| `chunks_downloaded` | Successful chunk downloads | Counter |
| `chunks_failed` | Failed chunk downloads (after retries) | Counter |
| `chunks_retried` | Chunk download retries | Counter |
| `download_duration_seconds` | Per-chunk download time | Histogram |
| `assembly_duration_seconds` | Image assembly time | Histogram |
| `encode_duration_seconds` | DDS encoding time | Histogram |
| `fuse_handler_active` | Active FUSE handler threads | Gauge |
| `pipeline_jobs_active` | Jobs currently in pipeline | Gauge |
| `downloads_active` | Active download tasks | Gauge |

### Integration Points

```rust
/// Telemetry hooks for observability
pub trait PipelineTelemetry: Send + Sync {
    fn on_job_submitted(&self, job_id: JobId);
    fn on_job_completed(&self, job_id: JobId, duration: Duration);
    fn on_job_failed(&self, job_id: JobId, error: &str);

    fn on_download_started(&self, job_id: JobId, chunk: (u8, u8));
    fn on_download_completed(&self, job_id: JobId, chunk: (u8, u8), bytes: usize, duration: Duration);
    fn on_download_failed(&self, job_id: JobId, chunk: (u8, u8), error: &str);
    fn on_download_retried(&self, job_id: JobId, chunk: (u8, u8), attempt: u32);

    fn on_assembly_completed(&self, job_id: JobId, duration: Duration);
    fn on_encode_completed(&self, job_id: JobId, duration: Duration);
}
```

---

## Telemetry Architecture

The telemetry system serves two primary objectives:
1. **Debugging & Observability** - Enable developers to understand system behavior, diagnose issues, and identify bottlenecks
2. **End-User Feedback** - Provide users with confidence that the system is working and show meaningful progress

### Design Principles

1. **Separation of Concerns** - Instrumentation, aggregation, and presentation are distinct layers
2. **Dependency Injection** - Components receive a `Telemetry` trait, not concrete implementations
3. **Zero-Cost When Disabled** - `NoOpTelemetry` implementation for testing or disabled telemetry
4. **Standards-Based** - Built on `tracing` crate, with OpenTelemetry export path
5. **Performance-Conscious** - Lock-free counters, sampling for high-frequency events

### Three-Layer Architecture

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                         TELEMETRY ARCHITECTURE                                   │
│                                                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐    │
│  │                     1. INSTRUMENTATION LAYER                             │    │
│  │                                                                          │    │
│  │  Built on `tracing` crate                                               │    │
│  │  Components emit spans, events, and metrics                             │    │
│  │  No knowledge of consumers                                              │    │
│  │                                                                          │    │
│  │  - Spans: Timing of operations (job processing, download, encode)       │    │
│  │  - Events: Discrete occurrences (job created, chunk failed)             │    │
│  │  - Metrics: Numeric values (queue depth, bytes downloaded)              │    │
│  │  - Logs: Human-readable debug messages                                  │    │
│  └─────────────────────────────────────────────────────────────────────────┘    │
│                                      │                                          │
│                                      ▼                                          │
│  ┌─────────────────────────────────────────────────────────────────────────┐    │
│  │                     2. AGGREGATION LAYER                                 │    │
│  │                                                                          │    │
│  │  Subscribes to tracing events                                           │    │
│  │  Maintains rolling windows for rates and throughput                     │    │
│  │  Computes derived metrics (p50, p95, p99 latencies)                     │    │
│  │  Provides snapshot interface for views                                  │    │
│  │                                                                          │    │
│  │  TelemetryAggregator {                                                  │    │
│  │      fn snapshot(&self) -> TelemetrySnapshot;                           │    │
│  │  }                                                                       │    │
│  └─────────────────────────────────────────────────────────────────────────┘    │
│                                      │                                          │
│                                      ▼                                          │
│  ┌─────────────────────────────────────────────────────────────────────────┐    │
│  │                     3. PRESENTATION LAYER                                │    │
│  │                                                                          │    │
│  │  View models transform snapshots for specific contexts                  │    │
│  │  Multiple simultaneous views supported                                  │    │
│  │                                                                          │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐    │    │
│  │  │ CLI View    │  │ GUI View    │  │ Log View    │  │ OTel Export │    │    │
│  │  │ (ASCII art) │  │ (World map) │  │ (Structured)│  │ (External)  │    │    │
│  │  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘    │    │
│  │                                                                          │    │
│  └─────────────────────────────────────────────────────────────────────────┘    │
│                                                                                  │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Use Cases: Questions Telemetry Must Answer

#### Debugging & Observability

| Question | Metrics/Events Needed |
|----------|----------------------|
| "Why is this tile taking so long?" | Per-job span with stage timing breakdown |
| "Is the network the bottleneck?" | Download queue depth, active downloads, throughput |
| "Are we CPU-bound on encoding?" | Encode queue depth, active encodes, duration histogram |
| "Why did this chunk fail?" | Error events with context (URL, HTTP status, attempt) |
| "Is the disk cache being hit?" | Cache hit/miss counters by tier |
| "Are requests being coalesced?" | Coalesce events, waiters-per-job distribution |
| "Is the system deadlocked?" | Queue depths over time, worker activity |
| "What's the memory footprint?" | Memory cache size, active job/task counts |
| "What's the error rate?" | Failed chunks/jobs, retry counts |
| "Where is time spent?" | Span durations by stage (download, assemble, encode) |

#### End-User Feedback

| Question | Presentation |
|----------|--------------|
| "Is anything happening?" | Activity indicators, pipeline stage counts |
| "How fast is it going?" | Tiles/second, MB/s throughput |
| "How much is cached?" | Cache utilization bars, hit rates |
| "Where are the failures?" | Error counts, retry counts |
| "How long until loaded?" | Queue depths, estimated completion |

### Integration with `tracing` Crate

We use the `tracing` ecosystem for instrumentation:

```rust
use tracing::{info, warn, error, debug, instrument, Span};

/// Example: Instrumented job processing
#[instrument(skip(ctx), fields(job_id = %job.id, tile = ?job.tile_coords))]
async fn process_job(job: Job, ctx: &PipelineContext) -> Result<Vec<u8>, JobError> {
    // Span automatically tracks duration

    let chunks = download_stage(&job, ctx).await?;
    info!(chunks_success = chunks.success_count(), chunks_failed = chunks.failure_count(), "Download stage complete");

    let image = assembly_stage(&job, chunks, ctx).await?;
    debug!("Assembly stage complete");

    let dds_data = encode_stage(&job, image, ctx).await?;
    debug!(size_bytes = dds_data.len(), "Encode stage complete");

    cache_stage(&job, &dds_data, ctx).await?;

    Ok(dds_data)
}

/// Example: Instrumented download task
#[instrument(skip(http_client), fields(chunk = ?(row, col), attempt))]
async fn download_chunk(
    tile: TileCoord,
    row: u8,
    col: u8,
    attempt: u32,
    http_client: &HttpClient,
) -> Result<ChunkData, DownloadError> {
    Span::current().record("attempt", attempt);

    match http_client.get(&url).await {
        Ok(response) => {
            info!(bytes = response.len(), "Chunk downloaded");
            Ok(ChunkData { row, col, data: response })
        }
        Err(e) => {
            warn!(error = %e, "Chunk download failed");
            Err(DownloadError::Http(e))
        }
    }
}
```

### Metrics Collection

```rust
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Core metrics collected by the telemetry system
pub struct PipelineMetrics {
    // Job metrics
    pub jobs_submitted: AtomicU64,
    pub jobs_completed: AtomicU64,
    pub jobs_failed: AtomicU64,
    pub jobs_active: AtomicUsize,

    // Download metrics
    pub chunks_downloaded: AtomicU64,
    pub chunks_failed: AtomicU64,
    pub chunks_retried: AtomicU64,
    pub chunks_cache_hits: AtomicU64,
    pub chunks_cache_misses: AtomicU64,
    pub bytes_downloaded: AtomicU64,
    pub downloads_active: AtomicUsize,
    pub download_queue_depth: AtomicUsize,

    // Encoding metrics
    pub encodes_completed: AtomicU64,
    pub encodes_active: AtomicUsize,
    pub encode_queue_depth: AtomicUsize,

    // Cache metrics
    pub memory_cache_hits: AtomicU64,
    pub memory_cache_misses: AtomicU64,
    pub memory_cache_size_bytes: AtomicUsize,
    pub memory_cache_entries: AtomicUsize,
    pub disk_cache_size_bytes: AtomicU64,
    pub disk_cache_entries: AtomicU64,

    // FUSE metrics
    pub fuse_requests_active: AtomicUsize,
    pub fuse_requests_waiting: AtomicUsize,
}

/// Snapshot of metrics at a point in time (for views)
#[derive(Clone, Debug)]
pub struct TelemetrySnapshot {
    pub timestamp: Instant,
    pub uptime: Duration,

    // Pipeline pressure (queue depths and active counts)
    pub fuse_waiting: usize,
    pub jobs_active: usize,
    pub downloads_active: usize,
    pub download_queue_depth: usize,
    pub encodes_active: usize,
    pub encode_queue_depth: usize,

    // Throughput (computed from rolling window)
    pub jobs_per_second: f64,
    pub chunks_per_second: f64,
    pub bytes_per_second: f64,
    pub peak_bytes_per_second: f64,

    // Cache stats
    pub memory_cache_hit_rate: f64,
    pub memory_cache_size_bytes: usize,
    pub memory_cache_entries: usize,
    pub disk_cache_hit_rate: f64,
    pub disk_cache_size_bytes: u64,
    pub disk_cache_entries: u64,

    // Error stats
    pub chunks_failed_total: u64,
    pub chunks_retried_total: u64,
    pub jobs_failed_total: u64,

    // Totals
    pub jobs_completed_total: u64,
    pub chunks_downloaded_total: u64,
    pub bytes_downloaded_total: u64,
}
```

### Telemetry Trait (Dependency Injection)

```rust
/// Trait for telemetry instrumentation, injected into components
pub trait Telemetry: Send + Sync {
    // Job lifecycle
    fn job_submitted(&self, job_id: JobId, tile: TileCoord);
    fn job_completed(&self, job_id: JobId, duration: Duration);
    fn job_failed(&self, job_id: JobId, error: &str);
    fn job_coalesced(&self, job_id: JobId, waiters: usize);

    // Download stage
    fn download_started(&self, job_id: JobId, chunk: (u8, u8));
    fn download_completed(&self, job_id: JobId, chunk: (u8, u8), bytes: usize, duration: Duration);
    fn download_failed(&self, job_id: JobId, chunk: (u8, u8), error: &str, attempt: u32);
    fn download_cache_hit(&self, job_id: JobId, chunk: (u8, u8));

    // Encoding stage
    fn encode_started(&self, job_id: JobId);
    fn encode_completed(&self, job_id: JobId, duration: Duration, size_bytes: usize);

    // Cache operations
    fn memory_cache_hit(&self, tile: TileCoord);
    fn memory_cache_miss(&self, tile: TileCoord);
    fn memory_cache_write(&self, tile: TileCoord, size_bytes: usize);
    fn disk_cache_write(&self, chunk: ChunkCoord, size_bytes: usize);

    // Queue pressure
    fn queue_depth_updated(&self, stage: PipelineStage, depth: usize);
}

/// No-op implementation for testing or disabled telemetry
pub struct NoOpTelemetry;

impl Telemetry for NoOpTelemetry {
    fn job_submitted(&self, _: JobId, _: TileCoord) {}
    fn job_completed(&self, _: JobId, _: Duration) {}
    // ... all methods are no-ops
}
```

### CLI Visualization

The CLI view presents an ASCII representation of the pipeline with pressure indicators:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│  XEarthLayer Pipeline Status                                    Uptime: 00:15:32│
├─────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│  FUSE ──► DOWNLOAD ──► ASSEMBLE ──► ENCODE ──► CACHE ──► DONE                   │
│   12       ████░░ 847    ██░░░░ 23    ███░░░ 8    █░░░░░ 2      1,247           │
│  waiting   active        active       active      active        completed       │
│                                                                                  │
├─────────────────────────────────────────────────────────────────────────────────┤
│  Network:  ▁▂▃▅▇█▇▅▃▂  42.3 MB/s (peak: 67.1 MB/s)   Chunks: 12,847/s         │
├─────────────────────────────────────────────────────────────────────────────────┤
│  Memory Cache:  ████████░░░░░░░░  1.6 GB / 2.0 GB  (142 tiles)                  │
│                 Hit rate: 94.2%  │  Hits: 2,341  │  Misses: 142                 │
│                                                                                  │
│  Disk Cache:    ██████░░░░░░░░░░  12.4 GB / 20.0 GB  (1.2M chunks)             │
│                 Hit rate: 87.3%  │  Hits: 98,234  │  Misses: 14,312             │
├─────────────────────────────────────────────────────────────────────────────────┤
│  Errors:  3 chunks failed (0.02%)  │  Retries: 127  │  Jobs failed: 0          │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### View Model (Presentation Layer)

```rust
/// View model for CLI display
pub struct CliViewModel {
    /// Pipeline stage pressures (0.0 - 1.0)
    pub fuse_pressure: f32,
    pub download_pressure: f32,
    pub assemble_pressure: f32,
    pub encode_pressure: f32,
    pub cache_pressure: f32,

    /// Stage counts
    pub fuse_waiting: usize,
    pub downloads_active: usize,
    pub assembles_active: usize,
    pub encodes_active: usize,
    pub cache_writes_active: usize,
    pub jobs_completed: u64,

    /// Network throughput
    pub network_throughput_history: Vec<f64>,  // Last N samples for sparkline
    pub network_throughput_current: f64,
    pub network_throughput_peak: f64,
    pub chunks_per_second: f64,

    /// Cache stats
    pub memory_cache_used: usize,
    pub memory_cache_total: usize,
    pub memory_cache_entries: usize,
    pub memory_cache_hit_rate: f64,
    pub memory_cache_hits: u64,
    pub memory_cache_misses: u64,

    pub disk_cache_used: u64,
    pub disk_cache_total: u64,
    pub disk_cache_entries: u64,
    pub disk_cache_hit_rate: f64,
    pub disk_cache_hits: u64,
    pub disk_cache_misses: u64,

    /// Errors
    pub chunks_failed: u64,
    pub chunk_failure_rate: f64,
    pub retries: u64,
    pub jobs_failed: u64,

    /// Timing
    pub uptime: Duration,
}

impl CliViewModel {
    /// Create view model from telemetry snapshot
    pub fn from_snapshot(snapshot: &TelemetrySnapshot, config: &PipelineConfig) -> Self {
        Self {
            // Calculate pressure as ratio of active to max capacity
            download_pressure: snapshot.downloads_active as f32
                / config.download.max_concurrent_global as f32,
            encode_pressure: snapshot.encodes_active as f32
                / config.encoding.max_concurrent as f32,
            // ... etc
        }
    }

    /// Render pressure bar (e.g., "████░░" for 66%)
    pub fn render_pressure_bar(&self, pressure: f32, width: usize) -> String {
        let filled = (pressure * width as f32).round() as usize;
        let empty = width - filled;
        format!("{}{}", "█".repeat(filled), "░".repeat(empty))
    }

    /// Render sparkline for throughput history
    pub fn render_sparkline(&self, values: &[f64]) -> String {
        const CHARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max = values.iter().cloned().fold(f64::MIN, f64::max);
        values.iter()
            .map(|v| {
                let idx = ((v / max) * 7.0).round() as usize;
                CHARS[idx.min(7)]
            })
            .collect()
    }
}
```

### OpenTelemetry Export Path

For future integration with external observability tools:

```rust
use tracing_subscriber::layer::SubscriberExt;
use tracing_opentelemetry::OpenTelemetryLayer;

/// Configure tracing with optional OpenTelemetry export
pub fn init_telemetry(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    let subscriber = tracing_subscriber::registry();

    // Always add formatter for logs
    let subscriber = subscriber.with(
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_level(true)
    );

    // Optionally add OpenTelemetry exporter
    if config.otel_enabled {
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(opentelemetry_otlp::new_exporter().tonic())
            .install_batch(opentelemetry::runtime::Tokio)?;

        let subscriber = subscriber.with(OpenTelemetryLayer::new(tracer));
    }

    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}
```

### Telemetry Configuration

```rust
pub struct TelemetryConfig {
    /// Enable/disable telemetry collection
    pub enabled: bool,

    /// CLI display refresh rate
    pub cli_refresh_interval: Duration,

    /// Rolling window size for rate calculations
    pub rate_window_seconds: u64,

    /// OpenTelemetry export settings
    pub otel_enabled: bool,
    pub otel_endpoint: Option<String>,

    /// Log level for structured logging
    pub log_level: tracing::Level,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cli_refresh_interval: Duration::from_secs(1),
            rate_window_seconds: 10,
            otel_enabled: false,
            otel_endpoint: None,
            log_level: tracing::Level::INFO,
        }
    }
}
```

### Future Considerations

1. **Time-Series Database** - If historical analysis becomes important, the OTel export path allows integration with Prometheus, InfluxDB, or similar without code changes

2. **GUI View** - A graphical view showing tiles on a world map can consume the same `TelemetrySnapshot`

3. **Distributed Tracing** - The span-based instrumentation supports distributed tracing if we ever need to debug across multiple processes

4. **Alerting** - Thresholds on metrics (error rate, queue depth) could trigger alerts

---

## Implementation Notes

This section documents design decisions made during implementation that deviate from or extend the original design.

### Dependency Inversion for Tokio Runtime

**Decision**: Abstract Tokio-specific operations behind traits to follow the Dependency Inversion Principle (DIP).

**Rationale**: The original design had pipeline stages directly calling Tokio primitives (`spawn_blocking`, `JoinSet`, `timeout`). This created tight coupling that made unit testing difficult without a Tokio runtime.

**Implementation**:

```rust
// Executor traits in pipeline/executor.rs
pub trait BlockingExecutor: Send + Sync + 'static {
    fn execute_blocking<F, R>(&self, f: F)
        -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where F: FnOnce() -> R + Send + 'static, R: Send + 'static;
}

pub trait ConcurrentRunner: Send + Sync + 'static {
    fn run_concurrent<F, R>(&self, futures: Vec<F>) -> ConcurrentResults<R>
    where F: Future<Output = R> + Send + 'static, R: Send + 'static;
}

pub trait Timer: Send + Sync + 'static {
    fn timeout<F, R>(&self, duration: Duration, future: F)
        -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}
```

**Implementations**:
- `TokioExecutor` - Production implementation using Tokio primitives
- `SyncExecutor` - Test implementation that executes synchronously (no runtime needed)

**Benefits**:
- Pipeline stages can be unit tested without `#[tokio::test]`
- Opens door for alternative runtime support (async-std, etc.)
- Clearer separation between business logic and runtime mechanics

### Adapter Pattern for Existing Types

**Decision**: Use the Adapter pattern to bridge pipeline traits to existing implementations rather than modifying existing code.

**Rationale**: The existing `Provider`, `TextureEncoder`, and cache implementations are stable and well-tested. Modifying them to implement pipeline traits would risk regressions and violate the Open-Closed Principle.

**Implementation**: Each adapter in its own module under `pipeline/adapters/`:

```
pipeline/adapters/
├── mod.rs                 # Re-exports all adapters
├── provider.rs            # ProviderAdapter
├── texture_encoder.rs     # TextureEncoderAdapter<E>
├── memory_cache.rs        # MemoryCacheAdapter
└── disk_cache.rs          # DiskCacheAdapter, NullDiskCache
```

**Adapter Summary**:

| Adapter | Pipeline Trait | Wraps | Key Responsibility |
|---------|----------------|-------|-------------------|
| `ProviderAdapter` | `ChunkProvider` | `Arc<dyn Provider>` | Async wrapper via `spawn_blocking`, error mapping with retry semantics |
| `TextureEncoderAdapter<E>` | `TextureEncoderAsync` | `E: TextureEncoder` | Error type conversion |
| `MemoryCacheAdapter` | `MemoryCache` | `cache::MemoryCache` | Inject provider/format into `CacheKey` |
| `DiskCacheAdapter` | `DiskCache` | Filesystem | Async chunk-level caching with `tokio::fs` |
| `NullDiskCache` | `DiskCache` | Nothing | No-op for testing/disabled cache |

**Error Mapping Strategy** (ProviderAdapter):

| ProviderError | ChunkDownloadError | Rationale |
|---------------|-------------------|-----------|
| `HttpError` | Retryable | Network issues are often transient |
| `UnsupportedCoordinates` | Permanent | Retrying won't help |
| `UnsupportedZoom` | Permanent | Retrying won't help |
| `InvalidResponse` | Permanent | Server returned bad data |
| `ProviderSpecific` | Permanent | Conservative default |

### Chunk-Level Disk Cache

**Decision**: The pipeline's `DiskCache` trait operates at chunk granularity (256×256 images) rather than tile granularity (4096×4096 DDS files).

**Rationale**: Per the architecture design, chunk-level caching provides:
- Better failure resilience (partial progress preserved)
- Higher effective capacity (JPEG chunks vs DDS tiles)
- Faster cold starts (reuse chunks across tiles)

**Directory Structure**:
```
{cache_dir}/chunks/{provider}/{zoom}/{tile_row}/{tile_col}/{chunk_row}_{chunk_col}.jpg
```

**Note**: This is separate from the existing `cache::DiskCache` which stores complete DDS tiles. The two caches serve different purposes:
- Chunk cache: Persists downloaded imagery across sessions
- Tile cache: Would cache assembled DDS files (not currently implemented in pipeline)

### Pipeline Context Type Parameters

**Decision**: `PipelineContext` uses 5 type parameters rather than trait objects.

```rust
pub struct PipelineContext<P, E, M, D, X>
where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
```

**Rationale**:
- Enables monomorphization for better performance
- Allows different concrete types in tests vs production
- Type safety at compile time

**Trade-off**: More verbose type signatures, but this is contained within the pipeline module.

### AsyncPassthroughFS Design (Phase 2)

**Decision**: Bridge synchronous FUSE callbacks to async pipeline using `Handle::block_on()` with oneshot channels.

**Rationale**: The FUSE `read()` callback is synchronous and must return data before the function returns. We need to:
1. Submit work to the async Tokio runtime
2. Wait for the result without spinning
3. Handle timeouts gracefully

**Implementation**:

```rust
// Key types in fuse/async_passthrough.rs
pub struct DdsRequest {
    pub job_id: JobId,
    pub tile: TileCoord,
    pub result_tx: oneshot::Sender<DdsResponse>,
}

pub type DdsHandler = Arc<dyn Fn(DdsRequest) + Send + Sync>;

// The bridge pattern
fn request_dds(&self, coords: &DdsFilename) -> Vec<u8> {
    let (tx, rx) = oneshot::channel();
    let request = DdsRequest { job_id, tile, result_tx: tx };

    // Submit to async runtime (doesn't block)
    (self.dds_handler)(request);

    // Block this FUSE thread waiting for result
    let result = self.runtime_handle.block_on(async {
        tokio::time::timeout(self.generation_timeout, rx).await
    });

    // Handle result or return placeholder on timeout
    ...
}
```

**Pipeline Runner**: `create_dds_handler()` in `pipeline/runner.rs` creates a `DdsHandler` that:
1. Spawns an async task on the Tokio runtime
2. Calls `process_tile()` through the pipeline stages
3. Sends result back via oneshot channel

**Coordinate Conversion**: DDS filenames use chunk-level coordinates (zoom + 4), but tiles use tile-level coordinates:
- Tile row = chunk row / 16
- Tile col = chunk col / 16
- Tile zoom = chunk zoom - 4

**Virtual Inodes**: DDS files that don't exist on disk get virtual inodes starting at `0x1000_0000_0000_0000` to distinguish them from real file inodes.

**Testing Note**: Tests that call `request_dds()` cannot use `#[tokio::test]` because `block_on()` panics when called from within a runtime. Tests create a new runtime manually instead.

### Module Organization

**Decision**: Pipeline module organized by responsibility:

```
pipeline/
├── mod.rs           # Public API exports
├── context.rs       # PipelineContext, trait definitions
├── error.rs         # ChunkResults, JobError, StageError
├── executor.rs      # BlockingExecutor, TokioExecutor, etc.
├── job.rs           # Job, JobId, JobResult, Priority
├── processor.rs     # process_job, process_tile orchestration
├── adapters/        # Adapter implementations (separate module)
└── stages/          # Pipeline stage implementations
    ├── mod.rs
    ├── download.rs
    ├── assembly.rs
    ├── encode.rs
    └── cache.rs
```

**Rationale**: Single Responsibility Principle - each file has one reason to change.

---

## Migration Path

### Phase 1: Core Pipeline ✅ Complete

1. ✅ Implement Job and Task models (`job.rs`, `error.rs`)
2. ✅ Implement async download stage with JoinSet (`stages/download.rs`)
3. ✅ Implement assembly stage with spawn_blocking (`stages/assembly.rs`)
4. ✅ Implement encode stage with spawn_blocking (`stages/encode.rs`)
5. ✅ Implement cache stage (`stages/cache.rs`)
6. ✅ Implement processor orchestration (`processor.rs`)
7. ✅ Apply DIP for Tokio runtime (`executor.rs`)
8. ✅ Create adapters for existing types (`adapters/`)
9. ✅ Unit test each stage in isolation (72 pipeline tests)

### Phase 2: FUSE Integration ✅ Complete

1. ✅ Create new AsyncPassthroughFS (`fuse/async_passthrough.rs`)
2. ✅ Implement multi-threaded FUSE dispatch with `runtime_handle.block_on()`
3. ✅ Wire up job submission via `DdsHandler` callback and oneshot channels
4. ✅ Create `create_dds_handler()` runner to connect FUSE to pipeline (`pipeline/runner.rs`)
5. ✅ Unit tests for async FUSE integration (8 tests)

### Phase 3: Full Integration ✅ Complete

1. ✅ Replace existing PassthroughFS with AsyncPassthroughFS in `XEarthLayerService`
2. ✅ Wire up adapters (ProviderAdapter, MemoryCacheAdapter, DiskCacheAdapter, TextureEncoderAdapter)
3. ✅ Add runtime management (`new()` / `with_runtime()` pattern for DI)
4. ✅ Remove old synchronous PassthroughFS implementation
5. ⏳ End-to-end testing with X-Plane (manual testing recommended)
6. ⏳ Performance benchmarking (deferred to Phase 4)

### Phase 3.1: Async HTTP & Request Coalescing ✅ Complete

This phase addressed a critical thread pool exhaustion issue discovered during X-Plane testing.

#### Problem: Thread Pool Exhaustion

During extended X-Plane sessions (~181,000+ requests), the system would deadlock. Root cause analysis revealed:

```
spawn_blocking for 256 chunks per tile
× multiple concurrent tile requests
= thread pool exhaustion → deadlock

With 10 tiles requested simultaneously:
  10 tiles × 256 chunks = 2,560 spawn_blocking calls
  Tokio's default blocking pool = 512 threads
  Result: All threads waiting on network I/O, no threads available to complete work
```

The original `ProviderAdapter` used `spawn_blocking` to wrap the blocking `reqwest::blocking` HTTP client:

```rust
// PROBLEMATIC: Each chunk download consumed a blocking thread
tokio::task::spawn_blocking(move || provider.download_chunk(row, col, zoom))
```

#### Solution: Fully Async HTTP Path

We refactored to use async reqwest throughout, eliminating `spawn_blocking` for network I/O:

1. **AsyncHttpClient trait** (`provider/http.rs`)
   ```rust
   pub trait AsyncHttpClient: Send + Sync {
       fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;
       fn post_json(&self, url: &str, json_body: &str) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;
   }
   ```

2. **AsyncProvider trait** (`provider/types.rs`)
   ```rust
   pub trait AsyncProvider: Send + Sync {
       async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ProviderError>;
       fn name(&self) -> &str;
       fn min_zoom(&self) -> u8;
       fn max_zoom(&self) -> u8;
   }
   ```

3. **Async provider implementations**
   - `AsyncBingMapsProvider<C: AsyncHttpClient>`
   - `AsyncGo2Provider<C: AsyncHttpClient>`
   - `AsyncGoogleMapsProvider<C: AsyncHttpClient>` (with async session creation)

4. **AsyncProviderAdapter** (`pipeline/adapters/provider.rs`)
   ```rust
   impl<P: AsyncProvider + 'static> ChunkProvider for AsyncProviderAdapter<P> {
       async fn download_chunk(&self, row: u32, col: u32, zoom: u8) -> Result<Vec<u8>, ChunkDownloadError> {
           // Direct async call - no spawn_blocking!
           self.provider.download_chunk(row, col, zoom).await.map_err(map_provider_error)
       }
   }
   ```

5. **AsyncProviderFactory** (`provider/factory.rs`)
   ```rust
   pub enum AsyncProviderType {
       Bing(AsyncBingMapsProvider<AsyncReqwestClient>),
       Go2(AsyncGo2Provider<AsyncReqwestClient>),
       Google(AsyncGoogleMapsProvider<AsyncReqwestClient>),
   }
   ```

#### Request Coalescing at Pipeline Level

Added `RequestCoalescer` (`pipeline/coalesce.rs`) to prevent duplicate tile processing:

```rust
pub struct RequestCoalescer {
    in_flight: Mutex<HashMap<TileCoord, broadcast::Sender<CoalescedResult>>>,
    stats: Mutex<CoalescerStats>,
}
```

When multiple FUSE requests arrive for the same tile:
1. First request registers and processes the tile
2. Subsequent requests subscribe to a broadcast channel
3. When processing completes, result is broadcast to all waiters
4. Statistics track coalescing effectiveness

Integrated in `pipeline/runner.rs`:
```rust
pub fn create_dds_handler<...>(...) -> DdsHandler {
    let coalescer = Arc::new(RequestCoalescer::new());

    Arc::new(move |request: DdsRequest| {
        // ...
        runtime_handle.spawn(async move {
            process_dds_request_coalesced(request, ..., coalescer, ...).await;
        });
    })
}
```

#### Design Decisions

| Decision | Rationale |
|----------|-----------|
| Async traits with RPITIT | Rust's `impl Future + Send` in traits avoids `async_trait` crate overhead |
| `AsyncReqwestClient` wraps `reqwest::Client` | Single connection pool shared across all downloads |
| `'static` bound on `AsyncProviderAdapter` | Required for `ChunkProvider` trait to work with async spawning |
| `broadcast` channel for coalescing | Multiple receivers can get the same result; dropping receivers doesn't affect sender |
| `Arc<P>` in `AsyncProviderAdapter` | Allows sharing provider across multiple requests |
| `from_arc()` constructor | Enables external Arc management for service integration |

#### Performance Comparison

| Aspect | Before (spawn_blocking) | After (async reqwest) |
|--------|------------------------|----------------------|
| Threads per tile | 256 (blocking) | 0 (event loop) |
| Max concurrent tiles | ~2 before exhaustion | Limited by bandwidth |
| Deadlock risk | High under load | None |
| Connection reuse | Per-call client | Shared connection pool |
| Memory per tile | ~2MB (thread stacks) | ~256KB (futures) |
| Request coalescing | At tile generator level | At pipeline level |

#### Files Modified

- `provider/http.rs` - Added `AsyncHttpClient`, `AsyncReqwestClient`
- `provider/types.rs` - Added `AsyncProvider` trait
- `provider/bing.rs` - Added `AsyncBingMapsProvider`
- `provider/go2.rs` - Added `AsyncGo2Provider`
- `provider/google.rs` - Added `AsyncGoogleMapsProvider`
- `provider/factory.rs` - Added `AsyncProviderFactory`, `AsyncProviderType`
- `pipeline/coalesce.rs` - New request coalescing module
- `pipeline/adapters/provider.rs` - Added `AsyncProviderAdapter`
- `pipeline/runner.rs` - Integrated coalescing into `create_dds_handler`
- `service/facade.rs` - Wired up async provider creation

### Phase 3.2: HTTP Concurrency Limiting ✅ Complete

This phase addressed network stack exhaustion discovered during X-Plane load testing.

#### Problem: Network Stack Exhaustion

During X-Plane scene loading, the system spawned too many concurrent HTTP requests:

```
256 chunks per tile × multiple concurrent tiles = thousands of HTTP requests
Result: Network stack exhaustion → connection failures → X-Plane crash
```

Log analysis showed 1,221 chunk download failures within 3 seconds, with errors like "error sending request" indicating the OS networking stack was overwhelmed.

#### Solution: Global HTTP Concurrency Limiter

Added `HttpConcurrencyLimiter` using Tokio semaphores to cap concurrent HTTP requests:

1. **HttpConcurrencyLimiter** (`pipeline/http_limiter.rs`)
   ```rust
   pub struct HttpConcurrencyLimiter {
       semaphore: Arc<Semaphore>,
       max_permits: usize,
       in_flight: AtomicUsize,
       peak_in_flight: AtomicUsize,
   }

   impl HttpConcurrencyLimiter {
       pub fn with_default_concurrency() -> Self {
           // CPU-aware default: min(num_cpus * 16, 256)
           let cpus = std::thread::available_parallelism()
               .map(|p| p.get())
               .unwrap_or(4);
           Self::new((cpus * 16).min(256))
       }

       pub async fn acquire(&self) -> HttpPermit<'_> {
           // Blocks until permit available
       }
   }
   ```

2. **Integration in download stage** (`pipeline/stages/download.rs`)
   ```rust
   pub async fn download_stage_with_limiter<P, D>(
       // ...existing params...
       http_limiter: Option<Arc<HttpConcurrencyLimiter>>,
   ) -> ChunkResults {
       // Each chunk download acquires permit before HTTP request
       // Permit released after response received (not after processing)
   }
   ```

3. **Injection point** (`pipeline/runner.rs`)
   ```rust
   let http_limiter = Arc::new(HttpConcurrencyLimiter::new(
       config.max_global_http_requests
   ));
   // Passed through create_dds_handler to process_tile
   ```

#### Design Decisions

| Decision | Rationale |
|----------|-----------|
| CPU-based default sizing | `min(cpus * 16, 256)` balances concurrency with OS limits |
| Semaphore-based | Natural async primitive, no busy-waiting |
| Permit per HTTP request | Granular control, cache hits don't consume permits |
| Release after response | Don't hold permits during image decoding/processing |
| Injected at runner level | Single creation point, domain-appropriate location |

#### Configuration

The `max_global_http_requests` field in `PipelineConfig` controls the limit:
- Auto-detected based on CPU count
- Formula: `min(num_cpus * 16, 256)`
- Example: 8-core system → 128 concurrent HTTP requests max

The `download.parallel` config setting was removed as it was unused and confusing to users. HTTP concurrency is now automatically tuned based on system capabilities.

#### Files Modified

- `pipeline/http_limiter.rs` - New module with `HttpConcurrencyLimiter`
- `pipeline/context.rs` - Added `max_global_http_requests` to `PipelineConfig`
- `pipeline/stages/download.rs` - Added `download_stage_with_limiter()`
- `pipeline/processor.rs` - Pass limiter through `process_tile()`
- `pipeline/runner.rs` - Create and inject limiter
- `config/file.rs` - Removed unused `download.parallel` setting
- `config/keys.rs` - Removed `DownloadParallel` config key

#### Results

After this change, X-Plane successfully loads scenery without crashes. The concurrency limiter prevents network stack exhaustion while still maintaining good throughput by allowing many concurrent requests (just not unbounded).

### Phase 4: Polish

1. Add comprehensive telemetry
2. Tune configuration defaults
3. Documentation updates
4. Performance benchmarking and comparison

---

## Open Questions

1. **Priority scheduling**: Should some tiles be higher priority? (e.g., closer to camera)

2. **Backpressure**: How do we handle overload? Reject jobs? Queue with limit?

3. **Cancellation**: If X-Plane closes a file handle, should we cancel the in-flight job?

4. **Prefetching**: Should we speculatively fetch adjacent tiles?

---

## References

- [Tokio Documentation](https://tokio.rs)
- [fuser crate](https://docs.rs/fuser)
- [Performance Bottlenecks Analysis](./performance-bottlenecks.md)
- [Original AutoOrtho](https://github.com/kubilus1/autoortho)

---

## Revision History

| Date | Change |
|------|--------|
| 2025-12-07 | Initial architecture design |
| 2025-12-07 | Added caching architecture with two-tier design (memory tiles, disk chunks) |
| 2025-12-07 | Added request coalescing design |
| 2025-12-07 | Added telemetry architecture with tracing integration and CLI visualization |
| 2025-12-07 | Phase 1 complete: Core pipeline, DIP for Tokio, adapters for existing types |
| 2025-12-07 | Added Implementation Notes section documenting design decisions |
| 2025-12-09 | Phase 2 complete: AsyncPassthroughFS, pipeline runner, FUSE integration |
| 2025-12-10 | Phase 3 complete: Full integration with XEarthLayerService, runtime DI, removed sync PassthroughFS |
| 2025-12-11 | Phase 3.1 complete: Refactored to async HTTP to fix thread pool exhaustion; added RequestCoalescer at pipeline level |
| 2025-12-14 | Phase 3.2 complete: Added HTTP concurrency limiter to prevent network stack exhaustion under load |
