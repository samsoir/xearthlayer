//! Where XEarthLayer keeps its files.
//!
//! One resolver, four roles, three platform layouts. Everything that needs a
//! path asks here rather than constructing one, so relocating a role is a
//! change in one place instead of a search for every caller that guessed.

mod apple;
mod base;
mod legacy;
mod xdg;

pub mod testing;

pub use apple::AppleDirectories;
pub use base::BaseDirectories;
pub use legacy::LegacyDirectories;
pub use xdg::XdgDirectories;

use std::path::PathBuf;
use std::sync::OnceLock;

/// The layout this build conforms to.
///
/// A layout version rather than an application version, so a future relocation
/// bumps it independently of releases. Recorded in `general.layout_version`.
///
/// **0 means the configuration predates the versioned layout**, which is why
/// [`crate::config::ConfigFile::default`] uses 0 and not this constant: the
/// parser overlays a file onto the defaults, so defaulting to the current
/// version would make every pre-0.5.0 configuration claim to be migrated
/// already.
pub const LAYOUT_VERSION: u32 = 1;

/// The layout in force for this process.
///
/// Resolved once. Every role accessor reads through it, so relocating a role is
/// a change here rather than a search for every caller that guessed.
fn active() -> &'static dyn BaseDirectories {
    static ACTIVE: OnceLock<Box<dyn BaseDirectories>> = OnceLock::new();
    ACTIVE
        .get_or_init(|| -> Box<dyn BaseDirectories> {
            // `cfg!` rather than `#[cfg]`: both implementations are plain path
            // arithmetic and compile everywhere, so removing one from the build
            // would only stop it being type checked on the other platform. Same
            // reasoning as the platform-gated preflight checks; see
            // docs/dev/preflight-design.md.
            if cfg!(target_os = "macos") {
                Box::new(AppleDirectories::from_home(home()))
            } else {
                Box::new(XdgDirectories::detect(home()))
            }
        })
        .as_ref()
}

/// The user's home directory, or the working directory if there is none.
///
/// The fallback matches what every call site did before this module existed.
/// It is a poor answer, but changing it is a behaviour change and belongs to
/// nobody's task here.
fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

// Role accessors. Each `_in` variant takes an explicit layout so tests never
// read a real home directory; the bare variant uses the active layout.

/// The layout in force for this process.
///
/// Exposed so the migration can name the target it is writing to, and so a test
/// can substitute one. Everything else should use the role accessors below
/// rather than composing paths from this.
pub fn layout() -> &'static dyn BaseDirectories {
    active()
}

/// An owned copy of the active layout.
///
/// [`layout`] borrows for the process lifetime, which suits a reader. Anything
/// that has to *store* the layout, such as the migration, needs its own, and a
/// test substitutes a different one in the same slot.
pub fn layout_snapshot() -> impl BaseDirectories {
    let l = layout();
    Snapshot {
        config: l.config_dir(),
        cache: l.cache_dir(),
        data: l.data_dir(),
        state: l.state_dir(),
    }
}

struct Snapshot {
    config: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    state: PathBuf,
}

impl BaseDirectories for Snapshot {
    fn config_dir(&self) -> PathBuf {
        self.config.clone()
    }
    fn cache_dir(&self) -> PathBuf {
        self.cache.clone()
    }
    fn data_dir(&self) -> PathBuf {
        self.data.clone()
    }
    fn state_dir(&self) -> PathBuf {
        self.state.clone()
    }
}

/// The directory holding user-editable configuration.
///
/// Exposed as a directory because two callers legitimately need the directory
/// rather than a file in it: the diagnostics report, which asks whether it
/// exists, and the migration, which names it to the user.
pub fn config_dir() -> PathBuf {
    active().config_dir()
}

/// `config.ini`.
pub fn config_file() -> PathBuf {
    config_file_in(active())
}
pub fn config_file_in(base: &dyn BaseDirectories) -> PathBuf {
    base.config_dir().join("config.ini")
}

/// The scenery index cache. Regenerable, so it lives with the caches.
pub fn scenery_index_cache() -> PathBuf {
    scenery_index_cache_in(active())
}
pub fn scenery_index_cache_in(base: &dyn BaseDirectories) -> PathBuf {
    base.cache_dir().join("scenery_index.cache")
}

/// The ortho union index cache. Regenerable.
pub fn ortho_union_index_cache() -> PathBuf {
    ortho_union_index_cache_in(active())
}
pub fn ortho_union_index_cache_in(base: &dyn BaseDirectories) -> PathBuf {
    base.cache_dir().join("ortho_union_index.cache")
}

