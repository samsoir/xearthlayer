//! FUSE-based position inference using dynamic envelope model.
//!
//! This module provides telemetry-free position and heading inference by tracking
//! tile requests from X-Plane via FUSE. Instead of relying on UDP telemetry,
//! it builds a "loaded envelope" from the pattern of tile requests and infers
//! movement from frontier expansion direction.
//!
//! # Dynamic Envelope Model
//!
//! The key insight is that X-Plane's tile loading pattern reveals the aircraft's
//! approximate position and movement direction:
//!
//! 1. **Track Requests**: Maintain a sliding window of recent tile requests
//! 2. **Build Envelope**: Compute bounding box and frontier tiles
//! 3. **Detect Movement**: Compare frontier snapshots to detect expansion direction
//! 4. **Infer Heading**: Direction of frontier expansion ≈ aircraft heading
//! 5. **Generate Prefetch**: Expand beyond frontier with fuzzy margins
//!
//! This approach is less efficient than telemetry-based prefetching (~100-150 tiles
//! vs ~50-80), but is more adaptive and provides graceful degradation when telemetry
//! is unavailable.
//!
//! # Usage
//!
//! ```ignore
//! use std::sync::Arc;
//! use xearthlayer::prefetch::inference::FuseRequestAnalyzer;
//! use xearthlayer::prefetch::config::FuseInferenceConfig;
//!
//! // Create analyzer with default config
//! let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
//!
//! // Get callback for wiring to FUSE filesystem
//! let callback = analyzer.callback();
//!
//! // Check if we have enough data for inference
//! if analyzer.is_active() {
//!     let tiles = analyzer.prefetch_tiles();
//!     // Submit tiles...
//! }
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::coord::TileCoord;

use super::config::FuseInferenceConfig;
use super::coordinates::tile_to_lat_lon_center;
use super::types::{PrefetchTile, PrefetchZone};

/// Callback type for FUSE tile request notifications.
///
/// This is invoked from the FUSE filesystem's hot path when X-Plane
/// requests a tile. The callback should be fast and non-blocking.
pub type TileRequestCallback = Arc<dyn Fn(TileCoord) + Send + Sync>;

/// Cardinal and intercardinal directions for frontier tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

impl Direction {
    /// All directions in order for iteration.
    pub const ALL: [Direction; 8] = [
        Direction::North,
        Direction::NorthEast,
        Direction::East,
        Direction::SouthEast,
        Direction::South,
        Direction::SouthWest,
        Direction::West,
        Direction::NorthWest,
    ];

    /// Convert heading (degrees, 0=north, 90=east) to nearest direction.
    pub fn from_heading(heading: f32) -> Self {
        let normalized = ((heading % 360.0) + 360.0) % 360.0;
        let index = ((normalized + 22.5) / 45.0) as usize % 8;
        Self::ALL[index]
    }

    /// Convert direction to approximate heading in degrees.
    pub fn to_heading(&self) -> f32 {
        match self {
            Direction::North => 0.0,
            Direction::NorthEast => 45.0,
            Direction::East => 90.0,
            Direction::SouthEast => 135.0,
            Direction::South => 180.0,
            Direction::SouthWest => 225.0,
            Direction::West => 270.0,
            Direction::NorthWest => 315.0,
        }
    }

    /// Get the opposite direction.
    pub fn opposite(&self) -> Self {
        match self {
            Direction::North => Direction::South,
            Direction::NorthEast => Direction::SouthWest,
            Direction::East => Direction::West,
            Direction::SouthEast => Direction::NorthWest,
            Direction::South => Direction::North,
            Direction::SouthWest => Direction::NorthEast,
            Direction::West => Direction::East,
            Direction::NorthWest => Direction::SouthEast,
        }
    }

    /// Get adjacent directions (±45°).
    pub fn adjacent(&self) -> [Self; 2] {
        match self {
            Direction::North => [Direction::NorthWest, Direction::NorthEast],
            Direction::NorthEast => [Direction::North, Direction::East],
            Direction::East => [Direction::NorthEast, Direction::SouthEast],
            Direction::SouthEast => [Direction::East, Direction::South],
            Direction::South => [Direction::SouthEast, Direction::SouthWest],
            Direction::SouthWest => [Direction::South, Direction::West],
            Direction::West => [Direction::SouthWest, Direction::NorthWest],
            Direction::NorthWest => [Direction::West, Direction::North],
        }
    }
}

