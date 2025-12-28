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
//!     dds_handler,
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

use crate::coord::TileCoord;
use crate::fuse::{DdsHandler, DdsRequest};
use crate::pipeline::JobId;
use crate::pipeline::MemoryCache;

use super::{SceneryIndex, SceneryTile};

/// Progress update from the prewarm prefetcher.
#[derive(Debug, Clone)]
pub enum PrewarmProgress {
    /// Starting prewarm with the given number of tiles.
    Starting { total_tiles: usize },
    /// A tile was loaded successfully.
    TileLoaded { cache_hit: bool },
    /// A tile failed to load.
    TileFailed,
    /// Prewarm completed.
    Complete {
        tiles_loaded: usize,
        cache_hits: usize,
        failures: usize,
    },
    /// Prewarm was cancelled.
    Cancelled { tiles_loaded: usize },
}

/// Configuration for the prewarm prefetcher.
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Radius in nautical miles around the airport to prewarm.
    pub radius_nm: f32,
    /// Maximum concurrent tile requests.
    pub max_concurrent: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            radius_nm: 100.0,
            max_concurrent: 8,
        }
    }
}

/// Pre-warm prefetcher for loading tiles around an airport.
pub struct PrewarmPrefetcher<M: MemoryCache> {
    scenery_index: Arc<SceneryIndex>,
    dds_handler: DdsHandler,
    memory_cache: Arc<M>,
    config: PrewarmConfig,
}

impl<M: MemoryCache + Send + Sync + 'static> PrewarmPrefetcher<M> {
    /// Create a new prewarm prefetcher.
    pub fn new(
        scenery_index: Arc<SceneryIndex>,
        dds_handler: DdsHandler,
        memory_cache: Arc<M>,
        config: PrewarmConfig,
    ) -> Self {
        Self {
            scenery_index,
            dds_handler,
            memory_cache,
            config,
        }
    }

    /// Run the prewarm prefetcher.
    ///
    /// Loads tiles within the configured radius of the given coordinates.
    /// Progress updates are sent through the channel.
    ///
    /// Returns the number of tiles successfully loaded.
    pub async fn run(
        &self,
        lat: f64,
        lon: f64,
        progress_tx: mpsc::Sender<PrewarmProgress>,
        cancellation: CancellationToken,
    ) -> usize {
        // Find all tiles within radius
        let tiles = self.scenery_index.tiles_near(lat, lon, self.config.radius_nm);

        if tiles.is_empty() {
            info!(
                lat = lat,
                lon = lon,
                radius_nm = self.config.radius_nm,
                "No tiles found in prewarm area"
            );
            let _ = progress_tx
                .send(PrewarmProgress::Complete {
                    tiles_loaded: 0,
                    cache_hits: 0,
                    failures: 0,
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

        let mut tiles_loaded = 0usize;
        let mut cache_hits = 0usize;
        let mut failures = 0usize;

        // Process tiles with limited concurrency
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.config.max_concurrent));

        // Create channels for results
        let (result_tx, mut result_rx) = mpsc::channel::<(TileCoord, Result<bool, ()>)>(32);

        let total_tiles = unique_tiles.len();

        // Spawn tasks for each tile
        for tile in unique_tiles {
            if cancellation.is_cancelled() {
                break;
            }

            let permit = match semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break,
            };

            let coord = tile.to_tile_coord();
            let result_sender = result_tx.clone();
            let dds_handler = Arc::clone(&self.dds_handler);
            let memory_cache = Arc::clone(&self.memory_cache);
            let cancel_token = cancellation.clone();

            tokio::spawn(async move {
                let _permit = permit;

                // Check if already cached
                if memory_cache
                    .get(coord.row, coord.col, coord.zoom)
                    .await
                    .is_some()
                {
                    let _ = result_sender.send((coord, Ok(true))).await;
                    return;
                }

                // Create the request
                let (tx, rx) = tokio::sync::oneshot::channel();
                let request = DdsRequest {
                    job_id: JobId::new(),
                    tile: coord.clone(),
                    result_tx: tx,
                    cancellation_token: cancel_token.clone(),
                    is_prefetch: true,
                };

                // Submit the request
                (dds_handler)(request);

                // Wait for result
                match rx.await {
                    Ok(response) => {
                        let _ = result_sender.send((coord, Ok(response.cache_hit))).await;
                    }
                    Err(_) => {
                        debug!(
                            row = coord.row,
                            col = coord.col,
                            zoom = coord.zoom,
                            "Prewarm tile request cancelled"
                        );
                        let _ = result_sender.send((coord, Err(()))).await;
                    }
                }
            });
        }

        // Drop original sender so receiver knows when all tasks are done
        drop(result_tx);

        // Collect results
        loop {
            if cancellation.is_cancelled() {
                info!(tiles_loaded = tiles_loaded, "Prewarm cancelled");
                let _ = progress_tx
                    .send(PrewarmProgress::Cancelled { tiles_loaded })
                    .await;
                return tiles_loaded;
            }

            tokio::select! {
                result = result_rx.recv() => {
                    match result {
                        Some((_coord, Ok(was_cached))) => {
                            tiles_loaded += 1;
                            if was_cached {
                                cache_hits += 1;
                            }
                            let _ = progress_tx.send(PrewarmProgress::TileLoaded {
                                cache_hit: was_cached,
                            }).await;
                        }
                        Some((_coord, Err(_))) => {
                            failures += 1;
                            let _ = progress_tx.send(PrewarmProgress::TileFailed).await;
                        }
                        None => {
                            // Channel closed, all tiles processed
                            break;
                        }
                    }
                }
                _ = cancellation.cancelled() => {
                    info!(tiles_loaded = tiles_loaded, "Prewarm cancelled");
                    let _ = progress_tx.send(PrewarmProgress::Cancelled { tiles_loaded }).await;
                    return tiles_loaded;
                }
            }
        }

        // Calculate any tiles that didn't get submitted due to cancellation
        let not_submitted = total_tiles.saturating_sub(tiles_loaded + failures);
        if not_submitted > 0 {
            failures += not_submitted;
        }

        info!(
            tiles_loaded = tiles_loaded,
            cache_hits = cache_hits,
            failures = failures,
            "Prewarm complete"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Complete {
                tiles_loaded,
                cache_hits,
                failures,
            })
            .await;

        tiles_loaded
    }

    /// Deduplicate tiles by their tile coordinate.
    ///
    /// Multiple .ter files may reference the same DDS texture.
    fn deduplicate_tiles(&self, tiles: &[SceneryTile]) -> Vec<SceneryTile> {
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        let mut result = Vec::with_capacity(tiles.len());

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
