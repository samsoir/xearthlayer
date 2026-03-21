//! JSON API for the debug map.
//!
//! Collects state from all data sources into a single JSON snapshot.
//! Called by the Leaflet.js map page every 2 seconds.

use serde::Serialize;

use crate::aircraft_position::AircraftPositionProvider;
use crate::geo_index::{PatchCoverage, PrefetchedRegion, RetainedRegion};
use crate::prefetch::adaptive::{AdaptivePrefetchConfig, PrefetchBox};

use super::state::DebugMapState;

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot types (serialised to JSON)
// ─────────────────────────────────────────────────────────────────────────────

/// Complete state snapshot returned by `/api/state`.
#[derive(Serialize, Default)]
pub struct DebugStateSnapshot {
    pub aircraft: Option<AircraftInfo>,
    pub sim_state: SimStateInfo,
    pub prefetch_box: Option<BoxBounds>,
    pub regions: Vec<RegionInfo>,
    pub tiles: Vec<TileInfo>,
    pub stats: StatsInfo,
}

/// A single DDS tile with its origin and cache result.
#[derive(Serialize)]
pub struct TileInfo {
    pub row: u32,
    pub col: u32,
    pub zoom: u8,
    pub lat: f64,
    pub lon: f64,
    pub origin: String,
    pub result: String,
}

/// Aircraft position and vectors.
#[derive(Serialize)]
pub struct AircraftInfo {
    pub latitude: f64,
    pub longitude: f64,
    pub heading: f32,
    pub track: Option<f32>,
    pub ground_speed: f32,
    pub altitude: f32,
}

/// Sim state from X-Plane Web API.
#[derive(Serialize, Default)]
pub struct SimStateInfo {
    pub paused: bool,
    pub on_ground: bool,
    pub scenery_loading: bool,
    pub replay: bool,
    pub sim_speed: i32,
}

/// Prefetch box geographic bounds.
#[derive(Serialize)]
pub struct BoxBounds {
    pub lat_min: f64,
    pub lat_max: f64,
    pub lon_min: f64,
    pub lon_max: f64,
}

/// A single DSF region with its current state and activity.
#[derive(Serialize)]
pub struct RegionInfo {
    pub lat: i32,
    pub lon: i32,
    pub state: RegionState,
    /// Number of tiles served from cache for FUSE requests.
    #[serde(skip_serializing_if = "is_zero")]
    pub fuse_hits: u32,
    /// Number of tiles X-Plane had to wait for (FUSE cache misses).
    #[serde(skip_serializing_if = "is_zero")]
    pub fuse_misses: u32,
    /// Number of tiles prefetched.
    #[serde(skip_serializing_if = "is_zero")]
    pub prefetch_generated: u32,
}

