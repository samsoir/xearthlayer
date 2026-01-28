//! CLI runner for common setup and operations.
//!
//! Encapsulates logging initialization and configuration loading
//! to reduce duplication across command handlers.

use crate::error::CliError;
use tracing::info;
use xearthlayer::config::ConfigFile;
use xearthlayer::logging::{init_logging_full, LoggingGuard};

/// Runner that manages CLI lifecycle and common operations.
pub struct CliRunner {
    /// Logging guard - keeps logging active while runner exists
    #[allow(dead_code)]
    logging_guard: LoggingGuard,
    /// Loaded configuration file
    config: ConfigFile,
}

impl CliRunner {
    /// Create a new CLI runner, loading config and initializing logging.
    ///
    /// When stdout is a TTY, stdout logging is disabled to prevent
    /// interference with the TUI dashboard.
    pub fn new() -> Result<Self, CliError> {
        Self::with_debug(false)
    }

    /// Create a new CLI runner with optional debug logging.
    ///
    /// When stdout is a TTY, stdout logging is disabled to prevent
    /// interference with the TUI dashboard.
    ///
    /// # Arguments
    ///
    /// * `debug_mode` - When true, enables debug-level logging regardless of RUST_LOG
    pub fn with_debug(debug_mode: bool) -> Result<Self, CliError> {
        // Load config file (or use defaults if not present)
        let config = ConfigFile::load()?;

        // Use log path from config
        let log_path = &config.logging.file;
        let log_dir = log_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let log_file = log_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "xearthlayer.log".to_string());

        // Disable stdout logging when running in a TTY since TUI will take over
        // This prevents log messages from corrupting the TUI display
        let stdout_enabled = !atty::is(atty::Stream::Stdout);

        let logging_guard = init_logging_full(&log_dir, &log_file, stdout_enabled, debug_mode)
            .map_err(|e| CliError::LoggingInit(e.to_string()))?;

        Ok(Self {
            logging_guard,
            config,
        })
    }

    /// Get the loaded configuration.
    pub fn config(&self) -> &ConfigFile {
        &self.config
    }

    /// Log startup information for a command.
    pub fn log_startup(&self, command: &str) {
        info!("XEarthLayer v{}", xearthlayer::VERSION);
        info!("XEarthLayer CLI: {} command", command);
    }
}
