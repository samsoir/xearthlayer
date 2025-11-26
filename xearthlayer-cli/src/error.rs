//! CLI error handling with user-friendly messages.
//!
//! Centralizes error handling for the CLI, providing consistent formatting
//! and appropriate exit codes.

use std::fmt;
use std::process;
use xearthlayer::service::ServiceError;

/// CLI-specific errors with user-friendly messages.
#[derive(Debug)]
pub enum CliError {
    /// Failed to initialize logging
    LoggingInit(String),
    /// Configuration error
    Config(String),
    /// Failed to create service
    ServiceCreation(ServiceError),
    /// Failed to download tile
    Download(ServiceError),
    /// Failed to write output file
    FileWrite { path: String, error: std::io::Error },
    /// FUSE server error
    Serve(ServiceError),
}

impl CliError {
    /// Exit the process with an appropriate error message and code.
    pub fn exit(&self) -> ! {
        eprintln!("Error: {}", self);

        // Print additional help for specific errors
        match self {
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
            CliError::ServiceCreation(e) => write!(f, "Failed to create service: {}", e),
            CliError::Download(e) => write!(f, "Failed to download tile: {}", e),
            CliError::FileWrite { path, error } => {
                write!(f, "Failed to write file '{}': {}", path, error)
            }
            CliError::Serve(e) => write!(f, "FUSE server error: {}", e),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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
