//! Pre-warm prefetcher for cold-start cache warming.
//!
//! This module provides a one-shot prefetcher that loads tiles around a
//! specific airport before the flight starts, using terrain file scanning.
//!
//! # Architecture
//!
//! The prewarm system:
//! 1. Computes an N×N grid of DSF (1°×1°) tiles centered on the target airport
//! 2. Scans `terrain/` folders in each ortho source using rayon for parallelism
//! 3. Reads `LOAD_CENTER` from each `.ter` file to get lat/lon
//! 4. Filters tiles within the DSF grid bounds
//! 5. Submits tiles to the executor and tracks their completion explicitly
//! 6. Reports progress as tiles complete (not just when submitted)
//!
//! This approach is efficient because:
//! - We scan actual files instead of generating possible filenames
//! - Rayon parallelizes file reading across CPU cores
//! - LOAD_CENTER gives exact lat/lon for fast filtering
//! - FuturesUnordered tracks job completion for accurate progress
//!
//! # Example
//!
//! ```ignore
//! let prewarm = PrewarmPrefetcher::new(
//!     ortho_index,
//!     dds_client,
//!     memory_cache,
//!     PrewarmConfig { grid_size: 8, ..Default::default() },
//! );
//!
//! // Load tiles around LFBO airport
//! let (progress_tx, mut progress_rx) = mpsc::channel(32);
//! let result = prewarm.run(43.6294, 1.3678, progress_tx, cancellation).await;
//! ```

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use rayon::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::coord::TileCoord;
use crate::executor::{DdsClient, MemoryCache, Priority};
use crate::ortho_union::OrthoUnionIndex;
use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};

use super::tile_based::DsfTileCoord;

/// Progress update from the prewarm prefetcher.
#[derive(Debug, Clone)]
pub enum PrewarmProgress {
    /// Starting prewarm with the given number of tiles.
    Starting { total_tiles: usize },
    /// A tile generation completed successfully.
    TileCompleted,
    /// A tile was already cached (no request needed).
    TileCached,
    /// Batch progress update (more efficient than per-tile messages).
    BatchProgress {
        completed: usize,
        cached: usize,
        failed: usize,
    },
    /// Prewarm completed.
    Complete {
        tiles_completed: usize,
        cache_hits: usize,
        failed: usize,
    },
    /// Prewarm was cancelled.
    Cancelled {
        tiles_completed: usize,
        tiles_pending: usize,
    },
}

/// Configuration for the prewarm prefetcher.
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Grid size in DSF tiles (N = N×N grid).
    /// Default: 8 (8×8 = 64 DSF tiles, ~480nm × 480nm at mid-latitudes)
    pub grid_size: u32,
    /// Maximum concurrent tile requests per batch.
    pub batch_size: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            grid_size: 8,
            batch_size: 50,
        }
    }
}

/// Pre-warm prefetcher for loading tiles around an airport.
///
/// Scans terrain files to find all DDS textures within a grid of 1°×1°
/// DSF tiles centered on the target airport.
pub struct PrewarmPrefetcher<M: MemoryCache> {
    ortho_index: Arc<OrthoUnionIndex>,
    dds_client: Arc<dyn DdsClient>,
    memory_cache: Arc<M>,
    config: PrewarmConfig,
}

