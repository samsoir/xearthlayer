//! Executor traits for abstracting async runtime operations.
//!
//! These traits allow the pipeline to be decoupled from specific async runtimes
//! like Tokio, following the Dependency Inversion Principle. The pipeline
//! depends on these abstractions, and concrete implementations (like Tokio)
//! depend on them too.
//!
//! # Design
//!
//! ```text
//! ┌─────────────────────┐
//! │   Pipeline Stages   │  (High-level policy)
//! │                     │
//! │  - download_stage   │
//! │  - assembly_stage   │
//! │  - encode_stage     │
//! └─────────┬───────────┘
//!           │ depends on
//!           ▼
//! ┌─────────────────────┐
//! │  Executor Traits    │  (Abstractions)
//! │                     │
//! │  - BlockingExecutor │
//! │  - ConcurrentRunner │
//! └─────────┬───────────┘
//!           │ implemented by
//!           ▼
//! ┌─────────────────────┐
//! │  TokioExecutor      │  (Low-level detail)
//! │                     │
//! │  - spawn_blocking   │
//! │  - JoinSet          │
//! └─────────────────────┘
//! ```

use std::future::Future;
use std::pin::Pin;

/// Trait for executing blocking (CPU-bound) work off the async runtime.
///
/// This abstracts over `tokio::task::spawn_blocking` and similar mechanisms
/// in other runtimes.
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

/// Tokio-based implementation of executor traits.
///
/// This is the production implementation that delegates to Tokio's
/// runtime primitives.
#[derive(Clone, Default)]
pub struct TokioExecutor;

impl TokioExecutor {
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
    fn test_executor_error_display() {
        let err = ExecutorError::TaskPanicked("oops".to_string());
        assert_eq!(format!("{}", err), "task panicked: oops");

        let err = ExecutorError::Timeout;
        assert_eq!(format!("{}", err), "operation timed out");
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
