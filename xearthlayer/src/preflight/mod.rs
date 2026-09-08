//! Prerequisite checks run before XEarthLayer does any work.
//!
//! A [`Preflight`] check is one prerequisite: a thing that must be true, or a
//! value that must be resolved, before a command can proceed. Checks are held
//! in a [`Runner`] and executed in registration order.
//!
//! The trait is generic over its context. A check declares nothing about other
//! checks and never calls one; it accepts a context, reads what it needs, and
//! contributes what it owns. Check families that operate on different contexts
//! get their own registries.

mod check;
mod runner;
mod types;

pub use check::Preflight;
pub use runner::{Reporter, Runner};
pub use types::{PreflightError, Remedy, RunOutcome, Status};
