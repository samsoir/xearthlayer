//! Logging infrastructure for XEarthLayer.
//!
//! Provides structured logging with file output and console output:
//! - Writes to `logs/xearthlayer.log` (cleared on session start)
//! - Also prints to stdout for CLI tailing
//! - Compact single-line format for release builds with UTC date+time
//!   (supports flight sim sessions lasting 24+ hours)
//! - Verbose multi-line format for debug builds
//! - Configurable via RUST_LOG environment variable

use std::fs;
use std::io;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[cfg(debug_assertions)]
use tracing_subscriber::fmt::format::FmtSpan;

#[cfg(not(debug_assertions))]
use tracing_subscriber::fmt::time::OffsetTime;

/// Guard that must be kept alive for the duration of logging.
///
/// Dropping this guard will flush and close the log file writer.
/// Field order matters: Rust drops struct fields in declaration order.
/// Chrome flush guard must drop before the writer guard to ensure
/// buffered trace events are flushed to the non-blocking writer
/// before it shuts down.
pub struct LoggingGuard {
    #[cfg(feature = "profiling")]
    _chrome_flush_guard: Option<tracing_chrome::FlushGuard>,
    #[cfg(feature = "profiling")]
    _chrome_writer_guard: Option<WorkerGuard>,
    _file_guard: WorkerGuard,
}

/// Initialize logging system.
///
/// Creates logs directory if needed, clears previous log file,
/// and sets up output to file and optionally stdout.
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
    init_logging_with_options(log_dir, log_file, true)
}

/// Initialize logging system with options.
///
/// Creates logs directory if needed, clears previous log file,
/// and sets up output to file and optionally stdout.
///
/// # Arguments
///
/// * `log_dir` - Directory for log files (e.g., "logs")
/// * `log_file` - Log filename (e.g., "xearthlayer.log")
/// * `stdout_enabled` - Whether to also output to stdout (disable for TUI mode)
///
/// # Returns
///
/// LoggingGuard that must be kept alive for logging to work
///
/// # Errors
///
/// Returns error if log directory cannot be created or log file cannot be cleared
pub fn init_logging_with_options(
    log_dir: &str,
    log_file: &str,
    stdout_enabled: bool,
) -> Result<LoggingGuard, io::Error> {
    init_logging_full(log_dir, log_file, stdout_enabled, false, false)
}

