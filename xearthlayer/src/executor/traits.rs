//! Core traits for the job executor framework.
//!
//! This module contains the abstract traits that define the contracts between
//! the executor framework and concrete implementations. These traits enable
//! dependency injection and testability.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Job Executor                             │
//! │                                                              │
//! │  Jobs & Tasks depend on these trait abstractions:           │
//! │  • ChunkProvider - Download imagery chunks                   │
//! │  • TextureEncoderAsync - Encode DDS textures                │
//! │  • MemoryCache - Fast tile-level caching                    │
//! │  • DiskCache - Persistent chunk-level caching               │
//! │  • BlockingExecutor - CPU-bound work execution              │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Adapter Implementations                    │
//! │                                                              │
//! │  Concrete types that implement these traits:                │
//! │  • AsyncProviderAdapter → ChunkProvider                     │
//! │  • TextureEncoderAdapter → TextureEncoderAsync              │
//! │  • ExecutorCacheAdapter → MemoryCache                       │
//! │  • ParallelDiskCache → DiskCache                            │
//! │  • TokioExecutor → BlockingExecutor                         │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use std::future::Future;
use std::pin::Pin;

// ============================================================================
// Chunk Provider Trait
// ============================================================================

/// Trait for async chunk providers.
///
/// Implementations download individual 256x256 JPEG chunks from satellite
/// imagery services (Bing, Google, etc.).
///
/// # Example
///
/// ```ignore
/// use xearthlayer::executor::ChunkProvider;
///
/// async fn download_tile<P: ChunkProvider>(provider: &P) {
///     // Download a chunk at zoom 16
///     let data = provider.download_chunk(12345, 67890, 16).await?;
///     println!("Downloaded {} bytes", data.len());
/// }
/// ```
pub trait ChunkProvider: Send + Sync + 'static {
    /// Downloads a chunk asynchronously.
    ///
    /// # Arguments
    ///
    /// * `row` - Global row coordinate at chunk resolution
    /// * `col` - Global column coordinate at chunk resolution
    /// * `zoom` - Zoom level (chunk zoom, typically tile_zoom + 4)
    ///
    /// # Returns
    ///
    /// Raw image data (JPEG) on success.
    fn download_chunk(
        &self,
        row: u32,
        col: u32,
        zoom: u8,
    ) -> impl Future<Output = Result<Vec<u8>, ChunkDownloadError>> + Send;

    /// Returns the provider name for logging.
    fn name(&self) -> &str;
}

/// Errors from chunk downloads.
#[derive(Debug, Clone)]
pub struct ChunkDownloadError {
    /// Human-readable error message.
    pub message: String,
    /// Whether this error is retryable (transient) or permanent.
    pub is_retryable: bool,
}

impl ChunkDownloadError {
    /// Creates a retryable error (transient failure like network timeout).
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_retryable: true,
        }
    }

    /// Creates a permanent error (won't succeed on retry).
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_retryable: false,
        }
    }
}

impl std::fmt::Display for ChunkDownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ChunkDownloadError {}

// ============================================================================
// Texture Encoder Trait
// ============================================================================

/// Trait for async texture encoding.
///
/// Wraps texture encoders for use with `spawn_blocking`. The actual encoding
/// is CPU-intensive and should not block the async runtime.
pub trait TextureEncoderAsync: Send + Sync + 'static {
    /// Encodes an RGBA image to DDS format.
    ///
    /// This is expected to be CPU-intensive and should be called via
    /// `BlockingExecutor::execute_blocking`.
    fn encode(&self, image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError>;

    /// Returns the expected DDS file size for given dimensions.
    ///
    /// Used for pre-allocating buffers and reporting file sizes to FUSE.
    fn expected_size(&self, width: u32, height: u32) -> usize;
}

/// Errors from texture encoding.
#[derive(Debug, Clone)]
pub struct TextureEncodeError {
    /// Human-readable error message.
    pub message: String,
}

impl TextureEncodeError {
    /// Creates a new texture encode error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TextureEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TextureEncodeError {}

// ============================================================================
// Memory Cache Trait
// ============================================================================

/// Trait for memory cache (tile-level).
///
/// Memory cache stores complete DDS tiles for fast repeated access.
/// Operations are async to support non-blocking cache implementations
/// that are safe to use from async contexts.
///
/// # Important
///
/// Implementations MUST be async-safe and not block the calling thread.
/// Using `std::sync::Mutex` or other blocking primitives in async code
/// can cause deadlocks by starving the Tokio runtime.
pub trait MemoryCache: Send + Sync + 'static {
    /// Gets a tile from the cache.
    ///
    /// Returns `Some(data)` if the tile is cached, `None` otherwise.
    fn get(&self, row: u32, col: u32, zoom: u8) -> impl Future<Output = Option<Vec<u8>>> + Send;

