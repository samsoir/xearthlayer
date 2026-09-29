//! `xearthlayer-publish`: create and publish XEarthLayer scenery packages.
//!
//! Publishing is its own binary rather than a subcommand of `xearthlayer`
//! (#284). The two are separate domains joined only by the package format,
//! which the `xearthlayer-package` crate carries for both, so a publisher's
//! machine needs none of the streaming runtime and the runtime links none of
//! the publisher's dependencies.
//!
//! The command layer implements the Command Pattern with trait-based
//! dependency injection:
//!
//! - `traits`: Core interfaces (`Output`, `PublisherService`, `CommandHandler`)
//! - `services`: Concrete implementations of the traits
//! - `args`: CLI argument types and parsing (clap-derived)
//! - `handlers`: Command handlers implementing business logic
//! - `output`: Shared output formatting utilities
//!
//! Each command handler implements the `CommandHandler` trait, depends only on
//! trait interfaces via `CommandContext`, and can be tested in isolation with
//! mock implementations (see `tests`).

mod args;
mod error;
mod handlers;
mod output;
mod services;
mod traits;

#[cfg(test)]
mod tests;

use std::process::ExitCode;

use clap::Parser;

use args::{
    AddArgs, BuildArgs, CoverageArgs, DedupeArgs, DeleteArgs, GapsArgs, InitArgs, ListArgs,
    PublishCommands, ReleaseArgs, ScanArgs, StatusArgs, UrlsArgs, ValidateArgs, VersionArgs,
};
use error::CliError;
use handlers::{
    AddHandler, BuildHandler, CoverageHandler, DedupeHandler, DeleteHandler, GapsHandler,
    InitHandler, ListHandler, ReleaseHandler, ScanHandler, StatusHandler, UrlsHandler,
    ValidateHandler, VersionHandler,
};
use services::{ConsoleOutput, ConsolePrompt, DefaultPublisherService};
use traits::{CommandContext, CommandHandler};

/// Create and publish XEarthLayer scenery packages.
#[derive(Parser)]
#[command(name = "xearthlayer-publish")]
#[command(version)]
#[command(about = "Create and publish XEarthLayer scenery packages", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: PublishCommands,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Returning ExitCode rather than calling process::exit lets destructors
    // run, the same discipline as the xearthlayer binary (#194).
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => ExitCode::from(e.report()),
    }
}

/// Run a publish subcommand.
///
/// Creates the production context with real implementations and dispatches
/// to the appropriate handler.
fn run(command: PublishCommands) -> Result<(), CliError> {
    // Create production context
    let output = ConsoleOutput::new();
    let publisher = DefaultPublisherService::new();
    let prompt = ConsolePrompt::new();
    let ctx = CommandContext::new(&output, &publisher, &prompt);

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
            metadata,
            repo,
        } => CoverageHandler::execute(
            CoverageArgs {
                output,
                width,
                height,
                dark,
                geojson,
                metadata,
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

        PublishCommands::Delete {
            region,
            r#type,
            dry_run,
            yes,
            repo,
        } => DeleteHandler::execute(
            DeleteArgs {
                region,
                package_type: r#type,
                dry_run,
                yes,
                repo,
            },
            &ctx,
        ),
    }
}

#[cfg(test)]
mod cli_tests {
    use clap::CommandFactory;

    use super::Cli;

    /// Every subcommand `xearthlayer publish` offered, and none it did not.
    const SUBCOMMANDS: [&str; 14] = [
        "init", "scan", "add", "list", "build", "urls", "version", "release", "status", "validate",
        "coverage", "dedupe", "gaps", "delete",
    ];

    #[test]
    fn the_binary_offers_every_publish_subcommand() {
        let command = Cli::command();
        let mut offered: Vec<&str> = command.get_subcommands().map(|c| c.get_name()).collect();
        offered.sort_unstable();
        let mut expected = SUBCOMMANDS.to_vec();
        expected.sort_unstable();

        assert_eq!(offered, expected);
    }

    #[test]
    fn an_unknown_subcommand_is_rejected_by_name() {
        let Err(err) = <Cli as clap::Parser>::try_parse_from(["xearthlayer-publish", "deploy"])
        else {
            panic!("unknown subcommand must not parse");
        };

        assert!(
            err.to_string().contains("deploy"),
            "the error should name the unrecognised subcommand: {err}"
        );
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }
}
