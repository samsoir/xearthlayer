# X-Plane Web API Telemetry Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace XGPS2/ForeFlight UDP telemetry with X-Plane's built-in Web API, add direct sim state detection, and remove all heuristic inference code — resulting in less code, zero user configuration, and more reliable prefetch behavior. This is a v0.4.0 breaking change.

**Architecture:** A new `WebApiAdapter` connects to X-Plane's Web API via REST (dataref ID lookup) and WebSocket (10Hz position + sim state subscriptions). It feeds `AircraftState` into the existing `StateAggregator` via mpsc channel, following the same pattern as the existing `NetworkAdapter`. Sim state datarefs (`on_ground`, `scenery_loading`, `paused`, `replay`) enable direct detection that replaces the `CircuitBreaker`, `BurstDetector`, `FuseLoadMonitor`, and `PhaseDetector` ground speed heuristic. The XGPS2 `TelemetryReceiver` and all heuristic code are deleted.

**Tech Stack:** Rust, `tokio-tungstenite` (WebSocket), `reqwest` (REST, already in deps), `serde_json` (already in deps), existing `StateAggregator` infrastructure.

---

## Phases

The work is structured in four phases to ensure each commit leaves the codebase in a working state:

| Phase | Tasks | Description |
|-------|-------|-------------|
| **1. Build new** | 1-4 | Create WebApiClient, SimState, WebApiAdapter. Wire into service. XGPS2 coexists temporarily. |
| **2. Integrate** | 5 | Use SimState in prefetch coordinator. Replace CircuitBreaker throttling and PhaseDetector ground speed. |
| **3. Remove old** | 6-9 | Delete XGPS2, CircuitBreaker, FuseLoadMonitor, BurstDetector. Config cleanup. |
| **4. Docs** | 10 | Update all documentation. |

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `aircraft_position/web_api/mod.rs` | Module declarations, `WebApiAdapter` struct and lifecycle |
| `aircraft_position/web_api/client.rs` | `WebApiClient` — REST dataref lookup, WebSocket connection + subscription |
| `aircraft_position/web_api/datarefs.rs` | Dataref name constants, ID resolution, value parsing, subscription message building |
| `aircraft_position/web_api/config.rs` | `WebApiConfig` — port, reconnect interval |
| `aircraft_position/web_api/sim_state.rs` | `SimState` — parsed sim state (paused, on_ground, loading, replay) with prefetch decision logic |

### Modified Files

| File | Change |
|------|--------|
| `Cargo.toml` | Add `tokio-tungstenite` dependency |
| `aircraft_position/mod.rs` | Add `pub mod web_api`; later remove `mod telemetry` (Task 6) |
| `aircraft_position/state.rs` | Update `PositionAccuracy::TELEMETRY` doc comment |
| `aircraft_position/aggregator.rs` | Update doc comments |
| `aircraft_position/provider.rs` | Add `receive_sim_state()` passthrough on `SharedAircraftPosition` |
| `service/orchestrator/apt.rs` | Add `start_web_api_adapter()`; later remove `start_apt_telemetry()` (Task 6) |
| `service/orchestrator/mod.rs` | Call `start_web_api_adapter()` in startup sequence |
| `service/orchestrator_config.rs` | Add `web_api_port` field |
| `prefetch/adaptive/coordinator/core.rs` | Accept `SimState` for throttling; remove `throttler` field |
| `prefetch/adaptive/coordinator/runner.rs` | Pass `SimState` to coordinator update cycle |
| `prefetch/adaptive/phase_detector.rs` | Accept `on_ground: bool` instead of ground speed threshold |
| `prefetch/adaptive/config.rs` | Remove circuit breaker config fields |
| `prefetch/mod.rs` | Remove CircuitBreaker, FuseLoadMonitor exports |
| `prefetch/throttler.rs` | Remove or simplify (CircuitBreaker was sole consumer) |
| `scene_tracker/tracker.rs` | Remove BurstDetector, FuseLoadMonitor impl |
| `scene_tracker/mod.rs` | Remove burst exports, simplify SceneTracker trait |
| `aircraft_position/inference.rs` | Remove burst subscription (uses SceneTrackerEvents) |
| `config/keys.rs` | Remove deprecated keys, add WebApiPort |
| `config/settings.rs` | Remove deprecated fields, add `web_api_port` |
| `config/parser.rs` | Update INI parsing |
| `config/writer.rs` | Update INI writing |
| `config/defaults.rs` | Remove old constants, add `DEFAULT_WEB_API_PORT` |
| `config/upgrade.rs` | Add deprecated keys |

### Deleted Files

