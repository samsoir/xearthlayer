//! Migrating a pre-0.5.0 installation to the platform-native layout.

use super::names;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use xearthlayer::config::ConfigFile;
use xearthlayer::paths::{BaseDirectories, LAYOUT_VERSION};
use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};

/// How the proposed layout is confirmed with the user.
///
/// Injected rather than decided inside the migration. Sniffing the environment
/// for a terminal is not enough: a unit test run from a developer's shell has a
/// terminal, so a migration that prompts when it sees one will prompt from
/// inside the test suite and wait for a keypress that never comes.
pub trait ConfirmLayout: Send + Sync {
    fn confirm(
        &self,
        proposals: Vec<super::proposal::ResourceProposal>,
    ) -> Vec<super::proposal::ResourceProposal>;
}

/// Accept the proposal without asking.
///
/// What tests use, and what the interactive confirmer falls back to when there
/// is no terminal to ask in.
pub struct AcceptProposal;

impl ConfirmLayout for AcceptProposal {
    fn confirm(
        &self,
        proposals: Vec<super::proposal::ResourceProposal>,
    ) -> Vec<super::proposal::ResourceProposal> {
        proposals
    }
}

/// Ask the user, when there is a terminal to ask in.
pub struct AskUser;

impl ConfirmLayout for AskUser {
    fn confirm(
        &self,
        mut proposals: Vec<super::proposal::ResourceProposal>,
    ) -> Vec<super::proposal::ResourceProposal> {
        use super::proposal::{negotiable, toggle};
        use std::io::IsTerminal;

        let pending = negotiable(&proposals);
        if pending.is_empty() {
            return proposals;
        }

        // `cfg!(test)` is a second line of defence behind injection. A test
        // must never block on input, and stdin *is* a terminal when the suite
        // is run from a shell, which is how a single unit test came to hang
        // `make verify` on every platform.
        let can_ask = !cfg!(test) && std::io::stdin().is_terminal();
        if !can_ask {
            // No terminal to ask in, so accept the proposal. That is a no-op
            // and always correct; blocking under a service manager would not
            // be.
            println!(
                "Some locations were kept where they are. Run 'xearthlayer migrate' \
                 from a terminal to review them."
            );
            println!();
            return AcceptProposal.confirm(proposals);
        }

        let theme = dialoguer::theme::ColorfulTheme::default();
        loop {
            let accept = dialoguer::Confirm::with_theme(&theme)
                .with_prompt("Use this layout?")
                .default(true)
                .interact()
                .unwrap_or(true);
            if accept {
                return proposals;
            }

            let choices: Vec<String> = pending
                .iter()
                .map(|&i| {
                    let p = &proposals[i];
                    format!(
                        "{}: {}",
                        p.resource.label(),
                        p.effective_path().unwrap_or_default().display()
                    )
                })
                .collect();

            let Ok(picked) = dialoguer::Select::with_theme(&theme)
                .with_prompt("Which location should change?")
                .items(&choices)
                .default(0)
                .interact()
            else {
                return proposals;
            };
            toggle(&mut proposals[pending[picked]]);
            println!();
            print!("{}", super::proposal::render(&proposals));
            println!();
        }
    }
}

/// Name of the marker left behind in the legacy directory.
const MARKER: &str = "MIGRATED.txt";

/// Move a pre-0.5.0 installation onto the platform-native layout.
///
/// # What moves
///
/// **Only `config.ini`, and it is copied rather than moved.** Nothing else is
/// relocated. Determining whether two paths are on the same filesystem is not
/// reliably answerable: `rename` returns `EXDEV` across btrfs subvolumes, bind
/// mounts and overlayfs layers, all inside what `df` shows as one filesystem.
/// Behaviour would vary between machines and be unreproducible in a bug report.
///
/// Everything else is **pinned**: where a resource sits at a legacy default and
/// holds real data, its old absolute path is written into the migrated
/// configuration. Nothing moves and nothing breaks.
///
/// # Why the legacy configuration is kept
///
/// Someone who rolls back to 0.4.8 must find a working installation, and 0.4.8
/// reads that file. Its paths are rewritten to the new absolute locations and a
/// comment header is prepended: INI comments are inert, so an older version
/// still parses it, and a human who opens it out of habit reads on line one
/// that it is no longer the file in use.
pub struct LayoutMigration {
    legacy_dir: PathBuf,
    target: Box<dyn BaseDirectories>,
    /// Whether this registration is the automatic one run before dispatch.
    automatic: bool,
    confirm: Box<dyn ConfirmLayout>,
}