impl<M: MemoryCache + Send + Sync + 'static> PrewarmPrefetcher<M> {
    /// Create a new prewarm prefetcher.
    pub fn new(
        ortho_index: Arc<OrthoUnionIndex>,
        dds_client: Arc<dyn DdsClient>,
        memory_cache: Arc<M>,
        config: PrewarmConfig,
    ) -> Self {
        Self {
            ortho_index,
            dds_client,
            memory_cache,
            config,
        }
    }

    /// Run the prewarm prefetcher.
    ///
    /// Generates an N×N grid of DSF tiles centered on the airport coordinates,
    /// then scans terrain files to find DDS textures within those tiles.
    /// Submits tiles to the executor and tracks their completion explicitly.
    /// Progress updates are sent through the channel as tiles complete.
    ///
    /// Returns the number of tiles that completed successfully.
    pub async fn run(
        &self,
        lat: f64,
        lon: f64,
        progress_tx: mpsc::Sender<PrewarmProgress>,
        cancellation: CancellationToken,
    ) -> usize {
        // Generate DSF grid centered on airport
        let dsf_tiles = generate_dsf_grid(lat, lon, self.config.grid_size);

        // Compute lat/lon bounds from DSF grid
        let bounds = DsfGridBounds::from_tiles(&dsf_tiles);

        info!(
            lat = lat,
            lon = lon,
            grid_size = self.config.grid_size,
            dsf_tiles = dsf_tiles.len(),
            bounds = ?bounds,
            "Starting prewarm terrain scan"
        );

        // Check for cancellation before expensive operation
        if cancellation.is_cancelled() {
            let _ = progress_tx
                .send(PrewarmProgress::Cancelled {
                    tiles_completed: 0,
                    tiles_pending: 0,
                })
                .await;
            return 0;
        }

        // Scan terrain files in parallel to find tiles within bounds
        let unique_tiles = self.scan_terrain_files(&bounds);

        if unique_tiles.is_empty() {
            info!(
                lat = lat,
                lon = lon,
                grid_size = self.config.grid_size,
                "No DDS tiles found in prewarm area"
            );
            let _ = progress_tx
                .send(PrewarmProgress::Complete {
                    tiles_completed: 0,
                    cache_hits: 0,
                    failed: 0,
                })
                .await;
            return 0;
        }

        info!(
            total = unique_tiles.len(),
            grid_size = self.config.grid_size,
            "Found tiles for prewarm"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Starting {
                total_tiles: unique_tiles.len(),
            })
            .await;

        // Track tiles that need generation and cache hits
        let mut tiles_to_generate = Vec::new();
        let mut cache_hits = 0usize;

        // First pass: check cache and collect tiles needing generation
        for tile in unique_tiles.iter() {
            if self
                .memory_cache
                .get(tile.row, tile.col, tile.zoom)
                .await
                .is_some()
            {
                cache_hits += 1;
            } else {
                tiles_to_generate.push(*tile);
            }
        }

        info!(
            tiles_to_generate = tiles_to_generate.len(),
            cache_hits, "Cache check complete, starting tile generation"
        );

        let total_to_generate = tiles_to_generate.len();

        // Get the sender for async submission with backpressure
        let sender = match self.dds_client.sender() {
            Some(s) => s,
            None => {
                warn!("DdsClient does not support async submission, falling back to sync");
                // Fallback: submit all at once (may fail if channel is full)
                let pending_futures: FuturesUnordered<_> = tiles_to_generate
                    .iter()
                    .map(|tile| {
                        self.dds_client.request_dds_with_options(
                            *tile,
                            Priority::PREFETCH,
                            RequestOrigin::Prewarm,
                            cancellation.child_token(),
                        )
                    })
                    .collect();
                return self
                    .wait_for_completions(
                        pending_futures,
                        total_to_generate,
                        cache_hits,
                        progress_tx,
                        cancellation,
                    )
                    .await;
            }
        };

        // Submit tiles with backpressure - this queues jobs as fast as the executor can accept them
        let mut pending_futures = FuturesUnordered::new();
        let mut tiles_iter = tiles_to_generate.into_iter();
        let mut submitted = 0usize;

        // Initial batch: fill the queue up to a reasonable concurrent limit
        const MAX_CONCURRENT: usize = 500;

        // Submit initial batch
        for tile in tiles_iter.by_ref().take(MAX_CONCURRENT) {
            let (tx, rx) = oneshot::channel();
            let request = JobRequest {
                tile,
                priority: Priority::PREFETCH,
                cancellation: cancellation.child_token(),
                response_tx: Some(tx),
                origin: RequestOrigin::Prewarm,
            };

            // Use async send with backpressure
            if sender.send(request).await.is_err() {
                warn!("Executor channel closed during prewarm submission");
                break;
            }
            pending_futures.push(rx);
            submitted += 1;
        }

        info!(initial_submitted = submitted, "Initial batch submitted");

        // Process completions and submit more tiles
        let mut tiles_completed = 0usize;
        let mut tiles_failed = 0usize;
        let mut pending_completed = 0usize;
        let mut pending_failed = 0usize;
        const PROGRESS_BATCH_SIZE: usize = 50;

        loop {
            // Check for cancellation
            if cancellation.is_cancelled() {
                let remaining = pending_futures.len() + tiles_iter.len();
                info!(
                    tiles_completed,
                    tiles_remaining = remaining,
                    "Prewarm cancelled"
                );
                let _ = progress_tx
                    .send(PrewarmProgress::Cancelled {
                        tiles_completed,
                        tiles_pending: remaining,
                    })
                    .await;
                return tiles_completed;
            }

            tokio::select! {
                // Wait for a tile to complete
                Some(result) = pending_futures.next() => {
                    match result {
                        Ok(response) if response.is_success() => {
                            tiles_completed += 1;
                            pending_completed += 1;
                        }
                        _ => {
                            tiles_failed += 1;
                            pending_failed += 1;
                        }
                    }

                    // Send batched progress updates
                    if pending_completed + pending_failed >= PROGRESS_BATCH_SIZE {
                        let _ = progress_tx.try_send(PrewarmProgress::BatchProgress {
                            completed: pending_completed,
                            cached: 0,
                            failed: pending_failed,
                        });
                        pending_completed = 0;
                        pending_failed = 0;
                    }

                    // Submit another tile if available
                    if let Some(tile) = tiles_iter.next() {
                        let (tx, rx) = oneshot::channel();
                        let request = JobRequest {
                            tile,
                            priority: Priority::PREFETCH,
                            cancellation: cancellation.child_token(),
                            response_tx: Some(tx),
                            origin: RequestOrigin::Prewarm,
                        };
                        if sender.send(request).await.is_ok() {
                            pending_futures.push(rx);
                        }
                    }
                }
                else => {
                    // No more pending futures - we're done
                    break;
                }
            }
        }

        // Send any remaining progress
        if pending_completed > 0 || pending_failed > 0 {
            let _ = progress_tx.try_send(PrewarmProgress::BatchProgress {
                completed: pending_completed,
                cached: 0,
                failed: pending_failed,
            });
        }

        info!(
            tiles_completed,
            tiles_failed, cache_hits, total_to_generate, "Prewarm complete"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Complete {
                tiles_completed,
                cache_hits,
                failed: tiles_failed,
            })
            .await;

        tiles_completed
    }

    /// Wait for all pending futures to complete (fallback for clients without sender).
    async fn wait_for_completions(
        &self,
        mut pending_futures: FuturesUnordered<oneshot::Receiver<DdsResponse>>,
        total_to_generate: usize,
        cache_hits: usize,
        progress_tx: mpsc::Sender<PrewarmProgress>,
        cancellation: CancellationToken,
    ) -> usize {
        let mut tiles_completed = 0usize;
        let mut tiles_failed = 0usize;

        // Progress tracking
        let mut pending_completed = 0usize;
        let mut pending_failed = 0usize;
        const PROGRESS_BATCH_SIZE: usize = 50;

        // Wait for all tiles to complete
        while let Some(result) = pending_futures.next().await {
            // Check for cancellation
            if cancellation.is_cancelled() {
                let remaining = pending_futures.len();
                info!(
                    tiles_completed,
                    tiles_remaining = remaining,
                    "Prewarm cancelled"
                );
                let _ = progress_tx
                    .send(PrewarmProgress::Cancelled {
                        tiles_completed,
                        tiles_pending: remaining,
                    })
                    .await;
                return tiles_completed;
            }

            // Track completion
            match result {
                Ok(response) if response.is_success() => {
                    tiles_completed += 1;
                    pending_completed += 1;
                }
                _ => {
                    tiles_failed += 1;
                    pending_failed += 1;
                }
            }

            // Send batched progress updates
            if pending_completed + pending_failed >= PROGRESS_BATCH_SIZE {
                let _ = progress_tx.try_send(PrewarmProgress::BatchProgress {
                    completed: pending_completed,
                    cached: 0, // Cache hits already counted
                    failed: pending_failed,
                });
                pending_completed = 0;
                pending_failed = 0;
            }
        }

        // Send any remaining progress
        if pending_completed > 0 || pending_failed > 0 {
            let _ = progress_tx.try_send(PrewarmProgress::BatchProgress {
                completed: pending_completed,
                cached: 0,
                failed: pending_failed,
            });
        }

        info!(
            tiles_completed,
            tiles_failed, cache_hits, total_to_generate, "Prewarm complete"
        );

        let _ = progress_tx
            .send(PrewarmProgress::Complete {
                tiles_completed,
                cache_hits,
                failed: tiles_failed,
            })
            .await;

        tiles_completed
    }

    /// Scan terrain files in all sources to find tiles within the given bounds.
    ///
    /// Uses rayon for parallel file scanning across sources and files.
    fn scan_terrain_files(&self, bounds: &DsfGridBounds) -> Vec<TileCoord> {
        let sources = self.ortho_index.sources();

        if sources.is_empty() {
            warn!("No ortho sources available for prewarm");
            return Vec::new();
        }

        debug!(
            source_count = sources.len(),
            "Scanning terrain files in sources"
        );

        // Collect all terrain directories
        let terrain_dirs: Vec<_> = sources
            .iter()
            .map(|s| s.source_path.join("terrain"))
            .filter(|p| p.exists())
            .collect();

        if terrain_dirs.is_empty() {
            warn!("No terrain directories found in any source");
            return Vec::new();
        }

        debug!(
            terrain_dirs = terrain_dirs.len(),
            "Found terrain directories"
        );

        // Scan all terrain directories in parallel
        let tiles: Vec<TileCoord> = terrain_dirs
            .par_iter()
            .flat_map(|terrain_dir| scan_terrain_directory(terrain_dir, bounds))
            .collect();

        // Deduplicate tiles (same tile might exist in multiple sources)
        let mut seen = HashSet::new();
        let unique_tiles: Vec<TileCoord> = tiles
            .into_iter()
            .filter(|tile| {
                let key = (tile.row, tile.col, tile.zoom);
                seen.insert(key)
            })
            .collect();

        debug!(
            total_scanned = seen.len(),
            unique = unique_tiles.len(),
            "Terrain scan complete"
        );

        unique_tiles
    }
}

