//! Logging abstraction layer.
//!
//! This module provides a logging interface that decouples application code from
//! specific logging implementations (like `tracing`). This follows the Dependency
//! Inversion Principle - high-level modules depend on abstractions, not concretions.
//!
//! # Architecture
//!
//! - `Logger` trait: The interface that all components use for logging
//! - `TracingLogger`: Production adapter that delegates to the `tracing` crate
//! - `NoOpLogger`: Silent logger for testing and benchmarking
//!
//! # Usage
//!
//! Components that need logging should accept an `Arc<dyn Logger>` and use
//! the provided macros:
//!
//! ```
//! use xearthlayer::log::{Logger, LogLevel, NoOpLogger};
//! use xearthlayer::{log_info, log_debug};
//! use std::sync::Arc;
//!
//! struct MyComponent {
//!     logger: Arc<dyn Logger>,
//! }
//!
//! impl MyComponent {
//!     fn new(logger: Arc<dyn Logger>) -> Self {
//!         Self { logger }
//!     }
//!
//!     fn do_work(&self) {
//!         log_info!(self.logger, "Starting work");
//!         // ... do work ...
//!         log_debug!(self.logger, "Work completed");
//!     }
//! }
//! ```
//!
//! # Benefits
//!
//! - **Testability**: Use `NoOpLogger` in tests to avoid log noise
//! - **Flexibility**: Swap logging backends without changing application code
//! - **Decoupling**: No direct dependency on `tracing` throughout the codebase

mod noop;
mod tracing_adapter;
mod r#trait;

pub use noop::NoOpLogger;
pub use r#trait::{LogLevel, Logger};
pub use tracing_adapter::TracingLogger;
