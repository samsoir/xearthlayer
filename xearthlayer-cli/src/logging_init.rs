//! CLI-level logging initialization.
//!
//! Which log a command writes is decided here, per command (#320):
//!
//! - `run` writes the flight log, `logging.file`, truncated when it starts.
//!   It is the only command that may: a flight's log is the diagnostic we ask
//!   for, and a package command opened during a flight used to erase it.
//! - The `packages` commands and the package manager append to their own log,
//!   [`xearthlayer::paths::packages_log_file`], so a failed install still
//!   leaves diagnostics (#194) and the next package command keeps them.
//! - Every other command writes no log. Its output is the terminal.

use std::path::{Path, PathBuf};

use crate::error::CliError;
use xearthlayer::config::ConfigFile;
use xearthlayer::logging::{init_logging_full, LogFileMode, LoggingGuard};
use xearthlayer::paths;

/// Default log filename used when the configured path has no file component.
const DEFAULT_LOG_FILE: &str = "xearthlayer.log";

/// Default parent directory used when the configured path has no parent.
const DEFAULT_LOG_DIR: &str = ".";

/// The log a command writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTarget {
    /// The flight log, `logging.file`, truncated: `run` only.
    Flight,
    /// The package commands' own log, appended.
    Packages,
    /// No log file: the terminal is the output.
    None,
}

/// The file a target writes and how it opens it, from the configuration.
pub(crate) fn log_file_for(
    target: LogTarget,
    config: &ConfigFile,
) -> Option<(PathBuf, LogFileMode)> {
    match target {
        LogTarget::Flight => Some((config.logging.file.clone(), LogFileMode::Truncate)),
        LogTarget::Packages => Some((paths::packages_log_file(), LogFileMode::Append)),
        LogTarget::None => None,
    }
}

/// Split a log path into the (`directory`, `filename`) tuple expected by
/// [`init_logging_full`].
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

/// Initialize the global tracing subscriber for a CLI subcommand, or install
/// none when its target is [`LogTarget::None`].
///
/// Suppresses stdout logging when stdout is a TTY (so a TUI isn't corrupted),
/// and returns a guard that must be kept alive for the duration of the
/// process.
///
/// # Errors
///
/// Returns [`CliError::LoggingInit`] if the log directory cannot be created
/// or the log file cannot be opened.
pub fn init_cli_logging(
    target: LogTarget,
    config: &ConfigFile,
    debug_mode: bool,
    profile_mode: bool,
) -> Result<Option<LoggingGuard>, CliError> {
    let Some((path, mode)) = log_file_for(target, config) else {
        return Ok(None);
    };
    let (log_dir, log_file) = derive_log_paths(&path);
    let stdout_enabled = !atty::is(atty::Stream::Stdout);

    let guard = init_logging_full(
        &log_dir,
        &log_file,
        mode,
        stdout_enabled,
        debug_mode,
        profile_mode,
    )
    .map_err(|e| CliError::LoggingInit(e.to_string()))?;

    if mode == LogFileMode::Append {
        // Sessions follow one another in the file; mark where this one starts.
        let command: Vec<String> = std::env::args().skip(1).collect();
        tracing::info!(
            version = xearthlayer::VERSION,
            command = %command.join(" "),
            "Session started"
        );
    }
    Ok(Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_flight_log_is_the_configured_file_truncated() {
        let mut config = ConfigFile::default();
        config.logging.file = PathBuf::from("/flights/xearthlayer.log");

        assert_eq!(
            log_file_for(LogTarget::Flight, &config),
            Some((
                PathBuf::from("/flights/xearthlayer.log"),
                LogFileMode::Truncate
            ))
        );
    }

    #[test]
    fn the_packages_log_is_its_own_file_appended_whatever_logging_file_says() {
        let mut config = ConfigFile::default();
        config.logging.file = PathBuf::from("/flights/xearthlayer.log");

        let (path, mode) = log_file_for(LogTarget::Packages, &config).unwrap();

        assert_eq!(path, paths::packages_log_file());
        assert_ne!(path, config.logging.file, "never the flight log (#320)");
        assert_eq!(mode, LogFileMode::Append);
    }

    #[test]
    fn no_target_means_no_file() {
        assert_eq!(log_file_for(LogTarget::None, &ConfigFile::default()), None);
    }

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
