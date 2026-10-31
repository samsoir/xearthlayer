//! The base directory contract.

use std::path::PathBuf;

/// The four base directories XEarthLayer stores things in.
///
/// A trait with per-platform implementations rather than `cfg` branches at each
/// call site. macOS is not XDG with different constants: configuration and data
/// collapse into one directory there, and state has no `dirs` answer at all, so
/// the platforms differ in **shape** and not only in values. Anything that
/// assumed four distinct directories would be correct on Linux and wrong on
/// macOS, silently.
pub trait BaseDirectories: Send + Sync {
    /// Configuration the user edits. Never deleted by anything but the user.
    fn config_dir(&self) -> PathBuf;

    /// Regenerable data. Safe for a system cleaner to delete at any time, so
    /// nothing irreplaceable may live here.
    fn cache_dir(&self) -> PathBuf;

    /// Data the program owns that is not regenerable, such as installed
    /// scenery packages.
    fn data_dir(&self) -> PathBuf;

    /// Logs and run state. Regenerable, but not something a cleaner should
    /// remove while the program is running.
    fn state_dir(&self) -> PathBuf;
}
