//! The context threaded through the bootstrap preflight pipeline.

use crate::config::ConfigFile;
use std::borrow::Cow;
use std::path::PathBuf;

/// Values accumulated by the bootstrap preflight pipeline.
///
/// Checks read what they need and contribute what they own. They never call one
/// another, so every field a check contributes is `Option` until that check has
/// run. A check that reads an absent field returns
/// [`Status::Unsatisfied`](super::Status) naming it, which turns a misordered
/// registry into a legible error rather than a silent `None`.
///
/// A concrete struct rather than a type-keyed store: this states the pipeline's
/// whole contract in one definition, the compiler enumerates every reader, and
/// absence is greppable. A dynamic bag hides all three.
///
/// # Inputs, not services
///
/// This carries **validated inputs**: resolved paths, the loaded configuration,
/// detected hardware. It does not carry constructed services.
/// [`service::builder`](crate::service), `service::runtime_builder` and the
/// service orchestrator already own construction and consume these values.
/// Preflight establishes preconditions; it does not start XEarthLayer.
///
/// # Library types only
///
/// Deliberately free of CLI argument types. A `clap` dependency here would make
/// the context unusable from anywhere but the binary, which is exactly the
/// coupling the generic [`Preflight`](super::Preflight) trait exists to avoid.
pub struct BootstrapContext {
    command: Cow<'static, str>,
    airport: Option<String>,
    config: Option<ConfigFile>,
    install_location: Option<PathBuf>,
    custom_scenery_path: Option<PathBuf>,
    xworld_pro: Option<crate::xworld_pro::Detection>,
    xworld_order_accepted: bool,
    startup_warnings: Vec<String>,
}

impl BootstrapContext {
    /// Create a context for `command`, carrying the requested airport if any.
    pub fn new(command: impl Into<Cow<'static, str>>, airport: Option<String>) -> Self {
        Self {
            command: command.into(),
            airport,
            config: None,
            install_location: None,
            custom_scenery_path: None,
            xworld_pro: None,
            xworld_order_accepted: false,
            startup_warnings: Vec::new(),
        }
    }

    /// The command being run. Checks use this to decide whether they apply.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The airport requested on the command line, if any.
    pub fn airport(&self) -> Option<&str> {
        self.airport.as_deref()
    }

    /// The loaded configuration, once a check has contributed it.
    pub fn config(&self) -> Option<&ConfigFile> {
        self.config.as_ref()
    }

    /// Where installed packages live, once a check has resolved it.
    pub fn install_location(&self) -> Option<&PathBuf> {
        self.install_location.as_ref()
    }

    /// X-Plane's Custom Scenery directory, once a check has resolved it.
    pub fn custom_scenery_path(&self) -> Option<&PathBuf> {
        self.custom_scenery_path.as_ref()
    }

    /// Contribute the loaded configuration.
    pub fn set_config(&mut self, config: ConfigFile) {
        self.config = Some(config);
    }

    /// Contribute the resolved package install location.
    pub fn set_install_location(&mut self, path: PathBuf) {
        self.install_location = Some(path);
    }

    /// Contribute the resolved Custom Scenery path.
    pub fn set_custom_scenery_path(&mut self, path: PathBuf) {
        self.custom_scenery_path = Some(path);
    }

    /// SimHeaven X-World Pro detection, contributed when `packages.xworld_pro`
    /// is on.
    pub fn xworld_pro(&self) -> Option<&crate::xworld_pro::Detection> {
        self.xworld_pro.as_ref()
    }

    /// Contribute the X-World Pro detection.
    pub fn set_xworld_pro(&mut self, detection: crate::xworld_pro::Detection) {
        self.xworld_pro = Some(detection);
    }

    /// The user chose to continue despite an X-World Pro order problem.
    pub fn xworld_order_accepted(&self) -> bool {
        self.xworld_order_accepted
    }

    /// Record that the user chose to continue despite the order problem.
    pub fn accept_xworld_order(&mut self) {
        self.xworld_order_accepted = true;
    }

    /// The warnings the checks reported, in order, for a front end that
    /// replaces the screen they were printed on (#326).
    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_warnings
    }

    /// Record the warnings the checks reported.
    pub fn set_startup_warnings(&mut self, warnings: Vec<String>) {
        self.startup_warnings = warnings;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_empty_before_any_check_contributes() {
        let ctx = BootstrapContext::new("run", None);
        assert_eq!(ctx.command(), "run");
        assert!(ctx.startup_warnings().is_empty());
        assert!(ctx.airport().is_none());
        assert!(ctx.config().is_none());
        assert!(ctx.install_location().is_none());
        assert!(ctx.custom_scenery_path().is_none());
        assert!(ctx.xworld_pro().is_none());
        assert!(!ctx.xworld_order_accepted());
    }

    #[test]
    fn the_xworld_pro_detection_and_order_answer_are_contributed() {
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_xworld_pro(crate::xworld_pro::Detection::default());
        ctx.accept_xworld_order();
        assert!(ctx.xworld_pro().is_some());
        assert!(ctx.xworld_order_accepted());
    }

    #[test]
    fn contributions_are_visible_to_later_readers() {
        let mut ctx = BootstrapContext::new("run", Some("KSFO".to_string()));
        ctx.set_install_location(PathBuf::from("/tmp/packages"));
        ctx.set_custom_scenery_path(PathBuf::from("/tmp/Custom Scenery"));
        assert_eq!(
            ctx.install_location(),
            Some(&PathBuf::from("/tmp/packages"))
        );
        assert_eq!(
            ctx.custom_scenery_path(),
            Some(&PathBuf::from("/tmp/Custom Scenery"))
        );
        assert_eq!(ctx.airport(), Some("KSFO"));
    }

    #[test]
    fn config_is_contributed_not_loaded_by_the_context() {
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_config(ConfigFile::default());
        assert!(
            ctx.config().is_some(),
            "the context stores what a check gives it and loads nothing itself"
        );
    }

    #[test]
    fn command_accepts_a_borrowed_static_without_allocating() {
        let ctx = BootstrapContext::new("diagnostics", None);
        assert_eq!(ctx.command(), "diagnostics");
    }
}
