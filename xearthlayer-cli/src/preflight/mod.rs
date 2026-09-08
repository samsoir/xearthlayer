//! XEarthLayer's own preflight checks and their registry.
//!
//! The framework lives in the library crate; the concrete checks live here,
//! because what counts as a prerequisite is a property of the CLI rather than
//! of the engine.

use crate::error::CliError;
use std::path::PathBuf;
use xearthlayer::config::{config_file_path, ConfigFileError};
use xearthlayer::preflight::{BootstrapContext, PreflightError, RunOutcome, Runner};

/// Check names.
///
/// Constants rather than literals at the call site because [`to_cli_error`]
/// dispatches on them: a typo would silently fall through to the generic arm
/// and change what the user sees.
pub mod config;
pub mod migrate;
pub mod paths;
pub mod proposal;
pub mod system;

pub mod names {
    pub const LAYOUT_MIGRATION: &str = "layout-migration";
    pub const FIRST_RUN: &str = "first-run";
    pub const LOAD_CONFIG: &str = "load-config";
    pub const LOG_STARTUP: &str = "log-startup";
    pub const FD_LIMIT: &str = "fd-limit";
    pub const CONFIG_UPGRADE: &str = "config-upgrade";
    pub const PACKAGES_INSTALLED: &str = "packages-installed";
    pub const RESOLVE_CUSTOM_SCENERY: &str = "resolve-custom-scenery";
    pub const CUSTOM_SCENERY_EXISTS: &str = "custom-scenery-exists";
    pub const AIRPORT_ICAO: &str = "airport-icao";
    pub const MACFUSE_AVAILABLE: &str = "macfuse-available";
    pub const NO_RUNNING_INSTANCE: &str = "no-running-instance";
}

/// The pre-0.5.0 installation directory.
///
/// Distinct from [`legacy_default_packages_dir`]: this is the directory the
/// migration reads from, that one is how `first-run` recognises an installation
/// that predates the new layout.
pub fn legacy_install_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
}

/// The legacy default package directory.
///
/// One definition: two checks need it, and a path rule that exists twice is a
/// path rule that drifts.
pub fn legacy_default_packages_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
        .join("packages")
}

/// Build the bootstrap registry.
///
/// **Registration order is execution order**, and this order reproduces what
/// `commands/run.rs` did inline. It is asserted by test, because it is
/// load-bearing and was previously implicit in the shape of one function.
/// Every environment source the registry's checks read.
///
/// A struct rather than a parameter list because the checks are injected
/// wholesale: no check consults the real environment directly, so this grows by
/// one field for each new check that touches the filesystem. Assembling it in
/// one place also means a test fabricating an environment is told, by the
/// compiler, everything it has to fabricate.
pub struct RegistryEnv {
    /// The pre-0.5.0 directory, for detecting and migrating an old install.
    pub legacy_dir: PathBuf,
    pub config_path: PathBuf,
    pub legacy_packages_dir: PathBuf,
    pub lock_path: PathBuf,
    pub macfuse_probe: PathBuf,
    pub detect_custom_scenery: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
}

impl RegistryEnv {
    /// The real environment.
    pub fn production() -> Self {
        Self {
            legacy_dir: legacy_install_dir(),
            config_path: config_file_path(),
            legacy_packages_dir: legacy_default_packages_dir(),
            lock_path: default_lock_path(),
            macfuse_probe: PathBuf::from(system::MACFUSE_BUNDLE),
            detect_custom_scenery: Box::new(|| xearthlayer::config::detect_custom_scenery().ok()),
        }
    }
}

/// Build the bootstrap registry against the real environment.
pub fn build_registry() -> Runner<BootstrapContext> {
    build_registry_with(RegistryEnv::production())
}