| File | Lines | Reason |
|------|-------|--------|
| `aircraft_position/telemetry/mod.rs` | ~256 | Replaced by `web_api/` |
| `aircraft_position/telemetry/protocol.rs` | ~453 | XGPS2/XATT2/DATA parsing no longer needed |
| `prefetch/circuit_breaker.rs` | ~200 | Replaced by `async_scenery_load_in_progress` dataref |
| `prefetch/load_monitor.rs` | ~183 | Only existed to support circuit breaker |
| `scene_tracker/burst.rs` | ~323 | Burst detection replaced by direct sim state |

**Net reduction:** ~1415 lines deleted, ~500-700 lines added = **~700-900 fewer lines of code**.

---

## Phase 1: Build New

### Task 1: Add `tokio-tungstenite` dependency and dataref utilities

**Files:**
- Modify: `xearthlayer/Cargo.toml`
- Create: `xearthlayer/src/aircraft_position/web_api/datarefs.rs`
- Create: `xearthlayer/src/aircraft_position/web_api/config.rs`
- Create: `xearthlayer/src/aircraft_position/web_api/mod.rs` (minimal, just module declarations)

Pure utility code — no integration with existing modules yet.

- [ ] **Step 1: Add `tokio-tungstenite` to Cargo.toml**

```toml
tokio-tungstenite = { version = "0.24", features = ["connect"] }
```

Verify it compiles: `cargo check -p xearthlayer`

- [ ] **Step 2: Create `web_api/mod.rs` with module declarations**

```rust
//! X-Plane Web API adapter for aircraft telemetry and sim state.
//!
//! Connects to X-Plane's built-in Web API (available since 12.1.1)
//! via REST for dataref ID lookup and WebSocket for 10Hz position
//! and sim state subscriptions.

pub mod client;
pub mod config;
pub mod datarefs;
pub mod sim_state;
```

- [ ] **Step 3: Add `pub mod web_api` to `aircraft_position/mod.rs`**

Add alongside existing `mod telemetry` (both coexist temporarily):
```rust
pub mod web_api;
```

- [ ] **Step 4: Write failing test — dataref ID lookup by name**

In `datarefs.rs`:
```rust
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
```

- [ ] **Step 5: Write failing test — parse WebSocket dataref update message**

```rust
#[test]
fn test_parse_dataref_update() {
    let msg = serde_json::json!({
        "type": "dataref_update_values",
        "data": { "100": 48.116, "200": 16.566 }
    });

    let id_to_name: HashMap<u64, String> = HashMap::from([
        (100, LATITUDE.to_string()),
        (200, LONGITUDE.to_string()),
    ]);
    let values = parse_dataref_update(&msg, &id_to_name);
    assert!((values[LATITUDE] - 48.116).abs() < 0.001);
    assert!((values[LONGITUDE] - 16.566).abs() < 0.001);
}
```

- [ ] **Step 6: Write failing test — WebSocket subscription message format**

```rust
#[test]
fn test_build_subscription_message() {
    let ids = vec![100u64, 200, 300];
    let msg = build_subscribe_message(1, &ids);
    let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(parsed["type"], "dataref_subscribe_values");
    assert_eq!(parsed["req_id"], 1);
    assert_eq!(parsed["params"]["datarefs"].as_array().unwrap().len(), 3);
    assert_eq!(parsed["params"]["datarefs"][0]["id"], 100);
}
```

- [ ] **Step 7: Write failing test — missing datarefs reported as error**

```rust
#[test]
fn test_resolve_missing_datarefs_returns_error() {
    let json = serde_json::json!({
        "data": [
            {"id": 100, "name": "sim/flightmodel/position/latitude", "value_type": "double"},
        ]
    });

    let result = resolve_dataref_ids_checked(&json, &[LATITUDE, LONGITUDE]);
    assert!(result.is_err(), "Should error when required datarefs are missing");
}
```

- [ ] **Step 8: Implement `datarefs.rs`**

Contents:
- Dataref name constants (position: 7, sim state: 5 = 12 total)
- `ALL_POSITION_DATAREFS` and `ALL_SIM_STATE_DATAREFS` arrays
- `resolve_dataref_ids(json, names) -> HashMap<&str, u64>` — best-effort lookup
- `resolve_dataref_ids_checked(json, names) -> Result<HashMap<&str, u64>>` — error if any missing
- `parse_dataref_update(msg, id_to_name) -> HashMap<String, f64>` — extract values from WebSocket message
- `build_subscribe_message(req_id, ids) -> String` — JSON subscription message

- [ ] **Step 9: Implement `config.rs`**

```rust
#[derive(Debug, Clone)]
pub struct WebApiConfig {
    /// X-Plane Web API port (default 8086).
    pub port: u16,
    /// Reconnect interval on disconnect (default 5s).
    pub reconnect_interval: Duration,
    /// REST request timeout (default 10s).
    pub request_timeout: Duration,
}
```

