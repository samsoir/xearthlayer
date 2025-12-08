//! Error types for the async pipeline.
//!
//! Errors are categorized by pipeline stage to aid in debugging and
//! to enable stage-specific error handling strategies.

use thiserror::Error;

/// Errors that can occur during job processing.
///
/// These errors represent failures at the job level. Individual chunk
/// failures during download are handled gracefully (magenta placeholders)
/// and don't necessarily cause a JobError.
#[derive(Debug, Error)]
pub enum JobError {
    /// Download stage failed catastrophically
    #[error("download stage failed: {0}")]
    DownloadFailed(#[from] StageError),

    /// Assembly stage failed
    #[error("assembly stage failed: {0}")]
    AssemblyFailed(String),

    /// Encoding stage failed
    #[error("encoding stage failed: {0}")]
    EncodingFailed(String),

    /// Cache write failed (non-fatal, logged as warning)
    #[error("cache write failed: {0}")]
    CacheFailed(String),

    /// Job was cancelled (e.g., FUSE request dropped)
    #[error("job cancelled")]
    Cancelled,

    /// Job timed out
    #[error("job timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Internal error (e.g., channel closed unexpectedly)
    #[error("internal error: {0}")]
    Internal(String),
}

/// Errors that can occur during individual pipeline stages.
///
/// These provide detailed context for debugging specific failures.
#[derive(Debug, Error)]
pub enum StageError {
    /// HTTP request failed
    #[error("HTTP error for chunk ({row}, {col}): {message}")]
    HttpError { row: u8, col: u8, message: String },

    /// Invalid or corrupt image data received
    #[error("invalid image data for chunk ({row}, {col}): {message}")]
    InvalidImageData { row: u8, col: u8, message: String },

    /// Image decoding failed
    #[error("image decode failed: {0}")]
    ImageDecodeFailed(String),

    /// DDS encoding failed
    #[error("DDS encoding failed: {0}")]
    DdsEncodeFailed(String),

    /// File I/O error
    #[error("I/O error: {0}")]
    IoError(String),

    /// Task panicked during spawn_blocking
    #[error("task panicked: {0}")]
    TaskPanicked(String),

    /// Provider returned an error
    #[error("provider error: {0}")]
    ProviderError(String),
}

/// Result of downloading chunks for a tile.
///
/// Tracks successful and failed chunks separately to enable partial
/// results with magenta placeholders for failures.
#[derive(Debug)]
pub struct ChunkResults {
    /// Successfully downloaded chunks: (row, col) -> JPEG data
    pub successes: Vec<ChunkSuccess>,

    /// Failed chunks: (row, col) with error info
    pub failures: Vec<ChunkFailure>,
}

/// A successfully downloaded chunk.
#[derive(Debug, Clone)]
pub struct ChunkSuccess {
    /// Chunk row within tile (0-15)
    pub row: u8,
    /// Chunk column within tile (0-15)
    pub col: u8,
    /// Raw image data (JPEG)
    pub data: Vec<u8>,
}

/// A chunk that failed to download after all retries.
#[derive(Debug, Clone)]
pub struct ChunkFailure {
    /// Chunk row within tile (0-15)
    pub row: u8,
    /// Chunk column within tile (0-15)
    pub col: u8,
    /// Number of attempts made
    pub attempts: u32,
    /// Last error message
    pub last_error: String,
}

impl ChunkResults {
    /// Creates a new empty ChunkResults.
    pub fn new() -> Self {
        Self {
            successes: Vec::with_capacity(256),
            failures: Vec::new(),
        }
    }

    /// Adds a successful chunk download.
    pub fn add_success(&mut self, row: u8, col: u8, data: Vec<u8>) {
        self.successes.push(ChunkSuccess { row, col, data });
    }

    /// Adds a failed chunk download.
    pub fn add_failure(&mut self, row: u8, col: u8, attempts: u32, last_error: String) {
        self.failures.push(ChunkFailure {
            row,
            col,
            attempts,
            last_error,
        });
    }

    /// Returns the number of successful downloads.
    #[inline]
    pub fn success_count(&self) -> usize {
        self.successes.len()
    }

    /// Returns the number of failed downloads.
    #[inline]
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Returns the total number of chunks processed (should be 256).
    #[inline]
    pub fn total_count(&self) -> usize {
        self.successes.len() + self.failures.len()
    }

    /// Returns true if all 256 chunks were successful.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.successes.len() == 256 && self.failures.is_empty()
    }

    /// Returns the success rate as a percentage (0.0 - 100.0).
    pub fn success_rate(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.0;
        }
        (self.successes.len() as f64 / total as f64) * 100.0
    }

    /// Gets the data for a specific chunk position, if it was successful.
    pub fn get(&self, row: u8, col: u8) -> Option<&[u8]> {
        self.successes
            .iter()
            .find(|c| c.row == row && c.col == col)
            .map(|c| c.data.as_slice())
    }
}

impl Default for ChunkResults {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_results_new() {
        let results = ChunkResults::new();
        assert_eq!(results.success_count(), 0);
        assert_eq!(results.failure_count(), 0);
        assert!(!results.is_complete());
    }

    #[test]
    fn test_chunk_results_add_success() {
        let mut results = ChunkResults::new();
        results.add_success(0, 0, vec![1, 2, 3]);
        results.add_success(0, 1, vec![4, 5, 6]);

        assert_eq!(results.success_count(), 2);
        assert_eq!(results.get(0, 0), Some(&[1u8, 2, 3][..]));
        assert_eq!(results.get(0, 1), Some(&[4u8, 5, 6][..]));
        assert_eq!(results.get(0, 2), None);
    }

    #[test]
    fn test_chunk_results_add_failure() {
        let mut results = ChunkResults::new();
        results.add_failure(5, 10, 3, "timeout".to_string());

        assert_eq!(results.failure_count(), 1);
        assert_eq!(results.failures[0].row, 5);
        assert_eq!(results.failures[0].col, 10);
        assert_eq!(results.failures[0].attempts, 3);
    }

    #[test]
    fn test_chunk_results_success_rate() {
        let mut results = ChunkResults::new();

        // Add 200 successes
        for i in 0..200 {
            results.add_success((i / 16) as u8, (i % 16) as u8, vec![0]);
        }

        // Add 56 failures
        for i in 200..256 {
            results.add_failure((i / 16) as u8, (i % 16) as u8, 1, "error".to_string());
        }

        assert_eq!(results.total_count(), 256);
        assert!((results.success_rate() - 78.125).abs() < 0.001);
    }

    #[test]
    fn test_chunk_results_is_complete() {
        let mut results = ChunkResults::new();

        // Add all 256 chunks as successes
        for row in 0..16u8 {
            for col in 0..16u8 {
                results.add_success(row, col, vec![0]);
            }
        }

        assert!(results.is_complete());
        assert_eq!(results.success_rate(), 100.0);
    }

    #[test]
    fn test_job_error_display() {
        let err = JobError::Timeout(std::time::Duration::from_secs(30));
        assert_eq!(format!("{}", err), "job timed out after 30s");

        let err = JobError::Cancelled;
        assert_eq!(format!("{}", err), "job cancelled");
    }

    #[test]
    fn test_stage_error_display() {
        let err = StageError::HttpError {
            row: 5,
            col: 10,
            message: "connection refused".to_string(),
        };
        assert_eq!(
            format!("{}", err),
            "HTTP error for chunk (5, 10): connection refused"
        );
    }
}
