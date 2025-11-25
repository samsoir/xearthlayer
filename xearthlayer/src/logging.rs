//! Logging infrastructure for XEarthLayer.
//!
//! Provides structured logging with file output and console output:
//! - Writes to `logs/xearthlayer.log` (cleared on session start)
//! - Also prints to stdout for CLI tailing
//! - Multi-line pretty format for readability
//! - Configurable via RUST_LOG environment variable

use std::fs;
use std::io;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Guard that must be kept alive for the duration of logging.
///
/// Dropping this guard will flush and close the log file writer.
pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

/// Initialize logging system.
///
/// Creates logs directory if needed, clears previous log file,
/// and sets up dual output to both file and stdout.
///
/// # Arguments
///
/// * `log_dir` - Directory for log files (e.g., "logs")
/// * `log_file` - Log filename (e.g., "xearthlayer.log")
///
/// # Returns
///
/// LoggingGuard that must be kept alive for logging to work
///
/// # Errors
///
/// Returns error if log directory cannot be created or log file cannot be cleared
pub fn init_logging(log_dir: &str, log_file: &str) -> Result<LoggingGuard, io::Error> {
    // Create logs directory if it doesn't exist
    fs::create_dir_all(log_dir)?;

    // Clear previous log file by writing empty content
    // This handles both existing and non-existing files
    let log_path = Path::new(log_dir).join(log_file);
    fs::write(&log_path, "")?;

    // Create file appender with non-blocking writer
    let file_appender = tracing_appender::rolling::never(log_dir, log_file);
    let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);

    // Create file layer with pretty multi-line format
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false) // No ANSI colors in file
        .with_span_events(FmtSpan::CLOSE)
        .pretty();

    // Create stdout layer with pretty multi-line format
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stdout)
        .with_ansi(true) // ANSI colors for terminal
        .with_span_events(FmtSpan::CLOSE)
        .pretty();

    // Create env filter (defaults to INFO if RUST_LOG not set)
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Initialize global subscriber with both layers
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

/// Get default log directory path.
pub fn default_log_dir() -> &'static str {
    "logs"
}

/// Get default log file name.
pub fn default_log_file() -> &'static str {
    "xearthlayer.log"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_log_dir() -> PathBuf {
        // Use unique directory for each test to avoid conflicts
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(format!("test_logs_{}", timestamp));
        // Clean up from previous test
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_default_paths() {
        assert_eq!(default_log_dir(), "logs");
        assert_eq!(default_log_file(), "xearthlayer.log");
    }

    #[test]
    fn test_creates_directory_and_file() {
        let log_dir = test_log_dir();
        let log_dir_str = log_dir.to_str().unwrap();

        assert!(!log_dir.exists(), "Test directory should not exist yet");

        // Can't test init_logging because of global subscriber, but we can test the file operations
        fs::create_dir_all(log_dir_str).expect("Failed to create directory");
        let log_path = Path::new(log_dir_str).join("test.log");
        fs::write(&log_path, "").expect("Failed to create log file");

        assert!(log_dir.exists(), "Log directory should be created");
        assert!(log_path.exists(), "Log file should be created");
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "",
            "Log file should be empty"
        );

        // Cleanup
        fs::remove_dir_all(&log_dir).expect("Failed to cleanup");
    }

    #[test]
    fn test_clears_existing_file() {
        let log_dir = test_log_dir();
        let log_dir_str = log_dir.to_str().unwrap();

        // Create directory and write some data
        fs::create_dir_all(log_dir_str).expect("Failed to create test dir");
        let log_file = log_dir.join("test.log");
        fs::write(&log_file, "old log data").expect("Failed to write test data");

        assert_eq!(
            fs::read_to_string(&log_file).unwrap(),
            "old log data",
            "Test file should contain old data"
        );

        // Clear the file by writing empty content
        fs::write(&log_file, "").expect("Failed to clear log file");

        // File should exist but be empty
        let contents = fs::read_to_string(&log_file).expect("Failed to read log file");
        assert_eq!(contents, "", "File should be cleared");

        // Cleanup
        fs::remove_dir_all(&log_dir).expect("Failed to cleanup");
    }

    #[test]
    fn test_nested_directory_creation() {
        let log_dir = PathBuf::from("test_logs_nested/deep/nested");
        let log_dir_str = log_dir.to_str().unwrap();

        // Clean up from previous test
        let _ = fs::remove_dir_all("test_logs_nested");

        // Create nested directory
        fs::create_dir_all(log_dir_str).expect("Failed to create nested directory");

        assert!(log_dir.exists(), "Nested directory should be created");

        let log_file = log_dir.join("test.log");
        fs::write(&log_file, "").expect("Failed to create log file");
        assert!(
            log_file.exists(),
            "Log file should exist in nested directory"
        );

        // Cleanup
        fs::remove_dir_all("test_logs_nested").expect("Failed to cleanup");
    }

    #[test]
    fn test_invalid_directory_error() {
        // Try to create log in a location that should fail (invalid path)
        #[cfg(unix)]
        let result = fs::create_dir_all("/root/forbidden/logs");

        #[cfg(windows)]
        let result = fs::create_dir_all("C:\\Windows\\System32\\logs");

        // Should return error, not panic
        assert!(
            result.is_err(),
            "Should return error for invalid log directory"
        );
    }

    #[test]
    fn test_guard_structure() {
        // Test that LoggingGuard can be created (doesn't test actual logging)
        // This verifies the struct compiles and can be instantiated
        use tracing_appender::non_blocking::NonBlocking;

        // Create a mock writer
        let (non_blocking, guard) = NonBlocking::new(std::io::sink());
        drop(non_blocking); // Simulate using the writer

        let _logging_guard = LoggingGuard { _file_guard: guard };

        // Guard is alive and will be dropped at end of scope
    }

    // Note: Testing actual log output requires integration tests because tracing
    // uses a global subscriber that can only be set once per process.
    // The unit tests above verify the file operations work correctly.
    // Actual logging behavior should be tested in integration tests or manually.
}