- [ ] **Step 10: Run tests, verify all pass**

Run: `cargo test -p xearthlayer web_api -- --nocapture`

- [ ] **Step 11: Commit**

```bash
git commit -m "feat(apt): add X-Plane Web API dataref utilities and config (#79)"
```

---

### Task 2: Add `SimState` type

**Files:**
- Create: `xearthlayer/src/aircraft_position/web_api/sim_state.rs`

Pure data type — no integration with existing modules.

- [ ] **Step 1: Write failing test — SimState from dataref values**

```rust
#[test]
fn test_sim_state_from_dataref_values() {
    let mut values = HashMap::new();
    values.insert(PAUSED.to_string(), 0.0_f64);
    values.insert(ON_GROUND.to_string(), 1.0);
    values.insert(SCENERY_LOADING.to_string(), 0.0);
    values.insert(IS_REPLAY.to_string(), 0.0);
    values.insert(SIM_SPEED.to_string(), 1.0);

    let state = SimState::from_dataref_values(&values);
    assert!(!state.paused);
    assert!(state.on_ground);
    assert!(!state.scenery_loading);
    assert!(!state.replay);
    assert_eq!(state.sim_speed, 1);
}
```

- [ ] **Step 2: Write failing test — should_prefetch logic**

```rust
#[test]
fn test_should_prefetch() {
    let flying = SimState { paused: false, on_ground: false, scenery_loading: false, replay: false, sim_speed: 1 };
    assert!(flying.should_prefetch());

    // Scenery loading — should NOT prefetch (X-Plane is saturated)
    assert!(!SimState { scenery_loading: true, ..flying }.should_prefetch());

    // Replay — should NOT prefetch (no real flight)
    assert!(!SimState { replay: true, ..flying }.should_prefetch());

    // Paused — SHOULD prefetch (opportunistic, no contention)
    assert!(SimState { paused: true, ..flying }.should_prefetch());

    // On ground — SHOULD prefetch (ground strategy handles this)
    assert!(SimState { on_ground: true, ..flying }.should_prefetch());
}
```

- [ ] **Step 3: Write failing test — SimState default is safe (unknown = no prefetch inhibit)**

```rust
#[test]
fn test_sim_state_default() {
    let state = SimState::default();
    assert!(state.should_prefetch(), "Default state should allow prefetch");
    assert!(!state.on_ground, "Default assumes airborne (safe for prefetch)");
}
```

- [ ] **Step 4: Implement `SimState`**

```rust
/// X-Plane sim state from direct dataref observation.
///
/// Replaces heuristic detection (CircuitBreaker, BurstDetector,
/// PhaseDetector ground speed) with explicit sim state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimState {
    pub paused: bool,
    pub on_ground: bool,
    pub scenery_loading: bool,
    pub replay: bool,
    pub sim_speed: i32,
}

impl Default for SimState {
    fn default() -> Self {
        Self {
            paused: false,
            on_ground: false,
            scenery_loading: false,
            replay: false,
            sim_speed: 1,
        }
    }
}

impl SimState {
    pub fn from_dataref_values(values: &HashMap<String, f64>) -> Self { ... }

    /// Whether prefetch should run in this state.
    ///
    /// Blocked by: scenery loading (X-Plane saturated), replay (no real flight).
    /// Allowed during: pause (opportunistic), on ground (ground strategy).
    pub fn should_prefetch(&self) -> bool {
        !self.scenery_loading && !self.replay
    }
}
```

- [ ] **Step 5: Run tests, verify pass**

- [ ] **Step 6: Commit**

```bash
git commit -m "feat(apt): add SimState type for direct X-Plane state detection (#79)"
```

---

### Task 3: Implement `WebApiClient`

**Files:**
- Create: `xearthlayer/src/aircraft_position/web_api/client.rs`

Handles REST and WebSocket communication. No adapter lifecycle yet — just the client protocol.

- [ ] **Step 1: Write failing test — client constructs correct REST URL**

```rust
#[test]
fn test_rest_url() {
    let config = WebApiConfig { port: 8086, ..Default::default() };
    let client = WebApiClient::new(config);
    assert_eq!(client.datarefs_url(), "http://localhost:8086/api/v3/datarefs");
}
```

- [ ] **Step 2: Write failing test — client constructs correct WebSocket URL**

```rust
#[test]
fn test_websocket_url() {
    let config = WebApiConfig { port: 9090, ..Default::default() };
    let client = WebApiClient::new(config);
    assert_eq!(client.websocket_url(), "ws://localhost:9090/api/v3");
}
```

- [ ] **Step 3: Write failing test — parse_position builds AircraftState with unit conversions**