/// Bounds for a grid of DSF tiles.
#[derive(Debug, Clone, Copy)]
struct DsfGridBounds {
    min_lat: f64,
    max_lat: f64,
    min_lon: f64,
    max_lon: f64,
}

impl DsfGridBounds {
    /// Compute bounds from a list of DSF tiles.
    fn from_tiles(tiles: &[DsfTileCoord]) -> Self {
        if tiles.is_empty() {
            return Self {
                min_lat: 0.0,
                max_lat: 0.0,
                min_lon: 0.0,
                max_lon: 0.0,
            };
        }

        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap() as f64;
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap() as f64 + 1.0; // DSF tile is 1° wide
        let min_lon = tiles.iter().map(|t| t.lon).min().unwrap() as f64;
        let max_lon = tiles.iter().map(|t| t.lon).max().unwrap() as f64 + 1.0; // DSF tile is 1° wide

        Self {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        }
    }

    /// Check if a lat/lon point is within these bounds.
    fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat < self.max_lat && lon >= self.min_lon && lon < self.max_lon
    }
}

/// Scan a terrain directory for tiles within the given bounds.
///
/// Reads each `.ter` file's LOAD_CENTER to get lat/lon and filters by bounds.
fn scan_terrain_directory(terrain_dir: &Path, bounds: &DsfGridBounds) -> Vec<TileCoord> {
    let entries = match std::fs::read_dir(terrain_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                path = %terrain_dir.display(),
                error = %e,
                "Failed to read terrain directory"
            );
            return Vec::new();
        }
    };

    // Collect .ter files first
    let ter_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "ter")
                .unwrap_or(false)
        })
        .collect();

    // Process files in parallel
    ter_files
        .par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            let filename = path.file_name()?.to_string_lossy();

            // Read LOAD_CENTER from the file
            let (lat, lon) = read_load_center(&path)?;

            // Check if within bounds
            if !bounds.contains(lat, lon) {
                return None;
            }

            // Parse tile coordinates from filename
            parse_ter_filename(&filename)
        })
        .collect()
}

