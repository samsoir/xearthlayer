//! Tracing library adapter implementation.

use crate::log::{LogLevel, Logger};
use std::fmt::Arguments;

/// Logger implementation that delegates to the `tracing` crate.
///
/// This adapter bridges our `Logger` trait to the `tracing` ecosystem,
/// allowing us to use `tracing`'s powerful features (subscribers, spans,
/// file output) while keeping our application code decoupled.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::log::{Logger, TracingLogger};
/// use std::sync::Arc;
///
/// // Assumes tracing subscriber is already initialized
/// let logger: Arc<dyn Logger> = Arc::new(TracingLogger);
/// logger.info(format_args!("Using tracing backend"));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingLogger;

impl TracingLogger {
    /// Create a new tracing logger adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Logger for TracingLogger {
    fn log(&self, level: LogLevel, args: Arguments<'_>) {
        match level {
            LogLevel::Trace => tracing::trace!("{}", args),
            LogLevel::Debug => tracing::debug!("{}", args),
            LogLevel::Info => tracing::info!("{}", args),
            LogLevel::Warn => tracing::warn!("{}", args),
            LogLevel::Error => tracing::error!("{}", args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracing_logger_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TracingLogger>();
    }

    #[test]
    fn test_tracing_logger_new() {
        let logger = TracingLogger::new();
        // Just verify it can be created
        let _ = logger;
    }

    #[test]
    fn test_tracing_logger_default() {
        let logger = TracingLogger;
        let _ = logger;
    }

    #[test]
    fn test_tracing_logger_as_trait_object() {
        let logger: Box<dyn Logger> = Box::new(TracingLogger);
        // These will log via tracing (may not appear without subscriber)
        logger.info(format_args!("test info"));
        logger.debug(format_args!("test debug"));
    }

    #[test]
    fn test_tracing_logger_clone() {
        let logger = TracingLogger;
        let cloned = logger;
        let _ = cloned;
    }

    #[test]
    fn test_tracing_logger_debug_impl() {
        let logger = TracingLogger;
        let debug_str = format!("{:?}", logger);
        assert_eq!(debug_str, "TracingLogger");
    }
}
