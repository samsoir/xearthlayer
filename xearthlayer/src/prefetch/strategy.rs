//! Prefetcher strategy trait.
//!
//! This module defines the `Prefetcher` trait that abstracts different
//! prefetching strategies, enabling dependency injection and swappable
//! implementations.
//!
//! # Available Strategies
//!
//! - [`AdaptivePrefetchCoordinator`](super::AdaptivePrefetchCoordinator): Adaptive
//!   prefetch with flight phase detection (ground/cruise). This is the only
//!   available strategy as of v0.4.0.
//!
//! The adaptive prefetcher automatically selects the appropriate mode (aggressive,
//! opportunistic, or disabled) based on measured throughput, and uses track-based
//! band calculation for cruise and ring-based for ground operations.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::{Prefetcher, AdaptivePrefetchCoordinator};
//!
//! // The adaptive prefetcher is created by the service orchestrator
//! // and is not typically constructed directly.
//! let coordinator = AdaptivePrefetchCoordinator::new(config, services);
//!
//! // Run the coordinator
//! coordinator.run(state_rx, cancellation_token).await;
//! ```

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::state::AircraftState;

/// Trait for prefetching strategies.
///
/// Implementations receive aircraft state updates and decide which tiles
/// to prefetch based on their strategy (radial, flight-path prediction, etc.).
///
/// The trait uses a boxed future return type to allow trait objects,
/// enabling runtime strategy selection.
///
/// # Example
///
/// ```ignore
/// // Create a prefetcher strategy via the service orchestrator
/// let coordinator = AdaptivePrefetchCoordinator::new(config, services);
///
/// // Run the coordinator (consumes the boxed prefetcher)
/// let boxed: Box<dyn Prefetcher> = Box::new(coordinator);
/// boxed.run(state_rx, cancellation_token).await;
/// ```
pub trait Prefetcher: Send {
    /// Run the prefetcher, processing state updates until cancelled.
    ///
    /// # Arguments
    ///
    /// * `state_rx` - Channel receiving aircraft state updates from telemetry
    /// * `cancellation_token` - Token to signal shutdown
    ///
    /// # Returns
    ///
    /// A future that completes when the prefetcher is cancelled or the
    /// state channel is closed.
    fn run(
        self: Box<Self>,
        state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Get a human-readable name for this prefetcher strategy.
    fn name(&self) -> &'static str;

    /// Get a description of this prefetcher strategy.
    fn description(&self) -> &'static str;

    /// Get a startup info string describing the prefetcher configuration.
    ///
    /// This is displayed during initialization to inform the user about
    /// the prefetcher settings. Each implementation should provide relevant
    /// configuration details.
    ///
    /// # Example output
    ///
    /// - AdaptivePrefetchCoordinator: "adaptive (aggressive), cruise + ground"
    fn startup_info(&self) -> String;
}
