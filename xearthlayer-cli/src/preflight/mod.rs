//! XEarthLayer's own preflight checks and their registry.
//!
//! The framework lives in the library crate; the concrete checks live here,
//! because what counts as a prerequisite is a property of the CLI rather than
//! of the engine.
//!
// Scaffolding lands before the checks that use it, and the project gate is
// `-D warnings`. Nothing in the binary consumes these until main() runs the
// registry. REMOVE THIS ATTRIBUTE in the wiring commit; the tests below
// already exercise every item it covers.
#![allow(dead_code)]

use crate::error::CliError;
use xearthlayer::preflight::BootstrapContext;

/// Check names.
///
/// Constants rather than literals at the call site because [`to_cli_error`]
/// dispatches on them: a typo would silently fall through to the generic arm
/// and change what the user sees.
pub mod config;

pub mod names {
    pub const FIRST_RUN: &str = "first-run";
    pub const LOAD_CONFIG: &str = "load-config";
    pub const FD_LIMIT: &str = "fd-limit";
    pub const CONFIG_UPGRADE: &str = "config-upgrade";
    pub const PACKAGES_INSTALLED: &str = "packages-installed";
    pub const RESOLVE_CUSTOM_SCENERY: &str = "resolve-custom-scenery";
    pub const CUSTOM_SCENERY_EXISTS: &str = "custom-scenery-exists";
    pub const AIRPORT_ICAO: &str = "airport-icao";
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

    #[test]
    fn check_names_are_unique() {
        let all = [
            names::FIRST_RUN,
            names::LOAD_CONFIG,
            names::FD_LIMIT,
            names::CONFIG_UPGRADE,
            names::PACKAGES_INSTALLED,
            names::RESOLVE_CUSTOM_SCENERY,
            names::CUSTOM_SCENERY_EXISTS,
            names::AIRPORT_ICAO,
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
