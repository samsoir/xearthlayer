//! CLI-level logging initialization.
//!
//! Centralizes the wiring between [`xearthlayer::config::ConfigFile`] and
//! [`xearthlayer::logging::init_logging_full`] so that every subcommand —
//! not just the ones that go through [`crate::runner::CliRunner`] — gets a
//! populated `xearthlayer.log` for diagnostics. See issue #194.

use std::path::Path;

use crate::error::CliError;
use xearthlayer::config::ConfigFile;
use xearthlayer::logging::{init_logging_full, LoggingGuard};

/// Default log filename used when the configured path has no file component.
const DEFAULT_LOG_FILE: &str = "xearthlayer.log";

/// Default parent directory used when the configured path has no parent.
const DEFAULT_LOG_DIR: &str = ".";

/// Split a configured log path into the (`directory`, `filename`) tuple
/// expected by [`init_logging_full`].
///
/// Falls back to safe defaults rather than failing: a config that omits
/// either component should still produce a working logger.
pub(crate) fn derive_log_paths(log_path: &Path) -> (String, String) {
    let dir = log_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_LOG_DIR.to_string());

    let file = log_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_LOG_FILE.to_string());

    (dir, file)
}

/// Initialize the global tracing subscriber for any CLI subcommand.
///
/// Reads the log file path from `config.logging.file`, suppresses stdout
/// logging when stdout is a TTY (so the TUI dashboard isn't corrupted),
/// and returns a guard that must be kept alive for the duration of the
/// process.
///
/// # Errors
///
/// Returns [`CliError::LoggingInit`] if the log directory cannot be created
/// or the log file cannot be cleared.
pub fn init_cli_logging(
    config: &ConfigFile,
    debug_mode: bool,
    profile_mode: bool,
) -> Result<LoggingGuard, CliError> {
    let (log_dir, log_file) = derive_log_paths(&config.logging.file);
    let stdout_enabled = !atty::is(atty::Stream::Stdout);

    init_logging_full(
        &log_dir,
        &log_file,
        stdout_enabled,
        debug_mode,
        profile_mode,
    )
    .map_err(|e| CliError::LoggingInit(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn derive_log_paths_splits_normal_path() {
        let path = PathBuf::from("/home/user/.xearthlayer/xearthlayer.log");
        let (dir, file) = derive_log_paths(&path);
        assert_eq!(dir, "/home/user/.xearthlayer");
        assert_eq!(file, "xearthlayer.log");
    }

    #[test]
    fn derive_log_paths_handles_bare_filename() {
        // A relative filename with no directory component should land in
        // the current working directory rather than failing.
        let path = PathBuf::from("xearthlayer.log");
        let (dir, file) = derive_log_paths(&path);
        assert_eq!(dir, DEFAULT_LOG_DIR);
        assert_eq!(file, "xearthlayer.log");
    }

    #[test]
    fn derive_log_paths_handles_root_path() {
        // PathBuf::from("/") has no file_name; we should fall back to the
        // default log filename rather than producing nonsense.
        let path = PathBuf::from("/");
        let (dir, file) = derive_log_paths(&path);
        assert_eq!(file, DEFAULT_LOG_FILE);
        // Parent of "/" is None or empty; either way the default applies.
        assert!(dir == DEFAULT_LOG_DIR || dir == "/");
    }

    #[test]
    fn derive_log_paths_preserves_nested_directories() {
        let path = PathBuf::from("/a/b/c/d/e.log");
        let (dir, file) = derive_log_paths(&path);
        assert_eq!(dir, "/a/b/c/d");
        assert_eq!(file, "e.log");
    }
}
