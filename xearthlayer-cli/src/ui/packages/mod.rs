//! The package manager TUI opened by `xearthlayer packages`.
//!
//! - `app`: the state and the pure `update`, which turns keys and messages
//!   into effects for the runner to carry out
//! - `input`: keys to actions, by tab and open modal
//! - `view`: draws the state; `widgets` holds the pieces

pub mod app;
pub mod input;
#[allow(dead_code)] // wired by the runner in Task 5
pub mod view;
#[allow(dead_code)] // wired by the runner in Task 5
pub mod widgets;