/// A recorded tile request from FUSE.
#[derive(Debug, Clone)]
pub struct TileRequest {
    /// The tile coordinates.
    pub coord: TileCoord,
    /// Timestamp when the request was recorded.
    pub timestamp: Instant,
    /// Tile center in geographic coordinates (lat, lon).
    pub center: (f64, f64),
}

/// Bounding box of tiles.
#[derive(Debug, Clone, Default)]
pub struct TileBounds {
    /// Minimum row (northernmost).
    pub min_row: u32,
    /// Maximum row (southernmost).
    pub max_row: u32,
    /// Minimum column (westernmost).
    pub min_col: u32,
    /// Maximum column (easternmost).
    pub max_col: u32,
}

impl TileBounds {
    /// Check if a tile is within the bounds.
    pub fn contains(&self, tile: &TileCoord) -> bool {
        tile.row >= self.min_row
            && tile.row <= self.max_row
            && tile.col >= self.min_col
            && tile.col <= self.max_col
    }

    /// Check if a tile is on the edge of the bounds.
    pub fn is_on_edge(&self, tile: &TileCoord) -> bool {
        tile.row == self.min_row
            || tile.row == self.max_row
            || tile.col == self.min_col
            || tile.col == self.max_col
    }

    /// Get the direction of a tile from the center of the bounds.
    pub fn direction_of(&self, tile: &TileCoord) -> Option<Direction> {
        if !self.is_on_edge(tile) {
            return None;
        }

        let center_row = (self.min_row + self.max_row) / 2;
        let center_col = (self.min_col + self.max_col) / 2;

        let north = tile.row == self.min_row;
        let south = tile.row == self.max_row;
        let west = tile.col == self.min_col;
        let east = tile.col == self.max_col;

        let row_diff = (tile.row as i64) - (center_row as i64);
        let col_diff = (tile.col as i64) - (center_col as i64);

        // Prioritize based on which edge the tile is on
        match (north, south, west, east) {
            (true, false, true, false) => Some(Direction::NorthWest),
            (true, false, false, true) => Some(Direction::NorthEast),
            (false, true, true, false) => Some(Direction::SouthWest),
            (false, true, false, true) => Some(Direction::SouthEast),
            (true, false, _, _) if col_diff.abs() < (self.max_col - self.min_col) as i64 / 3 => {
                Some(Direction::North)
            }
            (false, true, _, _) if col_diff.abs() < (self.max_col - self.min_col) as i64 / 3 => {
                Some(Direction::South)
            }
            (_, _, true, false) if row_diff.abs() < (self.max_row - self.min_row) as i64 / 3 => {
                Some(Direction::West)
            }
            (_, _, false, true) if row_diff.abs() < (self.max_row - self.min_row) as i64 / 3 => {
                Some(Direction::East)
            }
            (true, false, _, _) => {
                if col_diff < 0 {
                    Some(Direction::NorthWest)
                } else {
                    Some(Direction::NorthEast)
                }
            }
            (false, true, _, _) => {
                if col_diff < 0 {
                    Some(Direction::SouthWest)
                } else {
                    Some(Direction::SouthEast)
                }
            }
            (_, _, true, false) => {
                if row_diff < 0 {
                    Some(Direction::NorthWest)
                } else {
                    Some(Direction::SouthWest)
                }
            }
            (_, _, false, true) => {
                if row_diff < 0 {
                    Some(Direction::NorthEast)
                } else {
                    Some(Direction::SouthEast)
                }
            }
            _ => None,
        }
    }
}

/// The envelope of tiles X-Plane has loaded.
///
/// This represents the current "loaded area" based on observed tile requests.
#[derive(Debug, Clone, Default)]
pub struct LoadedEnvelope {
    /// Bounding box of loaded tiles.
    pub bounds: TileBounds,

    /// Frontier tiles by direction.
    ///
    /// Frontier tiles are those on the edge of the loaded area in each direction.
    pub frontier: HashMap<Direction, HashSet<TileCoord>>,

    /// Centroid of loaded area (position estimate) as (lat, lon).
    pub centroid: (f64, f64),

    /// Number of tiles in the envelope.
    pub tile_count: usize,

    /// Zoom level of the envelope (all tiles should be at this zoom).
    pub zoom: u8,
}

/// Snapshot of frontier state for movement detection.
#[derive(Debug, Clone)]
pub struct FrontierSnapshot {
    /// Timestamp of this snapshot.
    pub timestamp: Instant,
    /// Frontier tiles by direction at this time.
    pub frontier: HashMap<Direction, HashSet<TileCoord>>,
    /// Bounds at this time.
    pub bounds: TileBounds,
}

