//! Publisher CLI commands for creating and managing scenery packages.
//!
//! This module implements the Command Pattern with trait-based dependency
//! injection, providing a clean separation of concerns:
//!
//! - `traits`: Core interfaces (`Output`, `PublisherService`, `CommandHandler`)
//! - `services`: Concrete implementations of the traits
//! - `args`: CLI argument types and parsing (clap-derived)
//! - `handlers`: Command handlers implementing business logic
//! - `output`: Shared output formatting utilities
//!
//! # Architecture
//!
//! Each command handler:
//! - Implements the `CommandHandler` trait
//! - Depends only on trait interfaces via `CommandContext`
//! - Can be tested in isolation with mock implementations
//!
//! # Example
//!
//! ```ignore
//! // Production usage (in main dispatch)
//! let output = ConsoleOutput::new();
//! let publisher = DefaultPublisherService::new();
//! let ctx = CommandContext::new(&output, &publisher);
//! InitHandler::execute(args, &ctx)?;
//!
//! // Test usage
//! let output = MockOutput::new();
//! let publisher = MockPublisherService::new();
//! let ctx = CommandContext::new(&output, &publisher);
//! InitHandler::execute(args, &ctx)?;
//! assert!(output.contains("Initialized"));
//! ```

mod args;
mod handlers;
mod output;
mod services;
mod traits;

#[cfg(test)]
mod tests;

// Re-export public types
pub use args::PublishCommands;
pub use handlers::{
    AddHandler, BuildHandler, CoverageHandler, DedupeHandler, GapsHandler, InitHandler,
    ListHandler, ReleaseHandler, ScanHandler, StatusHandler, UrlsHandler, ValidateHandler,
    VersionHandler,
};
pub use services::{ConsoleOutput, DefaultPublisherService};
pub use traits::CommandHandler;

use args::{
    AddArgs, BuildArgs, CoverageArgs, DedupeArgs, GapsArgs, InitArgs, ListArgs, ReleaseArgs,
    ScanArgs, StatusArgs, UrlsArgs, ValidateArgs, VersionArgs,
};
use traits::CommandContext;

use crate::error::CliError;

/// Run a publish subcommand.
///
/// This is the main entry point for publish commands. It creates the
/// production context with real implementations and dispatches to the
/// appropriate handler.
pub fn run(command: PublishCommands) -> Result<(), CliError> {
    // Create production context
    let output = ConsoleOutput::new();
    let publisher = DefaultPublisherService::new();
    let ctx = CommandContext::new(&output, &publisher);

    // Dispatch to appropriate handler
    match command {
        PublishCommands::Init { path, part_size } => {
            InitHandler::execute(InitArgs { path, part_size }, &ctx)
        }

        PublishCommands::Scan { source, r#type } => ScanHandler::execute(
            ScanArgs {
                source,
                package_type: r#type,
            },
            &ctx,
        ),

        PublishCommands::Add {
            source,
            region,
            r#type,
            version,
            dedupe,
            priority,
            repo,
        } => AddHandler::execute(
            AddArgs {
                source,
                region,
                package_type: r#type,
                version,
                dedupe,
                priority,
                repo,
            },
            &ctx,
        ),

        PublishCommands::List { repo, verbose } => {
            ListHandler::execute(ListArgs { repo, verbose }, &ctx)
        }

        PublishCommands::Build {
            region,
            r#type,
            dedupe,
            priority,
            repo,
        } => BuildHandler::execute(
            BuildArgs {
                region,
                package_type: r#type,
                dedupe,
                priority,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Urls {
            region,
            r#type,
            base_url,
            verify,
            repo,
        } => UrlsHandler::execute(
            UrlsArgs {
                region,
                package_type: r#type,
                base_url,
                verify,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Version {
            region,
            r#type,
            bump,
            set,
            repo,
        } => VersionHandler::execute(
            VersionArgs {
                region,
                package_type: r#type,
                bump,
                set,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Release {
            region,
            r#type,
            metadata_url,
            repo,
        } => ReleaseHandler::execute(
            ReleaseArgs {
                region,
                package_type: r#type,
                metadata_url,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Status {
            region,
            r#type,
            repo,
        } => StatusHandler::execute(
            StatusArgs {
                region,
                package_type: r#type,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Validate { repo } => ValidateHandler::execute(ValidateArgs { repo }, &ctx),

        PublishCommands::Coverage {
            output,
            width,
            height,
            dark,
            geojson,
            repo,
        } => CoverageHandler::execute(
            CoverageArgs {
                output,
                width,
                height,
                dark,
                geojson,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Dedupe {
            region,
            r#type,
            priority,
            tile,
            dry_run,
            report,
            report_format,
            repo,
        } => DedupeHandler::execute(
            DedupeArgs {
                region,
                package_type: r#type,
                priority,
                tile,
                dry_run,
                report,
                report_format,
                repo,
            },
            &ctx,
        ),

        PublishCommands::Gaps {
            region,
            r#type,
            tile,
            report,
            report_format,
            repo,
        } => GapsHandler::execute(
            GapsArgs {
                region,
                package_type: r#type,
                tile,
                report,
                report_format,
                repo,
            },
            &ctx,
        ),
    }
}
