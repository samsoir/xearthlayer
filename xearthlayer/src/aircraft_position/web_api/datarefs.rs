//! X-Plane dataref constants, ID resolution, and value parsing.
//!
//! Datarefs are X-Plane's internal variables (position, speed, sim state, etc.).
//! The Web API exposes them by name via REST and by numeric ID via WebSocket.
//! IDs are memory addresses that change every X-Plane session, so we must
//! look them up by name on each connection.

use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Position datarefs
// ─────────────────────────────────────────────────────────────────────────────

/// Aircraft latitude in degrees (double).
pub const LATITUDE: &str = "sim/flightmodel/position/latitude";
/// Aircraft longitude in degrees (double).
pub const LONGITUDE: &str = "sim/flightmodel/position/longitude";
/// True heading — where the nose points, in degrees (float).
pub const TRUE_HEADING: &str = "sim/flightmodel/position/true_psi";
/// True track — ground path direction, in degrees (float).
pub const TRACK: &str = "sim/flightmodel/position/hpath";
/// Ground speed in **meters per second** (float). Convert to knots: × 1.94384.
pub const GROUND_SPEED: &str = "sim/flightmodel/position/groundspeed";
/// Elevation MSL in **meters** (double). Convert to feet: × 3.28084.
pub const ELEVATION: &str = "sim/flightmodel/position/elevation";
/// Height AGL in **meters** (float). Convert to feet: × 3.28084.
pub const AGL: &str = "sim/flightmodel/position/y_agl";

/// All position datarefs needed for `AircraftState`.
pub const ALL_POSITION_DATAREFS: &[&str] = &[
    LATITUDE,
    LONGITUDE,
    TRUE_HEADING,
    TRACK,
    GROUND_SPEED,
    ELEVATION,
    AGL,
];

// ─────────────────────────────────────────────────────────────────────────────
// Sim state datarefs
// ─────────────────────────────────────────────────────────────────────────────

/// Sim paused (int: 0 = running, 1 = paused).
pub const PAUSED: &str = "sim/time/paused";
/// Sim speed multiplier (int: 1 = normal).
pub const SIM_SPEED: &str = "sim/time/sim_speed";
/// Replay mode active (int: 0 = normal, 1 = replay).
pub const IS_REPLAY: &str = "sim/time/is_in_replay";
/// Any gear on ground (int: 0 = airborne, 1 = on ground).
pub const ON_GROUND: &str = "sim/flightmodel/failures/onground_any";
/// Async scenery loading in progress (int: 0 = idle, 1 = loading).
pub const SCENERY_LOADING: &str = "sim/graphics/scenery/async_scenery_load_in_progress";

/// All sim state datarefs needed for `SimState`.
pub const ALL_SIM_STATE_DATAREFS: &[&str] =
    &[PAUSED, SIM_SPEED, IS_REPLAY, ON_GROUND, SCENERY_LOADING];

/// All datarefs we subscribe to (position + sim state).
pub fn all_datarefs() -> Vec<&'static str> {
    let mut all = Vec::with_capacity(ALL_POSITION_DATAREFS.len() + ALL_SIM_STATE_DATAREFS.len());
    all.extend_from_slice(ALL_POSITION_DATAREFS);
    all.extend_from_slice(ALL_SIM_STATE_DATAREFS);
    all
}

// ─────────────────────────────────────────────────────────────────────────────
// ID resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Maps dataref name → numeric ID (for WebSocket subscription).
pub type DatarefIdMap = HashMap<String, u64>;

/// Maps numeric ID → dataref name (for parsing WebSocket updates).
pub type IdToNameMap = HashMap<u64, String>;

/// Build the reverse map (ID → name) from a DatarefIdMap.
pub fn invert_id_map(id_map: &DatarefIdMap) -> IdToNameMap {
    id_map.iter().map(|(k, v)| (*v, k.clone())).collect()
}

/// Resolve dataref IDs from the REST `/api/v3/datarefs` JSON response.
///
/// Returns a map of dataref name → ID for all requested names found
/// in the response. Names not found are silently skipped.
pub fn resolve_dataref_ids(json: &serde_json::Value, names: &[&str]) -> DatarefIdMap {
    let mut result = DatarefIdMap::with_capacity(names.len());
    let name_set: std::collections::HashSet<&str> = names.iter().copied().collect();

    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let (Some(name), Some(id)) = (
                entry.get("name").and_then(|n| n.as_str()),
                entry.get("id").and_then(|i| i.as_u64()),
            ) {
                if name_set.contains(name) {
                    result.insert(name.to_string(), id);
                }
            }
        }
    }

    result
}