/// Analyzes FUSE tile requests to build loaded envelope and generate prefetch tiles.
///
/// This is the core of the telemetry-free fallback system. It tracks X-Plane's
/// tile requests, builds a model of the loaded area, and generates prefetch
/// tiles beyond the frontier.
pub struct FuseRequestAnalyzer {
    /// Configuration for the analyzer.
    config: FuseInferenceConfig,

    /// Recent tile requests (sliding window).
    requests: RwLock<VecDeque<TileRequest>>,

    /// Set of unique tiles in the current window (for fast lookup).
    tile_set: RwLock<HashSet<(u32, u32, u8)>>,

    /// Current loaded envelope.
    envelope: RwLock<LoadedEnvelope>,

    /// History of frontier snapshots for movement detection.
    frontier_history: RwLock<VecDeque<FrontierSnapshot>>,

    /// Smoothed heading estimate (EMA).
    smoothed_heading: RwLock<Option<f32>>,

    /// Last update timestamp.
    last_update: RwLock<Instant>,
}

impl FuseRequestAnalyzer {
    /// Create a new analyzer with the given configuration.
    pub fn new(config: FuseInferenceConfig) -> Self {
        Self {
            config,
            requests: RwLock::new(VecDeque::new()),
            tile_set: RwLock::new(HashSet::new()),
            envelope: RwLock::new(LoadedEnvelope::default()),
            frontier_history: RwLock::new(VecDeque::new()),
            smoothed_heading: RwLock::new(None),
            last_update: RwLock::new(Instant::now()),
        }
    }

    /// Record a tile request from FUSE (thread-safe, called from hot path).
    ///
    /// This method is designed to be fast and non-blocking. It records the
    /// tile request and triggers an envelope update if enough time has passed.
    pub fn record_request(&self, coord: TileCoord) {
        let key = (coord.row, coord.col, coord.zoom);
        let (lat, lon) = tile_to_lat_lon_center(&coord);

        let request = TileRequest {
            coord,
            timestamp: Instant::now(),
            center: (lat, lon),
        };

        // Add to request queue
        {
            let mut requests = self.requests.write().unwrap();
            requests.push_back(request);
        }

        // Track unique tiles
        {
            let mut tile_set = self.tile_set.write().unwrap();
            tile_set.insert(key);
        }

        // Periodically prune and update envelope (not on every request to save CPU)
        let should_update = {
            let last_update = self.last_update.read().unwrap();
            last_update.elapsed().as_millis() > 500 // Update at most 2x per second
        };

        if should_update {
            self.prune_old_requests();
            self.update_envelope();
            *self.last_update.write().unwrap() = Instant::now();
        }
    }

    /// Create a callback for wiring to FUSE filesystem.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let analyzer = Arc::new(FuseRequestAnalyzer::new(config));
    /// let callback = analyzer.callback();
    /// filesystem.with_tile_request_callback(callback);
    /// ```
    pub fn callback(self: &Arc<Self>) -> TileRequestCallback {
        let analyzer = Arc::clone(self);
        Arc::new(move |coord| {
            analyzer.record_request(coord);
        })
    }

    /// Check if we have enough data for inference.
    ///
    /// Returns `true` if the number of unique tiles in the sliding window
    /// meets the minimum threshold.
    pub fn is_active(&self) -> bool {
        let tile_set = self.tile_set.read().unwrap();
        tile_set.len() >= self.config.min_requests_for_inference
    }

    /// Get current confidence level (0.0 - 1.0).
    ///
    /// Confidence is based on:
    /// - Number of tiles tracked (more = higher)
    /// - Stability of heading estimate (less jitter = higher)
    /// - Recency of data (fresher = higher)
    pub fn confidence(&self) -> f32 {
        let tile_set = self.tile_set.read().unwrap();
        let tile_count = tile_set.len();

        if tile_count < self.config.min_requests_for_inference {
            return 0.0;
        }

        // Base confidence from tile count (0.3 - 0.7)
        let count_factor = (tile_count as f32 / 50.0).min(1.0) * 0.4 + 0.3;

        // Adjust for heading stability
        let heading_factor = if self.smoothed_heading.read().unwrap().is_some() {
            0.3
        } else {
            0.0
        };

        // Adjust for data recency
        let requests = self.requests.read().unwrap();
        let recency_factor = if let Some(last) = requests.back() {
            let age_secs = last.timestamp.elapsed().as_secs_f32();
            if age_secs < 5.0 {
                0.3
            } else if age_secs < 15.0 {
                0.2
            } else {
                0.1
            }
        } else {
            0.0
        };

        (count_factor + heading_factor + recency_factor).min(1.0)
    }