```rust
#[test]
fn test_parse_position_with_unit_conversions() {
    let mut values = HashMap::new();
    values.insert(LATITUDE.to_string(), 48.116);
    values.insert(LONGITUDE.to_string(), 16.566);
    values.insert(TRUE_HEADING.to_string(), 205.6);
    values.insert(TRACK.to_string(), 210.0);
    values.insert(GROUND_SPEED.to_string(), 154.3);  // m/s
    values.insert(ELEVATION.to_string(), 10668.0);     // meters MSL
    values.insert(AGL.to_string(), 10650.0);           // meters AGL

    let state = parse_position(&values);
    assert!(state.is_some());
    let state = state.unwrap();

    assert!((state.latitude - 48.116).abs() < 0.001);
    assert!((state.longitude - 16.566).abs() < 0.001);
    assert!((state.heading - 205.6).abs() < 0.1);
    // Ground speed: 154.3 m/s × 1.94384 = ~299.8 kt
    assert!((state.ground_speed - 299.8).abs() < 1.0);
    // Elevation: 10668 m × 3.28084 = ~34,996 ft
    assert!((state.altitude - 34996.0).abs() < 10.0);
}
```

- [ ] **Step 4: Write failing test — parse_position returns None when required fields missing**

```rust
#[test]
fn test_parse_position_incomplete() {
    let mut values = HashMap::new();
    values.insert(LATITUDE.to_string(), 48.116);
    // Missing longitude, heading, etc.
    assert!(parse_position(&values).is_none());
}
```

- [ ] **Step 5: Implement `WebApiClient`**

```rust
pub struct WebApiClient {
    config: WebApiConfig,
    http_client: reqwest::Client,
}

impl WebApiClient {
    pub fn new(config: WebApiConfig) -> Self { ... }
    pub fn datarefs_url(&self) -> String { ... }
    pub fn websocket_url(&self) -> String { ... }

    /// Fetch all datarefs and resolve IDs for the requested names.
    pub async fn lookup_datarefs(&self, names: &[&str]) -> Result<DatarefMap, WebApiError> { ... }

    /// Connect to WebSocket and subscribe to the given dataref IDs.
    pub async fn connect_and_subscribe(&self, ids: &[u64]) -> Result<WebSocketStream, WebApiError> { ... }
}

/// Convert raw dataref values to an AircraftState with unit conversions.
pub fn parse_position(values: &HashMap<String, f64>) -> Option<AircraftState> { ... }
```

Unit conversions in `parse_position`:
- `groundspeed`: m/s × 1.94384 → knots
- `elevation`: m × 3.28084 → feet
- `y_agl`: m × 3.28084 → feet

- [ ] **Step 6: Run tests, verify pass**

- [ ] **Step 7: Commit**

```bash
git commit -m "feat(apt): add WebApiClient with REST lookup and position parsing (#79)"
```

---

### Task 4: Implement `WebApiAdapter` and wire into service

**Files:**
- Modify: `xearthlayer/src/aircraft_position/web_api/mod.rs`
- Modify: `xearthlayer/src/service/orchestrator/apt.rs`
- Modify: `xearthlayer/src/service/orchestrator/mod.rs`
- Modify: `xearthlayer/src/service/orchestrator_config.rs`

The adapter runs as an async task with automatic reconnection. It sends position updates and sim state via separate mpsc channels. Both XGPS2 and Web API coexist after this task — XGPS2 is removed in Task 6.

### Adapter Lifecycle

```
WebApiAdapter::start() spawns async task
    ↓
Loop (with reconnection):
    ├─ REST: lookup_datarefs() → resolve IDs
    ├─ WebSocket: connect_and_subscribe()
    └─ Message loop (10Hz):
        ├─ Parse position → AircraftState → position_tx
        └─ Parse sim state → SimState → sim_state_tx
    ↓
On disconnect: sleep(reconnect_interval), retry
On cancellation: exit
```

- [ ] **Step 1: Write failing test — adapter sends AircraftState on position update**

Use a mock WebSocket or test the internal `process_message()` method that converts a raw message into channel sends.

- [ ] **Step 2: Write failing test — adapter sends SimState on sim state update**

- [ ] **Step 3: Implement `WebApiAdapter`**

```rust
pub struct WebApiAdapter {
    client: WebApiClient,
    position_tx: mpsc::Sender<AircraftState>,
    sim_state_tx: mpsc::Sender<SimState>,
    config: WebApiConfig,
}

impl WebApiAdapter {
    pub fn new(
        config: WebApiConfig,
        position_tx: mpsc::Sender<AircraftState>,
        sim_state_tx: mpsc::Sender<SimState>,
    ) -> Self { ... }

    /// Start the adapter. Runs until cancelled.
    /// Reconnects automatically on disconnect.
    pub async fn run(&self, cancellation: CancellationToken) { ... }
}
```

