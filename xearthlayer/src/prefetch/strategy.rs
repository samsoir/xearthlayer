//! Prefetcher strategy trait.
//!
//! This module defines the `Prefetcher` trait that abstracts different
//! prefetching strategies, enabling dependency injection and swappable
//! implementations.
//!
//! # Available Strategies
//!
//! - [`RadialPrefetcher`](super::RadialPrefetcher): Simple cache-aware radial
//!   expansion around current position (recommended)
//! - [`PrefetchScheduler`](super::PrefetchScheduler): Complex flight-path
//!   prediction with cone and radial calculations
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::{Prefetcher, RadialPrefetcher, RadialPrefetchConfig};
//!
//! // Create a prefetcher strategy
//! let strategy: Box<dyn Prefetcher> = Box::new(
//!     RadialPrefetcher::new(memory_cache, dds_handler, config)
//! );
//!
//! // Run the prefetcher
//! strategy.run(state_rx, cancellation_token).await;
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
/// // Create a prefetcher strategy
/// let prefetcher: Box<dyn Prefetcher> = Box::new(
///     RadialPrefetcher::new(memory_cache, dds_handler, config)
///         .with_shared_status(shared_status)
/// );
///
/// // Run the prefetcher (consumes the boxed prefetcher)
/// prefetcher.run(state_rx, cancellation_token).await;
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
    /// - RadialPrefetcher: "radial, 3-tile radius, zoom 14"
    /// - PrefetchScheduler: "flight-path, 45° cone, 10nm distance"
    fn startup_info(&self) -> String;
}