    /// Get estimated aircraft position (centroid of envelope).
    ///
    /// Returns the geographic center of the loaded tile area.
    pub fn position(&self) -> Option<(f64, f64)> {
        if !self.is_active() {
            return None;
        }

        let envelope = self.envelope.read().unwrap();
        if envelope.tile_count == 0 {
            return None;
        }

        Some(envelope.centroid)
    }

    /// Get estimated heading from frontier movement.
    ///
    /// Returns `(heading, confidence)` where heading is in degrees (0-360)
    /// and confidence is 0.0-1.0.
    pub fn heading(&self) -> Option<(f32, f32)> {
        let smoothed = *self.smoothed_heading.read().unwrap();
        smoothed.map(|h| (h, self.confidence()))
    }

    /// Generate prefetch tiles beyond the frontier.
    ///
    /// Uses wider margins than telemetry mode to account for uncertainty.
    pub fn prefetch_tiles(&self) -> Vec<PrefetchTile> {
        if !self.is_active() {
            return Vec::new();
        }

        let envelope = self.envelope.read().unwrap();
        if envelope.tile_count == 0 {
            return Vec::new();
        }

        let confidence = self.confidence();
        let _effective_half_angle = self.config.effective_cone_half_angle(confidence);
        let depth = self.config.prefetch_depth_tiles as i32;

        // Get movement direction (if any)
        let movement_dir = self.detect_movement_direction();

        let mut tiles = Vec::new();
        let mut seen: HashSet<(u32, u32)> = HashSet::new();

        // Collect existing tiles to avoid
        for (row, col, _) in self.tile_set.read().unwrap().iter() {
            seen.insert((*row, *col));
        }

        if let Some(primary_dir) = movement_dir {
            // Focus prefetch in movement direction with fuzzy cone
            let adjacent = primary_dir.adjacent();
            let all_dirs = [primary_dir, adjacent[0], adjacent[1]];

            for (dir_idx, dir) in all_dirs.iter().enumerate() {
                let dir_tiles = self.expand_frontier_in_direction(&envelope, *dir, depth);
                let zone = if dir_idx == 0 {
                    PrefetchZone::ForwardCenter
                } else {
                    PrefetchZone::ForwardEdge
                };

                for (priority, coord) in dir_tiles {
                    let key = (coord.row, coord.col);
                    if seen.insert(key) {
                        tiles.push(PrefetchTile::new(
                            coord,
                            zone.base_priority() + priority,
                            zone,
                        ));
                    }
                }
            }

            // Add lateral buffers (perpendicular to movement)
            let lateral_depth = self
                .config
                .effective_lateral_depth(self.config.prefetch_depth_tiles);
            let lateral_dirs = self.lateral_directions(primary_dir);
            for dir in lateral_dirs {
                let dir_tiles =
                    self.expand_frontier_in_direction(&envelope, dir, lateral_depth as i32);
                for (priority, coord) in dir_tiles {
                    let key = (coord.row, coord.col);
                    if seen.insert(key) {
                        tiles.push(PrefetchTile::new(
                            coord,
                            PrefetchZone::LateralBuffer.base_priority() + priority,
                            PrefetchZone::LateralBuffer,
                        ));
                    }
                }
            }
        } else {
            // No clear movement direction - expand in all directions
            for dir in Direction::ALL.iter() {
                let dir_tiles = self.expand_frontier_in_direction(&envelope, *dir, depth);
                for (priority, coord) in dir_tiles {
                    let key = (coord.row, coord.col);
                    if seen.insert(key) {
                        tiles.push(PrefetchTile::new(
                            coord,
                            PrefetchZone::ForwardEdge.base_priority() + priority,
                            PrefetchZone::ForwardEdge,
                        ));
                    }
                }
            }
        }

        // Sort by priority (lower = higher priority)
        tiles.sort_by_key(|t| t.priority);

        tiles
    }

    /// Get the current envelope snapshot.
    pub fn envelope(&self) -> LoadedEnvelope {
        self.envelope.read().unwrap().clone()
    }

    /// Get the number of tracked tiles.
    pub fn tile_count(&self) -> usize {
        self.tile_set.read().unwrap().len()
    }

