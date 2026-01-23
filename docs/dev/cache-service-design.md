# Cache Service Architecture

> **Status**: ✅ Implemented
> **Created**: 2026-01-23
> **Author**: Sam de Freyssinet and Claude

## Problem Statement

The current cache system is brittle because it lacks clear domain boundaries:

1. **Scattered Ownership**: Cache eviction daemon is wired up in `run.rs`, dependent on CLI startup sequence, TUI mode, and mount order.

2. **Implicit Dependencies**: The eviction daemon requires `mount_manager.get_service()` to return a service, but in TUI mode mounting happens later inside `run_with_dashboard()`. This caused a silent failure where the GC daemon never started.

3. **Leaky Abstractions**: Domain concepts (tile coordinates, chunk coordinates) are embedded in cache interfaces, making them specific to current use cases rather than reusable.

4. **Cross-Cutting Concerns**: Metrics reporting is mixed into cache operations rather than being a separate concern.

### The Bug That Triggered This Refactor

```rust
// run.rs lines 336-358
if let Some(service) = mount_manager.get_service() {
    // This only works for non-TUI mode!
    // In TUI mode, get_service() returns None because
    // mounting happens later inside run_with_dashboard()
    runtime_handle.spawn(async move {
        run_eviction_daemon(eviction_config, eviction_cancel, metrics_client).await;
    });
}
```

Result: 105GB cache with 40GB limit - GC never ran in TUI mode (the default).

## Design Goals

1. **Self-Contained Services**: Each cache provider manages its own lifecycle, including garbage collection.

2. **Generic Interface**: Cache system knows nothing about tiles, coordinates, or domain concepts. Pure key-value store.

3. **Decorator Pattern**: Domain-specific logic (key translation, metrics) lives in decorators, not the cache.

4. **Bootstrap Pattern**: Application initialization happens in a well-defined sequence, separate from CLI/GUI front controllers.

5. **Interface Segregation**: Cache doesn't know about metrics. Decorators inject cross-cutting concerns.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                    Application Bootstrap                        │
│  Creates and wires all services in correct initialization order │
└──────────────────────────┬──────────────────────────────────────┘
                           │
        ┌──────────────────┼──────────────────┐
        ▼                  ▼                  ▼
┌───────────────┐  ┌───────────────┐  ┌───────────────┐
│ CacheService  │  │ CacheService  │  │    Runtime    │
│   (Memory)    │  │    (Disk)     │  │   (Executor)  │
│               │  │ [owns GC]     │  │               │
└───────┬───────┘  └───────┬───────┘  └───────────────┘
        │                  │
        │  Generic Cache trait (String key, Vec<u8> value)
        │
┌───────┴──────────────────┴────────────────────────────┐
│                  Domain Decorators                    │
│                                                       │
│  TileCacheClient          ChunkCacheClient            │
│  • TileCoord → key        • ChunkCoord → key          │
│  • Metrics injection      • Metrics injection         │
└───────────────────────────────────────────────────────┘
        │
        ▼
┌───────────────────────────────────────────────────────┐
│                    Consumers                          │
│  ExecutorDaemon, DownloadChunksTask, FUSE, etc.       │
└───────────────────────────────────────────────────────┘
```

## Core Interfaces

### Cache Trait

The fundamental cache interface. All cache providers implement this.

```rust
/// Generic cache interface for key-value storage.
///
/// Providers implement this trait to offer caching capabilities.
/// The interface is intentionally minimal and domain-agnostic.
#[async_trait]
pub trait Cache: Send + Sync {
    /// Store a value with the given key.
    async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), CacheError>;

    /// Retrieve a value by key. Returns None if not found.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;

    /// Delete a value by key. Returns true if key existed.
    async fn delete(&self, key: &str) -> Result<bool, CacheError>;

    /// Check if a key exists without retrieving the value.
    async fn contains(&self, key: &str) -> Result<bool, CacheError>;

    /// Update the maximum cache size. Provider handles eviction if needed.
    async fn set_max_size(&self, size_bytes: u64) -> Result<(), CacheError>;

    /// Trigger garbage collection manually.
    /// For providers with automatic GC, this may be a no-op.
    async fn gc(&self) -> Result<GcResult, CacheError>;
}

/// Result of a garbage collection operation.
#[derive(Debug, Clone, Default)]
pub struct GcResult {
    pub entries_removed: usize,
    pub bytes_freed: u64,
    pub duration_ms: u64,
}

/// Cache operation errors.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Cache is shutting down")]
    ShuttingDown,

    #[error("Key too large: {0} bytes (max: {1})")]
    KeyTooLarge(usize, usize),

    #[error("Value too large: {0} bytes (max: {1})")]
    ValueTooLarge(usize, usize),
}
```

### Cache Provider Lifecycle

Providers are started with configuration and manage their own shutdown.

```rust
/// Configuration for creating a cache provider.
pub struct CacheConfig {
    /// Maximum size in bytes.
    pub max_size_bytes: u64,