- [ ] **Step 4: Add `SimState` channel to `ServiceOrchestrator`**

The orchestrator needs to store the `SimState` receiver so the prefetch coordinator can read it. Add a `SharedSimState` (Arc<RwLock<SimState>>) field that the adapter writes to and the coordinator reads from.

- [ ] **Step 5: Add `start_web_api_adapter()` to `apt.rs`**

Follow the existing `start_apt_telemetry()` pattern:
1. Create channels for position and sim state
2. Create `WebApiAdapter`
3. Spawn adapter task with cancellation
4. Bridge position channel to `aircraft_position.receive_telemetry()`
5. Bridge sim state channel to `SharedSimState`
6. Spawn position logger (existing)

- [ ] **Step 6: Add `web_api_port` to `OrchestratorConfig`**

Wire from `PrefetchSettings` or a new `WebApiSettings` section. Default 8086.

- [ ] **Step 7: Call `start_web_api_adapter()` in startup sequence**

In `service/orchestrator/mod.rs`, add after `start_apt_telemetry()`:
```rust
if let Err(e) = self.start_web_api_adapter() {
    tracing::warn!("Failed to start Web API adapter: {}", e);
}
```

Both sources coexist — `StateAggregator` selects the best based on accuracy/freshness.

- [ ] **Step 8: Run full test suite**

- [ ] **Step 9: Commit**

```bash
git commit -m "feat(service): add WebApiAdapter with position and sim state channels (#79)

Coexists with XGPS2 temporarily — both feed into StateAggregator.
Web API is preferred (no user configuration needed)."
```

---

## Phase 2: Integrate

### Task 5: Use `SimState` in prefetch coordinator

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/runner.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/phase_detector.rs`
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs`

Replace circuit breaker throttling with `SimState`-based decisions. Update `PhaseDetector` to accept direct on-ground signal.

- [ ] **Step 1: Write failing test — coordinator skips cycle when scenery loading**

```rust
#[test]
fn test_coordinator_skips_when_scenery_loading() {
    let mut coord = AdaptivePrefetchCoordinator::with_defaults()
        .with_calibration(test_calibration());

    let loading = SimState { scenery_loading: true, ..SimState::default() };
    coord.set_sim_state(loading);

    let plan = coord.update((48.0, 15.0), 270.0, 200.0, 35000.0);
    assert!(plan.is_none(), "Should skip prefetch when scenery loading");
}
```

- [ ] **Step 2: Write failing test — coordinator skips during replay**

- [ ] **Step 3: Write failing test — coordinator continues when paused**

- [ ] **Step 4: Write failing test — PhaseDetector uses on_ground signal**

```rust
#[test]
fn test_phase_detector_uses_on_ground() {
    let config = AdaptivePrefetchConfig::default();
    let mut detector = PhaseDetector::new(&config);

    // On ground with high speed (e.g. takeoff roll) — should be Ground
    // because on_ground overrides speed
    let phase = detector.detect(200.0, 0.0, true); // gs=200kt, on_ground=true
    assert_eq!(phase, FlightPhase::Ground);

    // Airborne with low speed (e.g. slow flight) — should NOT be Ground
    let phase = detector.detect(30.0, 5000.0, false); // gs=30kt, on_ground=false
    assert_eq!(phase, FlightPhase::Cruise);
}
```

- [ ] **Step 5: Add `SimState` to `AdaptivePrefetchCoordinator`**

```rust
// In the coordinator struct:
sim_state: SimState,

// New method:
pub fn set_sim_state(&mut self, state: SimState) {
    self.sim_state = state;
}
```

At the top of `update()`, before strategy selection:
```rust
if !self.sim_state.should_prefetch() {
    return None;
}
```

- [ ] **Step 6: Update `PhaseDetector::detect()` signature**

Change from:
```rust
pub fn detect(&mut self, ground_speed_kt: f32, altitude_msl: f32) -> FlightPhase
```
To:
```rust
pub fn detect(&mut self, ground_speed_kt: f32, altitude_msl: f32, on_ground: bool) -> FlightPhase
```

Use `on_ground` as the primary ground/cruise signal. Keep `ground_speed_threshold_kt` as a fallback when Web API is unavailable (SimState defaults to `on_ground: false`).

- [ ] **Step 7: Update coordinator `update()` to pass `sim_state.on_ground` to PhaseDetector**

- [ ] **Step 8: Wire `SharedSimState` into prefetch runner loop**

The runner needs to read `SharedSimState` each tick and call `coordinator.set_sim_state()`.

- [ ] **Step 9: Remove `throttler` field from coordinator**

