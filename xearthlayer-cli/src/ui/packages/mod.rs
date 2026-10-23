//! The package manager TUI opened by `xearthlayer packages`.
//!
//! - `app`: the state and the pure `update`, which turns keys and messages
//!   into effects for the runner to carry out
//! - `input`: keys to actions, by tab and open modal

pub mod app;
pub mod input;