    /// Provider-specific settings.
    pub provider: ProviderConfig,
}

pub enum ProviderConfig {
    Memory {
        /// Time-to-live for entries (optional).
        ttl: Option<Duration>,
    },
    Disk {
        /// Directory for cache storage.
        directory: PathBuf,
        /// Interval between GC checks.
        gc_interval: Duration,
        /// Provider name for subdirectory structure.
        provider_name: String,
    },
}

/// A running cache service that can be shut down.
pub struct CacheService {
    cache: Arc<dyn Cache>,
    shutdown: CancellationToken,
}

impl CacheService {
    /// Start a new cache service with the given configuration.
    pub async fn start(config: CacheConfig) -> Result<Self, CacheError>;

    /// Get a reference to the cache for creating clients.
    pub fn cache(&self) -> Arc<dyn Cache>;

    /// Shutdown the cache service gracefully.
    pub async fn shutdown(self);
}
```

### Domain Decorators

Decorators translate domain concepts to generic cache operations.

```rust
/// Cache client for DDS tile storage.
///
/// Translates TileCoord to cache keys and optionally reports metrics.
pub struct TileCacheClient {
    cache: Arc<dyn Cache>,
    metrics: Option<MetricsClient>,
}

impl TileCacheClient {
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache, metrics: None }
    }

    pub fn with_metrics(cache: Arc<dyn Cache>, metrics: MetricsClient) -> Self {
        Self { cache, metrics: Some(metrics) }
    }

    /// Get a tile from cache.
    pub async fn get(&self, tile: &TileCoord) -> Option<Vec<u8>> {
        let key = Self::tile_to_key(tile);
        match self.cache.get(&key).await {
            Ok(Some(data)) => {
                if let Some(ref m) = self.metrics {
                    m.memory_cache_hit();
                }
                Some(data)
            }
            Ok(None) => {
                if let Some(ref m) = self.metrics {
                    m.memory_cache_miss();
                }
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "Cache get failed");
                None
            }
        }
    }

    /// Store a tile in cache.
    pub async fn set(&self, tile: &TileCoord, data: Vec<u8>) {
        let key = Self::tile_to_key(tile);
        let size = data.len() as u64;
        if let Err(e) = self.cache.set(&key, data).await {
            tracing::warn!(error = %e, "Cache set failed");
        }
        // Metrics for cache size updates handled by provider
    }

    fn tile_to_key(tile: &TileCoord) -> String {
        format!("tile:{}:{}:{}", tile.zoom, tile.row, tile.col)
    }
}

/// Cache client for JPEG chunk storage.
pub struct ChunkCacheClient {
    cache: Arc<dyn Cache>,
    metrics: Option<MetricsClient>,
}

impl ChunkCacheClient {
    // Similar pattern for chunk coordinates
    fn chunk_to_key(
        tile_row: u32, tile_col: u32, zoom: u8,
        chunk_row: u8, chunk_col: u8
    ) -> String {
        format!("chunk:{}:{}:{}:{}:{}", zoom, tile_row, tile_col, chunk_row, chunk_col)
    }
}
```

## Provider Implementations

### Memory Cache Provider

Wraps moka with automatic LRU eviction.

```rust
/// Memory cache provider using moka.
///
/// Moka handles LRU eviction automatically, so gc() is a no-op.
pub struct MemoryCacheProvider {
    cache: moka::future::Cache<String, Vec<u8>>,
}

impl MemoryCacheProvider {
    pub fn new(max_size_bytes: u64) -> Self {
        let cache = moka::future::Cache::builder()
            .weigher(|_key: &String, value: &Vec<u8>| value.len() as u32)
            .max_capacity(max_size_bytes)
            .build();
        Self { cache }
    }
}

#[async_trait]
impl Cache for MemoryCacheProvider {
    async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), CacheError> {
        self.cache.insert(key.to_string(), value).await;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Ok(self.cache.get(key).await)
    }

    async fn gc(&self) -> Result<GcResult, CacheError> {
        // Moka handles eviction automatically
        self.cache.run_pending_tasks().await;
        Ok(GcResult::default())
    }

    // ... other methods
}
```

### Disk Cache Provider

Manages its own GC daemon internally.

```rust
/// Disk cache provider with internal garbage collection.
///
/// Spawns a background task for periodic eviction when started.
/// The GC task is owned by the provider and cancelled on shutdown.
pub struct DiskCacheProvider {
    directory: PathBuf,
    max_size_bytes: u64,
    gc_handle: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
}

impl DiskCacheProvider {
    /// Start the disk cache provider with GC daemon.
    pub async fn start(config: DiskCacheConfig) -> Result<Self, CacheError> {
        let shutdown = CancellationToken::new();

        // Spawn internal GC daemon
        let gc_shutdown = shutdown.clone();
        let gc_dir = config.directory.clone();
        let gc_max_size = config.max_size_bytes;
        let gc_interval = config.gc_interval;

        let gc_handle = tokio::spawn(async move {
            Self::run_gc_daemon(gc_dir, gc_max_size, gc_interval, gc_shutdown).await;
        });

        Ok(Self {
            directory: config.directory,
            max_size_bytes: config.max_size_bytes,
            gc_handle: Some(gc_handle),
            shutdown,
        })
    }

