# XEarthLayer Network Statistics

## Overview

XEarthLayer tracks network download statistics in real-time, providing visibility into bandwidth usage, download performance, and error rates during operation. Statistics are logged periodically and can help diagnose performance issues or monitor network health during flight simulation sessions.

## Architecture

### Component Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    XEarthLayerService                       │
│                     (service/facade.rs)                     │
└──────────────────────┬──────────────────────────────────────┘
                       │ creates
                       ▼
         ┌─────────────────────────────┐
         │   Arc<NetworkStats>         │
         │   (orchestrator/stats.rs)   │
         └─────────────┬───────────────┘
                       │
            ┌──────────┴──────────┐
            │                     │
            ▼                     ▼
   ┌────────────────────┐ ┌─────────────────────┐
   │  TileOrchestrator  │ │ NetworkStatsLogger  │
   │                    │ │                     │
   │ Records:           │ │ Reads:              │
   │ • Chunk success    │ │ • Snapshot()        │
   │ • Chunk failure    │ │                     │
   │ • Retry attempts   │ │ Logs every 60s      │
   └────────────────────┘ └─────────────────────┘
```

### Data Flow

1. **Recording**: `TileOrchestrator` records download events to shared `NetworkStats`
2. **Aggregation**: `NetworkStats` maintains thread-safe counters and speed calculations
3. **Reporting**: `NetworkStatsLogger` periodically reads snapshots and logs metrics

## Tracked Metrics

### NetworkStats Fields

| Metric | Type | Description |
|--------|------|-------------|
| `bytes_downloaded` | `AtomicU64` | Total bytes downloaded this session |
| `chunks_downloaded` | `AtomicU64` | Number of successful chunk downloads |
| `chunks_failed` | `AtomicU64` | Number of chunks that failed after all retries |
| `retries` | `AtomicU64` | Total retry attempts (not counting initial attempt) |
| `activity_tracker` | `RwLock<ActivityTracker>` | Tracks active time and peak speed |

### Derived Metrics

| Metric | Calculation | Description |
|--------|-------------|-------------|
| `active_time_secs` | Accumulated active time | Time spent actively downloading (excludes idle) |
| `avg_bytes_per_sec` | `bytes / active_time_secs` | Average download speed during active periods |
| `peak_bytes_per_sec` | Max observed speed | Highest download speed reached |

### Active Time Tracking

The `ActivityTracker` accumulates download time only during active periods, providing accurate average speed calculations that aren't diluted by idle time:

```rust
struct ActivityTracker {
    /// Peak speed in bytes per second
    peak_bytes_per_sec: f64,
    /// Bytes at last sample (for peak speed calculation)
    last_sample_bytes: u64,
    /// Time of last sample (for peak speed calculation)
    last_sample_time: Instant,
    /// Total accumulated active download time in seconds
    active_time_secs: f64,
    /// Time of last activity (for detecting idle gaps)
    last_activity_time: Option<Instant>,
    /// Idle threshold - if no activity for this duration, stop counting time
    idle_threshold_secs: f64,  // Default: 2 seconds
}
```

**How it works**:
1. Each `record_chunk_success()` updates the activity tracker
2. Time since last activity is added to `active_time_secs` only if within the idle threshold (2 seconds)
3. Gaps longer than 2 seconds are not counted (user was idle, panning camera, etc.)
4. Average speed = total bytes / active time, giving accurate throughput measurements
5. Peak speed is calculated from 0.5-second sampling windows

## Recording Points

### TileOrchestrator Integration

The orchestrator records statistics at key points in the download loop:

```rust
// In download_chunks_parallel()
for attempt in 0..=max_retries {
    // Record retry (not on first attempt)
    if attempt > 0 {
        if let Some(ref stats) = network_stats {
            stats.record_retry();
        }
    }

    match provider.download_chunk(global_row, global_col, zoom) {
        Ok(chunk_data) => {
            // Record success with byte count
            if let Some(ref stats) = network_stats {
                stats.record_chunk_success(chunk_data.len());
            }
            break;
        }
        Err(_) if attempt < max_retries => {
            // Will retry
            continue;
        }
        Err(_) => {
            // Final failure
            if let Some(ref stats) = network_stats {
                stats.record_chunk_failure();
            }
            break;
        }
    }
}
```

### Recording Methods

```rust
impl NetworkStats {
    /// Record a successful chunk download.
    /// Updates bytes downloaded, chunk count, and peak speed tracking.
    pub fn record_chunk_success(&self, bytes: usize);

    /// Record a chunk that failed after all retries exhausted.
    pub fn record_chunk_failure(&self);

    /// Record a retry attempt.
    pub fn record_retry(&self);

