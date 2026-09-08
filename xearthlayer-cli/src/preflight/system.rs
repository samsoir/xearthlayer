//! System-level preflight checks.

use super::names;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use xearthlayer::airport::validate_airport_icao;
use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};

/// Write the startup banner to the log.
///
/// An action rather than a test, and it lives in the registry because its
/// **position** is what matters: after the configuration is loaded, and before
/// anything that can fail. A failed startup is exactly when a support log most
/// needs to say which version produced it, so this must not move behind the
/// checks that can reject the run.
///
/// Modelled as remediation so a diagnostic report, which only inspects, does
/// not write to the log as a side effect of being asked a question.
#[derive(Default)]
pub struct LogStartup {
    logged: AtomicBool,
}

impl Preflight<BootstrapContext> for LogStartup {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::LOG_STARTUP)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if self.logged.load(Ordering::SeqCst) {
            Status::Satisfied
        } else {
            Status::unsatisfied("startup has not been logged").remediable()
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        crate::runner::log_startup(ctx.command());
        self.logged.store(true, Ordering::SeqCst);
        Ok(Remedy::default())
    }
}

/// Raise the file descriptor soft limit to the hard maximum.
///
/// Processes inherit a soft FD limit (often 1024) that can be lower than the
/// hard limit. XEarthLayer needs many descriptors at once: HTTP connections to
/// the imagery CDN, disk cache handles, and FUSE descriptors for X-Plane's open
/// textures. Raising soft to hard needs no privileges and stays within what the
/// administrator allows.
///
/// **Best effort, never fatal.** Raising the limit has never blocked startup,
/// so the check is satisfied once an attempt has been made, whether or not the
/// attempt succeeded. That is why it tracks having tried rather than
/// re-reading the limit: re-reading would turn a failed raise into a startup
/// failure, which is a behaviour change.
pub struct FdLimit {
    attempted: AtomicBool,
    /// Injected so inspection is deterministic. Reading the real process limit
    /// from a test races every other test that raises it, and the limit is a
    /// property of the machine rather than of this code.
    limits: Box<dyn Fn() -> std::io::Result<(u64, u64)> + Send + Sync>,
}

impl Default for FdLimit {
    fn default() -> Self {
        Self {
            attempted: AtomicBool::new(false),
            limits: Box::new(|| rlimit::Resource::NOFILE.get()),
        }
    }
}

impl FdLimit {
    /// Construct with a fixed view of the limits.
    ///
    /// A test seam: production reads the real limit through `Default`.
    #[cfg(test)]
    pub fn with_limits(soft: u64, hard: u64) -> Self {
        Self {
            attempted: AtomicBool::new(false),
            limits: Box::new(move || Ok((soft, hard))),
        }
    }
}

impl Preflight<BootstrapContext> for FdLimit {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::FD_LIMIT)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if self.attempted.load(Ordering::SeqCst) {
            return Status::Satisfied;
        }
        match (self.limits)() {
            Ok((soft, hard)) if soft < hard => {
                Status::unsatisfied("file descriptor limit is below the hard maximum").remediable()
            }
            Ok((soft, _)) => {
                tracing::debug!(soft, "FD limit already at maximum");
                Status::Satisfied
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query FD limit");
                Status::Satisfied
            }
        }
    }

    fn remediate(
        &self,
        _ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        self.attempted.store(true, Ordering::SeqCst);
        match rlimit::Resource::NOFILE.get() {
            Ok((soft, hard)) if soft < hard => {
                if let Err(e) = rlimit::Resource::NOFILE.set(hard, hard) {
                    tracing::warn!(soft, hard, error = %e, "Failed to raise FD limit");
                } else {
                    tracing::info!(
                        old_soft = soft,
                        new_soft = hard,
                        "Raised file descriptor limit"
                    );
                }
            }
            Ok((soft, _)) => {
                tracing::debug!(soft, "FD limit already at maximum");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query FD limit");
            }
        }
        Ok(Remedy::default())
    }
}

