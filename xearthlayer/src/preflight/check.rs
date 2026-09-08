//! The [`Preflight`] trait.

use super::{PreflightError, Remedy, Status};
use std::borrow::Cow;

/// A single prerequisite, generic over the context it operates on.
///
/// A check declares nothing about other checks and never calls one. It accepts
/// a context, reads what it needs, and contributes what it owns. Check families
/// that operate on different contexts get their own registries, so the context
/// type is a parameter rather than one shared object every check must fit.
pub trait Preflight<C>: Send + Sync {
    /// Stable identifier, used in reports and error mapping.
    ///
    /// `Cow` rather than `&'static str` so a check constructed at runtime,
    /// including one supplied by a future plugin, can name itself.
    fn name(&self) -> Cow<'static, str>;

    /// Whether this check applies at all. Defaults to always.
    ///
    /// This is how a conditional check stays declarative: an airport
    /// validation applies only when an airport was requested, rather than the
    /// caller wrapping the check in an `if`.
    fn applies(&self, _ctx: &C) -> bool {
        true
    }

    /// Inspect the prerequisite.
    ///
    /// **Must have no side effects.** Report mode calls this and never
    /// [`Preflight::remediate`], so a violation mutates a user's installation
    /// when they only asked for a diagnostic.
    fn inspect(&self, ctx: &C) -> Status;

    /// Fix the prerequisite and contribute to the context.
    ///
    /// Called only in enforce mode, and only when [`Preflight::inspect`]
    /// returned a remediable [`Status::Unsatisfied`]. Defaults to doing
    /// nothing, which suits checks that only validate.
    fn remediate(&self, _ctx: &mut C) -> Result<Remedy<C>, PreflightError> {
        Ok(Remedy::default())
    }
}
