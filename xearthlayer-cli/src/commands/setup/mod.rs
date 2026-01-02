//! Setup wizard command for first-time configuration.
//!
//! Provides an interactive TUI-based wizard that guides users through
//! initial XEarthLayer configuration. Detects system hardware and
//! X-Plane installation to recommend optimal settings.
//!
//! # Architecture
//!
//! The setup wizard is split between two layers:
//!
//! - **Core library** (`xearthlayer::system`): Hardware detection and recommendation logic
//! - **CLI** (`wizard.rs`): TUI presentation using `dialoguer`
//!
//! This separation allows the same logic to be reused by:
//! - This CLI wizard
//! - A future GTK4 desktop application
//! - Any other frontend

mod wizard;

use crate::error::CliError;

/// Run the setup wizard.
pub fn run() -> Result<(), CliError> {
    wizard::run_wizard()
}
