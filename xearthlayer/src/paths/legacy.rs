//! The pre-0.5.0 layout: everything in `~/.xearthlayer`.

use super::BaseDirectories;
use std::path::PathBuf;

/// The single directory every role resolved to before 0.5.0.
const APP_DIR: &str = ".xearthlayer";

/// Every role in one dotted directory under the user's home.
///
/// Retained after the move to platform-native paths because the migration has
/// to name where things used to be, and because
/// [`crate::paths`] resolves against it during the conversion that precedes the
/// switch. It is also the degenerate case worth keeping tested: all four roles
/// returning the same directory is exactly what a caller assuming four distinct
/// directories would get wrong.
pub struct LegacyDirectories {
    root: PathBuf,
}

impl LegacyDirectories {
    pub fn from_home(home: PathBuf) -> Self {
        Self {
            root: home.join(APP_DIR),
        }
    }
}

impl BaseDirectories for LegacyDirectories {
    fn config_dir(&self) -> PathBuf {
        self.root.clone()
    }
    fn cache_dir(&self) -> PathBuf {
        self.root.clone()
    }
    fn data_dir(&self) -> PathBuf {
        self.root.clone()
    }
    fn state_dir(&self) -> PathBuf {
        self.root.clone()
    }
}