    /// Stores a tile in the cache.
    ///
    /// Eviction of old entries happens automatically based on the
    /// cache's eviction policy.
    fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) -> impl Future<Output = ()> + Send;

    /// Returns current cache size in bytes.
    fn size_bytes(&self) -> usize;

    /// Returns number of cached tiles.
    fn entry_count(&self) -> usize;
}

// ============================================================================
// Disk Cache Trait
// ============================================================================

/// Trait for disk cache (chunk-level).
///
/// Disk cache stores individual JPEG chunks for persistence across sessions.
/// Operations are async since they involve disk I/O.
pub trait DiskCache: Send + Sync + 'static {
    /// Gets a chunk from the cache.
    ///
    /// # Arguments
    ///
    /// * `tile_row`, `tile_col` - Tile coordinates
    /// * `zoom` - Tile zoom level
    /// * `chunk_row`, `chunk_col` - Chunk position within tile (0-15)
    fn get(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
    ) -> impl Future<Output = Option<Vec<u8>>> + Send;

    /// Stores a chunk in the cache.
    fn put(
        &self,
        tile_row: u32,
        tile_col: u32,
        zoom: u8,
        chunk_row: u8,
        chunk_col: u8,
        data: Vec<u8>,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send;
}

// ============================================================================
// Blocking Executor Trait
// ============================================================================

/// Trait for executing blocking (CPU-bound) work off the async runtime.
///
/// This abstracts over `tokio::task::spawn_blocking` and similar mechanisms
/// in other runtimes. CPU-intensive operations like image encoding should
/// use this to avoid blocking async worker threads.
pub trait BlockingExecutor: Send + Sync + 'static {
    /// Executes a blocking closure on a thread pool.
    ///
    /// The closure runs on a dedicated thread pool to avoid blocking
    /// the async runtime's worker threads.
    ///
    /// # Type Parameters
    ///
    /// * `F` - The closure type
    /// * `R` - The return type (must be Send to cross thread boundary)
    fn execute_blocking<F, R>(
        &self,
        f: F,
    ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;
}

/// Errors that can occur during executor operations.
#[derive(Debug, Clone)]
pub enum ExecutorError {
    /// A spawned task panicked
    TaskPanicked(String),
    /// Operation timed out
    Timeout,
    /// Executor was shut down
    Shutdown,
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::TaskPanicked(msg) => write!(f, "task panicked: {}", msg),
            ExecutorError::Timeout => write!(f, "operation timed out"),
            ExecutorError::Shutdown => write!(f, "executor shut down"),
        }
    }
}

impl std::error::Error for ExecutorError {}

// ============================================================================
// Tokio Executor Implementation
// ============================================================================

/// Tokio-based implementation of executor traits.
///
/// This is the production implementation that delegates to Tokio's
/// runtime primitives.
#[derive(Clone, Default)]
pub struct TokioExecutor;

impl TokioExecutor {
    /// Creates a new Tokio executor.
    pub fn new() -> Self {
        Self
    }
}

impl BlockingExecutor for TokioExecutor {
    fn execute_blocking<F, R>(
        &self,
        f: F,
    ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        Box::pin(async move {
            tokio::task::spawn_blocking(f)
                .await
                .map_err(|e| ExecutorError::TaskPanicked(e.to_string()))
        })
    }
}

// ============================================================================
// Concurrent Runner Trait
// ============================================================================

/// Type alias for concurrent execution results to reduce type complexity.
pub type ConcurrentResults<R> = Pin<Box<dyn Future<Output = Vec<Result<R, ExecutorError>>> + Send>>;

/// Trait for running multiple futures concurrently and collecting results.
///
/// This abstracts over `tokio::task::JoinSet` and similar patterns.
pub trait ConcurrentRunner: Send + Sync + 'static {
    /// Runs multiple futures concurrently and collects their results.
    ///
    /// Results are returned as they complete (not in submission order).
    /// If a task panics, its result is an error.
    fn run_concurrent<F, R>(&self, futures: Vec<F>) -> ConcurrentResults<R>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static;
}