    // ==================== Private Methods ====================

    /// Prune requests older than max_request_age.
    fn prune_old_requests(&self) {
        let max_age = self.config.max_request_age();
        let mut requests = self.requests.write().unwrap();
        let mut tile_set = self.tile_set.write().unwrap();

        // Build set of tiles still in window
        let mut active_tiles = HashSet::new();

        requests.retain(|req| {
            let keep = req.timestamp.elapsed() < max_age;
            if keep {
                active_tiles.insert((req.coord.row, req.coord.col, req.coord.zoom));
            }
            keep
        });

        // Update tile set to match
        *tile_set = active_tiles;
    }

    /// Update the envelope from current requests.
    fn update_envelope(&self) {
        let requests = self.requests.read().unwrap();

        if requests.is_empty() {
            return;
        }

        // Find bounds
        let mut min_row = u32::MAX;
        let mut max_row = 0;
        let mut min_col = u32::MAX;
        let mut max_col = 0;
        let mut zoom = 14u8; // Default
        let mut lat_sum = 0.0;
        let mut lon_sum = 0.0;
        let mut tile_coords: HashSet<TileCoord> = HashSet::new();

        for req in requests.iter() {
            min_row = min_row.min(req.coord.row);
            max_row = max_row.max(req.coord.row);
            min_col = min_col.min(req.coord.col);
            max_col = max_col.max(req.coord.col);
            zoom = req.coord.zoom;
            lat_sum += req.center.0;
            lon_sum += req.center.1;
            tile_coords.insert(req.coord);
        }

        let count = requests.len();
        let centroid = (lat_sum / count as f64, lon_sum / count as f64);

        let bounds = TileBounds {
            min_row,
            max_row,
            min_col,
            max_col,
        };

        // Build frontier
        let mut frontier: HashMap<Direction, HashSet<TileCoord>> = HashMap::new();
        for dir in Direction::ALL.iter() {
            frontier.insert(*dir, HashSet::new());
        }

        for coord in tile_coords.iter() {
            if let Some(dir) = bounds.direction_of(coord) {
                frontier.get_mut(&dir).unwrap().insert(*coord);
            }
        }

        // Update envelope
        {
            let mut envelope = self.envelope.write().unwrap();
            envelope.bounds = bounds.clone();
            envelope.frontier = frontier.clone();
            envelope.centroid = centroid;
            envelope.tile_count = tile_coords.len();
            envelope.zoom = zoom;
        }

        // Save frontier snapshot for movement detection
        {
            let mut history = self.frontier_history.write().unwrap();
            history.push_back(FrontierSnapshot {
                timestamp: Instant::now(),
                frontier,
                bounds,
            });

            // Keep only recent history
            while history.len() > self.config.frontier_history_size {
                history.pop_front();
            }
        }

        // Detect and smooth heading
        self.update_heading_estimate();
    }

    /// Detect movement direction from frontier history.
    fn detect_movement_direction(&self) -> Option<Direction> {
        let history = self.frontier_history.read().unwrap();

        if history.len() < 2 {
            return None;
        }

        // Compare oldest and newest snapshots
        let oldest = history.front()?;
        let newest = history.back()?;

        // Calculate expansion in each direction
        let mut expansion_scores: HashMap<Direction, i32> = HashMap::new();

        for dir in Direction::ALL.iter() {
            let old_tiles = oldest.frontier.get(dir).map(|s| s.len()).unwrap_or(0);
            let new_tiles = newest.frontier.get(dir).map(|s| s.len()).unwrap_or(0);

            // Calculate expansion (more tiles = expansion in that direction)
            let expansion = new_tiles as i32 - old_tiles as i32;

            // Also check bounds expansion
            let bounds_expansion = match dir {
                Direction::North => oldest.bounds.min_row as i32 - newest.bounds.min_row as i32,
                Direction::South => newest.bounds.max_row as i32 - oldest.bounds.max_row as i32,
                Direction::East => newest.bounds.max_col as i32 - oldest.bounds.max_col as i32,
                Direction::West => oldest.bounds.min_col as i32 - newest.bounds.min_col as i32,
                Direction::NorthEast => {
                    (oldest.bounds.min_row as i32 - newest.bounds.min_row as i32)
                        + (newest.bounds.max_col as i32 - oldest.bounds.max_col as i32)
                }
                Direction::NorthWest => {
                    (oldest.bounds.min_row as i32 - newest.bounds.min_row as i32)
                        + (oldest.bounds.min_col as i32 - newest.bounds.min_col as i32)
                }
                Direction::SouthEast => {
                    (newest.bounds.max_row as i32 - oldest.bounds.max_row as i32)
                        + (newest.bounds.max_col as i32 - oldest.bounds.max_col as i32)
                }
                Direction::SouthWest => {
                    (newest.bounds.max_row as i32 - oldest.bounds.max_row as i32)
                        + (oldest.bounds.min_col as i32 - newest.bounds.min_col as i32)
                }
            };

            expansion_scores.insert(*dir, expansion + bounds_expansion * 2);
        }

        // Find direction with highest expansion
        expansion_scores
            .iter()
            .filter(|(_, &score)| score > 0)
            .max_by_key(|(_, &score)| score)
            .map(|(dir, _)| *dir)
    }