/// Initialize logging system with full options including debug and profile modes.
///
/// Creates logs directory if needed, clears previous log file,
/// and sets up output to file and optionally stdout.
///
/// # Arguments
///
/// * `log_dir` - Directory for log files (e.g., "logs")
/// * `log_file` - Log filename (e.g., "xearthlayer.log")
/// * `stdout_enabled` - Whether to also output to stdout (disable for TUI mode)
/// * `debug_mode` - Whether to enable debug-level logging (overrides RUST_LOG)
/// * `profile_mode` - Whether to enable Chrome Trace profiling (requires `profiling` feature)
///
/// # Returns
///
/// LoggingGuard that must be kept alive for logging to work
///
/// # Errors
///
/// Returns error if log directory cannot be created or log file cannot be cleared
pub fn init_logging_full(
    log_dir: &str,
    log_file: &str,
    stdout_enabled: bool,
    debug_mode: bool,
    profile_mode: bool,
) -> Result<LoggingGuard, io::Error> {
    // Create logs directory if it doesn't exist
    fs::create_dir_all(log_dir)?;

    // Clear previous log file by writing empty content
    // This handles both existing and non-existing files
    let log_path = Path::new(log_dir).join(log_file);
    fs::write(&log_path, "")?;

    // Create file appender with non-blocking writer
    let file_appender = tracing_appender::rolling::never(log_dir, log_file);
    let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);

    // Create env filter
    // Priority: profile_mode > debug_mode flag > RUST_LOG env var > default (info)
    //
    // When debug_mode is enabled, we only enable DEBUG for xearthlayer crate.
    // Third-party crates (especially fuse3) produce extremely verbose DEBUG output
    // that can flood the log and cause performance issues.
    //
    // When profile_mode is enabled, we enable DEBUG for xearthlayer so that
    // profiling spans (debug_span!) fire, and suppress fuse3 to WARN level.
    // fuse3 internal spans contributed 70% of trace events (1.1M of 1.6M) in
    // testing, causing a 14x slowdown from mutex contention on the trace writer.
    let env_filter = if profile_mode {
        EnvFilter::new("info,xearthlayer=debug,fuse3=warn")
    } else if debug_mode {
        EnvFilter::new("info,xearthlayer=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    // Chrome tracing layer for --profile mode (requires `profiling` feature)
    //
    // Uses a non-blocking writer to avoid mutex contention: tracing-chrome
    // holds an internal Mutex for every span/event write. With dozens of
    // concurrent tokio tasks, synchronous file I/O under that lock causes
    // severe contention. The non-blocking wrapper sends writes to a
    // background thread via a channel, keeping the critical section fast.
    #[cfg(feature = "profiling")]
    let (chrome_layer, chrome_flush_guard, chrome_writer_guard) = if profile_mode {
        let trace_path = Path::new(log_dir).join(format!(
            "xearthlayer-trace-{}.json",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        ));
        eprintln!("Profile trace: {}", trace_path.display());
        let trace_file = fs::File::create(&trace_path)?;
        let (non_blocking_trace, writer_guard) = tracing_appender::non_blocking(trace_file);
        let (layer, flush_guard) = tracing_chrome::ChromeLayerBuilder::new()
            .writer(non_blocking_trace)
            .build();
        (Some(layer), Some(flush_guard), Some(writer_guard))
    } else {
        (None, None, None)
    };

    #[cfg(not(feature = "profiling"))]
    if profile_mode {
        eprintln!("Warning: --profile requires the 'profiling' feature. Rebuild with: cargo build --features profiling");
    }

    // Use different formats for debug vs release builds
    #[cfg(debug_assertions)]
    {
        // Debug: verbose multi-line format with file locations
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking_file)
            .with_ansi(false)
            .with_span_events(FmtSpan::CLOSE)
            .pretty();

        // Only add stdout layer if enabled (disabled for TUI mode)
        let stdout_layer = stdout_enabled.then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(io::stdout)
                .with_ansi(true)
                .with_span_events(FmtSpan::CLOSE)
                .pretty()
        });

        #[cfg(feature = "profiling")]
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stdout_layer)
            .with(chrome_layer)
            .init();

        #[cfg(not(feature = "profiling"))]
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stdout_layer)
            .init();
    }

    #[cfg(not(debug_assertions))]
    {
        // Release: compact single-line format without file locations
        // Format: "2025-11-26 10:30:45Z INFO message" (Zulu/UTC timestamps for long sessions)
        let time_format =
            time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]Z")
                .expect("valid format");
        let timer = OffsetTime::new(time::UtcOffset::UTC, time_format);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking_file)
            .with_ansi(false)
            .with_target(false)
            .with_file(false)
            .with_line_number(false)
            .with_timer(timer.clone());

        // Only add stdout layer if enabled (disabled for TUI mode)
        let stdout_layer = stdout_enabled.then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(io::stdout)
                .with_ansi(true)
                .with_target(false)
                .with_file(false)
                .with_line_number(false)
                .with_timer(timer)
        });

        #[cfg(feature = "profiling")]
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stdout_layer)
            .with(chrome_layer)
            .init();

        #[cfg(not(feature = "profiling"))]
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stdout_layer)
            .init();
    }

    Ok(LoggingGuard {
        #[cfg(feature = "profiling")]
        _chrome_flush_guard: chrome_flush_guard,
        #[cfg(feature = "profiling")]
        _chrome_writer_guard: chrome_writer_guard,
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

        let _logging_guard = LoggingGuard {
            #[cfg(feature = "profiling")]
            _chrome_flush_guard: None,
            #[cfg(feature = "profiling")]
            _chrome_writer_guard: None,
            _file_guard: guard,
        };

        // Guard is alive and will be dropped at end of scope
    }

    // Note: Testing actual log output requires integration tests because tracing
    // uses a global subscriber that can only be set once per process.
    // The unit tests above verify the file operations work correctly.
    // Actual logging behavior should be tested in integration tests or manually.
}
