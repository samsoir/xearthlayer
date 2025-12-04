//! Core traits for the packages command handler pattern.
//!
//! This module defines the interfaces that handlers depend on, enabling
//! dependency injection and testability.

use std::path::Path;

use xearthlayer::manager::{
    InstallResult, InstallStage, InstalledPackage, LocalPackageStore, ManagerResult, PackageInfo,
    PackageStatus,
};
use xearthlayer::package::{PackageLibrary, PackageMetadata, PackageType};

use crate::error::CliError;

/// Type alias for progress callback to reduce complexity.
pub type ProgressCallback = Box<dyn Fn(InstallStage, f64, &str) + Send + Sync>;

// ============================================================================
// Output Trait - Abstracts console/UI output
// ============================================================================

/// Trait for outputting messages to the user.
///
/// This abstraction allows handlers to produce output without depending on
/// `println!` directly, making them testable.
pub trait Output: Send + Sync {
    /// Print a line of text.
    fn println(&self, message: &str);

    /// Print text without a newline.
    #[allow(dead_code)]
    fn print(&self, message: &str);

    /// Print an empty line.
    fn newline(&self) {
        self.println("");
    }

    /// Print a section header.
    fn header(&self, title: &str) {
        self.println(title);
        self.println(&"=".repeat(title.len()));
    }

    /// Print a sub-section header.
    #[allow(dead_code)]
    fn subheader(&self, title: &str) {
        self.println(title);
        self.println(&"-".repeat(title.len()));
    }

    /// Print an indented line.
    fn indented(&self, message: &str) {
        self.println(&format!("  {}", message));
    }

    /// Print a warning message.
    #[allow(dead_code)]
    fn warning(&self, message: &str) {
        self.println(&format!("Warning: {}", message));
    }

    /// Print an error message.
    fn error(&self, message: &str) {
        self.println(&format!("Error: {}", message));
    }

    /// Print a success message.
    fn success(&self, message: &str) {
        self.println(&format!("Success: {}", message));
    }
}

// ============================================================================
// Package Manager Service Trait
// ============================================================================

/// Trait for package manager operations.
///
/// Abstracts the various package manager functions to allow mocking in tests.
pub trait PackageManagerService: Send + Sync {
    /// Create a local package store.
    fn create_store(&self, install_dir: &Path) -> LocalPackageStore;

    /// Fetch a package library from a URL.
    fn fetch_library(&self, url: &str) -> ManagerResult<PackageLibrary>;

    /// Fetch package metadata from a URL.
    fn fetch_metadata(&self, url: &str) -> ManagerResult<PackageMetadata>;

    /// Check for package updates.
    fn check_updates(
        &self,
        store: &LocalPackageStore,
        library: &PackageLibrary,
    ) -> Vec<(PackageInfo, PackageStatus)>;

    /// Install a package.
    fn install_package(
        &self,
        metadata: &PackageMetadata,
        install_dir: &Path,
        temp_dir: &Path,
        on_progress: Option<ProgressCallback>,
    ) -> Result<InstallResult, CliError>;

    /// Remove a package.
    fn remove_package(
        &self,
        store: &LocalPackageStore,
        region: &str,
        package_type: PackageType,
    ) -> ManagerResult<()>;

    /// Get an installed package.
    fn get_package(
        &self,
        store: &LocalPackageStore,
        region: &str,
        package_type: PackageType,
    ) -> ManagerResult<InstalledPackage>;

    /// List all installed packages.
    fn list_packages(&self, store: &LocalPackageStore) -> ManagerResult<Vec<InstalledPackage>>;
}

// ============================================================================
// User Interaction Trait
// ============================================================================

/// Trait for user interaction (prompts, confirmation).
pub trait UserInteraction: Send + Sync {
    /// Prompt for yes/no confirmation.
    fn confirm(&self, message: &str) -> bool;

    /// Read a line of input from the user.
    #[allow(dead_code)]
    fn read_line(&self) -> Option<String>;
}

// ============================================================================
// Command Context - Bundles dependencies for handlers
// ============================================================================

/// Context providing dependencies to command handlers.
///
/// This struct bundles all the interfaces that handlers need, allowing
/// dependency injection. In production, this uses real implementations;
/// in tests, it can use mocks.
pub struct CommandContext<'a> {
    /// Output interface for user messages.
    pub output: &'a dyn Output,

    /// Package manager service for operations.
    pub manager: &'a dyn PackageManagerService,

    /// User interaction for prompts.
    pub interaction: &'a dyn UserInteraction,
}

impl<'a> CommandContext<'a> {
    /// Create a new command context.
    pub fn new(
        output: &'a dyn Output,
        manager: &'a dyn PackageManagerService,
        interaction: &'a dyn UserInteraction,
    ) -> Self {
        Self {
            output,
            manager,
            interaction,
        }
    }
}

// ============================================================================
// Command Handler Trait
// ============================================================================

/// Trait for command handlers.
///
/// Each packages subcommand has a handler that implements this trait.
/// Handlers receive their arguments and a context providing dependencies.
pub trait CommandHandler {
    /// The arguments type for this handler.
    type Args;

    /// Execute the command with the given arguments and context.
    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError>;
}
