//! CLI command implementations.
//!
//! Each subcommand has its own module with argument definitions and handlers.
//!
//! # Command Modules
//!
//! - [`cache`] - Cache management (clear, stats)
//! - [`config`] - Configuration management (get, set, list, path)
//! - [`diagnostics`] - System diagnostics for bug reports
//! - [`download`] - Single tile download
//! - [`init`] - Configuration initialization
//! - [`packages`] - Package management (install, remove, update)
//! - [`publish`] - Package publishing (for scenery creators)
//! - [`run`] - Main command (mount all packages)
//! - [`start`] - Mount single scenery pack

pub mod cache;
pub mod common;
pub mod config;
pub mod diagnostics;
pub mod download;
pub mod init;
pub mod packages;
pub mod publish;
pub mod run;
pub mod start;