The `throttler: Option<Arc<dyn PrefetchThrottler>>` was the CircuitBreaker. SimState replaces it. Remove the field, the `with_throttler()` builder, and the `should_throttle()` check.

- [ ] **Step 10: Run tests, fix any failures**

Existing tests that use `AlwaysThrottle`/`NeverThrottle` mocks need updating.

- [ ] **Step 11: Commit**

```bash
git commit -m "feat(prefetch): use SimState for throttling and phase detection (#79)

scenery_loading replaces CircuitBreaker, on_ground replaces PhaseDetector
ground speed threshold. Paused state allows opportunistic prefetch."
```

---

## Phase 3: Remove Old

### Task 6: Delete XGPS2/TelemetryReceiver

**Files:**
- Delete: `xearthlayer/src/aircraft_position/telemetry/mod.rs` (~256 lines)
- Delete: `xearthlayer/src/aircraft_position/telemetry/protocol.rs` (~453 lines)
- Modify: `xearthlayer/src/aircraft_position/mod.rs`
- Modify: `xearthlayer/src/service/orchestrator/apt.rs`
- Modify: `xearthlayer/src/service/orchestrator/mod.rs`

- [ ] **Step 1: Remove `start_apt_telemetry()` from `apt.rs`**

Delete the entire method. The `start_web_api_adapter()` (added in Task 4) now handles telemetry.

- [ ] **Step 2: Remove `start_apt_telemetry()` call from `mod.rs` startup sequence**

- [ ] **Step 3: Delete `aircraft_position/telemetry/` directory**

```bash
rm -r xearthlayer/src/aircraft_position/telemetry/
```

- [ ] **Step 4: Remove from `aircraft_position/mod.rs`**

Remove:
```rust
mod telemetry;
pub use telemetry::{TelemetryError, TelemetryReceiver, TelemetryReceiverConfig};
```

- [ ] **Step 5: Fix compilation errors**

Search for remaining references to `TelemetryReceiver`, `TelemetryReceiverConfig`, `TelemetryError` across the codebase and remove them.

- [ ] **Step 6: Update integration tests**

`tests/aircraft_position_integration.rs` likely references `TelemetryReceiver`. Remove or update those tests.

- [ ] **Step 7: Run tests, verify pass**

- [ ] **Step 8: Commit**

```bash
git commit -m "refactor(apt): remove XGPS2/ForeFlight UDP telemetry (#79)

Replaced by WebApiAdapter which connects directly to X-Plane's
built-in Web API. No user configuration required.

Deleted: aircraft_position/telemetry/ (~709 lines)"
```

---

### Task 7: Delete CircuitBreaker, FuseLoadMonitor, and PrefetchThrottler

**Files:**
- Delete: `xearthlayer/src/prefetch/circuit_breaker.rs` (~200 lines)
- Delete: `xearthlayer/src/prefetch/load_monitor.rs` (~183 lines)
- Modify: `xearthlayer/src/prefetch/mod.rs`
- Modify: `xearthlayer/src/prefetch/throttler.rs`
- Modify: `xearthlayer/src/scene_tracker/tracker.rs`
- Modify: `xearthlayer/src/service/facade.rs`
- Modify: `xearthlayer/src/fuse/fuse3/ortho_union_fs.rs` (if it records FUSE activity)

- [ ] **Step 1: Delete `circuit_breaker.rs` and `load_monitor.rs`**

- [ ] **Step 2: Remove exports from `prefetch/mod.rs`**

Remove:
```rust
mod circuit_breaker;
mod load_monitor;
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState, RESOURCE_SATURATION_THRESHOLD};
pub use load_monitor::{FuseLoadMonitor, SharedFuseLoadMonitor};
```

- [ ] **Step 3: Remove `FuseLoadMonitor` impl from `scene_tracker/tracker.rs`**

`DefaultSceneTracker` implements `FuseLoadMonitor`. Remove the impl block and the `use crate::prefetch::FuseLoadMonitor` import.

- [ ] **Step 4: Clean up `throttler.rs`**

Check if `PrefetchThrottler` trait has any remaining consumers after Task 5 removed it from the coordinator. If only test mocks (`AlwaysThrottle`, `NeverThrottle`) remain, delete the entire file and remove `mod throttler` from `prefetch/mod.rs`. If the trait is used elsewhere, keep it but remove CircuitBreaker references.

- [ ] **Step 5: Fix compilation errors**

Key locations to check:
- `service/facade.rs` — likely references `FuseLoadMonitor` or `SharedFuseLoadMonitor`
- `service/builder.rs` — may construct CircuitBreaker
- `fuse/fuse3/ortho_union_fs.rs` — may call load monitor methods
- `prefetch/adaptive/coordinator/` — any remaining throttler references