impl RegionInfo {
    fn new(lat: i32, lon: i32, state: RegionState) -> Self {
        Self {
            lat,
            lon,
            state,
            fuse_hits: 0,
            fuse_misses: 0,
            prefetch_generated: 0,
        }
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Region state for map colour coding.
#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RegionState {
    /// Prefetch submitted, awaiting completion.
    InProgress,
    /// All tiles confirmed in cache via prefetch.
    Prefetched,
    /// No scenery data exists for this region.
    NoCoverage,
    /// In the retained window (tracked by SceneryWindow).
    Retained,
    /// Owned by a scenery patch.
    Patched,
    /// X-Plane loaded tiles on-demand (prefetch missed).
    FuseLoaded,
    /// Mix of prefetch and on-demand loading.
    Mixed,
}

/// Pipeline statistics.
#[derive(Serialize, Default)]
pub struct StatsInfo {
    pub memory_cache_hit_rate: f64,
    pub tiles_submitted: u64,
    pub deferred_cycles: u64,
    pub prefetch_mode: String,
    pub active_strategy: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// State collection
// ─────────────────────────────────────────────────────────────────────────────

/// Collect current state from all sources into a JSON-serialisable snapshot.
pub fn collect_snapshot(state: &DebugMapState) -> DebugStateSnapshot {
    let aircraft = collect_aircraft(state);
    let sim_state = collect_sim_state(state);
    let prefetch_box = compute_prefetch_box(&aircraft);
    let regions = collect_regions(state);
    let tiles = collect_tiles(state);
    let stats = collect_stats(state);

    DebugStateSnapshot {
        aircraft,
        sim_state,
        prefetch_box,
        regions,
        tiles,
        stats,
    }
}

fn collect_aircraft(state: &DebugMapState) -> Option<AircraftInfo> {
    let status = state.aircraft_position.status();
    let aircraft_state = status.state?;

    Some(AircraftInfo {
        latitude: aircraft_state.latitude,
        longitude: aircraft_state.longitude,
        heading: aircraft_state.heading,
        track: aircraft_state.track,
        ground_speed: aircraft_state.ground_speed,
        altitude: aircraft_state.altitude,
    })
}

fn collect_sim_state(state: &DebugMapState) -> SimStateInfo {
    match state.sim_state.read() {
        Ok(sim) => SimStateInfo {
            paused: sim.paused,
            on_ground: sim.on_ground,
            scenery_loading: sim.scenery_loading,
            replay: sim.replay,
            sim_speed: sim.sim_speed,
        },
        Err(_) => SimStateInfo::default(),
    }
}

fn compute_prefetch_box(aircraft: &Option<AircraftInfo>) -> Option<BoxBounds> {
    let aircraft = aircraft.as_ref()?;
    let track = aircraft.track.unwrap_or(aircraft.heading) as f64;

    let config = AdaptivePrefetchConfig::default();
    let pbox = PrefetchBox::new(config.forward_margin, config.behind_margin);
    let (lat_min, lat_max, lon_min, lon_max) =
        pbox.bounds(aircraft.latitude, aircraft.longitude, track);

    Some(BoxBounds {
        lat_min,
        lat_max,
        lon_min,
        lon_max,
    })
}

fn collect_regions(state: &DebugMapState) -> Vec<RegionInfo> {
    use std::collections::HashMap;

    // Start with tile activity — this is the ground truth of what's been loaded
    let activity = state.tile_activity.snapshot();
    let mut region_map: HashMap<(i32, i32), RegionInfo> = HashMap::new();

    // Layer 1: Tile activity (FUSE + prefetch generation events)
    for (region, act) in &activity {
        let region_state = if act.fuse_generated > 0 && act.prefetch_generated > 0 {
            RegionState::Mixed
        } else if act.fuse_generated > 0 {
            RegionState::FuseLoaded
        } else if act.prefetch_generated > 0 {
            RegionState::Prefetched
        } else if act.fuse_cache_hits > 0 || act.prefetch_cache_hits > 0 {
            RegionState::Prefetched // all cache hits = was prefetched earlier
        } else {
            continue;
        };

        let mut info = RegionInfo::new(region.lat, region.lon, region_state);
        info.fuse_hits = act.fuse_cache_hits;
        info.fuse_misses = act.fuse_generated;
        info.prefetch_generated = act.prefetch_generated;
        region_map.insert((region.lat, region.lon), info);
    }

    // Layer 2: GeoIndex prefetch state (overlay on top of activity)
    if let Some(ref geo_index) = state.geo_index {
        for (region, prefetched) in geo_index.iter::<PrefetchedRegion>() {
            let key = (region.lat, region.lon);
            if !region_map.contains_key(&key) {
                let geo_state = if prefetched.is_in_progress() {
                    RegionState::InProgress
                } else if prefetched.is_prefetched() {
                    RegionState::Prefetched
                } else {
                    RegionState::NoCoverage
                };
                region_map.insert(key, RegionInfo::new(region.lat, region.lon, geo_state));
            }
        }

        // Layer 3: Patch coverage
        for region in geo_index.regions::<PatchCoverage>() {
            let key = (region.lat, region.lon);
            if !region_map.contains_key(&key) {
                region_map.insert(
                    key,
                    RegionInfo::new(region.lat, region.lon, RegionState::Patched),
                );
            }
        }

        // Layer 4: Retained regions
        for region in geo_index.regions::<RetainedRegion>() {
            let key = (region.lat, region.lon);
            if !region_map.contains_key(&key) {
                region_map.insert(
                    key,
                    RegionInfo::new(region.lat, region.lon, RegionState::Retained),
                );
            }
        }
    }

    region_map.into_values().collect()
}

fn collect_tiles(state: &DebugMapState) -> Vec<TileInfo> {
    use super::activity::TileCacheResult;
    use super::activity::TileOrigin;

    state
        .tile_activity
        .tile_snapshot()
        .into_iter()
        .map(|(key, act)| TileInfo {
            row: key.row,
            col: key.col,
            zoom: key.zoom,
            lat: act.lat,
            lon: act.lon,
            origin: match act.origin {
                TileOrigin::Fuse => "fuse".to_string(),
                TileOrigin::Prefetch => "prefetch".to_string(),
            },
            result: match act.result {
                TileCacheResult::CacheHit => "cache_hit".to_string(),
                TileCacheResult::Generated => "generated".to_string(),
            },
        })
        .collect()
}

fn collect_stats(state: &DebugMapState) -> StatsInfo {
    let snapshot = state.prefetch_status.snapshot();

    let (tiles_submitted, deferred_cycles, active_strategy) =
        if let Some(ref detailed) = snapshot.detailed_stats {
            (
                detailed.tiles_submitted_total,
                detailed.deferred_cycles,
                String::new(), // Active strategy not in detailed stats
            )
        } else {
            (snapshot.stats.tiles_submitted, 0, String::new())
        };

    StatsInfo {
        memory_cache_hit_rate: 0.0, // TODO: wire from TelemetrySnapshot when available
        tiles_submitted,
        deferred_cycles,
        prefetch_mode: format!("{:?}", snapshot.prefetch_mode),
        active_strategy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_state_snapshot_serialises() {
        let snapshot = DebugStateSnapshot {
            aircraft: Some(AircraftInfo {
                latitude: 48.0,
                longitude: 15.0,
                heading: 270.0,
                track: Some(265.0),
                ground_speed: 450.0,
                altitude: 35000.0,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("48"));
        assert!(json.contains("270"));
    }

    #[test]
    fn test_region_info_serialises_state() {
        let region = RegionInfo::new(48, 15, RegionState::Prefetched);
        let json = serde_json::to_string(&region).unwrap();
        assert!(json.contains("\"prefetched\""));
    }

    #[test]
    fn test_region_info_serialises_in_progress() {
        let region = RegionInfo::new(48, 15, RegionState::InProgress);
        let json = serde_json::to_string(&region).unwrap();
        assert!(json.contains("\"in_progress\""));
    }

    #[test]
    fn test_box_bounds_serialises() {
        let bounds = BoxBounds {
            lat_min: 45.0,
            lat_max: 49.0,
            lon_min: 12.0,
            lon_max: 16.0,
        };
        let json = serde_json::to_string(&bounds).unwrap();
        assert!(json.contains("45"));
        assert!(json.contains("49"));
    }

    #[test]
    fn test_compute_prefetch_box_heading_west() {
        let aircraft = Some(AircraftInfo {
            latitude: 48.0,
            longitude: 15.0,
            heading: 270.0,
            track: Some(270.0),
            ground_speed: 450.0,
            altitude: 35000.0,
        });
        let bounds = compute_prefetch_box(&aircraft).unwrap();
        // Heading west: lon biased west (3° ahead), east (1° behind)
        assert!(bounds.lon_min < 13.0, "West edge should be ~12°");
        assert!(bounds.lon_max < 17.0, "East edge should be ~16°");
    }

    #[test]
    fn test_compute_prefetch_box_none_without_aircraft() {
        let bounds = compute_prefetch_box(&None);
        assert!(bounds.is_none());
    }

    #[test]
    fn test_empty_snapshot_serialises() {
        let snapshot = DebugStateSnapshot::default();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"aircraft\":null"));
        assert!(json.contains("\"regions\":[]"));
    }
}