/// Build the registry against an explicit environment.
///
/// **Registration order is execution order**, and this order reproduces what
/// `commands/run.rs` did inline. It is asserted by test, because it is
/// load-bearing and was previously implicit in the shape of one function.
pub fn build_registry_with(env: RegistryEnv) -> Runner<BootstrapContext> {
    let RegistryEnv {
        legacy_dir,
        config_path,
        legacy_packages_dir,
        lock_path,
        macfuse_probe,
        detect_custom_scenery,
    } = env;

    let mut runner = Runner::new();

    // First. FirstRun tests the configuration file at the resolver's path, so
    // an unmigrated installation would read as a fresh one.
    runner.register(Box::new(migrate::LayoutMigration::automatic(
        legacy_dir,
        Box::new(xearthlayer::paths::layout_snapshot()),
    )));
    runner.register(Box::new(config::FirstRun::with_paths(
        config_path.clone(),
        legacy_packages_dir,
    )));
    runner.register(Box::new(config::LoadConfig::with_path(config_path.clone())));
    runner.register(Box::<system::LogStartup>::default());
    runner.register(Box::<system::FdLimit>::default());
    runner.register(Box::new(config::ConfigUpgradeWarning::with_path(
        config_path,
    )));
    runner.register(Box::new(paths::PackagesInstalled));
    runner.register(Box::new(paths::ResolveCustomScenery::with_detector(
        detect_custom_scenery,
    )));
    runner.register(Box::new(paths::CustomSceneryExists));
    runner.register(Box::new(system::AirportIcao));
    runner.register(Box::new(system::MacFuseAvailable::with_probe(
        macfuse_probe,
    )));

    // Last: the lock is claimed only for a run that will actually proceed.
    // Claiming it earlier would leave a lock behind for a run rejected by a
    // later prerequisite.
    runner.register(Box::new(system::NoRunningInstance::new(lock_path)));
    runner
}

/// Where the instance lock lives.
pub fn default_lock_path() -> PathBuf {
    xearthlayer::paths::lock_file()
}

/// Run every applicable prerequisite for this command.
///
/// Executes before dispatch, so `run` and `setup` get the same answer to
/// "does this installation exist". Placing it inside `run` alone would leave
/// the setup wizard treating an existing user as new.
pub fn enforce(ctx: &mut BootstrapContext) -> Result<(), CliError> {
    let mut registry = build_registry();
    registry.set_reporter(Box::new(|_check, message| eprintln!("{}", message)));
    finish(registry.enforce(ctx), ctx)
}

fn finish(
    outcome: Result<RunOutcome, PreflightError>,
    ctx: &BootstrapContext,
) -> Result<(), CliError> {
    match outcome {
        Ok(RunOutcome::AllSatisfied) => Ok(()),
        Ok(RunOutcome::Failed {
            check,
            reason,
            hint,
        }) => Err(to_cli_error(&check, reason, hint, ctx)),
        Err(e) => Err(remediation_to_cli_error(e)),
    }
}

/// Map a remediation failure onto the error the CLI already produces.
///
/// A corrupt configuration file has always rendered as
/// [`CliError::ConfigFile`], with its own help text pointing at the file. The
/// check attaches the underlying error precisely so that survives.
pub fn remediation_to_cli_error(e: PreflightError) -> CliError {
    let message = e.message.clone();
    match e.into_source() {
        Some(source) => match source.downcast::<ConfigFileError>() {
            Ok(cause) => CliError::ConfigFile(*cause),
            Err(_) => CliError::Config(message),
        },
        None => CliError::Config(message),
    }
}

