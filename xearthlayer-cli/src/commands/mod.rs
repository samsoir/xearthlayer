//! CLI command implementations.
//!
//! Each subcommand has its own module with argument definitions and handlers.
//!
//! # Command Modules
//!
//! - [`cache`] - Cache management (clear, stats)
//! - [`config`] - Configuration management (get, set, list, path)
//! - [`diagnostics`] - System diagnostics for bug reports
//! - [`init`] - Configuration initialization
//! - [`packages`] - Package management (install, remove, update)
//! - [`patches`] - Tile patches management (list, validate, path)
//! - [`publish`] - Package publishing (for scenery creators)
//! - [`run`] - Main command (mount all packages)
//! - [`scenery_index`] - SceneryIndex cache management (update, clear, status)
//! - [`setup`] - Interactive setup wizard

pub mod cache;
pub mod common;
pub mod config;
pub mod diagnostics;
pub mod init;
pub mod packages;
pub mod patches;
pub mod publish;
pub mod run;
pub mod scenery_index;
pub mod setup;
