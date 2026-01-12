//! Pre-warm prefetcher for cold-start cache warming.
//!
//! This module provides a one-shot prefetcher that loads tiles around a
//! specific airport before the flight starts.
//!
//! # Example
//!
//! ```ignore
//! let prewarm = PrewarmPrefetcher::new(
//!     scenery_index,
//!     dds_client,
//!     memory_cache,
//!     PrewarmConfig { radius_nm: 100.0, ..Default::default() },
//! );
//!
//! // Load tiles around LFBO airport
//! let (progress_tx, mut progress_rx) = mpsc::channel(32);
//! let result = prewarm.run(43.6294, 1.3678, progress_tx, cancellation).await;
//! ```

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::executor::{DdsClient, MemoryCache};

use super::{SceneryIndex, SceneryTile};

/// Progress update from the prewarm prefetcher.
#[derive(Debug, Clone)]
pub enum PrewarmProgress {
    /// Starting prewarm with the given number of tiles.
    Starting { total_tiles: usize },
    /// A tile was submitted for prefetch (fire-and-forget).
    TileSubmitted,
    /// A tile was already cached (no request needed).
    TileCached,
    /// Prewarm completed.
    Complete {
        tiles_submitted: usize,
        cache_hits: usize,
    },
    /// Prewarm was cancelled.
    Cancelled { tiles_submitted: usize },
}

/// Configuration for the prewarm prefetcher.
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Radius in nautical miles around the airport to prewarm.
    pub radius_nm: f32,
    /// Maximum concurrent tile requests per batch.
    pub batch_size: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            radius_nm: 100.0,
            batch_size: 50,
        }
    }
}

/// Pre-warm prefetcher for loading tiles around an airport.
pub struct PrewarmPrefetcher<M: MemoryCache> {
    scenery_index: Arc<SceneryIndex>,
    dds_client: Arc<dyn DdsClient>,
    memory_cache: Arc<M>,
    config: PrewarmConfig,
}

impl<M: MemoryCache + Send + Sync + 'static> PrewarmPrefetcher<M> {
    /// Create a new prewarm prefetcher.
    pub fn new(
        scenery_index: Arc<SceneryIndex>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        config: PrewarmConfig,
    ) -> Self {
        Self {
            scenery_index,
            dds_client,
            memory_cache,
            config,
        }
    }

    /// Run the prewarm prefetcher.
    ///
    /// Submits prefetch requests for tiles within the configured radius.
    /// Uses fire-and-forget - tiles are submitted and will be cached when complete.
    /// Progress updates are sent through the channel.
    ///
    /// Returns the number of tiles submitted for prefetch.
    pub async fn run(
        &self,
        lat: f64,
        lon: f64,
        progress_tx: mpsc::Sender<PrewarmProgress>,
        cancellation: CancellationToken,
    ) -> usize {
        // Find all tiles within radius
        let tiles = self
            .scenery_index
            .tiles_near(lat, lon, self.config.radius_nm);

        if tiles.is_empty() {
            info!(
                lat = lat,
                lon = lon,
                radius_nm = self.config.radius_nm,
                "No tiles found in prewarm area"
            );
            let _ = progress_tx
                .send(PrewarmProgress::Complete {
                    tiles_submitted: 0,
                    cache_hits: 0,
                })
                .await;
            return 0;
        }

        // Deduplicate by tile coordinate (multiple .ter files may reference same tile)
        let unique_tiles = self.deduplicate_tiles(&tiles);

        info!(
            total = unique_tiles.len(),
            radius_nm = self.config.radius_nm,
            "Starting prewarm"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Starting {
                total_tiles: unique_tiles.len(),
            })
            .await;

        let mut tiles_submitted = 0usize;
        let mut cache_hits = 0usize;

        // Process tiles in batches
        for (idx, tile) in unique_tiles.iter().enumerate() {
            if cancellation.is_cancelled() {
                info!(tiles_submitted, "Prewarm cancelled");
                let _ = progress_tx.try_send(PrewarmProgress::Cancelled { tiles_submitted });
                return tiles_submitted;
            }

            let coord = tile.to_tile_coord();

            // Check if already cached
            if self
                .memory_cache
                .get(coord.row, coord.col, coord.zoom)
                .await
                .is_some()
            {
                cache_hits += 1;
                let _ = progress_tx.try_send(PrewarmProgress::TileCached);
                continue;
            }

            debug!(
                idx = idx,
                row = coord.row,
                col = coord.col,
                zoom = coord.zoom,
                "Prewarm submitting tile"
            );

            // Fire-and-forget prefetch request
            self.dds_client.prefetch(coord);
            tiles_submitted += 1;
            let _ = progress_tx.try_send(PrewarmProgress::TileSubmitted);

            // Small yield to avoid blocking other async tasks
            if idx % self.config.batch_size == 0 {
                tokio::task::yield_now().await;
            }
        }

        info!(
            tiles_submitted,
            cache_hits,
            total = unique_tiles.len(),
            "Prewarm complete"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Complete {
                tiles_submitted,
                cache_hits,
            })
            .await;

        tiles_submitted
    }

    /// Deduplicate tiles by their coordinates.
    fn deduplicate_tiles(&self, tiles: &[SceneryTile]) -> Vec<SceneryTile> {
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        let mut result = Vec::new();

        for tile in tiles {
            let coord = tile.to_tile_coord();
            let key = (coord.row, coord.col, coord.zoom);
            if seen.insert(key) {
                result.push(*tile);
            }
        }

        result
    }
}
