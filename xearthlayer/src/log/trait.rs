//! Logger trait definition.

use std::fmt::Arguments;

/// Log level for filtering messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Verbose debugging information
    Trace,
    /// Debugging information
    Debug,
    /// General information
    Info,
    /// Warning messages
    Warn,
    /// Error messages
    Error,
}

/// Logging interface for application components.
///
/// This trait provides a logging abstraction that allows components to log
/// messages without depending on a specific logging implementation.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow sharing across threads.
///
/// # Example
///
/// ```
/// use xearthlayer::log::{Logger, LogLevel, NoOpLogger};
/// use xearthlayer::{log_info, log_debug};
/// use std::sync::Arc;
///
/// let logger: Arc<dyn Logger> = Arc::new(NoOpLogger);
/// log_info!(logger, "Application started");
/// log_debug!(logger, "Debug message");
/// ```
pub trait Logger: Send + Sync {
    /// Log a message at the specified level.
    ///
    /// This is the core method that implementations must provide.
    /// The convenience methods (`trace`, `debug`, `info`, `warn`, `error`)
    /// delegate to this method.
    fn log(&self, level: LogLevel, args: Arguments<'_>);

    /// Log a trace-level message.
    fn trace(&self, args: Arguments<'_>) {
        self.log(LogLevel::Trace, args);
    }

    /// Log a debug-level message.
    fn debug(&self, args: Arguments<'_>) {
        self.log(LogLevel::Debug, args);
    }

    /// Log an info-level message.
    fn info(&self, args: Arguments<'_>) {
        self.log(LogLevel::Info, args);
    }

    /// Log a warning-level message.
    fn warn(&self, args: Arguments<'_>) {
        self.log(LogLevel::Warn, args);
    }

    /// Log an error-level message.
    fn error(&self, args: Arguments<'_>) {
        self.log(LogLevel::Error, args);
    }
}

/// Convenience macros for logging with format strings.
///
/// These macros provide a familiar interface similar to `tracing` macros.
#[macro_export]
macro_rules! log_trace {
    ($logger:expr, $($arg:tt)*) => {
        $logger.trace(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($logger:expr, $($arg:tt)*) => {
        $logger.debug(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($logger:expr, $($arg:tt)*) => {
        $logger.info(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($logger:expr, $($arg:tt)*) => {
        $logger.warn(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($logger:expr, $($arg:tt)*) => {
        $logger.error(format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn test_log_level_equality() {
        assert_eq!(LogLevel::Info, LogLevel::Info);
        assert_ne!(LogLevel::Info, LogLevel::Debug);
    }

    #[test]
    fn test_log_level_debug() {
        let level = LogLevel::Debug;
        assert_eq!(format!("{:?}", level), "Debug");
    }

    #[test]
    fn test_log_level_clone() {
        let level = LogLevel::Warn;
        let cloned = level;
        assert_eq!(level, cloned);
    }
}
