//! Logging infrastructure for XEarthLayer.
//!
//! Provides structured logging with file output and console output:
//! - Writes to a log file, truncated or appended per [`LogFileMode`]
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

/// What happens to a log file that already exists when logging starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFileMode {
    /// Empty it first: the file holds one session, the latest.
    Truncate,
    /// Keep it and write after what is there: the file holds every session.
    Append,
}

/// Prepare the log file for a session: the directory exists, and so does the
/// file, emptied when `mode` says so.
///
/// Separate from the subscriber so it can be tested: the global subscriber
/// can be set only once per process.
fn prepare_log_file(log_path: &Path, mode: LogFileMode) -> io::Result<()> {
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir)?;
    }
    match mode {
        LogFileMode::Truncate => fs::write(log_path, ""),
        LogFileMode::Append => fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_path)
            .map(drop),
    }
}

/// Initialize logging system with full options including debug and profile modes.
///
/// Creates the log directory if needed, truncates or keeps the log file per
/// `mode`, and sets up output to file and optionally stdout.
///
/// # Arguments
///
/// * `log_dir` - Directory for log files (e.g., "logs")
/// * `log_file` - Log filename (e.g., "xearthlayer.log")
/// * `mode` - Whether an existing log file is emptied first or appended to
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
/// Returns error if log directory cannot be created or log file cannot be opened
pub fn init_logging_full(
    log_dir: &str,
    log_file: &str,
    mode: LogFileMode,
    stdout_enabled: bool,
    debug_mode: bool,
    profile_mode: bool,
) -> Result<LoggingGuard, io::Error> {
    prepare_log_file(&Path::new(log_dir).join(log_file), mode)?;

    // Create file appender with non-blocking writer
    let file_appender = tracing_appender::rolling::never(log_dir, log_file);
    let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);

    // Create env filter
    // Flags are combinable: --debug + --profile enables both debug logs and profiling spans.
    //
    // When debug_mode is enabled, we only enable DEBUG for xearthlayer crate.
    // fuse3 is pinned to WARN in every arm: it instruments every FUSE handler with
    // an INFO span, so at the default `info` level it emits a log line per syscall.
    // On macOS this is catastrophic — Finder/Spotlight/save-panel constantly stat the
    // mount, and combined with the debug build's FmtSpan::CLOSE + pretty() format it
    // floods the log at ~10 MB/s. WARN keeps fuse3 quiet while preserving our logs.
    //
    // When profile_mode is enabled, we enable DEBUG for the "profiling" target —
    // our spans use `target: "profiling"` to isolate them from ambient events.
    let env_filter = match (profile_mode, debug_mode) {
        (true, true) => EnvFilter::new("info,fuse3=warn,profiling=debug,xearthlayer=debug"),
        (true, false) => EnvFilter::new("info,fuse3=warn,profiling=debug"),
        (false, true) => EnvFilter::new("info,fuse3=warn,xearthlayer=debug"),
        (false, false) => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,fuse3=warn"))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn truncate_creates_the_directory_and_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("deep").join("nested").join("test.log");

        prepare_log_file(&log_path, LogFileMode::Truncate).unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "");
    }

    #[test]
    fn truncate_empties_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        fs::write(&log_path, "old log data").unwrap();

        prepare_log_file(&log_path, LogFileMode::Truncate).unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "");
    }

    #[test]
    fn append_keeps_an_existing_file_and_creates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        fs::write(&log_path, "earlier session\n").unwrap();

        prepare_log_file(&log_path, LogFileMode::Append).unwrap();
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "earlier session\n",
            "an appended log keeps what is there"
        );

        let fresh = dir.path().join("new").join("test.log");
        prepare_log_file(&fresh, LogFileMode::Append).unwrap();
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "");
    }

    #[test]
    fn an_unwritable_directory_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "").unwrap();

        let result = prepare_log_file(&blocker.join("test.log"), LogFileMode::Truncate);

        assert!(result.is_err());
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
