//! Configuration-family preflight checks.

use super::names;
use std::borrow::Cow;
use std::path::PathBuf;
use xearthlayer::config::{analyze_config, ConfigFile};
use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};

/// Is this a first run: no configuration file and no packages?
///
/// Both must be absent. A user with packages but no config is not new, they are
/// misconfigured, and telling them to run the setup wizard would be wrong.
pub struct FirstRun {
    config_path: PathBuf,
    legacy_packages_dir: PathBuf,
}

impl FirstRun {
    /// Construct with explicit paths so tests never consult a real home
    /// directory.
    pub fn with_paths(config_path: PathBuf, legacy_packages_dir: PathBuf) -> Self {
        Self {
            config_path,
            legacy_packages_dir,
        }
    }
}

impl Preflight<BootstrapContext> for FirstRun {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::FIRST_RUN)
    }

    /// Only `run`. `setup` and `init` exist to create the state this check
    /// rejects, so applying it there would lock a new user out of the only
    /// commands that help them.
    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if !self.config_path.exists() && !self.legacy_packages_dir.exists() {
            Status::unsatisfied("no configuration file and no installed packages")
        } else {
            Status::Satisfied
        }
    }
}

/// Load the configuration file and contribute it to the context.
pub struct LoadConfig {
    path: PathBuf,
}

impl LoadConfig {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Preflight<BootstrapContext> for LoadConfig {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::LOAD_CONFIG)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        if ctx.config().is_some() {
            Status::Satisfied
        } else {
            Status::unsatisfied("configuration has not been loaded").remediable()
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        match ConfigFile::load_from(&self.path) {
            Ok(config) => {
                ctx.set_config(config);
                Ok(Remedy::default())
            }
            // Attach the ConfigFileError so the CLI can rebuild the error it has
            // always shown for a corrupt config, including its help text.
            Err(e) => Err(PreflightError::new(names::LOAD_CONFIG, e.to_string()).with_source(e)),
        }
    }
}

/// Warn when the configuration file is missing settings this version knows
/// about, or carries settings it no longer does.
///
/// Informational: it must never block startup, which is why an analysis failure
/// is logged and reported as satisfied rather than escalated.
pub struct ConfigUpgradeWarning {
    path: PathBuf,
}

impl ConfigUpgradeWarning {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Render the warning.
    ///
    /// Reproduces `commands/run.rs` as it stood before the framework, byte for
    /// byte including the surrounding blank lines. The trailing newline is
    /// supplied by the reporter's `eprintln!`.
    pub fn message(missing: usize, deprecated: usize) -> String {
        let mut out = format!(
            "\nWarning: Your configuration file is missing {} new setting(s)\n",
            missing
        );
        if deprecated > 0 {
            out.push_str(&format!(
                "         and contains {} deprecated setting(s).\n",
                deprecated
            ));
        }
        out.push_str("\nRun 'xearthlayer config upgrade' to update your configuration.\n");
        out.push_str("Use 'xearthlayer config upgrade --dry-run' to preview changes first.\n");
        out
    }
}