/// Resolve dataref IDs, returning an error if any requested names are missing.
pub fn resolve_dataref_ids_checked(
    json: &serde_json::Value,
    names: &[&str],
) -> Result<DatarefIdMap, Vec<String>> {
    let resolved = resolve_dataref_ids(json, names);
    let missing: Vec<String> = names
        .iter()
        .filter(|name| !resolved.contains_key(**name))
        .map(|name| name.to_string())
        .collect();

    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(missing)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket message parsing
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a `dataref_update_values` WebSocket message into name → value pairs.
///
/// The WebSocket sends updates as `{"type": "dataref_update_values", "data": {"id": value}}`.
/// We use the `id_to_name` map to convert numeric IDs back to dataref names.
/// Unknown IDs are silently skipped.
pub fn parse_dataref_update(
    msg: &serde_json::Value,
    id_to_name: &IdToNameMap,
) -> HashMap<String, f64> {
    let mut result = HashMap::new();

    if msg.get("type").and_then(|t| t.as_str()) != Some("dataref_update_values") {
        return result;
    }

    if let Some(data) = msg.get("data").and_then(|d| d.as_object()) {
        for (id_str, value) in data {
            if let (Ok(id), Some(val)) = (id_str.parse::<u64>(), value.as_f64()) {
                if let Some(name) = id_to_name.get(&id) {
                    result.insert(name.clone(), val);
                }
            }
        }
    }

    result
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket subscription
// ─────────────────────────────────────────────────────────────────────────────

/// Build a WebSocket subscription message for the given dataref IDs.
///
/// Format: `{"req_id": N, "type": "dataref_subscribe_values", "params": {"datarefs": [{"id": X}, ...]}}`
pub fn build_subscribe_message(req_id: u64, ids: &[u64]) -> String {
    let datarefs: Vec<serde_json::Value> =
        ids.iter().map(|id| serde_json::json!({"id": id})).collect();

    serde_json::json!({
        "req_id": req_id,
        "type": "dataref_subscribe_values",
        "params": {
            "datarefs": datarefs
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_datarefs_count() {
        let all = all_datarefs();
        assert_eq!(all.len(), 12, "7 position + 5 sim state");
    }

    #[test]
    fn test_resolve_dataref_ids_from_json() {
        let json = serde_json::json!({
            "data": [
                {"id": 100, "name": "sim/flightmodel/position/latitude", "value_type": "double"},
                {"id": 200, "name": "sim/flightmodel/position/longitude", "value_type": "double"},
                {"id": 300, "name": "sim/other/dataref", "value_type": "float"},
            ]
        });

        let resolved = resolve_dataref_ids(&json, &[LATITUDE, LONGITUDE]);
        assert_eq!(resolved.get(LATITUDE), Some(&100u64));
        assert_eq!(resolved.get(LONGITUDE), Some(&200u64));
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn test_resolve_missing_datarefs_returns_error() {
        let json = serde_json::json!({
            "data": [
                {"id": 100, "name": "sim/flightmodel/position/latitude", "value_type": "double"},
            ]
        });

        let result = resolve_dataref_ids_checked(&json, &[LATITUDE, LONGITUDE]);
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], LONGITUDE);
    }

    #[test]
    fn test_resolve_empty_response() {
        let json = serde_json::json!({"data": []});
        let resolved = resolve_dataref_ids(&json, &[LATITUDE]);
        assert!(resolved.is_empty());
    }

    #[test]
    fn test_parse_dataref_update() {
        let msg = serde_json::json!({
            "type": "dataref_update_values",
            "data": { "100": 48.116, "200": 16.566 }
        });

        let id_to_name: IdToNameMap =
            HashMap::from([(100, LATITUDE.to_string()), (200, LONGITUDE.to_string())]);
        let values = parse_dataref_update(&msg, &id_to_name);
        assert!((values[LATITUDE] - 48.116).abs() < 0.001);
        assert!((values[LONGITUDE] - 16.566).abs() < 0.001);
    }

    #[test]
    fn test_parse_dataref_update_wrong_type() {
        let msg = serde_json::json!({
            "type": "some_other_message",
            "data": { "100": 48.116 }
        });

        let id_to_name: IdToNameMap = HashMap::from([(100, LATITUDE.to_string())]);
        let values = parse_dataref_update(&msg, &id_to_name);
        assert!(values.is_empty());
    }

    #[test]
    fn test_parse_dataref_update_unknown_ids_skipped() {
        let msg = serde_json::json!({
            "type": "dataref_update_values",
            "data": { "100": 48.116, "999": 42.0 }
        });

        let id_to_name: IdToNameMap = HashMap::from([(100, LATITUDE.to_string())]);
        let values = parse_dataref_update(&msg, &id_to_name);
        assert_eq!(values.len(), 1);
        assert!(values.contains_key(LATITUDE));
    }

    #[test]
    fn test_build_subscription_message() {
        let ids = vec![100u64, 200, 300];
        let msg = build_subscribe_message(1, &ids);
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "dataref_subscribe_values");
        assert_eq!(parsed["req_id"], 1);
        let datarefs = parsed["params"]["datarefs"].as_array().unwrap();
        assert_eq!(datarefs.len(), 3);
        assert_eq!(datarefs[0]["id"], 100);
        assert_eq!(datarefs[1]["id"], 200);
        assert_eq!(datarefs[2]["id"], 300);
    }

    #[test]
    fn test_invert_id_map() {
        let id_map: DatarefIdMap = HashMap::from([
            (LATITUDE.to_string(), 100u64),
            (LONGITUDE.to_string(), 200u64),
        ]);
        let inverted = invert_id_map(&id_map);
        assert_eq!(inverted.get(&100), Some(&LATITUDE.to_string()));
        assert_eq!(inverted.get(&200), Some(&LONGITUDE.to_string()));
    }
}
