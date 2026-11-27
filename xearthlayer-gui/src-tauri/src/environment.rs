//! The paths every command resolves from the configuration.
//!
//! The same precedence rules the CLI applies in its preflight checks and
//! package commands (`xearthlayer-cli/src/preflight/paths.rs`,
//! `commands/packages/mod.rs`). They are repeated here until that logic moves
//! into the core library for both clients.

use std::path::PathBuf;

use xearthlayer::config::ConfigFile;

/// The configuration and the locations derived from it.
pub struct Environment {
    pub config: ConfigFile,
    /// Whether `config.ini` exists; when it does not, `config` is the defaults.
    pub config_exists: bool,
    pub install_location: PathBuf,
    pub custom_scenery: Option<PathBuf>,
    pub temp_dir: PathBuf,
}

impl Environment {
    /// Load the configuration, falling back to the defaults when it is missing.
    /// An unreadable configuration is an error: running on defaults would
    /// silently ignore what the user set.
    pub fn load() -> Result<Self, String> {
        let config_exists = xearthlayer::config::config_file_path().exists();
        let config = if config_exists {
            ConfigFile::load().map_err(|e| format!("Failed to read the configuration: {e}"))?
        } else {
            ConfigFile::default()
        };
        Ok(Self::from_config(config, config_exists))
    }

    pub fn from_config(config: ConfigFile, config_exists: bool) -> Self {
        let install_location = install_location(&config);
        let custom_scenery = custom_scenery(&config, xearthlayer::config::detect_custom_scenery);
        let temp_dir = config
            .packages
            .temp_dir
            .clone()
            .unwrap_or_else(xearthlayer::paths::temp_dir);
        Self {
            config,
            config_exists,
            install_location,
            custom_scenery,
            temp_dir,
        }
    }

    /// The Custom Scenery folder, or the error `run` gives without one.
    pub fn require_custom_scenery(&self) -> Result<PathBuf, String> {
        self.custom_scenery.clone().ok_or_else(|| {
            "No X-Plane Custom Scenery folder is configured and none was detected. \
             Choose one in Setup."
                .to_string()
        })
    }

    pub fn library_url(&self) -> Result<String, String> {
        self.config.packages.library_url.clone().ok_or_else(|| {
            "No package library URL is configured (packages.library_url).".to_string()
        })
    }
}

/// Where packages are installed: `packages.install_location`, else the
/// platform default.
pub fn install_location(config: &ConfigFile) -> PathBuf {
    config
        .packages
        .install_location
        .clone()
        .unwrap_or_else(xearthlayer::paths::packages_dir)
}

/// `packages.custom_scenery_path` > `xplane.scenery_dir` > auto-detect, the
/// precedence `xearthlayer run` uses.
pub fn custom_scenery<E>(
    config: &ConfigFile,
    detect: impl FnOnce() -> Result<PathBuf, E>,
) -> Option<PathBuf> {
    config
        .packages
        .custom_scenery_path
        .clone()
        .or_else(|| config.xplane.scenery_dir.clone())
        .or_else(|| detect().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> Result<PathBuf, ()> {
        panic!("detection must not run when a path is configured")
    }

    #[test]
    fn the_custom_scenery_path_wins_over_the_scenery_dir() {
        let mut config = ConfigFile::default();
        config.packages.custom_scenery_path = Some("/a".into());
        config.xplane.scenery_dir = Some("/b".into());
        assert_eq!(custom_scenery(&config, never), Some(PathBuf::from("/a")));
    }

    #[test]
    fn the_scenery_dir_is_used_when_no_custom_scenery_path_is_set() {
        let mut config = ConfigFile::default();
        config.packages.custom_scenery_path = None;
        config.xplane.scenery_dir = Some("/b".into());
        assert_eq!(custom_scenery(&config, never), Some(PathBuf::from("/b")));
    }

    #[test]
    fn detection_is_the_last_resort() {
        let mut config = ConfigFile::default();
        config.packages.custom_scenery_path = None;
        config.xplane.scenery_dir = None;
        assert_eq!(
            custom_scenery(&config, || Ok::<_, ()>("/detected".into())),
            Some(PathBuf::from("/detected"))
        );
        assert_eq!(custom_scenery(&config, || Err(())), None);
    }

    #[test]
    fn an_unset_install_location_is_the_platform_default() {
        let mut config = ConfigFile::default();
        config.packages.install_location = None;
        assert_eq!(
            install_location(&config),
            xearthlayer::paths::packages_dir()
        );
    }
}