impl Preflight<BootstrapContext> for ConfigUpgradeWarning {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::CONFIG_UPGRADE)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if !self.path.exists() {
            return Status::Satisfied;
        }
        match analyze_config(&self.path) {
            Ok(analysis) if analysis.needs_upgrade => Status::Warning(Self::message(
                analysis.missing_keys.len(),
                analysis.deprecated_keys.len(),
            )),
            Ok(_) => Status::Satisfied,
            Err(e) => {
                // Logging only, no state change: inspection stays pure. Config
                // upgrade is advisory and must not become a startup blocker.
                tracing::warn!("Failed to analyze config for upgrade: {}", e);
                Status::Satisfied
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFileError;
    use xearthlayer::preflight::Preflight;

    fn ctx_for(command: &'static str) -> BootstrapContext {
        BootstrapContext::new(command, None)
    }

    // ---- FirstRun ----

    #[test]
    fn first_run_is_unsatisfied_when_neither_config_nor_packages_exist() {
        let dir = tempfile::tempdir().unwrap();
        let check =
            FirstRun::with_paths(dir.path().join("config.ini"), dir.path().join("packages"));
        assert!(matches!(
            check.inspect(&ctx_for("run")),
            Status::Unsatisfied {
                remediable: false,
                ..
            }
        ));
    }

    #[test]
    fn first_run_is_satisfied_when_packages_exist_without_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("packages")).unwrap();
        let check =
            FirstRun::with_paths(dir.path().join("config.ini"), dir.path().join("packages"));
        assert_eq!(check.inspect(&ctx_for("run")), Status::Satisfied);
    }

    #[test]
    fn first_run_is_satisfied_when_config_exists_without_packages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.ini"), "[general]\n").unwrap();
        let check =
            FirstRun::with_paths(dir.path().join("config.ini"), dir.path().join("packages"));
        assert_eq!(check.inspect(&ctx_for("run")), Status::Satisfied);
    }

    #[test]
    fn first_run_applies_only_to_run() {
        let dir = tempfile::tempdir().unwrap();
        let check =
            FirstRun::with_paths(dir.path().join("config.ini"), dir.path().join("packages"));
        assert!(check.applies(&ctx_for("run")));
        assert!(
            !check.applies(&ctx_for("setup")),
            "setup exists to create the state this check rejects"
        );
        assert!(!check.applies(&ctx_for("init")));
        assert!(!check.applies(&ctx_for("diagnostics")));
    }

    // ---- LoadConfig ----

    #[test]
    fn load_config_contributes_and_is_then_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "[general]\nupdate_check = false\n").unwrap();
        let check = LoadConfig::with_path(path);
        let mut ctx = ctx_for("run");

        assert!(matches!(
            check.inspect(&ctx),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        check.remediate(&mut ctx).expect("valid config loads");
        assert!(ctx.config().is_some());
        assert_eq!(check.inspect(&ctx), Status::Satisfied);
    }

    #[test]
    fn load_config_carries_the_underlying_error_so_the_cli_can_render_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "this is not an ini file at all\n[[[\n").unwrap();
        let check = LoadConfig::with_path(path);
        let mut ctx = ctx_for("run");

        let err = check
            .remediate(&mut ctx)
            .expect_err("a corrupt config fails");
        assert_eq!(err.check, names::LOAD_CONFIG);
        assert!(
            err.source_ref()
                .and_then(|s| s.downcast_ref::<ConfigFileError>())
                .is_some(),
            "the ConfigFileError must survive so CliError::ConfigFile can be rebuilt"
        );
    }

    // ---- ConfigUpgradeWarning ----

    #[test]
    fn upgrade_warning_is_satisfied_when_no_config_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let check = ConfigUpgradeWarning::with_path(dir.path().join("absent.ini"));
        assert_eq!(check.inspect(&ctx_for("run")), Status::Satisfied);
    }

    #[test]
    fn upgrade_warning_reproduces_the_current_message_exactly() {
        // Byte-for-byte against commands/run.rs:437-457 as it stood before the
        // framework. The reporter appends the final newline via eprintln!.
        let rendered = ConfigUpgradeWarning::message(3, 0);
        assert_eq!(
            rendered,
            "\nWarning: Your configuration file is missing 3 new setting(s)\n\
             \nRun 'xearthlayer config upgrade' to update your configuration.\n\
             Use 'xearthlayer config upgrade --dry-run' to preview changes first.\n"
        );
    }

    #[test]
    fn upgrade_warning_includes_the_deprecated_line_only_when_there_are_any() {
        let with = ConfigUpgradeWarning::message(3, 1);
        assert_eq!(
            with,
            "\nWarning: Your configuration file is missing 3 new setting(s)\n\
             \u{20}        and contains 1 deprecated setting(s).\n\
             \nRun 'xearthlayer config upgrade' to update your configuration.\n\
             Use 'xearthlayer config upgrade --dry-run' to preview changes first.\n"
        );
        assert!(!ConfigUpgradeWarning::message(3, 0).contains("deprecated"));
    }

    #[test]
    fn upgrade_warning_applies_only_to_run() {
        let dir = tempfile::tempdir().unwrap();
        let check = ConfigUpgradeWarning::with_path(dir.path().join("config.ini"));
        assert!(check.applies(&ctx_for("run")));
        assert!(!check.applies(&ctx_for("config")));
    }
}
