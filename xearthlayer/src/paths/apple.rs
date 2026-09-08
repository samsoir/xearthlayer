//! Apple platform layout, used on macOS.

use super::BaseDirectories;
use std::path::PathBuf;

/// Directory name under each Apple base. Capitalised, per macOS convention.
const APP_DIR: &str = "XEarthLayer";

/// Paths per Apple's file system layout.
///
/// Two properties differ from XDG and both are load-bearing:
///
/// - **Configuration and data collapse.** `dirs::config_dir()` and
///   `dirs::data_dir()` both return `~/Library/Application Support`, so the two
///   roles resolve to one directory. Nothing may assume they differ.
/// - **State has no `dirs` answer.** `dirs::state_dir()` is Linux only and
///   returns `None` here, so the target is chosen deliberately:
///   `~/Library/Logs`, which is where macOS expects application logs and is
///   what Console.app shows.
pub struct AppleDirectories {
    home: PathBuf,
}

impl AppleDirectories {
    pub fn from_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn library(&self) -> PathBuf {
        self.home.join("Library")
    }
}

impl BaseDirectories for AppleDirectories {
    fn config_dir(&self) -> PathBuf {
        self.library().join("Application Support").join(APP_DIR)
    }

    /// The same directory as [`config_dir`](Self::config_dir). See the type
    /// documentation.
    fn data_dir(&self) -> PathBuf {
        self.config_dir()
    }

    fn cache_dir(&self) -> PathBuf {
        self.library().join("Caches").join(APP_DIR)
    }

    fn state_dir(&self) -> PathBuf {
        self.library().join("Logs").join(APP_DIR)
    }
}