    /// Internal GC daemon - runs until shutdown.
    async fn run_gc_daemon(
        directory: PathBuf,
        max_size_bytes: u64,
        interval: Duration,
        shutdown: CancellationToken,
    ) {
        tracing::info!(
            dir = %directory.display(),
            max_bytes = max_size_bytes,
            interval_secs = interval.as_secs(),
            "Disk cache GC daemon started"
        );

        // Initial GC check
        if let Err(e) = Self::run_gc_cycle(&directory, max_size_bytes).await {
            tracing::warn!(error = %e, "Initial GC cycle failed");
        }

        // Periodic GC loop
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Disk cache GC daemon shutting down");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = Self::run_gc_cycle(&directory, max_size_bytes).await {
                        tracing::warn!(error = %e, "GC cycle failed");
                    }
                }
            }
        }
    }

    /// Shutdown the provider, stopping the GC daemon.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(handle) = self.gc_handle.take() {
            let _ = handle.await;
        }
    }
}
```

## Application Bootstrap

The bootstrap pattern separates application construction from front controllers (CLI, GUI).

```rust
/// XEarthLayer application with all services wired together.
pub struct XEarthLayerApp {
    memory_cache: CacheService,
    disk_cache: CacheService,
    runtime: XEarthLayerRuntime,
    // ... other services
}

impl XEarthLayerApp {
    /// Build and start the application with configuration.
    pub async fn start(config: AppConfig) -> Result<Self, AppError> {
        // 1. Start cache services first (no dependencies)
        let memory_cache = CacheService::start(CacheConfig {
            max_size_bytes: config.cache.memory_size,
            provider: ProviderConfig::Memory { ttl: None },
        }).await?;

        let disk_cache = CacheService::start(CacheConfig {
            max_size_bytes: config.cache.disk_size,
            provider: ProviderConfig::Disk {
                directory: config.cache.directory.clone(),
                gc_interval: Duration::from_secs(60),
                provider_name: config.provider.name.clone(),
            },
        }).await?;

        // 2. Create cache clients with decorators
        let tile_cache = TileCacheClient::with_metrics(
            memory_cache.cache(),
            metrics_client.clone(),
        );

        let chunk_cache = ChunkCacheClient::with_metrics(
            disk_cache.cache(),
            metrics_client.clone(),
        );

        // 3. Start runtime with cache clients
        let runtime = RuntimeBuilder::new()
            .with_tile_cache(Arc::new(tile_cache))
            .with_chunk_cache(Arc::new(chunk_cache))
            // ... other dependencies
            .build()?;

        Ok(Self {
            memory_cache,
            disk_cache,
            runtime,
        })
    }

    /// Shutdown all services gracefully.
    pub async fn shutdown(self) {
        // Shutdown in reverse order of startup
        self.runtime.shutdown().await;
        self.disk_cache.shutdown().await;
        self.memory_cache.shutdown().await;
    }
}
```

The CLI becomes thin:

```rust
// xearthlayer-cli/src/commands/run.rs
pub fn run(args: RunArgs) -> Result<(), CliError> {
    let config = load_config()?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let app = XEarthLayerApp::start(config).await?;

        // Wait for shutdown signal
        wait_for_shutdown().await;

        app.shutdown().await;
        Ok(())
    })
}
```

## Migration Path

### Phase 1: Create Cache Service Infrastructure
- Add `cache/service.rs` with `Cache` trait
- Add `cache/memory/provider.rs` wrapping moka
- Add `cache/disk/provider.rs` with internal GC
- Keep existing code working

### Phase 2: Create Decorators
- Add `TileCacheClient` for memory cache access
- Add `ChunkCacheClient` for disk cache access
- Wire decorators to existing consumers

### Phase 3: Bootstrap Pattern
- Create `XEarthLayerApp` builder
- Move service initialization out of `run.rs`
- Remove external GC daemon wiring from CLI

### Phase 4: Cleanup
- Remove old `DiskCacheAdapter`, `ExecutorCacheAdapter`
- Remove eviction daemon code from `run.rs`
- Update documentation

## Testing Strategy

1. **Unit Tests**: Each cache provider tested in isolation
2. **Integration Tests**: Cache service lifecycle (start, operations, shutdown)
3. **Decorator Tests**: Key translation, metrics emission
4. **End-to-End**: Full bootstrap → operation → shutdown cycle

## References

- Current eviction daemon: `xearthlayer/src/cache/disk_eviction.rs`
- Current memory cache: `xearthlayer/src/cache/memory.rs`
- Current adapters: `xearthlayer/src/executor/adapters/`
- Moka documentation: https://docs.rs/moka/