impl LayoutMigration {
    /// The registration in the bootstrap registry, run before dispatch.
    pub fn automatic(legacy_dir: PathBuf, target: Box<dyn BaseDirectories>) -> Self {
        Self {
            legacy_dir,
            target,
            automatic: true,
            confirm: Box::new(AskUser),
        }
    }

    /// The registration used by `xearthlayer migrate`, which the user asked for
    /// explicitly and which therefore applies to that command.
    pub fn on_demand(legacy_dir: PathBuf, target: Box<dyn BaseDirectories>) -> Self {
        Self {
            legacy_dir,
            target,
            automatic: false,
            confirm: Box::new(AskUser),
        }
    }

    /// Replace how the layout is confirmed.
    ///
    /// A test seam. Nothing in the suite may hold [`AskUser`], because a test
    /// that reaches a prompt waits for a keypress that never comes.
    #[cfg(test)]
    pub fn confirming_with(mut self, confirm: Box<dyn ConfirmLayout>) -> Self {
        self.confirm = confirm;
        self
    }

    /// The legacy `config.ini`.
    fn legacy_config_file(&self) -> PathBuf {
        self.legacy_dir.join("config.ini")
    }

    /// Where the configuration is migrated to.
    pub fn target_config_file(&self) -> PathBuf {
        self.target.config_dir().join("config.ini")
    }

    fn write_marker(&self, migrated_to: &Path) -> std::io::Result<()> {
        let body = format!(
            "XEarthLayer moved its files out of this directory.\n\
             \n\
             Performed by:   XEarthLayer {}\n\
             Layout version: {}\n\
             \n\
             Configuration:  {}\n\
             \n\
             Files left here are not read by XEarthLayer {} or later. The index\n\
             caches are stale and will be rebuilt in their new location; they are\n\
             safe to delete. Anything else in this directory was put here by you.\n",
            env!("CARGO_PKG_VERSION"),
            LAYOUT_VERSION,
            migrated_to.display(),
            env!("CARGO_PKG_VERSION"),
        );
        std::fs::write(self.legacy_dir.join(MARKER), body)
    }

