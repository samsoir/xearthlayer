//! Service orchestrator for XEarthLayer backend.
//!
//! This module provides `ServiceOrchestrator` which coordinates the startup,
//! operation, and shutdown of all XEarthLayer backend services.
//!
//! # Architecture
//!
//! The orchestrator owns and manages:
//! - **Aircraft Position & Telemetry (APT)** - unified position aggregation
//! - **Prefetch system** - predictive tile caching
//! - **Scene tracker** - FUSE load monitoring
//! - **FUSE mounts** (via `MountManager`) - virtual filesystem
//!
//! Cache services are now managed internally by `XEarthLayerService` via `CacheLayer`,
//! which is created during `XEarthLayerService::start()`. This ensures proper metrics
//! integration: MetricsSystem is created first, then CacheLayer with metrics client,
//! so the GC daemon can report cache evictions.
//!
//! # Startup Sequence
//!
//! 1. ServiceBuilder is created (deferred service creation)
//! 2. FUSE mount triggers `XEarthLayerService::start()`:
//!    - MetricsSystem created first
//!    - CacheLayer created with metrics (GC daemon starts)
//!    - Service fully initialized
//! 3. APT module starts telemetry reception
//! 4. Prefetch system subscribes to APT telemetry
//! 5. FUSE mounts become active
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{ServiceOrchestrator, OrchestratorConfig};
//!
//! let config = OrchestratorConfig::from_config_file(...);
//! let orchestrator = ServiceOrchestrator::start(config)?;
//!
//! // Access telemetry for UI
//! let snapshot = orchestrator.telemetry_snapshot();
//!
//! // Graceful shutdown
//! orchestrator.shutdown();
//! ```

mod apt;
mod core;
mod prefetch;
mod types;

pub use self::core::ServiceOrchestrator;
pub use self::types::{MountResult, PrefetchHandle, StartupProgress, StartupResult};