impl ConcurrentRunner for TokioExecutor {
    fn run_concurrent<F, R>(&self, futures: Vec<F>) -> ConcurrentResults<R>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        Box::pin(async move {
            use tokio::task::JoinSet;

            let mut set = JoinSet::new();
            for fut in futures {
                set.spawn(fut);
            }

            let mut results = Vec::with_capacity(set.len());
            while let Some(result) = set.join_next().await {
                results.push(result.map_err(|e| ExecutorError::TaskPanicked(e.to_string())));
            }
            results
        })
    }
}

// ============================================================================
// Timer Trait
// ============================================================================

/// Trait for timing operations (timeouts, delays).
pub trait Timer: Send + Sync + 'static {
    /// Wraps a future with a timeout.
    ///
    /// Returns `Err(ExecutorError::Timeout)` if the future doesn't complete
    /// within the specified duration.
    fn timeout<F, R>(
        &self,
        duration: std::time::Duration,
        future: F,
    ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static;

    /// Sleeps for the specified duration.
    fn sleep(&self, duration: std::time::Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl Timer for TokioExecutor {
    fn timeout<F, R>(
        &self,
        duration: std::time::Duration,
        future: F,
    ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        Box::pin(async move {
            tokio::time::timeout(duration, future)
                .await
                .map_err(|_| ExecutorError::Timeout)
        })
    }

    fn sleep(&self, duration: std::time::Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

// ============================================================================
// Sync Executor (Test Only)
// ============================================================================

/// Synchronous executor for testing.
///
/// Executes "blocking" work immediately on the current thread.
/// Useful for unit tests that don't need a real async runtime.
#[cfg(test)]
pub struct SyncExecutor;

#[cfg(test)]
impl BlockingExecutor for SyncExecutor {
    fn execute_blocking<F, R>(
        &self,
        f: F,
    ) -> Pin<Box<dyn Future<Output = Result<R, ExecutorError>> + Send>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let result = f();
        Box::pin(std::future::ready(Ok(result)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_download_error_retryable() {
        let err = ChunkDownloadError::retryable("network timeout");
        assert!(err.is_retryable);
        assert_eq!(err.message, "network timeout");
    }

    #[test]
    fn test_chunk_download_error_permanent() {
        let err = ChunkDownloadError::permanent("unsupported zoom");
        assert!(!err.is_retryable);
        assert_eq!(err.message, "unsupported zoom");
    }

    #[test]
    fn test_texture_encode_error() {
        let err = TextureEncodeError::new("encoding failed");
        assert_eq!(err.message, "encoding failed");
        assert_eq!(format!("{}", err), "encoding failed");
    }

    #[test]
    fn test_executor_error_display() {
        assert_eq!(
            format!("{}", ExecutorError::TaskPanicked("oops".to_string())),
            "task panicked: oops"
        );
        assert_eq!(format!("{}", ExecutorError::Timeout), "operation timed out");
        assert_eq!(format!("{}", ExecutorError::Shutdown), "executor shut down");
    }

    #[tokio::test]
    async fn test_tokio_executor_blocking() {
        let executor = TokioExecutor::new();
        let result = executor.execute_blocking(|| 42).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_tokio_executor_concurrent() {
        let executor = TokioExecutor::new();

        // Use Pin<Box<...>> to give all futures the same type
        let futures: Vec<Pin<Box<dyn Future<Output = i32> + Send>>> = vec![
            Box::pin(async { 1 }),
            Box::pin(async { 2 }),
            Box::pin(async { 3 }),
        ];

        let results: Vec<_> = executor
            .run_concurrent(futures)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Results may be in any order
        assert_eq!(results.len(), 3);
        assert!(results.contains(&1));
        assert!(results.contains(&2));
        assert!(results.contains(&3));
    }

    #[tokio::test]
    async fn test_tokio_executor_timeout_success() {
        let executor = TokioExecutor::new();

        let result = executor
            .timeout(std::time::Duration::from_secs(1), async { 42 })
            .await;

        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_tokio_executor_timeout_expires() {
        let executor = TokioExecutor::new();

        let result = executor
            .timeout(std::time::Duration::from_millis(10), async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                42
            })
            .await;

        assert!(matches!(result, Err(ExecutorError::Timeout)));
    }

    #[test]
    fn test_sync_executor_blocking() {
        // This test doesn't need #[tokio::test]!
        let executor = SyncExecutor;

        // We need to poll the future, but we can do it simply
        let future = executor.execute_blocking(|| 42);

        // Use futures::executor for a simple sync test
        let result = futures::executor::block_on(future);
        assert_eq!(result.unwrap(), 42);
    }
}