    /// Rewrite the legacy configuration so an older version still works, and
    /// prepend a notice so a human editing it is not silently ignored.
    fn annotate_legacy(&self, config: &ConfigFile) -> std::io::Result<()> {
        let mut pinned = config.clone();
        pinned.general.layout_version = 0;
        let path = self.legacy_config_file();
        pinned
            .save_to(&path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let body = std::fs::read_to_string(&path)?;
        let notice = format!(
            "; This file is no longer read by XEarthLayer {} or later.\n\
             ; The configuration in use is:\n\
             ;   {}\n\
             ; Edits here have no effect. This copy is kept so that an older\n\
             ; version of XEarthLayer still finds a working installation.\n\
             \n",
            env!("CARGO_PKG_VERSION"),
            self.target_config_file().display(),
        );
        std::fs::write(&path, notice + &body)
    }
}

/// Tell the user about data the accepted layout walks away from.
///
/// We do not move data, so anything left behind has to be named. A user who
/// discovers an unexplained several hundred gigabyte directory months later has
/// been failed by this migration even though nothing broke.
fn report_guidance(proposals: &[super::proposal::ResourceProposal]) {
    let items = super::proposal::guidance(proposals);
    if items.is_empty() {
        return;
    }

    println!("Some files are left in their old locations:");
    println!();
    for item in items {
        println!("  {}", item.resource.label());
        println!("    {}", item.left_behind.display());
        println!("    {}", item.note);
        if let Some(command) = item.command {
            println!("    {}", command);
        }
        println!();
    }
}

impl LayoutMigration {
    /// Show the proposed layout, then let the confirmer decide.
    ///
    /// The readback is always printed: it answers "where will my files be",
    /// which matters whether or not anything is being asked. Only the asking is
    /// delegated.
    fn negotiate(
        &self,
        proposals: Vec<super::proposal::ResourceProposal>,
    ) -> Vec<super::proposal::ResourceProposal> {
        println!();
        println!("XEarthLayer is moving to the standard directories for your system.");
        println!();
        print!("{}", super::proposal::render(&proposals));
        println!();
        self.confirm.confirm(proposals)
    }
}

impl Preflight<BootstrapContext> for LayoutMigration {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::LAYOUT_MIGRATION)
    }

    /// **Every command except `migrate` itself.**
    ///
    /// Unlike every other check, this applies beyond `run`: the setup wizard
    /// decides independently whether an installation exists, and `config`,
    /// `packages` and `diagnostics` all read paths. A migration scoped to `run`
    /// would leave `setup` treating a migrated user as new.
    ///
    /// `migrate` is the exception, and only for the automatic registration.
    /// The bootstrap registry executes before command dispatch, so migrating
    /// there would run the migration before `migrate --dry-run` could report on
    /// it, and the dry run would describe work it had already done. The
    /// on-demand registration the command builds for itself applies normally.
    fn applies(&self, ctx: &BootstrapContext) -> bool {
        !(self.automatic && ctx.command() == "migrate")
    }

    fn inspect(&self, _ctx: &BootstrapContext) -> Status {
        if !self.legacy_config_file().exists() {
            // Nothing to migrate: a fresh installation, or one whose legacy
            // directory has already been cleaned up.
            return Status::Satisfied;
        }

        let target = self.target_config_file();
        if target.exists() {
            let version = ConfigFile::load_from(&target)
                .map(|c| c.general.layout_version)
                .unwrap_or(0);
            if version >= LAYOUT_VERSION {
                return Status::Satisfied;
            }
        }

        Status::unsatisfied(format!(
            "configuration is still in the pre-{} layout at {}",
            LAYOUT_VERSION,
            self.legacy_dir.display()
        ))
        .remediable()
    }

    fn remediate(
        &self,
        _ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let fail = |e: std::io::Error, what: &str| {
            PreflightError::new(names::LAYOUT_MIGRATION, format!("{}: {}", what, e))
        };

        let mut config = ConfigFile::load_from(&self.legacy_config_file()).map_err(|e| {
            PreflightError::new(
                names::LAYOUT_MIGRATION,
                format!(
                    "Failed to read {}: {}",
                    self.legacy_config_file().display(),
                    e
                ),
            )
        })?;

        // The decision table decides; this only writes the answer. Keeping the
        // two apart is what lets the governing invariant, that accepting the
        // proposal never breaks a working installation, be asserted directly
        // rather than inferred from what a prompt rendered.
        let proposals =
            super::proposal::propose(&config, &self.legacy_dir, self.target.as_ref(), |p| {
                p.exists()
            });
        let proposals = self.negotiate(proposals);
        report_guidance(&proposals);
        super::proposal::apply(&proposals, &mut config);

        config.general.layout_version = LAYOUT_VERSION;

        std::fs::create_dir_all(self.target.config_dir())
            .map_err(|e| fail(e, "Failed to create the configuration directory"))?;
        config.save_to(&self.target_config_file()).map_err(|e| {
            PreflightError::new(
                names::LAYOUT_MIGRATION,
                format!("Failed to write the migrated configuration: {}", e),
            )
        })?;

        self.annotate_legacy(&config)
            .map_err(|e| fail(e, "Failed to annotate the legacy configuration"))?;
        self.write_marker(&self.target_config_file())
            .map_err(|e| fail(e, "Failed to write the migration marker"))?;

        Ok(Remedy {
            message: Some(format!(
                "Configuration moved to {}",
                self.target_config_file().display()
            )),
            register: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFile;
    use xearthlayer::paths::testing::TestDirectories;
    use xearthlayer::paths::LAYOUT_VERSION;

    struct Fixture {
        _dir: tempfile::TempDir,
        legacy: PathBuf,
        base: TestDirectories,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(".xearthlayer");
        std::fs::create_dir_all(&legacy).unwrap();
        let base = TestDirectories::rooted_at(&dir.path().join("native"));
        Fixture {
            _dir: dir,
            legacy,
            base,
        }
    }

    fn write_legacy_config(f: &Fixture, body: &str) {
        std::fs::write(f.legacy.join("config.ini"), body).unwrap();
    }

    /// Every test builds the migration through here, so no test can hold the
    /// interactive confirmer and block the suite waiting for a keypress.
    fn check(f: &Fixture) -> LayoutMigration {
        LayoutMigration::automatic(
            f.legacy.clone(),
            Box::new(TestDirectories::rooted_at(
                f.base.config_dir().parent().unwrap(),
            )),
        )
        .confirming_with(Box::new(AcceptProposal))
    }

    #[test]
    fn accepting_the_proposal_asks_nothing_and_changes_nothing() {
        // The regression this guards: remediate() prompted whenever stdin was a
        // terminal, which it is for anyone running the suite from a shell, so a
        // unit test sat waiting for a keypress and hung `make verify` on every
        // platform. Confirmation is injected now, and this pins the
        // non-interactive confirmer as a pure pass-through.
        let f = fixture();
        std::fs::create_dir_all(f.legacy.join("packages")).unwrap();
        write_legacy_config(&f, "[general]\n");

        let proposals = crate::preflight::proposal::propose(
            &ConfigFile::load_from(&f.legacy.join("config.ini")).unwrap(),
            &f.legacy,
            &TestDirectories::rooted_at(f.base.config_dir().parent().unwrap()),
            |p| p.exists(),
        );
        assert!(
            !crate::preflight::proposal::negotiable(&proposals).is_empty(),
            "the fixture must actually have something to ask about"
        );

        let before: Vec<_> = proposals.iter().map(|p| p.disposition).collect();
        let after = AcceptProposal.confirm(proposals);
        assert_eq!(
            after.iter().map(|p| p.disposition).collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn nothing_to_migrate_when_there_is_no_legacy_configuration() {
        let f = fixture();
        assert_eq!(
            check(&f).inspect(&BootstrapContext::new("run", None)),
            Status::Satisfied
        );
    }

    #[test]
    fn a_legacy_configuration_asks_to_be_migrated() {
        let f = fixture();
        write_legacy_config(&f, "[general]\nupdate_check = true\n");
        assert!(matches!(
            check(&f).inspect(&BootstrapContext::new("run", None)),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
    }

    #[test]
    fn migration_copies_the_configuration_and_stamps_the_layout() {
        let f = fixture();
        write_legacy_config(&f, "[general]\nupdate_check = false\n");
        let c = check(&f);
        let mut ctx = BootstrapContext::new("run", None);

        c.remediate(&mut ctx).expect("migration succeeds");

        let migrated = c.target_config_file();
        assert!(migrated.exists(), "the configuration must be copied");
        let config = ConfigFile::load_from(&migrated).unwrap();
        assert_eq!(config.general.layout_version, LAYOUT_VERSION);
        assert!(
            !config.general.update_check,
            "settings must survive the copy"
        );
        assert_eq!(c.inspect(&ctx), Status::Satisfied);
    }

    #[test]
    fn the_legacy_configuration_is_kept_and_still_parses() {
        // Copy, not move. Someone who rolls back to 0.4.8 must find a working
        // installation, and 0.4.8 reads this file.
        let f = fixture();
        write_legacy_config(&f, "[general]\nupdate_check = false\n");
        check(&f)
            .remediate(&mut BootstrapContext::new("run", None))
            .unwrap();

        let legacy_config = f.legacy.join("config.ini");
        assert!(legacy_config.exists(), "the legacy file must not be moved");
        let parsed = ConfigFile::load_from(&legacy_config)
            .expect("an older version must still be able to parse it");
        assert!(!parsed.general.update_check);
    }

    #[test]
    fn the_legacy_configuration_gains_a_notice_a_human_will_see_first() {
        let f = fixture();
        write_legacy_config(&f, "[general]\nupdate_check = true\n");
        check(&f)
            .remediate(&mut BootstrapContext::new("run", None))
            .unwrap();
        let body = std::fs::read_to_string(f.legacy.join("config.ini")).unwrap();
        let first = body.lines().next().unwrap();
        assert!(
            first.starts_with(';'),
            "an INI comment, so old versions ignore it"
        );
        assert!(
            body.contains("no longer read"),
            "editing this file must not silently do nothing"
        );
    }

    #[test]
    fn a_marker_records_what_happened_and_where_things_went() {
        let f = fixture();
        write_legacy_config(&f, "[general]\n");
        let c = check(&f);
        c.remediate(&mut BootstrapContext::new("run", None))
            .unwrap();

        let marker = std::fs::read_to_string(f.legacy.join("MIGRATED.txt")).unwrap();
        assert!(marker.contains(&LAYOUT_VERSION.to_string()));
        assert!(marker.contains(env!("CARGO_PKG_VERSION")));
        assert!(
            marker.contains(&c.target_config_file().display().to_string()),
            "the marker must name where the configuration went"
        );
    }

    #[test]
    fn packages_at_the_legacy_default_are_pinned_not_stranded() {
        // The governing invariant: accepting the migration must never break a
        // working install. We do not move bulk data, so the only safe answer is
        // to write the old location into the new configuration.
        let f = fixture();
        write_legacy_config(&f, "[general]\n");
        let packages = f.legacy.join("packages");
        std::fs::create_dir_all(packages.join("na")).unwrap();

        let c = check(&f);
        c.remediate(&mut BootstrapContext::new("run", None))
            .unwrap();

        let config = ConfigFile::load_from(&c.target_config_file()).unwrap();
        assert_eq!(
            config.packages.install_location.as_deref(),
            Some(packages.as_path()),
            "installed scenery must still be found after migrating"
        );
    }

    #[test]
    fn an_absent_legacy_packages_directory_is_not_pinned() {
        let f = fixture();
        write_legacy_config(&f, "[general]\n");
        let c = check(&f);
        c.remediate(&mut BootstrapContext::new("run", None))
            .unwrap();
        let config = ConfigFile::load_from(&c.target_config_file()).unwrap();
        assert_eq!(
            config.packages.install_location,
            Some(xearthlayer::paths::packages_dir_in(
                &TestDirectories::rooted_at(
                    c.target_config_file().parent().unwrap().parent().unwrap()
                )
            )),
            "nothing to pin, so it adopts the new location and says so explicitly"
        );
    }

    #[test]
    fn an_explicit_setting_is_never_overwritten_by_pinning() {
        let f = fixture();
        write_legacy_config(
            &f,
            "[packages]\ninstall_location = /media/FlightSim/Packages\n",
        );
        std::fs::create_dir_all(f.legacy.join("packages")).unwrap();
        let c = check(&f);
        c.remediate(&mut BootstrapContext::new("run", None))
            .unwrap();
        let config = ConfigFile::load_from(&c.target_config_file()).unwrap();
        assert_eq!(
            config.packages.install_location,
            Some(PathBuf::from("/media/FlightSim/Packages"))
        );
    }

    #[test]
    fn migration_applies_to_every_command_not_only_run() {
        // run and setup each decide whether an installation exists. If this
        // applied only to run, setup would treat a migrated user as new.
        let f = fixture();
        let c = check(&f);
        for command in ["run", "setup", "config", "diagnostics", "packages"] {
            assert!(
                c.applies(&BootstrapContext::new(command, None)),
                "{command} needs the migrated layout too"
            );
        }
    }

    #[test]
    fn migration_does_not_apply_to_the_migrate_command_itself() {
        // The registry runs before dispatch. If this applied, the automatic
        // migration would already have run by the time `migrate --dry-run`
        // reported, so the dry run would describe work it had just done.
        let f = fixture();
        assert!(!check(&f).applies(&BootstrapContext::new("migrate", None)));
    }

    #[test]
    fn the_on_demand_registration_applies_to_the_migrate_command() {
        // The command builds its own registration precisely so that asking for
        // a migration runs one.
        let f = fixture();
        let c = LayoutMigration::on_demand(
            f.legacy.clone(),
            Box::new(TestDirectories::rooted_at(
                f.base.config_dir().parent().unwrap(),
            )),
        )
        .confirming_with(Box::new(AcceptProposal));
        assert!(c.applies(&BootstrapContext::new("migrate", None)));
    }

    #[test]
    fn an_already_migrated_installation_is_a_no_op() {
        let f = fixture();
        write_legacy_config(&f, "[general]\n");
        let c = check(&f);
        c.remediate(&mut BootstrapContext::new("run", None))
            .unwrap();

        let before = std::fs::read_to_string(c.target_config_file()).unwrap();
        assert_eq!(
            c.inspect(&BootstrapContext::new("run", None)),
            Status::Satisfied
        );
        assert_eq!(
            std::fs::read_to_string(c.target_config_file()).unwrap(),
            before
        );
    }
}