    /// Update smoothed heading estimate.
    fn update_heading_estimate(&self) {
        if let Some(dir) = self.detect_movement_direction() {
            let new_heading = dir.to_heading();
            let smoothing = self.config.heading_smoothing;

            let mut smoothed = self.smoothed_heading.write().unwrap();
            *smoothed = match *smoothed {
                Some(prev) => {
                    // Handle wrap-around for heading averaging
                    let diff = new_heading - prev;
                    let adjusted_diff = if diff > 180.0 {
                        diff - 360.0
                    } else if diff < -180.0 {
                        diff + 360.0
                    } else {
                        diff
                    };

                    let mut result = prev + smoothing * adjusted_diff;
                    if result < 0.0 {
                        result += 360.0;
                    } else if result >= 360.0 {
                        result -= 360.0;
                    }
                    Some(result)
                }
                None => Some(new_heading),
            };
        }
    }

    /// Expand frontier tiles in a given direction.
    ///
    /// Returns tiles with their priority (distance from frontier).
    fn expand_frontier_in_direction(
        &self,
        envelope: &LoadedEnvelope,
        dir: Direction,
        depth: i32,
    ) -> Vec<(u32, TileCoord)> {
        let mut tiles = Vec::new();

        // Get row/col delta for this direction
        let (row_delta, col_delta) = match dir {
            Direction::North => (-1, 0),
            Direction::NorthEast => (-1, 1),
            Direction::East => (0, 1),
            Direction::SouthEast => (1, 1),
            Direction::South => (1, 0),
            Direction::SouthWest => (1, -1),
            Direction::West => (0, -1),
            Direction::NorthWest => (-1, -1),
        };

        // Get frontier tiles in this direction (or all edge tiles if none)
        let frontier_tiles: Vec<TileCoord> = envelope
            .frontier
            .get(&dir)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_else(|| {
                // Fall back to edge calculation from bounds
                let mut edge = Vec::new();
                for row in envelope.bounds.min_row..=envelope.bounds.max_row {
                    for col in envelope.bounds.min_col..=envelope.bounds.max_col {
                        let coord = TileCoord {
                            row,
                            col,
                            zoom: envelope.zoom,
                        };
                        if envelope.bounds.is_on_edge(&coord) {
                            if let Some(tile_dir) = envelope.bounds.direction_of(&coord) {
                                if tile_dir == dir {
                                    edge.push(coord);
                                }
                            }
                        }
                    }
                }
                edge
            });

        // Expand from each frontier tile
        for frontier_tile in frontier_tiles {
            for d in 1..=depth {
                let new_row = frontier_tile.row as i32 + row_delta * d;
                let new_col = frontier_tile.col as i32 + col_delta * d;

                if new_row >= 0 && new_col >= 0 {
                    let coord = TileCoord {
                        row: new_row as u32,
                        col: new_col as u32,
                        zoom: envelope.zoom,
                    };
                    tiles.push((d as u32, coord));
                }
            }
        }

        tiles
    }

    /// Get lateral directions perpendicular to a primary direction.
    fn lateral_directions(&self, primary: Direction) -> [Direction; 2] {
        match primary {
            Direction::North | Direction::South => [Direction::East, Direction::West],
            Direction::East | Direction::West => [Direction::North, Direction::South],
            Direction::NorthEast | Direction::SouthWest => {
                [Direction::NorthWest, Direction::SouthEast]
            }
            Direction::NorthWest | Direction::SouthEast => {
                [Direction::NorthEast, Direction::SouthWest]
            }
        }
    }
}