/// Read the LOAD_CENTER lat/lon from a .ter file.
///
/// The .ter file format has LOAD_CENTER on an early line:
/// ```text
/// A
/// 800
/// TERRAIN
///
/// LOAD_CENTER 43.48481 1.09863 7098 4096
/// ```
fn read_load_center(path: &Path) -> Option<(f64, f64)> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    // LOAD_CENTER is typically in the first 10 lines
    for line in reader.lines().take(10) {
        let line = line.ok()?;
        if line.starts_with("LOAD_CENTER") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let lat: f64 = parts[1].parse().ok()?;
                let lon: f64 = parts[2].parse().ok()?;
                return Some((lat, lon));
            }
        }
    }

    None
}

/// Parse a terrain filename to extract tile coordinates.
///
/// Filename format: `row_col_provider+zoom.ter` or `row_col_provider+zoom_sea.ter`
/// Examples: `23952_32960_BI16.ter`, `23952_32960_BI16_sea.ter`
///
/// **IMPORTANT**: The row/col in TER filenames are CHUNK coordinates (same as DDS filenames).
/// We must convert to TILE coordinates:
/// - tile_row = chunk_row / 16
/// - tile_col = chunk_col / 16
/// - tile_zoom = chunk_zoom - 4
fn parse_ter_filename(filename: &str) -> Option<TileCoord> {
    // Remove .ter extension
    let name = filename.strip_suffix(".ter")?;

    // Handle _sea suffix and other suffixes like _sea_overlay
    let name = name
        .strip_suffix("_sea_overlay")
        .or_else(|| name.strip_suffix("_sea"))
        .unwrap_or(name);

    // Split by underscore: row_col_type+zoom
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() != 3 {
        return None;
    }

    // Parse CHUNK row and column (these are 16× tile coordinates)
    let chunk_row: u32 = parts[0].parse().ok()?;
    let chunk_col: u32 = parts[1].parse().ok()?;

    // Parse CHUNK zoom from provider+zoom (e.g., "BI16" -> 16, "GO218" -> 18)
    let provider_zoom = parts[2];
    if provider_zoom.len() < 2 {
        return None;
    }

    // Zoom is last 2 digits (this is CHUNK zoom, not tile zoom)
    let zoom_str = &provider_zoom[provider_zoom.len() - 2..];
    let chunk_zoom: u8 = zoom_str.parse().ok()?;

    // Convert CHUNK coordinates to TILE coordinates:
    // - Each tile contains 16×16 chunks
    // - Tile zoom = chunk zoom - 4 (2^4 = 16)
    let tile_row = chunk_row / 16;
    let tile_col = chunk_col / 16;
    let tile_zoom = chunk_zoom.saturating_sub(4);

    Some(TileCoord {
        row: tile_row,
        col: tile_col,
        zoom: tile_zoom,
    })
}

