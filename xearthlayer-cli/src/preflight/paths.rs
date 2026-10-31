//! Path-resolution preflight checks.

use super::{legacy_default_packages_dir, names};
use std::borrow::Cow;
use std::path::PathBuf;
use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};

/// Resolve the Custom Scenery directory from configuration, falling back to
/// auto-detection of the X-Plane installation.
///
/// Precedence: `packages.custom_scenery_path` > `xplane.scenery_dir` >
/// auto-detect. Auto-detection is only attempted when both config values are
/// unset.
///
/// Moved here from `commands/run.rs` with its tests. One definition, because
/// duplicating a precedence rule is how the two copies drift.
pub fn resolve_custom_scenery_path(
    custom_scenery_path: Option<PathBuf>,
    scenery_dir: Option<PathBuf>,
    detect: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    custom_scenery_path.or(scenery_dir).or_else(detect)
}

/// Resolve where packages are installed, and require that the directory exists.
///
/// Resolves during remediation and judges during the inspection that follows.
pub struct PackagesInstalled;

impl Preflight<BootstrapContext> for PackagesInstalled {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::PACKAGES_INSTALLED)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        if ctx.config().is_none() {
            // Names the missing value rather than unwrapping. A misordered
            // registry becomes a legible error instead of a panic.
            return Status::unsatisfied("configuration has not been loaded");
        }
        match ctx.install_location() {
            None => {
                Status::unsatisfied("package install location has not been resolved").remediable()
            }
            Some(path) if !path.exists() => {
                Status::unsatisfied(format!("no ortho packages at {}", path.display()))
            }
            Some(_) => Status::Satisfied,
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let location = ctx
            .config()
            .and_then(|c| c.packages.install_location.clone())
            .unwrap_or_else(legacy_default_packages_dir);
        ctx.set_install_location(location);
        Ok(Remedy::default())
    }
}

/// Resolve X-Plane's Custom Scenery directory and contribute it.
pub struct ResolveCustomScenery {
    detect: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
}

impl ResolveCustomScenery {
    /// Inject the detector so tests never probe for a real X-Plane install.
    pub fn with_detector(detect: impl Fn() -> Option<PathBuf> + Send + Sync + 'static) -> Self {
        Self {
            detect: Box::new(detect),
        }
    }
}

impl Preflight<BootstrapContext> for ResolveCustomScenery {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::RESOLVE_CUSTOM_SCENERY)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        if ctx.config().is_none() {
            return Status::unsatisfied("configuration has not been loaded");
        }
        if ctx.custom_scenery_path().is_some() {
            Status::Satisfied
        } else {
            Status::unsatisfied("Custom Scenery path has not been resolved").remediable()
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let config = ctx.config();
        let resolved = resolve_custom_scenery_path(
            config.and_then(|c| c.packages.custom_scenery_path.clone()),
            config.and_then(|c| c.xplane.scenery_dir.clone()),
            || (self.detect)(),
        );

        match resolved {
            Some(path) => {
                ctx.set_custom_scenery_path(path);
                Ok(Remedy::default())
            }
            None => Err(PreflightError::new(
                names::RESOLVE_CUSTOM_SCENERY,
                "No Custom Scenery path configured. \
                 Run 'xearthlayer init' or set packages.custom_scenery_path in config.ini",
            )),
        }
    }
}

/// Require that the resolved Custom Scenery directory exists.
pub struct CustomSceneryExists;

impl Preflight<BootstrapContext> for CustomSceneryExists {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::CUSTOM_SCENERY_EXISTS)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        match ctx.custom_scenery_path() {
            None => Status::unsatisfied("Custom Scenery path has not been resolved"),
            Some(path) if !path.exists() => Status::unsatisfied(format!(
                "Custom Scenery directory does not exist: {}\n\
                 Check your configuration or run 'xearthlayer init'",
                path.display()
            )),
            Some(_) => Status::Satisfied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFile;

    fn ctx_with_config() -> BootstrapContext {
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_config(ConfigFile::default());
        ctx
    }

    // ---- resolve_custom_scenery_path (moved from commands/run.rs) ----

    #[test]
    fn explicit_custom_scenery_path_wins() {
        let resolved = resolve_custom_scenery_path(
            Some(PathBuf::from("/configured")),
            Some(PathBuf::from("/scenery-dir")),
            || panic!("auto-detect must not run when config is set"),
        );
        assert_eq!(resolved, Some(PathBuf::from("/configured")));
    }