- [ ] **Step 6: Run tests, verify pass**

- [ ] **Step 7: Commit**

```bash
git commit -m "refactor(prefetch): remove CircuitBreaker and FuseLoadMonitor (#79, #59)

Replaced by direct sim state detection via X-Plane Web API dataref
async_scenery_load_in_progress. Resolves circuit breaker self-tripping (#59).

Deleted: circuit_breaker.rs (~200 lines), load_monitor.rs (~183 lines)"
```

---

### Task 8: Delete BurstDetector, simplify SceneTracker trait, clean up InferenceAdapter

**Files:**
- Delete: `xearthlayer/src/scene_tracker/burst.rs` (~323 lines)
- Modify: `xearthlayer/src/scene_tracker/mod.rs`
- Modify: `xearthlayer/src/scene_tracker/tracker.rs`
- Modify: `xearthlayer/src/scene_tracker/model.rs` (if `LoadingBurst` is removed)
- Modify: `xearthlayer/src/aircraft_position/inference.rs`

**Critical dependency:** `InferenceAdapter` subscribes to burst events via `SceneTrackerEvents::subscribe_bursts()`. Removing bursts breaks inference unless we update it first.

- [ ] **Step 1: Update `InferenceAdapter` to remove burst subscription**

The inference adapter currently triggers on burst completion events. Change it to use only the fallback timer (every 30s). Burst events were used to trigger faster inference when X-Plane loaded new scenery, but with the Web API providing position directly, inference is a low-priority fallback.

- [ ] **Step 2: Remove burst methods from `SceneTracker` trait**

Remove from `SceneTracker`:
```rust
fn is_burst_active(&self) -> bool;
fn current_burst_tiles(&self) -> Vec<DdsTileCoord>;
```

Remove from `SceneTrackerEvents`:
```rust
fn subscribe_bursts(&self) -> broadcast::Receiver<LoadingBurst>;
```

- [ ] **Step 3: Delete `burst.rs`**

- [ ] **Step 4: Remove `BurstDetector` from `DefaultSceneTracker`**

Remove the `burst_detector` field, `BurstConfig` from constructor, and all calls to `detector.record_tile()` and `detector.check_burst_complete()`.

- [ ] **Step 5: Remove `LoadingBurst` from `model.rs` if no longer used**

Check all references to `LoadingBurst`. If only burst.rs and inference.rs used it, and both are updated, remove it from model.rs and mod.rs exports.

- [ ] **Step 6: Update `scene_tracker/mod.rs` exports**

Remove:
```rust
pub use burst::{BurstConfig, BurstDetector};
```

Remove `LoadingBurst` from model exports if deleted.

- [ ] **Step 7: Fix compilation errors — mock SceneTrackers in tests**

Many test files have mock `SceneTracker` impls that include `is_burst_active()` and `current_burst_tiles()`. Remove these methods from all mocks:
- `prefetch/adaptive/scenery_window.rs` (MockSceneTracker)
- `prefetch/adaptive/coordinator/test_support.rs` (DummyTracker, StableBoundsTracker)
- Integration tests

- [ ] **Step 8: Run tests, verify pass**

- [ ] **Step 9: Commit**

```bash
git commit -m "refactor(scene_tracker): remove BurstDetector, simplify SceneTracker trait (#79)

Burst detection replaced by direct sim state from X-Plane Web API.
InferenceAdapter now uses timer-only fallback.

Deleted: burst.rs (~323 lines), removed 3 trait methods"
```

---

### Task 9: Configuration cleanup

**Files:**
- Modify: `xearthlayer/src/config/keys.rs`
- Modify: `xearthlayer/src/config/settings.rs`
- Modify: `xearthlayer/src/config/parser.rs`
- Modify: `xearthlayer/src/config/writer.rs`
- Modify: `xearthlayer/src/config/defaults.rs`
- Modify: `xearthlayer/src/config/upgrade.rs`
- Modify: `xearthlayer/src/service/orchestrator_config.rs`

- [ ] **Step 1: Add deprecated keys to `upgrade.rs`**

```rust
"prefetch.udp_port",
"prefetch.circuit_breaker_open_ms",
"prefetch.circuit_breaker_half_open_secs",
```

- [ ] **Step 2: Remove from `keys.rs`** — `PrefetchUdpPort`, `PrefetchCircuitBreakerOpenMs`, `PrefetchCircuitBreakerHalfOpenSecs` enum variants and all match arms

- [ ] **Step 3: Remove from `settings.rs`** — `udp_port`, `circuit_breaker_open_ms`, `circuit_breaker_half_open_secs` fields from `PrefetchSettings`

- [ ] **Step 4: Remove from `parser.rs`** — INI parsing blocks

- [ ] **Step 5: Remove from `writer.rs`** — template strings and value substitutions

