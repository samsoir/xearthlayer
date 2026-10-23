//! The package manager's widgets: the frame around the tabs, one list and
//! details pair per tab, and the dialogs.
//!
//! - `chrome`: title bar, run banner, status line, footer, and the helpers
//!   every list shares
//! - `detail`: the details pane
//! - `local`, `library`, `progress`: the three tabs
//! - `help`: the help and confirmation dialogs

pub mod chrome;
pub mod detail;
pub mod help;
pub mod library;
pub mod local;
pub mod progress;