    #[test]
    fn scenery_dir_used_when_custom_scenery_path_unset() {
        let resolved =
            resolve_custom_scenery_path(None, Some(PathBuf::from("/scenery-dir")), || {
                panic!("auto-detect must not run when scenery_dir is set")
            });
        assert_eq!(resolved, Some(PathBuf::from("/scenery-dir")));
    }

    #[test]
    fn auto_detect_used_when_both_config_values_unset() {
        let resolved = resolve_custom_scenery_path(None, None, || Some(PathBuf::from("/detected")));
        assert_eq!(resolved, Some(PathBuf::from("/detected")));
    }

    #[test]
    fn none_when_nothing_configured_and_detection_fails() {
        let resolved = resolve_custom_scenery_path(None, None, || None);
        assert_eq!(resolved, None);
    }

    // ---- PackagesInstalled ----

    #[test]
    fn packages_names_the_missing_config_rather_than_panicking() {
        // Reads what LoadConfig contributes. Run it against a context where
        // that check has not run: the reason must say so.
        let ctx = BootstrapContext::new("run", None);
        match PackagesInstalled.inspect(&ctx) {
            Status::Unsatisfied { reason, .. } => assert!(
                reason.contains("configuration"),
                "reason must name the missing value, got: {reason}"
            ),
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn packages_asks_to_be_remediated_before_the_location_is_resolved() {
        let ctx = ctx_with_config();
        assert!(matches!(
            PackagesInstalled.inspect(&ctx),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
    }

    #[test]
    fn packages_is_unsatisfied_when_the_resolved_directory_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_config();
        ctx.set_install_location(dir.path().join("nope"));
        assert!(matches!(
            PackagesInstalled.inspect(&ctx),
            Status::Unsatisfied {
                remediable: false,
                ..
            }
        ));
    }

    #[test]
    fn packages_is_satisfied_once_the_resolved_directory_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_config();
        ctx.set_install_location(dir.path().to_path_buf());
        assert_eq!(PackagesInstalled.inspect(&ctx), Status::Satisfied);
    }

    #[test]
    fn packages_remediation_contributes_the_configured_location() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ConfigFile::default();
        config.packages.install_location = Some(dir.path().to_path_buf());
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_config(config);

        PackagesInstalled.remediate(&mut ctx).unwrap();
        assert_eq!(ctx.install_location(), Some(&dir.path().to_path_buf()));
        assert_eq!(PackagesInstalled.inspect(&ctx), Status::Satisfied);
    }

    // ---- ResolveCustomScenery ----

    #[test]
    fn resolve_custom_scenery_message_is_unchanged_when_nothing_is_configured() {
        let mut ctx = ctx_with_config();
        let check = ResolveCustomScenery::with_detector(|| None);
        let err = check.remediate(&mut ctx).expect_err("nothing to resolve");
        assert_eq!(
            err.message,
            "No Custom Scenery path configured. \
             Run 'xearthlayer init' or set packages.custom_scenery_path in config.ini"
        );
    }

    #[test]
    fn resolve_custom_scenery_contributes_the_detected_path() {
        let mut ctx = ctx_with_config();
        let check = ResolveCustomScenery::with_detector(|| Some(PathBuf::from("/detected")));
        check.remediate(&mut ctx).unwrap();
        assert_eq!(ctx.custom_scenery_path(), Some(&PathBuf::from("/detected")));
    }

    // ---- CustomSceneryExists ----

    #[test]
    fn custom_scenery_exists_names_the_missing_value_rather_than_panicking() {
        let ctx = BootstrapContext::new("run", None);
        match CustomSceneryExists.inspect(&ctx) {
            Status::Unsatisfied { reason, .. } => assert!(
                reason.contains("Custom Scenery path"),
                "reason must name the missing value, got: {reason}"
            ),
            other => panic!("expected Unsatisfied naming the missing value, got {other:?}"),
        }
    }

    #[test]
    fn custom_scenery_exists_message_is_unchanged_when_the_directory_is_absent() {
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_custom_scenery_path(PathBuf::from("/definitely/not/here"));
        match CustomSceneryExists.inspect(&ctx) {
            Status::Unsatisfied { reason, .. } => assert_eq!(
                reason,
                "Custom Scenery directory does not exist: /definitely/not/here\n\
                 Check your configuration or run 'xearthlayer init'"
            ),
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn custom_scenery_exists_is_satisfied_for_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_custom_scenery_path(dir.path().to_path_buf());
        assert_eq!(CustomSceneryExists.inspect(&ctx), Status::Satisfied);
    }
}