/// Generate an N×N grid of DSF tiles centered on the given coordinates.
///
/// The grid is centered on the DSF tile containing (lat, lon), with
/// tiles extending (grid_size / 2) in each direction.
///
/// # Arguments
///
/// * `lat` - Latitude of center point
/// * `lon` - Longitude of center point
/// * `grid_size` - Number of tiles per side (e.g., 8 for 8×8 grid)
///
/// # Returns
///
/// Vector of DSF tile coordinates covering the grid area.
pub fn generate_dsf_grid(lat: f64, lon: f64, grid_size: u32) -> Vec<DsfTileCoord> {
    let center = DsfTileCoord::from_lat_lon(lat, lon);
    let half = (grid_size / 2) as i32;

    let mut tiles = Vec::with_capacity((grid_size * grid_size) as usize);

    // Generate grid centered on center tile
    // For grid_size=8: offsets from -4 to +3 (8 tiles)
    for lat_offset in -half..=(half - 1) {
        for lon_offset in -half..=(half - 1) {
            tiles.push(center.offset(lat_offset, lon_offset));
        }
    }

    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_dsf_grid_8x8() {
        // LFBO airport: 43.6294, 1.3678
        let tiles = generate_dsf_grid(43.6294, 1.3678, 8);

        // Should be 8×8 = 64 tiles
        assert_eq!(tiles.len(), 64);

        // Center should be DSF tile (43, 1)
        let center = DsfTileCoord::from_lat_lon(43.6294, 1.3678);
        assert_eq!(center.lat, 43);
        assert_eq!(center.lon, 1);

        // Grid should include tiles from -4 to +3 offset from center
        // So for lat: 43-4=39 to 43+3=46
        // For lon: 1-4=-3 to 1+3=4
        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap();
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap();
        let min_lon = tiles.iter().map(|t| t.lon).min().unwrap();
        let max_lon = tiles.iter().map(|t| t.lon).max().unwrap();

        assert_eq!(min_lat, 39); // 43 - 4
        assert_eq!(max_lat, 46); // 43 + 3
        assert_eq!(min_lon, -3); // 1 - 4
        assert_eq!(max_lon, 4); // 1 + 3
    }

    #[test]
    fn test_generate_dsf_grid_4x4() {
        let tiles = generate_dsf_grid(0.0, 0.0, 4);

        // Should be 4×4 = 16 tiles
        assert_eq!(tiles.len(), 16);

        // Grid should be -2 to +1 offset from center (0, 0)
        let min_lat = tiles.iter().map(|t| t.lat).min().unwrap();
        let max_lat = tiles.iter().map(|t| t.lat).max().unwrap();

        assert_eq!(min_lat, -2);
        assert_eq!(max_lat, 1);
    }

    #[test]
    fn test_generate_dsf_grid_near_pole() {
        // Test near north pole - should clamp to valid range
        let tiles = generate_dsf_grid(88.5, 0.0, 4);

        // All tiles should have valid lat (clamped to 89 max)
        for tile in &tiles {
            assert!(tile.lat <= 89, "Latitude should be clamped to 89");
            assert!(tile.lat >= -90);
        }
    }

    #[test]
    fn test_generate_dsf_grid_near_antimeridian() {
        // Test near antimeridian - should wrap correctly
        let tiles = generate_dsf_grid(0.0, 178.0, 8);

        // Should have tiles that wrap from positive to negative longitude
        let has_positive = tiles.iter().any(|t| t.lon > 0);
        let has_negative = tiles.iter().any(|t| t.lon < 0);

        assert!(has_positive);
        assert!(has_negative);
    }

    #[test]
    fn test_dsf_grid_bounds() {
        let tiles = generate_dsf_grid(43.6294, 1.3678, 8);
        let bounds = DsfGridBounds::from_tiles(&tiles);

        // Bounds should cover lat 39-47 (8 tiles starting at 39) and lon -3 to 5
        assert_eq!(bounds.min_lat, 39.0);
        assert_eq!(bounds.max_lat, 47.0); // 46 + 1
        assert_eq!(bounds.min_lon, -3.0);
        assert_eq!(bounds.max_lon, 5.0); // 4 + 1

        // Test contains
        assert!(bounds.contains(43.5, 1.5)); // Center area
        assert!(bounds.contains(39.0, -3.0)); // Corner (inclusive)
        assert!(!bounds.contains(47.0, 5.0)); // Just outside
        assert!(!bounds.contains(38.9, 1.0)); // Below min_lat
    }

    #[test]
    fn test_parse_ter_filename() {
        // Standard format: chunk coords (23952, 32960) at chunk zoom 16
        // → tile coords (1497, 2060) at tile zoom 12
        let tile = parse_ter_filename("23952_32960_BI16.ter").unwrap();
        assert_eq!(tile.row, 23952 / 16); // 1497
        assert_eq!(tile.col, 32960 / 16); // 2060
        assert_eq!(tile.zoom, 16 - 4); // 12

        // With _sea suffix
        let tile = parse_ter_filename("23952_32960_BI16_sea.ter").unwrap();
        assert_eq!(tile.row, 1497);
        assert_eq!(tile.col, 2060);
        assert_eq!(tile.zoom, 12);

        // GO2 provider at chunk zoom 16
        let tile = parse_ter_filename("10000_19808_GO216.ter").unwrap();
        assert_eq!(tile.row, 10000 / 16); // 625
        assert_eq!(tile.col, 19808 / 16); // 1238
        assert_eq!(tile.zoom, 12);

        // With _sea_overlay suffix
        let tile = parse_ter_filename("10000_19808_GO216_sea_overlay.ter").unwrap();
        assert_eq!(tile.row, 625);
        assert_eq!(tile.col, 1238);
        assert_eq!(tile.zoom, 12);

        // Higher zoom: chunk zoom 18 → tile zoom 14
        let tile = parse_ter_filename("93248_139168_BI18.ter").unwrap();
        assert_eq!(tile.row, 93248 / 16); // 5828
        assert_eq!(tile.col, 139168 / 16); // 8698
        assert_eq!(tile.zoom, 18 - 4); // 14

        // Invalid format
        assert!(parse_ter_filename("invalid.ter").is_none());
        assert!(parse_ter_filename("23952_32960.ter").is_none()); // Missing zoom part
    }
}