    /// Get a snapshot of current statistics.
    pub fn snapshot(&self) -> NetworkStatsSnapshot;
}
```

## Periodic Logging

### NetworkStatsLogger

A background thread logs statistics at configurable intervals (default: 60 seconds).

```rust
pub struct NetworkStatsLogger {
    /// Handle to the logger thread
    thread_handle: Option<JoinHandle<()>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl NetworkStatsLogger {
    /// Start the network stats logger thread.
    pub fn start(
        stats: Arc<NetworkStats>,
        logger: Arc<dyn Logger>,
        interval_secs: u64,
    ) -> Self;
}
```

### Log Output Format

When there has been download activity, the logger outputs:

```
[NET] Downloaded: 256 chunks (45.2 MB), avg: 2.8 MB/s, peak: 5.1 MB/s, retries: 12, errors: 2
```

| Field | Description |
|-------|-------------|
| `chunks` | Number of chunks successfully downloaded |
| `size` | Total bytes in human-readable format |
| `avg` | Average download speed over session |
| `peak` | Peak download speed observed |
| `retries` | Number of retry attempts |
| `errors` | Number of chunks that ultimately failed |

**Note**: The logger only outputs when there has been activity (non-zero chunks or failures). This prevents log spam during idle periods.

### Thread Lifecycle

**Startup**:
```rust
let network_stats_logger = Some(NetworkStatsLogger::start(
    network_stats,
    logger.clone(),
    60, // Log every 60 seconds
));
```

**Running**:
- Sleeps in 1-second intervals for responsive shutdown
- Logs stats every `interval_secs` (default: 60)
- Only logs when there's been activity

**Shutdown** (via Drop):
```rust
impl Drop for NetworkStatsLogger {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}
```

## Thread Safety

All statistics are stored using atomic types for lock-free concurrent access:

```rust
pub struct NetworkStats {
    bytes_downloaded: AtomicU64,
    chunks_downloaded: AtomicU64,
    chunks_failed: AtomicU64,
    retries: AtomicU64,
    start_time: Instant,
    peak_tracker: RwLock<PeakSpeedTracker>,
}
```

**Design decisions**:
- `AtomicU64` for counters: Lock-free, efficient for high-frequency updates
- `RwLock` for peak tracker: Allows concurrent reads from logger, exclusive writes from orchestrator
- `Ordering::Relaxed` for counters: Sufficient for statistics (exact ordering not critical)
- Thread-safe `Send + Sync` implementation for sharing across threads

## Service Integration

### Wiring in XEarthLayerService

```rust
impl XEarthLayerService {
    pub fn new(
        config: ServiceConfig,
        provider_config: ProviderConfig,
        logger: Arc<dyn Logger>,
    ) -> Result<Self, ServiceError> {
        // Create network stats tracker
        let network_stats = Arc::new(NetworkStats::new());

        // Create orchestrator with network stats
        let orchestrator = TileOrchestrator::with_config(provider, *config.download())
            .with_network_stats(network_stats.clone());

        // ... create generators, cache, etc. ...

        // Start network stats logger
        let network_stats_logger = Some(NetworkStatsLogger::start(
            network_stats,
            logger.clone(),
            60,
        ));

        Ok(Self {
            // ...
            network_stats_logger,
        })
    }
}
```

### Lifetime Management

The `NetworkStatsLogger` is stored in `XEarthLayerService`:

```rust
pub struct XEarthLayerService {
    // ... other fields ...

    /// Network stats logger (keeps logger thread alive)
    #[allow(dead_code)]
    network_stats_logger: Option<NetworkStatsLogger>,
}
```

When `XEarthLayerService` is dropped:
1. `NetworkStatsLogger::drop()` is called
2. Shutdown signal is sent to background thread
3. Thread joins and terminates cleanly

## Speed Formatting

Speeds are formatted for human readability:

```rust
fn format_speed(bytes_per_sec: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;

    if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.1} KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}
```

**Examples**:
- `5242880.0` → `"5.0 MB/s"`
- `153600.0` → `"150.0 KB/s"`
- `512.0` → `"512 B/s"`

## Performance Considerations

### Low Overhead Design

- **Atomic operations**: Counter updates are ~1 nanosecond
- **No allocation**: Recording methods don't allocate memory
- **Lazy peak calculation**: Peak speed only calculated when window expires
- **Infrequent logging**: 60-second intervals minimize I/O overhead

### Memory Usage

```
NetworkStats:
  - 4 × AtomicU64 = 32 bytes
  - Instant = 16 bytes
  - RwLock<PeakSpeedTracker> ≈ 56 bytes
  Total: ~104 bytes

NetworkStatsLogger:
  - Option<JoinHandle> = 8 bytes
  - Arc<AtomicBool> = 8 bytes
  Total: ~16 bytes (plus thread stack)
```

## Example Session

### Typical Flight Session Output

```
2025-11-27T10:15:32Z INFO  [NET] Downloaded: 256 chunks (45.2 MB), avg: 2.8 MB/s, peak: 5.1 MB/s, retries: 3, errors: 0
2025-11-27T10:16:32Z INFO  [NET] Downloaded: 512 chunks (91.8 MB), avg: 3.1 MB/s, peak: 5.1 MB/s, retries: 5, errors: 0
2025-11-27T10:17:32Z INFO  [NET] Downloaded: 768 chunks (138.4 MB), avg: 3.0 MB/s, peak: 5.3 MB/s, retries: 8, errors: 1
```

### Interpreting the Output

- **Chunk count**: 256 chunks = 1 tile (16×16 grid)
- **Retries increasing**: Network may be congested
- **Peak vs Average**: Peak higher suggests bursty downloads
- **Errors**: Usually transient; tile generation falls back to placeholders

## Comparison with Cache Statistics

| Aspect | Network Stats | Cache Stats |
|--------|--------------|-------------|
| **Scope** | Download activity only | Cache hits/misses/evictions |
| **Counters** | Cumulative (session) | Cumulative (session) |
| **Log prefix** | `[NET]` | `[CACHE]` |
| **Interval** | 60 seconds | 60 seconds |
| **Thread** | `network-stats` | Part of `CacheStatsLogger` |

Both logging systems use the same interval and follow similar patterns for consistency in log output.

## Potential Future Enhancements

1. **Per-provider stats**: Track download metrics separately for Bing vs Google
2. **Histogram of download times**: Track distribution of chunk download latencies
3. **Bandwidth throttling**: Use stats to implement rate limiting
4. **Export to file**: Write stats to JSON for external analysis
5. **Stats reset command**: Allow resetting counters mid-session
6. **Real-time dashboard**: WebSocket endpoint for live monitoring
