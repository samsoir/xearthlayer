//! CLI runner for common setup and operations.
//!
//! Encapsulates configuration loading and startup-banner logging
//! for command handlers that need a loaded config.
//!
//! Logging initialization is intentionally NOT performed here; it is
//! lifted into [`crate::main`] so every subcommand benefits, not just
//! the ones that go through this runner. See issue #194.

use crate::error::CliError;
use tracing::info;
use xearthlayer::config::ConfigFile;

/// Runner that manages CLI lifecycle and common operations.
pub struct CliRunner {
    /// Loaded configuration file
    config: ConfigFile,
}

impl CliRunner {
    /// Create a new CLI runner, loading config from the default path.
    pub fn new() -> Result<Self, CliError> {
        let config = ConfigFile::load()?;
        Ok(Self { config })
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
