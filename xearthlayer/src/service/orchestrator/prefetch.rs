//! Prefetch system wiring for the service orchestrator.
//!
//! Contains `start_prefetch()` and `start_prefetch_with_cache()`, which compose
//! the adaptive prefetch coordinator from ~7 subsystems.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::info;

use crate::aircraft_position::AircraftPositionBroadcaster;
use crate::executor::{DdsClient, MemoryCache};
use crate::prefetch::{
    warn_if_legacy, AdaptivePrefetchConfig, AdaptivePrefetchCoordinator, CircuitBreaker, Prefetcher,
};

use super::super::error::ServiceError;
use super::{PrefetchHandle, ServiceOrchestrator};

impl ServiceOrchestrator {
    /// Start the prefetch system.
    ///
    /// This starts the prefetch daemon that predictively caches tiles
    /// based on aircraft position and heading.
    pub fn start_prefetch(&mut self) -> Result<(), ServiceError> {
        if !self.config.prefetch_enabled() {
            info!("Prefetch disabled by configuration");
            return Ok(());
        }

        let service = self
            .mount_manager
            .get_service()
            .ok_or_else(|| ServiceError::NotStarted("No service available for prefetch".into()))?;

        let dds_client = service
            .dds_client()
            .ok_or_else(|| ServiceError::NotStarted("DDS client not available".into()))?;

        let runtime_handle = service.runtime_handle().clone();
        let resource_pools = service.resource_pools();

        // Try legacy adapter first, then new cache bridge architecture
        if let Some(memory_cache) = service.memory_cache_adapter() {
            self.start_prefetch_with_cache(
                &runtime_handle,
                dds_client,
                memory_cache,
                resource_pools,
            )?;
        } else if let Some(memory_cache) = service.memory_cache_bridge() {
            self.start_prefetch_with_cache(
                &runtime_handle,
                dds_client,
                memory_cache,
                resource_pools,
            )?;
        } else {
            tracing::warn!("Memory cache not available, prefetch disabled");
            return Ok(());
        }

        Ok(())
    }

    /// Internal helper to start prefetch with a specific memory cache type.
    fn start_prefetch_with_cache<M: MemoryCache + 'static>(
        &mut self,
        runtime_handle: &Handle,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        resource_pools: Option<Arc<crate::executor::ResourcePools>>,
    ) -> Result<(), ServiceError> {
        use crate::prefetch::AircraftState as PrefetchAircraftState;

        // Create channel for prefetch telemetry data
        let (state_tx, state_rx) = mpsc::channel(32);

        // Bridge APT telemetry to prefetch channel
        let mut apt_rx = self.aircraft_position.subscribe();
        let bridge_cancel = self.cancellation.clone();
        runtime_handle.spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = bridge_cancel.cancelled() => {
                        tracing::debug!("APT-to-prefetch telemetry bridge cancelled");
                        break;
                    }

                    result = apt_rx.recv() => {
                        match result {
                            Ok(apt_state) => {
                                let prefetch_state = PrefetchAircraftState::new(
                                    apt_state.latitude,
                                    apt_state.longitude,
                                    apt_state.heading,
                                    apt_state.ground_speed,
                                    apt_state.altitude,
                                );
                                if state_tx.send(prefetch_state).await.is_err() {
                                    tracing::debug!("Prefetch channel closed");
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::debug!("APT broadcast channel closed");
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::trace!("APT-to-prefetch bridge lagged by {} messages", n);
                            }
                        }
                    }
                }
            }
        });

        // Build prefetcher
        let config = &self.config.prefetch;

        // Keep a reference to DDS client for adaptive strategy
        let dds_client_for_adaptive = Arc::clone(&dds_client);

        // Warn if a legacy strategy is configured (all now use adaptive)
        warn_if_legacy(&config.strategy);

        // Build and start the prefetcher
        let prefetcher_cancel = self.cancellation.clone();

        // Build adaptive prefetch coordinator
        let adaptive_config = AdaptivePrefetchConfig::from_prefetch_config(config);
        let mut coordinator = AdaptivePrefetchCoordinator::new(adaptive_config);

        // Wire DDS client
        coordinator = coordinator.with_dds_client(dds_client_for_adaptive);

        // Wire memory cache for tile existence checks (Bug 5 fix)
        coordinator = coordinator.with_memory_cache(memory_cache);

        // Wire circuit breaker as throttler (with optional resource pool monitoring)
        let mut circuit_breaker = CircuitBreaker::new(
            config.circuit_breaker.clone(),
            self.mount_manager.load_monitor(),
        );
        if let Some(ref pools) = resource_pools {
            circuit_breaker = circuit_breaker.with_resource_pools(Arc::clone(pools));
            tracing::info!("Resource pool utilization monitoring wired to circuit breaker");
        }
        coordinator = coordinator.with_throttler(Arc::new(circuit_breaker));

        // Wire scenery index if available
        if self.scenery_index.tile_count() > 0 {
            coordinator = coordinator.with_scenery_index(Arc::clone(&self.scenery_index));
        }

        // Wire ortho union index for disk-based tile filtering (Issue #39)
        // This prevents prefetch from downloading tiles that already exist on disk
        if let Some(ortho_index) = self.mount_manager.ortho_union_index() {
            coordinator = coordinator.with_ortho_union_index(ortho_index);
            tracing::info!(
                "Ortho union index wired to prefetch (disk-based tile filtering enabled)"
            );
        }

        // Wire GeoIndex for patched region filtering (Issue #51)
        if let Some(geo_index) = self.geo_index() {
            coordinator = coordinator.with_geo_index(geo_index);
            tracing::info!("GeoIndex wired to prefetch (patched region filtering enabled)");
        }

        // Wire shared status for TUI display
        coordinator = coordinator.with_shared_status(Arc::clone(&self.prefetch_status));

        let prefetcher: Box<dyn Prefetcher> = Box::new(coordinator);
        let handle = runtime_handle.spawn(async move {
            prefetcher.run(state_rx, prefetcher_cancel).await;
        });

        self.prefetch_handle = Some(PrefetchHandle { handle });
        info!(strategy = "adaptive", "Prefetch system started");

        Ok(())
    }
}
