//! CLI error handling with user-friendly messages.
//!
//! Centralizes error handling for the CLI, providing consistent formatting
//! and appropriate exit codes.

use std::fmt;
use std::process;
use xearthlayer::config::ConfigFileError;
use xearthlayer::service::ServiceError;

/// CLI-specific errors with user-friendly messages.
#[derive(Debug)]
pub enum CliError {
    /// Failed to initialize logging
    LoggingInit(String),
    /// Configuration error
    Config(String),
    /// Failed to load config file
    ConfigFile(ConfigFileError),
    /// Failed to create service
    ServiceCreation(ServiceError),
    /// Failed to download tile
    Download(ServiceError),
    /// Failed to write output file
    FileWrite { path: String, error: std::io::Error },
    /// FUSE server error
    Serve(ServiceError),
    /// Failed to clear cache
    CacheClear(String),
    /// Failed to get cache stats
    CacheStats(String),
    /// Publisher error
    Publish(String),
    /// Package manager error
    Packages(String),
}

impl CliError {
    /// Exit the process with an appropriate error message and code.
    pub fn exit(&self) -> ! {
        eprintln!("Error: {}", self);

        // Print additional help for specific errors
        match self {
            CliError::ConfigFile(_) => {
                eprintln!();
                eprintln!("Check your config file at ~/.xearthlayer/config.ini");
                eprintln!("Run 'xearthlayer init' to create a default config file.");
            }
            CliError::ServiceCreation(ServiceError::ProviderError(_)) => {
                eprintln!();
                eprintln!("If using Google Maps provider, make sure:");
                eprintln!("  1. Map Tiles API is enabled in Google Cloud Console");
                eprintln!("  2. Billing is enabled for your project");
                eprintln!("  3. Your API key is valid and unrestricted");
            }
            CliError::Serve(_) => {
                eprintln!();
                eprintln!("Common issues:");
                eprintln!("  1. FUSE not installed: sudo apt install fuse (Linux)");
                eprintln!("  2. Permissions: You may need to add your user to 'fuse' group");
                eprintln!(
                    "  3. Mountpoint in use: Try unmounting with: fusermount -u <mountpoint>"
                );
            }
            CliError::Publish(_) => {
                eprintln!();
                eprintln!("Run 'xearthlayer publish --help' for usage information.");
            }
            CliError::Packages(_) => {
                eprintln!();
                eprintln!("Run 'xearthlayer packages --help' for usage information.");
            }
            _ => {}
        }

        process::exit(1)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::LoggingInit(msg) => write!(f, "Failed to initialize logging: {}", msg),
            CliError::Config(msg) => write!(f, "Configuration error: {}", msg),
            CliError::ConfigFile(e) => write!(f, "Config file error: {}", e),
            CliError::ServiceCreation(e) => write!(f, "Failed to create service: {}", e),
            CliError::Download(e) => write!(f, "Failed to download tile: {}", e),
            CliError::FileWrite { path, error } => {
                write!(f, "Failed to write file '{}': {}", path, error)
            }
            CliError::Serve(e) => write!(f, "FUSE server error: {}", e),
            CliError::CacheClear(msg) => write!(f, "Failed to clear cache: {}", msg),
            CliError::CacheStats(msg) => write!(f, "Failed to get cache stats: {}", msg),
            CliError::Publish(msg) => write!(f, "Publisher error: {}", msg),
            CliError::Packages(msg) => write!(f, "Package manager error: {}", msg),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::ConfigFile(e) => Some(e),
            CliError::ServiceCreation(e) => Some(e),
            CliError::Download(e) => Some(e),
            CliError::FileWrite { error, .. } => Some(error),
            CliError::Serve(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ServiceError> for CliError {
    fn from(e: ServiceError) -> Self {
        CliError::Download(e)
    }
}

impl From<ConfigFileError> for CliError {
    fn from(e: ConfigFileError) -> Self {
        CliError::ConfigFile(e)
    }
}
