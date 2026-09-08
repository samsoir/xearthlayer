//! Migrating a pre-0.5.0 installation to the platform-native layout.

use super::names;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use xearthlayer::config::ConfigFile;
use xearthlayer::paths::{BaseDirectories, LAYOUT_VERSION};
use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};

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
}

impl LayoutMigration {
    pub fn new(legacy_dir: PathBuf, target: Box<dyn BaseDirectories>) -> Self {
        Self { legacy_dir, target }
    }

    /// The legacy `config.ini`.
    fn legacy_config_file(&self) -> PathBuf {
        self.legacy_dir.join("config.ini")
    }

    /// Where the configuration is migrated to.
    pub fn target_config_file(&self) -> PathBuf {
        self.target.config_dir().join("config.ini")
    }

    /// Pin a legacy default that holds data, so nothing is stranded.
    ///
    /// Only when the configuration does not already say where the resource is:
    /// an explicit setting is the user's answer and is never overwritten.
    fn pin_if_populated(current: &mut Option<PathBuf>, legacy: &Path) {
        if current.is_none() && legacy.exists() {
            *current = Some(legacy.to_path_buf());
        }
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

impl Preflight<BootstrapContext> for LayoutMigration {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::LAYOUT_MIGRATION)
    }

    /// **Every command**, unlike every other check.
    ///
    /// `run` and the setup wizard each decide independently whether an
    /// installation exists, and `config`, `packages` and `diagnostics` all read
    /// paths. A migration that applied only to `run` would leave `setup`
    /// treating a user as new.
    fn applies(&self, _ctx: &BootstrapContext) -> bool {
        true
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

        // Pin what holds data, so accepting the migration cannot break a
        // working installation. Packages and patches are user data we will not
        // move; the log, the staging directory and the index caches are
        // regenerable and adopt the new locations silently.
        Self::pin_if_populated(
            &mut config.packages.install_location,
            &self.legacy_dir.join("packages"),
        );
        Self::pin_if_populated(
            &mut config.patches.directory,
            &self.legacy_dir.join("patches"),
        );

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

    fn check(f: &Fixture) -> LayoutMigration {
        LayoutMigration::new(
            f.legacy.clone(),
            Box::new(TestDirectories::rooted_at(
                f.base.config_dir().parent().unwrap(),
            )),
        )
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
        assert!(
            config.packages.install_location.is_none(),
            "nothing to pin, so the new default applies"
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
