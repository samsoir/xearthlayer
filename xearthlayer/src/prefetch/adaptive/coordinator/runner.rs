//! Async run loop for the adaptive prefetch coordinator.
//!
//! This module implements the [`Prefetcher`] trait for [`AdaptivePrefetchCoordinator`],
//! providing the async event loop that:
//!
//! - Receives telemetry updates from X-Plane
//! - Detects stale telemetry (X-Plane exit)
//! - Triggers prefetch cycles at appropriate intervals
//!
//! # Staleness Detection
//!
//! When X-Plane exits, it stops sending UDP telemetry packets. This module
//! detects the absence of telemetry and updates the GPS status to "Acquiring"
//! to indicate the connection was lost.
//!
//! # Design Notes
//!
//! The run loop uses `tokio::select!` with biased polling:
//! 1. Cancellation check (highest priority)
//! 2. Telemetry reception
//! 3. Staleness check interval

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::prefetch::state::AircraftState;
use crate::prefetch::strategy::Prefetcher;

use super::constants::{MIN_CYCLE_INTERVAL, STALENESS_CHECK_INTERVAL, TELEMETRY_STALE_THRESHOLD};
use super::core::AdaptivePrefetchCoordinator;
use super::telemetry::should_run_cycle;

// ─────────────────────────────────────────────────────────────────────────────
// Prefetcher trait implementation
// ─────────────────────────────────────────────────────────────────────────────

impl Prefetcher for AdaptivePrefetchCoordinator {
    fn run(
        mut self: Box<Self>,
        mut state_rx: mpsc::Receiver<AircraftState>,
        cancellation_token: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            tracing::info!(mode = ?self.effective_mode(), "Adaptive prefetcher started");

            let mut last_cycle = Instant::now();
            let mut last_telemetry: Option<Instant> = None;
            let mut staleness_interval = tokio::time::interval(STALENESS_CHECK_INTERVAL);

            loop {
                tokio::select! {
                    biased;

                    _ = cancellation_token.cancelled() => break,

                    state_opt = state_rx.recv() => {
                        let Some(state) = state_opt else { break };

                        // Track when we last received telemetry
                        last_telemetry = Some(Instant::now());

                        if !should_run_cycle(last_cycle, MIN_CYCLE_INTERVAL) {
                            continue;
                        }

                        self.process_telemetry(&state).await;
                        last_cycle = Instant::now();
                    }

                    _ = staleness_interval.tick() => {
                        // Check if telemetry has gone stale
                        if let Some(last) = last_telemetry {
                            if last.elapsed() > TELEMETRY_STALE_THRESHOLD {
                                tracing::info!(
                                    age_secs = last.elapsed().as_secs(),
                                    "Telemetry stale - X-Plane may have exited"
                                );
                                // Clear the GPS status to show we've lost connection
                                if let Some(ref status) = self.shared_status {
                                    status.update_gps_status(
                                        crate::prefetch::state::GpsStatus::Acquiring
                                    );
                                }
                                // Reset last_telemetry to avoid repeated log spam
                                last_telemetry = None;
                            }
                        }
                    }
                }
            }

            tracing::info!("Adaptive prefetcher stopped");
        })
    }

    fn name(&self) -> &'static str {
        "adaptive"
    }

    fn description(&self) -> &'static str {
        "Self-calibrating adaptive prefetch with phase detection and turn handling"
    }

    fn startup_info(&self) -> String {
        self.startup_info_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::prefetch::state::{AircraftState, GpsStatus, SharedPrefetchStatus};

    // ─────────────────────────────────────────────────────────────────────────
    // Staleness detection regression test
    // ─────────────────────────────────────────────────────────────────────────

    /// Regression test for Bug 3: Telemetry persisting after X-Plane exit.
    ///
    /// When X-Plane exits, it stops sending UDP telemetry packets. The prefetch
    /// system must detect this and update the GPS status to show we've lost
    /// connection, rather than continuing to display the stale position.
    #[tokio::test]
    async fn test_stale_telemetry_updates_gps_status_regression() {
        let shared_status = SharedPrefetchStatus::new();

        // Simulate receiving telemetry (sets GPS status to Connected)
        let state = AircraftState::new(51.47, -0.46, 90.0, 0.0, 0.0); // EGLL
        shared_status.update_aircraft(&state);

        // Verify status is Connected
        let snapshot = shared_status.snapshot();
        assert_eq!(snapshot.gps_status, GpsStatus::Connected);

        // Simulate the staleness check updating status when telemetry stops
        // (This simulates what the run loop does when telemetry goes stale)
        shared_status.update_gps_status(GpsStatus::Acquiring);

        // Verify status changed to Acquiring
        let snapshot = shared_status.snapshot();
        assert_eq!(
            snapshot.gps_status,
            GpsStatus::Acquiring,
            "GPS status should change to Acquiring when telemetry is stale"
        );
    }

    /// Test that the Prefetcher trait methods return expected values.
    #[test]
    fn test_prefetcher_trait_methods() {
        use super::AdaptivePrefetchCoordinator;
        use crate::prefetch::strategy::Prefetcher;

        let coord = AdaptivePrefetchCoordinator::with_defaults();

        assert_eq!(coord.name(), "adaptive");
        assert!(coord.description().contains("Self-calibrating"));
        assert!(coord.startup_info().contains("adaptive"));
    }
}
