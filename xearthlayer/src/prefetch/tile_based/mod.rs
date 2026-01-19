//! Tile-based prefetch strategy for X-Plane 12.
//!
//! This module implements a prefetch strategy that aligns with X-Plane's actual
//! loading behavior: whole 1° DSF tiles loaded in bursts, followed by quiet
//! periods of 1-2+ minutes.
//!
//! # Architecture
//!
//! ```text
//! FUSE Layer (Fuse3OrthoUnionFS)
//!   │
//!   │ DdsAccessEvent (via channel)
//!   ▼
//! TileBasedPrefetcher
//!   ├─ TileBurstTracker (tracks which DSF tiles are being loaded)
//!   ├─ TilePredictor (predicts next tiles based on heading)
//!   └─ CircuitBreaker (detects quiet periods - reused from existing)
//! ```
//!
//! # Key Insight
//!
//! X-Plane loads entire DSF tiles (1° × 1° grid), not individual textures.
//! The prefetcher exploits quiet periods between tile loads to prefetch
//! the next row of tiles based on movement direction.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::prefetch::tile_based::{TileBasedPrefetcher, TileBasedConfig};
//!
//! let prefetcher = TileBasedPrefetcher::new(
//!     ortho_index,
//!     dds_client,
//!     memory_cache,
//!     access_rx,
//!     circuit_breaker,
//!     TileBasedConfig::default(),
//! );
//!
//! // Run with telemetry for heading-aware prediction
//! prefetcher.run(state_rx, cancellation_token).await;
//! ```

mod burst_tracker;
mod dsf_coord;
mod predictor;
mod prefetcher;

pub use burst_tracker::TileBurstTracker;
pub use dsf_coord::DsfTileCoord;
pub use predictor::TilePredictor;
pub use prefetcher::{TileBasedConfig, TileBasedPrefetcher};

use std::time::Instant;

/// Event sent from FUSE when a DDS file is accessed.
///
/// The FUSE layer sends these events via a channel whenever X-Plane requests
/// a DDS texture. The prefetcher uses these events to track which DSF tiles
/// are actively being loaded.
///
/// # Design Notes
///
/// - Fire-and-forget: FUSE sends events without waiting for acknowledgment
/// - Bounded channel: Prevents backpressure on FUSE operations
/// - DSF-level granularity: Individual DDS accesses are aggregated to DSF tiles
#[derive(Debug, Clone)]
pub struct DdsAccessEvent {
    /// The 1° DSF tile containing the requested DDS texture.
    pub dsf_tile: DsfTileCoord,

    /// When the access occurred.
    ///
    /// Used for burst detection and quiet period calculation.
    pub timestamp: Instant,
}

impl DdsAccessEvent {
    /// Create a new DDS access event.
    pub fn new(dsf_tile: DsfTileCoord) -> Self {
        Self {
            dsf_tile,
            timestamp: Instant::now(),
        }
    }

    /// Create from raw coordinates.
    ///
    /// Convenience constructor that creates the DSF tile from lat/lon.
    pub fn from_coords(lat: f64, lon: f64) -> Self {
        Self::new(DsfTileCoord::from_lat_lon(lat, lon))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dds_access_event_creation() {
        let tile = DsfTileCoord::new(60, -146);
        let event = DdsAccessEvent::new(tile);

        assert_eq!(event.dsf_tile.lat, 60);
        assert_eq!(event.dsf_tile.lon, -146);
    }

    #[test]
    fn test_dds_access_event_from_coords() {
        let event = DdsAccessEvent::from_coords(60.5, -145.3);

        assert_eq!(event.dsf_tile.lat, 60);
        assert_eq!(event.dsf_tile.lon, -146);
    }
}