/// The update check's 24 hour cache.
pub fn version_check_file() -> PathBuf {
    version_check_file_in(active())
}
pub fn version_check_file_in(base: &dyn BaseDirectories) -> PathBuf {
    base.state_dir().join("version_check.json")
}

/// The default log file. Overridable by `logging.file`.
pub fn log_file() -> PathBuf {
    log_file_in(active())
}
pub fn log_file_in(base: &dyn BaseDirectories) -> PathBuf {
    base.state_dir().join("xearthlayer.log")
}

/// The running instance lock.
pub fn lock_file() -> PathBuf {
    lock_file_in(active())
}
pub fn lock_file_in(base: &dyn BaseDirectories) -> PathBuf {
    base.state_dir().join("xearthlayer.lock")
}

/// Default location for installed packages. Overridable by
/// `packages.install_location`.
pub fn packages_dir() -> PathBuf {
    packages_dir_in(active())
}
pub fn packages_dir_in(base: &dyn BaseDirectories) -> PathBuf {
    base.data_dir().join("packages")
}

/// Default location for scenery patches. Overridable by `patches.directory`.
pub fn patches_dir() -> PathBuf {
    patches_dir_in(active())
}
pub fn patches_dir_in(base: &dyn BaseDirectories) -> PathBuf {
    base.data_dir().join("patches")
}

/// Default staging directory for package downloads. Overridable by
/// `packages.temp_dir`.
pub fn temp_dir() -> PathBuf {
    temp_dir_in(active())
}
pub fn temp_dir_in(base: &dyn BaseDirectories) -> PathBuf {
    base.cache_dir().join("tmp")
}

