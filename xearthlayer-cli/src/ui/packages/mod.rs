//! The package manager TUI opened by `xearthlayer packages`.
//!
//! - `app`: the state and the pure `update`, which turns keys and messages
//!   into effects for the runner to carry out
//! - `input`: keys to actions, by tab and open modal
//! - `view`: draws the state; `widgets` holds the pieces
//! - `effects`: carries out the effects: the session's install queue, and a
//!   worker thread for the slow reads
//! - `runner`: the terminal loop, signals and restoring the terminal

pub mod app;
pub mod effects;
pub mod input;
pub mod runner;
pub mod view;
pub mod widgets;