- [ ] **Step 6: Remove from `defaults.rs`** — `DEFAULT_PREFETCH_UDP_PORT`, `DEFAULT_CIRCUIT_BREAKER_OPEN_MS`, `DEFAULT_CIRCUIT_BREAKER_HALF_OPEN_SECS`

- [ ] **Step 7: Add `web_api_port` setting** (if not already added in Task 4)

Add to `settings.rs`, `keys.rs`, `parser.rs`, `writer.rs`, `defaults.rs` with default 8086. Add to `orchestrator_config.rs` passthrough.

- [ ] **Step 8: Remove circuit breaker fields from `AdaptivePrefetchConfig`**

Remove `circuit_breaker_open_ms`, `circuit_breaker_half_open_secs` if still present after Task 5.

- [ ] **Step 9: Run tests, verify pass**

- [ ] **Step 10: Commit**

```bash
git commit -m "refactor(config): remove deprecated settings, add web_api_port (#79)"
```

---

## Phase 4: Docs

### Task 10: Documentation updates

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/configuration.md`
- Modify: `docs/dev/adaptive-prefetch-design.md`
- Modify: `README.md`

**Note:** Version bump to 0.4.0 deferred to release branch creation.

- [ ] **Step 1: Update `CLAUDE.md`**

- Remove TelemetryReceiver/XGPS2 from architecture section
- Add WebApiAdapter to Aircraft Position & Telemetry section
- Remove CircuitBreaker, BurstDetector, FuseLoadMonitor from Predictive Tile Caching
- Add SimState to prefetch description
- Update key files table: remove telemetry/, circuit_breaker.rs, burst.rs; add web_api/
- Remove `prefetch.udp_port` from config section
- Update threading model if affected

- [ ] **Step 2: Update `docs/configuration.md`**

- Remove `udp_port` and circuit breaker settings from prefetch table
- Remove ForeFlight/XGPS2 setup instructions
- Add `web_api_port` setting
- Update example config files (3 locations: table, example section, complete config)

- [ ] **Step 3: Update `docs/dev/adaptive-prefetch-design.md`**

- Remove circuit breaker section
- Update telemetry source: Web API instead of UDP
- Document SimState integration
- Update architecture diagram

- [ ] **Step 4: Update `README.md`**

- Remove ForeFlight setup instructions from "Quick Start" or "Setup"
- Add note: "XEarthLayer connects to X-Plane automatically via its built-in Web API"
- Note minimum X-Plane version: 12.1.1

- [ ] **Step 5: Run `make pre-commit`**

- [ ] **Step 6: Commit**

```bash
git commit -m "docs: update documentation for Web API telemetry (#79)"
```

---

## Task Dependency Graph

```
Phase 1 (Build):
  Task 1 (datarefs+config) ─┐
  Task 2 (SimState)         ─┼─→ Task 3 (WebApiClient) ─→ Task 4 (Wire adapter)
                              │
Phase 2 (Integrate):          │
  Task 4 ─→ Task 5 (SimState in coordinator)
                              │
Phase 3 (Remove):             ↓
  Task 4 ─→ Task 6 (Delete XGPS2)
  Task 5 ─→ Task 7 (Delete CircuitBreaker+FuseLoadMonitor)
  Task 7 ─→ Task 8 (Delete BurstDetector+SceneTracker cleanup)
  Tasks 6,7,8 ─→ Task 9 (Config cleanup)
                              │
Phase 4 (Docs):               ↓
  Task 9 ─→ Task 10 (Documentation)
```

Tasks 1 and 2 can be done in parallel.
Tasks 6, 7, 8 can be done in any order after their respective dependencies, but the plan orders them for minimal cascading compilation errors.

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| X-Plane Web API not running (pre-12.1.1, disabled) | Graceful degradation: adapter logs warning, falls back to inference/network sources |
| WebSocket disconnects during flight | Auto-reconnect with exponential backoff; position persists in model during gap |
| Dataref IDs change per session | Lookup by name on each connect/reconnect, not cached across sessions |
| Removing XGPS2 breaks users on X-Plane < 12.1.1 | v0.4.0 semver — breaking change documented in release notes |
| `async_scenery_load_in_progress` not reliable | Fallback: executor backpressure (PR #57) still provides resource-based throttling |
| Large scope — 10 tasks | Tasks are independently committable; each leaves the codebase in a working state |
| Test count decrease from deleted code | Expected — removed ~1100 lines of heuristic code. New tests for Web API replace them |
| `PrefetchThrottler` trait may have consumers beyond CircuitBreaker | Check all impls before deleting; keep trait if test mocks still need it |
| InferenceAdapter burst subscription | Updated in Task 8 before BurstDetector deletion |
