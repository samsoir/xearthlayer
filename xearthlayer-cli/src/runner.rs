//! CLI runner for common setup and operations.
//!
//! Encapsulates logging initialization, service creation, and file operations
//! to reduce duplication across command handlers.

use crate::error::CliError;
use std::sync::Arc;
use tracing::info;
use xearthlayer::config::{ConfigFile, TextureConfig};
use xearthlayer::log::TracingLogger;
use xearthlayer::logging::{init_logging_full, LoggingGuard};
use xearthlayer::provider::ProviderConfig;
use xearthlayer::service::{ServiceConfig, XEarthLayerService};

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

    /// Create a service with the given configuration.
    pub fn create_service(
        &self,
        config: ServiceConfig,
        provider_config: &ProviderConfig,
    ) -> Result<XEarthLayerService, CliError> {
        if provider_config.requires_api_key() {
            info!("Creating {} session...", provider_config.name());
            println!("Creating {} session...", provider_config.name());
        }

        // Use TracingLogger to delegate library logging to tracing crate
        let logger = Arc::new(TracingLogger);

        XEarthLayerService::new(config, provider_config.clone(), logger)
            .map_err(CliError::ServiceCreation)
            .inspect(|_| info!("Service created successfully"))
    }

    /// Save DDS data to a file.
    pub fn save_dds(
        &self,
        path: &str,
        data: &[u8],
        texture_config: &TextureConfig,
    ) -> Result<(), CliError> {
        let format = texture_config.format();
        let mipmap_count = texture_config.mipmap_count();

        info!(
            "Saving DDS ({:?} compression, {} mipmap levels)",
            format, mipmap_count
        );
        println!(
            "Saving DDS ({:?} compression, {} mipmap levels)...",
            format, mipmap_count
        );

        std::fs::write(path, data).map_err(|e| CliError::FileWrite {
            path: path.to_string(),
            error: e,
        })?;

        let size_mb = data.len() as f64 / 1_048_576.0;
        info!("DDS saved successfully: {} ({:.2} MB)", path, size_mb);
        println!("✓ Saved successfully: {} ({:.2} MB)", path, size_mb);
        println!("  Format: {:?}", format);
        println!("  Mipmaps: {}", mipmap_count);

        Ok(())
    }
}