/// Default location for the generated tile cache. Overridable by
/// `cache.directory`.
///
/// This tier was already compliant before the resolver existed: it defaulted to
/// `dirs::cache_dir()`, which on Linux is exactly the XDG cache directory. The
/// flip therefore moves nothing there. On macOS it gains the capitalised
/// application name, which reaches only a fresh install, since an installation
/// that has written a configuration file keeps whatever `cache.directory` it
/// stored.
pub fn tile_cache_dir() -> PathBuf {
    active().cache_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn xdg_falls_back_to_the_spec_defaults_when_nothing_is_set() {
        let home = PathBuf::from("/home/pilot");
        let d = XdgDirectories::from_parts(home.clone(), None, None, None, None);
        assert_eq!(d.config_dir(), home.join(".config/xearthlayer"));
        assert_eq!(d.cache_dir(), home.join(".cache/xearthlayer"));
        assert_eq!(d.data_dir(), home.join(".local/share/xearthlayer"));
        assert_eq!(d.state_dir(), home.join(".local/state/xearthlayer"));
    }

    #[test]
    fn xdg_honours_the_environment_when_it_is_set() {
        let d = XdgDirectories::from_parts(
            PathBuf::from("/home/pilot"),
            Some(PathBuf::from("/xdg/config")),
            Some(PathBuf::from("/xdg/cache")),
            Some(PathBuf::from("/xdg/data")),
            Some(PathBuf::from("/xdg/state")),
        );
        assert_eq!(d.config_dir(), PathBuf::from("/xdg/config/xearthlayer"));
        assert_eq!(d.cache_dir(), PathBuf::from("/xdg/cache/xearthlayer"));
        assert_eq!(d.data_dir(), PathBuf::from("/xdg/data/xearthlayer"));
        assert_eq!(d.state_dir(), PathBuf::from("/xdg/state/xearthlayer"));
    }

    #[test]
    fn xdg_roles_are_four_distinct_directories() {
        let d = XdgDirectories::from_parts(PathBuf::from("/home/pilot"), None, None, None, None);
        let all = [d.config_dir(), d.cache_dir(), d.data_dir(), d.state_dir()];
        let mut unique = all.to_vec();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "XDG separates all four roles");
    }

    #[test]
    fn apple_collapses_config_and_data_and_puts_state_in_logs() {
        // dirs::config_dir() and dirs::data_dir() both return Application
        // Support on macOS, and dirs::state_dir() returns None there. Nothing
        // may assume the four roles are four distinct directories.
        let home = PathBuf::from("/Users/pilot");
        let d = AppleDirectories::from_home(home.clone());
        let app_support = home.join("Library/Application Support/XEarthLayer");
        assert_eq!(d.config_dir(), app_support);
        assert_eq!(d.data_dir(), app_support);
        assert_eq!(d.cache_dir(), home.join("Library/Caches/XEarthLayer"));
        assert_eq!(d.state_dir(), home.join("Library/Logs/XEarthLayer"));
    }

    #[test]
    fn apple_uses_the_capitalised_bundle_name() {
        let d = AppleDirectories::from_home(PathBuf::from("/Users/pilot"));
        assert!(
            d.config_dir().to_string_lossy().contains("XEarthLayer"),
            "macOS convention capitalises the application directory"
        );
    }

    #[test]
    fn legacy_puts_every_role_in_one_directory() {
        let home = PathBuf::from("/home/pilot");
        let d = LegacyDirectories::from_home(home.clone());
        let dot = home.join(".xearthlayer");
        assert_eq!(d.config_dir(), dot);
        assert_eq!(d.cache_dir(), dot);
        assert_eq!(d.data_dir(), dot);
        assert_eq!(d.state_dir(), dot);
    }

    #[test]
    fn every_role_resolves_under_the_layout_it_belongs_to() {
        let dir = tempfile::tempdir().unwrap();
        let base = testing::TestDirectories::rooted_at(dir.path());

        assert_eq!(config_file_in(&base), base.config_dir().join("config.ini"));
        assert_eq!(
            scenery_index_cache_in(&base),
            base.cache_dir().join("scenery_index.cache")
        );
        assert_eq!(
            ortho_union_index_cache_in(&base),
            base.cache_dir().join("ortho_union_index.cache")
        );
        assert_eq!(
            version_check_file_in(&base),
            base.state_dir().join("version_check.json")
        );
        assert_eq!(log_file_in(&base), base.state_dir().join("xearthlayer.log"));
        assert_eq!(
            lock_file_in(&base),
            base.state_dir().join("xearthlayer.lock")
        );
        assert_eq!(packages_dir_in(&base), base.data_dir().join("packages"));
        assert_eq!(patches_dir_in(&base), base.data_dir().join("patches"));
        assert_eq!(temp_dir_in(&base), base.cache_dir().join("tmp"));
    }

    #[test]
    fn regenerable_files_live_in_the_cache_and_state_never_in_config() {
        // The point of separating the roles. A cleaner may delete the cache
        // directory at any time, so nothing irreplaceable may resolve into it,
        // and config.ini must not sit where one would look.
        let dir = tempfile::tempdir().unwrap();
        let base = testing::TestDirectories::rooted_at(dir.path());
        for regenerable in [
            scenery_index_cache_in(&base),
            ortho_union_index_cache_in(&base),
            temp_dir_in(&base),
        ] {
            assert!(regenerable.starts_with(base.cache_dir()), "{regenerable:?}");
        }
        assert!(!config_file_in(&base).starts_with(base.cache_dir()));
    }

    #[test]
    fn the_active_layout_is_platform_native() {
        // The inverse of the assertion this replaced. Nothing resolves into
        // ~/.xearthlayer any more.
        let home = dirs::home_dir().unwrap();
        let expected_config = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/XEarthLayer/config.ini")
        } else {
            home.join(".config/xearthlayer/config.ini")
        };
        assert_eq!(config_file(), expected_config);
        assert!(
            !config_file().starts_with(home.join(".xearthlayer")),
            "nothing may still resolve into the legacy directory"
        );
    }

    #[test]
    fn every_role_leaves_the_legacy_directory() {
        let legacy = dirs::home_dir().unwrap().join(".xearthlayer");
        for path in [
            config_file(),
            scenery_index_cache(),
            ortho_union_index_cache(),
            version_check_file(),
            log_file(),
            lock_file(),
            packages_dir(),
            patches_dir(),
            temp_dir(),
            tile_cache_dir(),
        ] {
            assert!(!path.starts_with(&legacy), "still legacy: {path:?}");
        }
    }

    #[test]
    fn the_tile_cache_now_reads_through_the_active_layout() {
        // On Linux this is the same directory dirs::cache_dir() gave, so no
        // existing cache moves. On macOS it gains the capitalised application
        // name, which only affects a fresh install: an install with a config
        // file keeps whatever cache.directory it already stored.
        assert_eq!(tile_cache_dir(), active().cache_dir());
        if cfg!(not(target_os = "macos")) {
            assert_eq!(
                tile_cache_dir(),
                dirs::cache_dir().unwrap().join("xearthlayer"),
                "the Linux tile cache must not move"
            );
        }
    }

    #[test]
    fn a_trait_object_can_hold_any_implementation() {
        // Nothing may depend on the concrete type: the whole point is that the
        // active implementation is chosen once and read through the trait.
        let all: Vec<Box<dyn BaseDirectories>> = vec![
            Box::new(XdgDirectories::from_parts(
                PathBuf::from("/home/pilot"),
                None,
                None,
                None,
                None,
            )),
            Box::new(AppleDirectories::from_home(PathBuf::from("/Users/pilot"))),
            Box::new(LegacyDirectories::from_home(PathBuf::from("/home/pilot"))),
        ];
        for d in all {
            assert!(d.config_dir().is_absolute());
        }
    }
}
