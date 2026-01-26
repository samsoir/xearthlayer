//! Prewarm orchestrator for background cache warming.
//!
//! This module provides `PrewarmOrchestrator` which handles the startup
//! and lifecycle of the prewarm system that pre-caches tiles around airports.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::PrewarmOrchestrator;
//!
//! let result = PrewarmOrchestrator::start(
//!     orchestrator,
//!     "KSFO",
//!     xplane_env,
//!     aircraft_position,
//!     config,
//!     runtime_handle,
//! )?;
//!
//! // Query status in UI loop
//! let status = result.handle.status();
//! if status.is_complete {
//!     println!("Done: {}/{}", status.completed, status.total);
//! }
//!
//! // Cancel if needed
//! result.handle.cancel();
//! ```

use tokio::runtime::Handle;

use std::sync::Arc;

use crate::aircraft_position::SharedAircraftPosition;
use crate::airport::AirportIndex;
use crate::executor::{DdsClient, MemoryCache};
use crate::prefetch::{
    generate_dsf_grid, start_prewarm, DsfGridBounds, FileTerrainScanner, PrewarmHandle,
    TerrainScanner,
};
use crate::xplane::XPlaneEnvironment;

use super::orchestrator_config::PrewarmConfig as OrchestratorPrewarmConfig;
use super::ServiceOrchestrator;

/// Error returned when prewarm fails to start.
#[derive(Debug, Clone)]
pub struct PrewarmStartError {
    message: String,
}

impl PrewarmStartError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for PrewarmStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Prewarm start error: {}", self.message)
    }
}

impl std::error::Error for PrewarmStartError {}

/// Result of a successful prewarm start.
pub struct PrewarmStartResult {
    /// Handle to the running prewarm task.
    pub handle: PrewarmHandle,
    /// Name of the airport being prewarmed.
    pub airport_name: String,
    /// Number of tiles to prewarm (actual count after scanning).
    pub tile_count: usize,
}

/// Orchestrates background cache pre-warming around airports.
///
/// This struct provides a clean API for starting prewarm operations:
///
/// 1. Airport lookup from X-Plane's apt.dat database
/// 2. APT position seeding with airport coordinates
/// 3. DSF grid generation and terrain scanning
/// 4. Starting the prewarm context with authoritative job tracking
pub struct PrewarmOrchestrator;

impl PrewarmOrchestrator {
    /// Start prewarm for a given airport.
    ///
    /// Uses tile-based (DSF grid) enumeration to find all DDS textures within
    /// an N×N grid of 1°×1° tiles centered on the target airport.
    ///
    /// # Arguments
    ///
    /// * `orchestrator` - Service orchestrator with mounted services
    /// * `icao` - Airport ICAO code (e.g., "KSFO")
    /// * `xplane_env` - X-Plane environment for apt.dat lookup
    /// * `aircraft_position` - Aircraft position for APT seeding
    /// * `config` - Prewarm configuration (grid size, batch size)
    /// * `runtime_handle` - Tokio runtime handle for spawning
    ///
    /// # Returns
    ///
    /// Returns a `PrewarmStartResult` containing:
    /// - `handle` - Handle to query status and cancel the prewarm
    /// - `airport_name` - Name of the airport being prewarmed
    /// - `tile_count` - Actual number of tiles found to prewarm
    pub fn start(
        orchestrator: &mut ServiceOrchestrator,
        icao: &str,
        xplane_env: Option<&XPlaneEnvironment>,
        aircraft_position: &SharedAircraftPosition,
        config: &OrchestratorPrewarmConfig,
        runtime_handle: &Handle,
    ) -> Result<PrewarmStartResult, PrewarmStartError> {
        // Get X-Plane environment for apt.dat lookup
        let xplane_env = xplane_env
            .ok_or_else(|| PrewarmStartError::new("X-Plane installation not detected"))?;

        // Get apt.dat path
        let apt_dat_path = xplane_env.apt_dat_path().ok_or_else(|| {
            PrewarmStartError::new(format!(
                "Airport database not found at {}",
                xplane_env.earth_nav_data_path().display()
            ))
        })?;

        // Load airport index from apt.dat
        let airport_index = AirportIndex::from_apt_dat(&apt_dat_path).map_err(|e| {
            PrewarmStartError::new(format!("Failed to load airport database: {}", e))
        })?;

        // Look up the airport
        let airport = airport_index.get(icao).ok_or_else(|| {
            PrewarmStartError::new(format!("Airport '{}' not found in apt.dat", icao))
        })?;

        // Get OrthoUnionIndex from mount manager
        let ortho_index = orchestrator
            .ortho_union_index()
            .ok_or_else(|| PrewarmStartError::new("OrthoUnionIndex not available for prewarm"))?;

        // Seed APT with airport position (manual reference source)
        // This provides initial position for prefetch and dashboard before telemetry connects
        if aircraft_position.receive_manual_reference(airport.latitude, airport.longitude) {
            tracing::info!(
                icao = %icao,
                lat = airport.latitude,
                lon = airport.longitude,
                "APT seeded with airport position"
            );
        }

        // Generate DSF grid and scan for tiles
        let dsf_tiles = generate_dsf_grid(airport.latitude, airport.longitude, config.grid_size);
        let bounds = DsfGridBounds::from_tiles(&dsf_tiles);

        tracing::debug!(
            airport_lat = airport.latitude,
            airport_lon = airport.longitude,
            airport_name = %airport.name,
            grid_size = config.grid_size,
            dsf_tiles = dsf_tiles.len(),
            bounds = ?bounds,
            "Starting tile-based prewarm"
        );

        // Scan terrain files to find DDS tiles
        let scanner = FileTerrainScanner::new(Arc::clone(&ortho_index));
        let tiles = scanner.scan(&bounds);

        tracing::info!(
            icao = %icao,
            airport = %airport.name,
            tiles = tiles.len(),
            "Found tiles for prewarm"
        );

        if tiles.is_empty() {
            return Err(PrewarmStartError::new(format!(
                "No tiles found in prewarm area for {}",
                icao
            )));
        }

        // Get service for DDS client and memory cache
        let service = orchestrator
            .service()
            .ok_or_else(|| PrewarmStartError::new("No services available for prewarm"))?;

        let dds_client = service
            .dds_client()
            .ok_or_else(|| PrewarmStartError::new("DDS client not available for prewarm"))?;

        let tile_count = tiles.len();
        let airport_name = airport.name.clone();

        // Start prewarm with appropriate cache type
        let handle = if let Some(memory_cache) = service.memory_cache_adapter() {
            Self::start_prewarm_with_cache(icao, tiles, dds_client, memory_cache, runtime_handle)
        } else if let Some(memory_cache) = service.memory_cache_bridge() {
            Self::start_prewarm_with_cache(icao, tiles, dds_client, memory_cache, runtime_handle)
        } else {
            return Err(PrewarmStartError::new(
                "Memory cache not available for prewarm",
            ));
        };

        Ok(PrewarmStartResult {
            handle,
            airport_name,
            tile_count,
        })
    }

    /// Helper to start prewarm with a generic memory cache type.
    fn start_prewarm_with_cache<M: MemoryCache + Send + Sync + 'static>(
        icao: &str,
        tiles: Vec<crate::coord::TileCoord>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        runtime_handle: &Handle,
    ) -> PrewarmHandle {
        start_prewarm(
            icao.to_string(),
            tiles,
            dds_client,
            memory_cache,
            runtime_handle,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prewarm_start_error_display() {
        let error = PrewarmStartError::new("test error");
        assert!(error.to_string().contains("test error"));
    }
}