/// Validate a requested airport ICAO code against X-Plane's database.
///
/// Early, before heavy initialisation, so a typo costs a second rather than a
/// scene load.
pub struct AirportIcao;

impl Preflight<BootstrapContext> for AirportIcao {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::AIRPORT_ICAO)
    }

    /// The conditional becomes declarative: the check applies when an airport
    /// was actually requested, rather than the caller wrapping it in an `if`.
    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run" && ctx.airport().is_some()
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(scenery) = ctx.custom_scenery_path() else {
            return Status::unsatisfied("Custom Scenery path has not been resolved");
        };
        let Some(icao) = ctx.airport() else {
            return Status::Satisfied;
        };
        match validate_airport_icao(scenery, icao) {
            Ok(()) => Status::Satisfied,
            Err(e) => Status::unsatisfied(e.to_string()),
        }
    }
}

/// Require macFUSE to be installed before attempting to mount.
///
/// Without this a missing or unapproved kext surfaces as an opaque mount
/// failure, which tells the user nothing about what to do.
///
/// # What is probed, and what is not
///
/// The presence of `/Library/Filesystems/macfuse.fs`. Deliberately **not**
/// whether the kext is loaded: macFUSE loads on demand at first mount rather
/// than at boot, so an empty `kmutil showloaded` is the normal state before a
/// mount and would make this check reject a working installation.
///
/// # Why there is no `#[cfg]` here
///
/// The check compiles on every platform and is excluded at runtime by
/// [`Preflight::applies`]. Code removed by `#[cfg]` is never type checked on
/// the other platform, which is how a macOS path silently rots until someone
/// builds on a Mac. This way Linux compiles it and runs its tests.
pub struct MacFuseAvailable {
    probe: PathBuf,
}

/// Where macFUSE installs its filesystem bundle.
const MACFUSE_BUNDLE: &str = "/Library/Filesystems/macfuse.fs";

impl Default for MacFuseAvailable {
    fn default() -> Self {
        Self {
            probe: PathBuf::from(MACFUSE_BUNDLE),
        }
    }
}

impl MacFuseAvailable {
    /// Construct with an explicit probe path, so tests do not depend on
    /// whether the machine running them has macFUSE.
    #[cfg(test)]
    pub fn with_probe(probe: PathBuf) -> Self {
        Self { probe }
    }
}

