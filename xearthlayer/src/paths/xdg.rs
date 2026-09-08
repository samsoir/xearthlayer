//! XDG Base Directory layout, used on Linux.

use super::BaseDirectories;
use std::path::PathBuf;

/// Directory name under each XDG base. Lowercase, per XDG practice.
const APP_DIR: &str = "xearthlayer";

/// Paths per the XDG Base Directory Specification.
///
/// Honours `$XDG_CONFIG_HOME`, `$XDG_CACHE_HOME`, `$XDG_DATA_HOME` and
/// `$XDG_STATE_HOME`, falling back to the specification's defaults.
pub struct XdgDirectories {
    config: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    state: PathBuf,
}

impl XdgDirectories {
    /// Build from explicit parts.
    ///
    /// The four `Option`s are the `$XDG_*_HOME` values. Taking them as
    /// arguments rather than reading the environment is what lets a test assert
    /// both the set and unset cases without mutating process state, which is
    /// shared between concurrently running tests.
    pub fn from_parts(
        home: PathBuf,
        config: Option<PathBuf>,
        cache: Option<PathBuf>,
        data: Option<PathBuf>,
        state: Option<PathBuf>,
    ) -> Self {
        Self {
            config: config.unwrap_or_else(|| home.join(".config")).join(APP_DIR),
            cache: cache.unwrap_or_else(|| home.join(".cache")).join(APP_DIR),
            data: data
                .unwrap_or_else(|| home.join(".local").join("share"))
                .join(APP_DIR),
            state: state
                .unwrap_or_else(|| home.join(".local").join("state"))
                .join(APP_DIR),
        }
    }

    /// Build from the running process's environment.
    ///
    /// An `$XDG_*` value is honoured only when it is an absolute path, per the
    /// specification, which says a relative value must be ignored.
    pub fn detect(home: PathBuf) -> Self {
        let read = |var: &str| honour(std::env::var_os(var));
        Self::from_parts(
            home,
            read("XDG_CONFIG_HOME"),
            read("XDG_CACHE_HOME"),
            read("XDG_DATA_HOME"),
            read("XDG_STATE_HOME"),
        )
    }
}

/// Should this `$XDG_*_HOME` value be used?
///
/// The specification says a value that is not an absolute path must be treated
/// as unset. Separated from [`XdgDirectories::detect`] so the rule can be
/// asserted without mutating the environment, which is process-global and
/// therefore shared with every concurrently running test.
fn honour(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_value_is_honoured() {
        assert_eq!(
            honour(Some("/xdg/config".into())),
            Some(PathBuf::from("/xdg/config"))
        );
    }

    #[test]
    fn a_relative_value_is_ignored_per_the_specification() {
        assert_eq!(honour(Some("relative/config".into())), None);
    }

    #[test]
    fn an_empty_value_is_ignored() {
        // An empty string is not absolute, so it falls out of the same rule.
        // Worth pinning: an exported-but-empty variable is a common shell
        // accident and must not resolve paths to the application directory
        // alone.
        assert_eq!(honour(Some("".into())), None);
    }

    #[test]
    fn an_unset_value_is_ignored() {
        assert_eq!(honour(None), None);
    }
}

impl BaseDirectories for XdgDirectories {
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
