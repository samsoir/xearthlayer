//! Outcome types for prerequisite checks.

use super::Preflight;
use std::borrow::Cow;
use std::fmt;

/// Outcome of inspecting a single prerequisite.
///
/// Produced by [`Preflight::inspect`], which is pure: report mode calls it and
/// nothing else, so a check that mutates during inspection would silently
/// change a user's installation when they ask for a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The prerequisite is met.
    Satisfied,
    /// The prerequisite is met, but something is worth telling the user.
    /// Execution continues.
    Warning(String),
    /// The prerequisite is not met.
    Unsatisfied {
        /// What is wrong, in the user's terms.
        ///
        /// When a check reads a value an earlier check contributes, this must
        /// name the missing value. That is what turns a misordered registry
        /// into a legible error instead of a silent `None` several steps later.
        reason: String,
        /// Whether [`Preflight::remediate`] can fix it.
        remediable: bool,
        /// Optional second line: what the user should do about it.
        hint: Option<String>,
    },
}

impl Status {
    /// An unmet prerequisite that the framework cannot fix.
    pub fn unsatisfied(reason: impl Into<String>) -> Self {
        Status::Unsatisfied {
            reason: reason.into(),
            remediable: false,
            hint: None,
        }
    }

    /// Mark an unmet prerequisite as fixable. No effect on other variants.
    pub fn remediable(self) -> Self {
        match self {
            Status::Unsatisfied { reason, hint, .. } => Status::Unsatisfied {
                reason,
                remediable: true,
                hint,
            },
            other => other,
        }
    }

    /// Attach guidance shown below the reason. No effect on other variants.
    pub fn with_hint(self, hint: impl Into<String>) -> Self {
        match self {
            Status::Unsatisfied {
                reason, remediable, ..
            } => Status::Unsatisfied {
                reason,
                remediable,
                hint: Some(hint.into()),
            },
            other => other,
        }
    }
}

/// What a successful remediation produced.
pub struct Remedy<C> {
    /// Reported to the user when present.
    pub message: Option<String>,
    /// Checks discovered by this one, appended at the tail of the queue.
    ///
    /// This is how a future plugin-discovery check contributes further checks
    /// without the registry needing to know plugins exist. Appending at the
    /// tail keeps execution a straight line: nothing is ever spliced in ahead
    /// of a check that has already run.
    pub register: Vec<Box<dyn Preflight<C>>>,
}

impl<C> Default for Remedy<C> {
    fn default() -> Self {
        Self {
            message: None,
            register: Vec::new(),
        }
    }
}

/// A remediation failed.
///
/// Deliberately not `CliError`: that type lives in the binary crate, and naming
/// it here would make [`Preflight`] unimplementable from anywhere else.
#[derive(Debug)]
pub struct PreflightError {
    /// Name of the check that failed to remediate.
    pub check: Cow<'static, str>,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for PreflightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.check, self.message)
    }
}

impl std::error::Error for PreflightError {}

/// Result of running a whole registry.
#[derive(Debug)]
pub enum RunOutcome {
    /// Every applicable check passed.
    AllSatisfied,
    /// A check was unmet and could not be remediated. Execution stopped there.
    Failed {
        check: Cow<'static, str>,
        reason: String,
        hint: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsatisfied_carries_reason_and_remediability() {
        let s = Status::unsatisfied("configuration has not been loaded");
        match s {
            Status::Unsatisfied {
                reason,
                remediable,
                hint,
            } => {
                assert_eq!(reason, "configuration has not been loaded");
                assert!(!remediable, "unsatisfied defaults to not remediable");
                assert!(hint.is_none());
            }
            other => panic!("expected Unsatisfied, got {:?}", other),
        }
    }

    #[test]
    fn remediable_unsatisfied_opts_in_explicitly() {
        let s = Status::unsatisfied("file descriptor limit is below the hard maximum").remediable();
        assert!(matches!(
            s,
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
    }

    #[test]
    fn with_hint_attaches_a_second_line() {
        let s = Status::unsatisfied("macFUSE is not installed").with_hint("See docs/macos.md");
        match s {
            Status::Unsatisfied { hint, .. } => {
                assert_eq!(hint.as_deref(), Some("See docs/macos.md"))
            }
            other => panic!("expected Unsatisfied, got {:?}", other),
        }
    }

    #[test]
    fn hint_and_remediable_do_not_apply_to_satisfied() {
        assert_eq!(Status::Satisfied.remediable(), Status::Satisfied);
        assert_eq!(Status::Satisfied.with_hint("ignored"), Status::Satisfied);
    }

    #[test]
    fn remedy_defaults_to_no_message_and_no_registrations() {
        let r: Remedy<()> = Remedy::default();
        assert!(r.message.is_none());
        assert!(r.register.is_empty());
    }

    #[test]
    fn preflight_error_displays_check_and_message() {
        let e = PreflightError {
            check: "load-config".into(),
            message: "permission denied".to_string(),
        };
        assert_eq!(e.to_string(), "load-config: permission denied");
    }
}