impl Preflight<BootstrapContext> for MacFuseAvailable {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::MACFUSE_AVAILABLE)
    }

    /// `cfg!` rather than `#[cfg]`: a runtime constant that excludes the check
    /// off macOS without excluding the code from compilation.
    fn applies(&self, ctx: &BootstrapContext) -> bool {
        cfg!(target_os = "macos") && ctx.command() == "run"
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if self.probe.exists() {
            Status::Satisfied
        } else {
            Status::unsatisfied(format!(
                "macFUSE is not installed: {} does not exist",
                self.probe.display()
            ))
            .with_hint(
                "Install macFUSE from https://macfuse.io, then approve the system \
                 extension. See docs/macos.md for the approval steps.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- MacFuseAvailable ----

    #[test]
    fn macfuse_is_satisfied_when_the_bundle_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("macfuse.fs");
        std::fs::create_dir(&bundle).unwrap();
        let check = MacFuseAvailable::with_probe(bundle);
        assert_eq!(
            check.inspect(&BootstrapContext::new("run", None)),
            Status::Satisfied
        );
    }

    #[test]
    fn macfuse_is_unsatisfied_and_points_at_the_install_guide() {
        let dir = tempfile::tempdir().unwrap();
        let check = MacFuseAvailable::with_probe(dir.path().join("absent.fs"));
        match check.inspect(&BootstrapContext::new("run", None)) {
            Status::Unsatisfied {
                reason,
                remediable,
                hint,
            } => {
                assert!(!remediable, "we cannot install macFUSE for the user");
                assert!(reason.contains("macFUSE is not installed"), "{reason}");
                assert!(
                    hint.as_deref()
                        .unwrap_or_default()
                        .contains("docs/macos.md"),
                    "the hint must point at the approval steps"
                );
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn macfuse_check_only_applies_on_macos() {
        // Compiles and runs on every platform; excluded at runtime off macOS.
        // The assertion follows the target so this test is meaningful on both.
        let check = MacFuseAvailable::default();
        let ctx = BootstrapContext::new("run", None);
        assert_eq!(check.applies(&ctx), cfg!(target_os = "macos"));
    }

    #[test]
    fn macfuse_check_does_not_apply_outside_run() {
        let check = MacFuseAvailable::default();
        assert!(!check.applies(&BootstrapContext::new("packages", None)));
    }

    // ---- LogStartup ----

    #[test]
    fn log_startup_records_once_and_is_then_satisfied() {
        let check = LogStartup::default();
        let mut ctx = BootstrapContext::new("run", None);
        assert!(matches!(
            check.inspect(&ctx),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        check.remediate(&mut ctx).unwrap();
        assert_eq!(check.inspect(&ctx), Status::Satisfied);
    }

    // ---- FdLimit ----

    #[test]
    fn fd_limit_asks_to_be_raised_when_soft_is_below_hard() {
        let check = FdLimit::with_limits(1024, 524_288);
        assert!(matches!(
            check.inspect(&BootstrapContext::new("run", None)),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
    }

    #[test]
    fn fd_limit_is_satisfied_when_soft_already_equals_hard() {
        let check = FdLimit::with_limits(524_288, 524_288);
        assert_eq!(
            check.inspect(&BootstrapContext::new("run", None)),
            Status::Satisfied
        );
    }

    #[test]
    fn fd_limit_is_satisfied_after_an_attempt_even_if_raising_failed() {
        // Best effort. Raising the limit has never been fatal, and turning it
        // into a startup blocker would be a behaviour change. The injected
        // limits stay below hard, so only the attempt can satisfy this.
        let check = FdLimit::with_limits(1024, 524_288);
        let mut ctx = BootstrapContext::new("run", None);
        check.remediate(&mut ctx).expect("raising is never fatal");
        assert_eq!(check.inspect(&ctx), Status::Satisfied);
    }

    #[test]
    fn fd_limit_applies_only_to_run() {
        let check = FdLimit::default();
        assert!(check.applies(&BootstrapContext::new("run", None)));
        assert!(!check.applies(&BootstrapContext::new("config", None)));
    }

    // ---- AirportIcao ----

    #[test]
    fn airport_check_does_not_apply_without_the_flag() {
        assert!(!AirportIcao.applies(&BootstrapContext::new("run", None)));
    }

    #[test]
    fn airport_check_applies_when_the_flag_is_present() {
        assert!(AirportIcao.applies(&BootstrapContext::new("run", Some("KSFO".into()))));
    }

    #[test]
    fn airport_check_does_not_apply_outside_run() {
        assert!(!AirportIcao.applies(&BootstrapContext::new("packages", Some("KSFO".into()))));
    }

    #[test]
    fn airport_check_names_the_missing_scenery_path_rather_than_panicking() {
        let ctx = BootstrapContext::new("run", Some("KSFO".into()));
        match AirportIcao.inspect(&ctx) {
            Status::Unsatisfied { reason, .. } => assert!(
                reason.contains("Custom Scenery path"),
                "reason must name the missing value, got: {reason}"
            ),
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn airport_check_is_unsatisfied_when_the_scenery_path_is_not_an_xplane_install() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = BootstrapContext::new("run", Some("KSFO".into()));
        ctx.set_custom_scenery_path(PathBuf::from(dir.path()));
        assert!(matches!(
            AirportIcao.inspect(&ctx),
            Status::Unsatisfied {
                remediable: false,
                ..
            }
        ));
    }
}
