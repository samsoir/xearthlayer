//! Error reporting for the publishing binary.
//!
//! One variant, because every failure a publisher can hit is already a
//! message the handler composed for them. The type exists so that `main` has
//! a single place to print and to choose an exit code, and so the handlers
//! keep the `CliError::Publish(..)` spelling they had inside `xearthlayer`
//! before the separation (#284).

use std::fmt;

/// A failure the publishing binary reports and exits on.
#[derive(Debug)]
pub enum CliError {
    /// Publisher error, already phrased for the user.
    Publish(String),
}

impl CliError {
    /// Print the user-facing message and return the process exit code.
    ///
    /// The caller exits; this function never does, so destructors run.
    pub fn report(&self) -> u8 {
        eprintln!("Error: {}", self);
        eprintln!();
        eprintln!("Run 'xearthlayer-publish --help' for usage information.");
        1
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Publish(msg) => write!(f, "Publisher error: {}", msg),
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_publish_error_is_a_failure_exit() {
        let err = CliError::Publish("repository not found".to_string());

        assert_eq!(err.to_string(), "Publisher error: repository not found");
        assert_eq!(err.report(), 1);
    }
}