/// Map a failed check onto the [`CliError`] the CLI already produces for it.
///
/// # Why this dispatches on the check name
///
/// Two of these errors are not plain messages. [`CliError::NeedsSetup`] prints
/// a welcome and **exits 0**, because a fresh install is not a failure.
/// [`CliError::NoPackages`] renders installation guidance built from the
/// resolved install location. Neither survives a generic
/// "reason plus optional hint" rendering, and both must be preserved byte for
/// byte while the framework is introduced.
///
/// This is a deliberate temporary seam. It disappears once we are free to
/// change those two messages, at which point every check maps uniformly.
pub fn to_cli_error(
    check: &str,
    reason: String,
    hint: Option<String>,
    ctx: &BootstrapContext,
) -> CliError {
    match check {
        names::FIRST_RUN => CliError::NeedsSetup,
        names::PACKAGES_INSTALLED => CliError::NoPackages {
            install_location: ctx.install_location().cloned().unwrap_or_default(),
        },
        _ => CliError::Config(match hint {
            Some(hint) => format!("{}\n{}", reason, hint),
            None => reason,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn first_run_maps_to_needs_setup_preserving_the_welcome_and_exit_zero() {
        let ctx = BootstrapContext::new("run", None);
        let e = to_cli_error(names::FIRST_RUN, "no config".to_string(), None, &ctx);
        assert!(matches!(e, CliError::NeedsSetup));
    }

    #[test]
    fn packages_maps_to_no_packages_carrying_the_install_location() {
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_install_location(PathBuf::from("/opt/packages"));
        let e = to_cli_error(names::PACKAGES_INSTALLED, "missing".to_string(), None, &ctx);
        match e {
            CliError::NoPackages { install_location } => {
                assert_eq!(install_location, PathBuf::from("/opt/packages"))
            }
            other => panic!("expected NoPackages, got {:?}", other),
        }
    }

    #[test]
    fn everything_else_becomes_a_config_error_with_the_hint_appended() {
        let ctx = BootstrapContext::new("run", None);
        let e = to_cli_error(
            "some.check",
            "it is broken".to_string(),
            Some("fix it".to_string()),
            &ctx,
        );
        assert_eq!(e.to_string(), "Configuration error: it is broken\nfix it");
    }

    #[test]
    fn a_reason_without_a_hint_is_not_padded() {
        let ctx = BootstrapContext::new("run", None);
        let e = to_cli_error("some.check", "it is broken".to_string(), None, &ctx);
        assert_eq!(e.to_string(), "Configuration error: it is broken");
    }

    /// An environment in which every prerequisite is satisfiable.
    ///
    /// Includes the macFUSE bundle. On macOS that check applies and a CI runner
    /// has no macFUSE installed, so omitting it made this fabrication complete
    /// on Linux and incomplete on macOS.
    fn fabricate_complete_environment() -> (tempfile::TempDir, RegistryEnv) {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        let scenery = dir.path().join("Custom Scenery");
        let macfuse = dir.path().join("macfuse.fs");
        std::fs::create_dir(&packages).unwrap();
        std::fs::create_dir(&scenery).unwrap();
        std::fs::create_dir(&macfuse).unwrap();
        let config_path = dir.path().join("config.ini");
        std::fs::write(
            &config_path,
            format!(
                "[packages]\ninstall_location = {}\ncustom_scenery_path = {}\n",
                packages.display(),
                scenery.display()
            ),
        )
        .unwrap();
        let env = RegistryEnv {
            // Absent: there is nothing to migrate, which is the state a
            // complete environment is in.
            legacy_dir: dir.path().join("no-legacy-install"),
            config_path,
            legacy_packages_dir: dir.path().join("no-legacy-packages"),
            lock_path: dir.path().join("xearthlayer.lock"),
            macfuse_probe: macfuse,
            detect_custom_scenery: Box::new(|| None),
        };
        (dir, env)
    }

    /// An environment in which nothing exists.
    fn fabricate_empty_environment(dir: &tempfile::TempDir) -> RegistryEnv {
        RegistryEnv {
            legacy_dir: dir.path().join("no-legacy-install"),
            config_path: dir.path().join("absent.ini"),
            legacy_packages_dir: dir.path().join("absent-packages"),
            lock_path: dir.path().join("xearthlayer.lock"),
            macfuse_probe: dir.path().join("absent-macfuse.fs"),
            detect_custom_scenery: Box::new(|| None),
        }
    }

    #[test]
    fn the_bootstrap_pipeline_fills_every_context_field() {
        // The property the whole design rests on: preflight leaves behind the
        // inputs XEarthLayer launches from. Adding a context field with no
        // check to fill it fails here.
        let (_dir, env) = fabricate_complete_environment();
        let mut registry = build_registry_with(env);
        let mut ctx = BootstrapContext::new("run", None);

        let outcome = registry.enforce(&mut ctx).expect("no remediation failure");
        assert!(
            matches!(outcome, RunOutcome::AllSatisfied),
            "expected every prerequisite satisfied, got {outcome:?}"
        );

        assert!(ctx.config().is_some(), "no check contributed the config");
        assert!(
            ctx.install_location().is_some(),
            "no check contributed the install location"
        );
        assert!(
            ctx.custom_scenery_path().is_some(),
            "no check contributed the Custom Scenery path"
        );
    }

    #[test]
    fn the_fabricated_environment_satisfies_the_macos_only_check_too() {
        // The registry never runs macfuse-available on Linux, because applies()
        // is false there, so the completeness test above cannot cover it here.
        // Asserting it directly means an incomplete fabrication fails on both
        // platforms rather than only on the one that runs the check. Omitting
        // this is what let a macOS-only CI failure through.
        let (_dir, env) = fabricate_complete_environment();
        use xearthlayer::preflight::{Preflight, Status};
        let check = system::MacFuseAvailable::with_probe(env.macfuse_probe.clone());
        assert_eq!(
            check.inspect(&BootstrapContext::new("run", None)),
            Status::Satisfied
        );
    }

    #[test]
    fn registration_order_reproduces_what_run_did_inline() {
        // Load-bearing and, until now, implicit in the shape of one function.
        let dir = tempfile::tempdir().unwrap();
        let registry = build_registry_with(fabricate_empty_environment(&dir));
        let order: Vec<String> = registry
            .check_names()
            .iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(
            order,
            vec![
                names::LAYOUT_MIGRATION,
                names::FIRST_RUN,
                names::LOAD_CONFIG,
                names::LOG_STARTUP,
                names::FD_LIMIT,
                names::CONFIG_UPGRADE,
                names::PACKAGES_INSTALLED,
                names::RESOLVE_CUSTOM_SCENERY,
                names::CUSTOM_SCENERY_EXISTS,
                names::AIRPORT_ICAO,
                names::MACFUSE_AVAILABLE,
                names::NO_RUNNING_INSTANCE,
            ]
        );
    }

    #[test]
    fn setup_does_not_inherit_runs_prerequisites() {
        // setup exists to create the state these checks require. If any applied,
        // a new user could never reach the wizard.
        let dir = tempfile::tempdir().unwrap();
        let registry = build_registry_with(fabricate_empty_environment(&dir));
        let ctx = BootstrapContext::new("setup", None);
        let applying: Vec<String> = registry
            .report(&ctx)
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(
            applying,
            vec![names::LAYOUT_MIGRATION],
            "setup must inherit the layout migration and nothing else: the \
             wizard needs the migrated layout, but it exists to create the \
             state every other prerequisite requires"
        );
    }

    #[test]
    fn a_first_run_environment_still_reports_needs_setup() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = build_registry_with(fabricate_empty_environment(&dir));
        let mut ctx = BootstrapContext::new("run", None);
        let err = finish(registry.enforce(&mut ctx), &ctx).expect_err("first run blocks");
        assert!(
            matches!(err, CliError::NeedsSetup),
            "a fresh install must still print the welcome and exit 0, got {err:?}"
        );
    }

    #[test]
    fn a_corrupt_config_still_renders_as_a_config_file_error() {
        // CliError::ConfigFile carries its own help text pointing at the file.
        // Degrading it to a generic message would be a behaviour change.
        let cause = ConfigFileError::WriteError("bad ini".to_string());
        let e = PreflightError::new(names::LOAD_CONFIG, cause.to_string()).with_source(cause);
        assert!(matches!(
            remediation_to_cli_error(e),
            CliError::ConfigFile(_)
        ));
    }

    #[test]
    fn a_remediation_failure_without_a_source_is_a_plain_config_error() {
        let e = PreflightError::new(names::RESOLVE_CUSTOM_SCENERY, "nothing configured");
        match remediation_to_cli_error(e) {
            CliError::Config(msg) => assert_eq!(msg, "nothing configured"),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn check_names_are_unique() {
        let all = [
            names::LAYOUT_MIGRATION,
            names::FIRST_RUN,
            names::LOAD_CONFIG,
            names::LOG_STARTUP,
            names::FD_LIMIT,
            names::CONFIG_UPGRADE,
            names::PACKAGES_INSTALLED,
            names::RESOLVE_CUSTOM_SCENERY,
            names::CUSTOM_SCENERY_EXISTS,
            names::AIRPORT_ICAO,
            names::MACFUSE_AVAILABLE,
            names::NO_RUNNING_INSTANCE,
        ];
        let mut seen: Vec<&str> = all.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            all.len(),
            "to_cli_error dispatches on these, so a duplicate would silently \
             reroute one check's error to another's"
        );
    }
}