impl std::fmt::Debug for FuseRequestAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuseRequestAnalyzer")
            .field("tile_count", &self.tile_count())
            .field("is_active", &self.is_active())
            .field("confidence", &self.confidence())
            .field("heading", &self.heading())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn make_tile(row: u32, col: u32, zoom: u8) -> TileCoord {
        TileCoord { row, col, zoom }
    }

    // ==================== Direction Tests ====================

    #[test]
    fn test_direction_from_heading() {
        assert_eq!(Direction::from_heading(0.0), Direction::North);
        assert_eq!(Direction::from_heading(22.0), Direction::North);
        assert_eq!(Direction::from_heading(23.0), Direction::NorthEast);
        assert_eq!(Direction::from_heading(45.0), Direction::NorthEast);
        assert_eq!(Direction::from_heading(90.0), Direction::East);
        assert_eq!(Direction::from_heading(180.0), Direction::South);
        assert_eq!(Direction::from_heading(270.0), Direction::West);
        assert_eq!(Direction::from_heading(350.0), Direction::North);
        assert_eq!(Direction::from_heading(-10.0), Direction::North);
        assert_eq!(Direction::from_heading(360.0), Direction::North);
    }

    #[test]
    fn test_direction_to_heading() {
        assert_eq!(Direction::North.to_heading(), 0.0);
        assert_eq!(Direction::NorthEast.to_heading(), 45.0);
        assert_eq!(Direction::East.to_heading(), 90.0);
        assert_eq!(Direction::South.to_heading(), 180.0);
        assert_eq!(Direction::West.to_heading(), 270.0);
    }

    #[test]
    fn test_direction_opposite() {
        assert_eq!(Direction::North.opposite(), Direction::South);
        assert_eq!(Direction::NorthEast.opposite(), Direction::SouthWest);
        assert_eq!(Direction::East.opposite(), Direction::West);
    }

    #[test]
    fn test_direction_adjacent() {
        let adj = Direction::North.adjacent();
        assert!(adj.contains(&Direction::NorthWest));
        assert!(adj.contains(&Direction::NorthEast));
    }

    // ==================== TileBounds Tests ====================

    #[test]
    fn test_tile_bounds_contains() {
        let bounds = TileBounds {
            min_row: 5,
            max_row: 10,
            min_col: 5,
            max_col: 10,
        };

        assert!(bounds.contains(&make_tile(7, 7, 14)));
        assert!(bounds.contains(&make_tile(5, 5, 14)));
        assert!(bounds.contains(&make_tile(10, 10, 14)));
        assert!(!bounds.contains(&make_tile(4, 7, 14)));
        assert!(!bounds.contains(&make_tile(11, 7, 14)));
    }

    #[test]
    fn test_tile_bounds_is_on_edge() {
        let bounds = TileBounds {
            min_row: 5,
            max_row: 10,
            min_col: 5,
            max_col: 10,
        };

        assert!(bounds.is_on_edge(&make_tile(5, 7, 14))); // Top edge
        assert!(bounds.is_on_edge(&make_tile(10, 7, 14))); // Bottom edge
        assert!(bounds.is_on_edge(&make_tile(7, 5, 14))); // Left edge
        assert!(bounds.is_on_edge(&make_tile(7, 10, 14))); // Right edge
        assert!(!bounds.is_on_edge(&make_tile(7, 7, 14))); // Center
    }

    // ==================== FuseRequestAnalyzer Tests ====================

    #[test]
    fn test_analyzer_not_active_initially() {
        let analyzer = FuseRequestAnalyzer::new(FuseInferenceConfig::default());
        assert!(!analyzer.is_active());
        assert_eq!(analyzer.confidence(), 0.0);
        assert!(analyzer.position().is_none());
        assert!(analyzer.heading().is_none());
    }

    #[test]
    fn test_analyzer_record_request() {
        let analyzer = FuseRequestAnalyzer::new(FuseInferenceConfig::default());

        // Record some tiles
        for row in 100..105 {
            for col in 200..205 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }

        // Should have 25 tiles tracked
        assert_eq!(analyzer.tile_count(), 25);
    }

    #[test]
    fn test_analyzer_becomes_active() {
        let mut config = FuseInferenceConfig::default();
        config.min_requests_for_inference = 5; // Lower for test

        let analyzer = FuseRequestAnalyzer::new(config);

        // Record enough tiles to become active
        for row in 100..105 {
            analyzer.record_request(make_tile(row, 200, 14));
        }

        assert!(analyzer.is_active());
        assert!(analyzer.confidence() > 0.0);
    }

    #[test]
    fn test_analyzer_envelope_update() {
        let mut config = FuseInferenceConfig::default();
        config.min_requests_for_inference = 5;

        let analyzer = FuseRequestAnalyzer::new(config);

        // Record tiles in a pattern
        for row in 100..110 {
            for col in 200..210 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }

        // Force envelope update
        analyzer.update_envelope();

        let envelope = analyzer.envelope();
        assert_eq!(envelope.bounds.min_row, 100);
        assert_eq!(envelope.bounds.max_row, 109);
        assert_eq!(envelope.bounds.min_col, 200);
        assert_eq!(envelope.bounds.max_col, 209);
        assert!(envelope.tile_count > 0);
    }

    #[test]
    fn test_analyzer_position_estimate() {
        let mut config = FuseInferenceConfig::default();
        config.min_requests_for_inference = 5;

        let analyzer = FuseRequestAnalyzer::new(config);

        // Record tiles centered around a known location
        for row in 100..110 {
            for col in 200..210 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }

        analyzer.update_envelope();

        let pos = analyzer.position();
        assert!(pos.is_some(), "Should have position estimate");

        let (lat, lon) = pos.unwrap();
        // Position should be somewhere reasonable (not zero)
        assert!(lat.abs() < 90.0);
        assert!(lon.abs() < 180.0);
    }

    #[test]
    fn test_analyzer_prefetch_tiles_empty_when_inactive() {
        let analyzer = FuseRequestAnalyzer::new(FuseInferenceConfig::default());
        let tiles = analyzer.prefetch_tiles();
        assert!(tiles.is_empty());
    }

    #[test]
    fn test_analyzer_prefetch_tiles_generated() {
        let mut config = FuseInferenceConfig::default();
        config.min_requests_for_inference = 5;
        config.prefetch_depth_tiles = 3;

        let analyzer = FuseRequestAnalyzer::new(config);

        // Record tiles in a pattern
        for row in 100..110 {
            for col in 200..210 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }

        analyzer.update_envelope();

        let tiles = analyzer.prefetch_tiles();

        // Should generate some prefetch tiles
        assert!(!tiles.is_empty(), "Should generate prefetch tiles");

        // All tiles should be outside the original bounds
        let envelope = analyzer.envelope();
        for tile in &tiles {
            // At least one dimension should be outside bounds
            let outside = tile.coord.row < envelope.bounds.min_row
                || tile.coord.row > envelope.bounds.max_row
                || tile.coord.col < envelope.bounds.min_col
                || tile.coord.col > envelope.bounds.max_col;
            assert!(
                outside,
                "Prefetch tile {:?} should be outside envelope",
                tile
            );
        }
    }

    #[test]
    fn test_analyzer_callback_creation() {
        let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));
        let callback = analyzer.callback();

        // Use the callback
        callback(make_tile(100, 200, 14));
        callback(make_tile(101, 200, 14));

        assert_eq!(analyzer.tile_count(), 2);
    }

    #[test]
    fn test_analyzer_thread_safety() {
        use std::thread;

        let analyzer = Arc::new(FuseRequestAnalyzer::new(FuseInferenceConfig::default()));

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let analyzer = Arc::clone(&analyzer);
                thread::spawn(move || {
                    for row in (100 + i * 10)..(100 + i * 10 + 10) {
                        for col in 200..210 {
                            analyzer.record_request(make_tile(row, col, 14));
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have tracked tiles from all threads
        assert!(analyzer.tile_count() > 0);
    }

    // ==================== Movement Detection Tests ====================

    #[test]
    fn test_movement_detection_north() {
        let mut config = FuseInferenceConfig::default();
        config.min_requests_for_inference = 5;
        config.frontier_history_size = 3;

        let analyzer = FuseRequestAnalyzer::new(config);

        // Initial position - use same column range to isolate north movement
        for row in 100..110 {
            for col in 200..210 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }
        analyzer.update_envelope();

        // Move north (smaller row numbers) - use same column range
        for row in 90..100 {
            for col in 200..210 {
                analyzer.record_request(make_tile(row, col, 14));
            }
        }
        analyzer.update_envelope();

        // Check movement direction - should detect northward expansion
        let dir = analyzer.detect_movement_direction();
        // Accept North or NorthEast/NorthWest as valid (detection is fuzzy)
        assert!(
            matches!(
                dir,
                Some(Direction::North) | Some(Direction::NorthEast) | Some(Direction::NorthWest)
            ),
            "Expected northward direction, got {:?}",
            dir
        );
    }
}
