//! No-operation logger implementation.

use crate::log::{LogLevel, Logger};
use std::fmt::Arguments;

/// A logger that discards all messages.
///
/// Useful for:
/// - Unit tests where log output would be noise
/// - Benchmarks where logging overhead should be eliminated
/// - Silent operation modes
///
/// # Example
///
/// ```
/// use xearthlayer::log::{Logger, NoOpLogger};
/// use std::sync::Arc;
///
/// let logger: Arc<dyn Logger> = Arc::new(NoOpLogger);
/// logger.info(format_args!("This message is discarded"));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpLogger;

impl Logger for NoOpLogger {
    #[inline]
    fn log(&self, _level: LogLevel, _args: Arguments<'_>) {
        // Intentionally empty - discard all log messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_logger_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoOpLogger>();
    }

    #[test]
    fn test_noop_logger_as_trait_object() {
        let logger: Box<dyn Logger> = Box::new(NoOpLogger);
        logger.info(format_args!("test message"));
        logger.debug(format_args!("debug message"));
        logger.warn(format_args!("warn message"));
        logger.error(format_args!("error message"));
        logger.trace(format_args!("trace message"));
    }

    #[test]
    fn test_noop_logger_default() {
        let logger = NoOpLogger;
        logger.info(format_args!("test"));
    }

    #[test]
    fn test_noop_logger_clone() {
        let logger = NoOpLogger;
        let cloned = logger;
        cloned.info(format_args!("test"));
    }

    #[test]
    fn test_noop_logger_debug_impl() {
        let logger = NoOpLogger;
        let debug_str = format!("{:?}", logger);
        assert_eq!(debug_str, "NoOpLogger");
    }
}
