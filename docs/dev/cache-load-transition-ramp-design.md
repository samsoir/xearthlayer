# Cache Load Transition Ramp Design

**Goal:** Redesign flight phase transitions to ensure X-Plane has maximum system resources during critical phases (takeoff, landing) while allowing prefetch to operate efficiently during stable flight.

**Architecture:** Three-phase model (Ground / Transition / Cruise) with an observation-based hold during takeoff that releases when the aircraft is demonstrably airborne, replacing the current fixed-timer approach.

**Tech Stack:** Rust, existing prefetch framework (`AdaptivePrefetchCoordinator`, `PhaseDetector`, `TransitionThrottle`)

---

## Problem Summary

### Current Behavior

The `TransitionThrottle` fires at Ground→Cruise (GS ≥ 40kt) with a fixed 45-second grace period followed by a 30-second ramp. Flight testing (LSGG → EGCC, 2026-02-26) revealed:

1. **Trigger fires too early**: GS ≥ 40kt occurs during the takeoff roll (still on the runway). The throttle blocks ALL prefetch while the aircraft is accelerating on the ground — a phase where ground prefetch should still be running.

2. **Grace period is arbitrary**: The 45-second timer doesn't adapt to aircraft type. A 737 is at 1000ft AGL within 25-30 seconds of V1, while a C172 at a mountain strip may take 90+ seconds. The fixed timer is too long for fast jets and possibly too short for slow GA aircraft.

3. **Cold start gap**: When the grace period ends and the ramp begins, the aircraft is already in cruise needing tiles that were never prefetched. The 45s + 30s = 75 seconds of reduced/zero prefetch creates a cache gap that takes minutes to recover from.

4. **No AGL data**: The `PhaseDetector` receives `agl_ft = 0.0` because XGPS2/ForeFlight telemetry provides MSL altitude, not AGL. The phase detector effectively only uses ground speed for phase detection.

### Observed Impact

From the LSGG → EGCC flight test:
- TransitionThrottle fired at 20:56:27 (GS crossed 40kt on takeoff roll)
- Zero prefetch submissions for 45 seconds (grace period)
- By 21:01 (grace + ramp), prefetch resumed but was filling empty cache
- 714 on-demand tiles vs 98 prefetch tiles during the recovery period
- Per-tile latency spiked to 14-15 seconds during the catch-up phase

---

## Design

### Three-Phase Model

Replace the current two-phase model (Ground / Cruise) with three phases:

```
                    GS ≥ 40kt (sustained 2s)
    Ground ──────────────────────────────────► Transition (takeoff)
       ▲                                           │
       │     GS < 40kt (sustained 15s)             │ +1000ft MSL or 90s timeout
       └───────────────────────────────────── Cruise ◄──────────┘
```

```rust
pub enum FlightPhase {
    /// Aircraft is on the ground (taxi, parking, runway hold).
    /// Condition: GS < 40kt
    Ground,

    /// Aircraft is transitioning (takeoff detected, not yet established in flight).
    /// Condition: GS ≥ 40kt until MSL > baseline + 1000ft or 90s elapsed
    Transition,

    /// Aircraft is in stable flight (cruise, enroute, approach).
    /// Condition: Established after Transition release
    Cruise,
}
```

**Phase transitions:**

| From | To | Trigger | Notes |
|------|----|---------|-------|
| Ground | Transition | GS ≥ 40kt sustained 2s | Records current MSL as baseline |
| Transition | Cruise | MSL ≥ baseline + 1000ft OR 90s elapsed | Whichever comes first |
| Cruise | Ground | GS < 40kt sustained 15s | Longer hysteresis avoids false triggers during slow flight |
| Ground | Cruise | — | Not possible; must pass through Transition |

**Coordinator behavior per phase:**

| Phase | Prefetch Strategy | Throttle Fraction | Rationale |
|-------|------------------|-------------------|-----------|
| Ground | Ground (ring) | 1.0 (full rate) | No contention; fill nearby tiles |
| Transition | None | 0.0 (held) | Sterile cockpit; X-Plane gets all resources |
| Cruise (first 30s) | Cruise (band) | 0.25 → 1.0 (ramp) | Graduated return after transition |
| Cruise (steady) | Cruise (band) | 1.0 (full rate) | Normal operation |

### PhaseDetector Changes

The `PhaseDetector` gains:

1. **Transition phase**: New enum variant and detection logic.

2. **MSL tracking**: Records MSL altitude at Ground→Transition entry.

3. **Climb-based release**: Checks MSL delta on each update while in Transition.

4. **Simplified method signature**: `update()` takes ground speed and MSL only (AGL removed — not available from telemetry).

**Updated `update()` signature:**

```rust
pub fn update(&mut self, ground_speed_kt: f32, msl_ft: f32) -> bool
```

**Transition→Cruise release logic:**

```rust
// When current phase is Transition:
if let Some(baseline_msl) = self.transition_msl {
    let climbed = msl_ft - baseline_msl;
    let elapsed = self.phase_entered_at.elapsed();
    if climbed >= TAKEOFF_CLIMB_THRESHOLD_FT || elapsed >= TAKEOFF_TIMEOUT {
        // Release: Transition → Cruise
    }
}
```

**Landing detection:** Deferred to the scenery prefetch redesign. The asymmetry between takeoff (cold start, nothing cached) and landing (warm state, tiles pre-filled) means landing detection is a lower priority and is more naturally solved by a geospatially-aware prefetch system that knows what's cached in the destination area.

### TransitionThrottle Simplification

The `TransitionThrottle` simplifies from an internal-timer state machine to a phase-driven fraction calculator:

**Current design (timer-based):**
```
Ground→Cruise triggers GracePeriod(45s) → RampingUp(30s) → Idle
```

**New design (phase-driven):**
```
Phase = Transition  →  fraction() = 0.0
Phase = Cruise, just entered from Transition  →  fraction() ramps 0.25 → 1.0 over 30s
Phase = Cruise, ramp complete  →  fraction() = 1.0
Phase = Ground  →  fraction() = 1.0
```

The throttle no longer owns any timer logic. It receives the current phase on each tick and outputs the appropriate fraction. The "when to release the hold" decision lives entirely in the `PhaseDetector`.

**Interface:**

```rust
impl TransitionThrottle {
    pub fn new() -> Self;

    /// Notify the throttle of a phase change.
    /// Entering Transition: hold starts.
    /// Entering Cruise from Transition: ramp starts.
    /// Entering Ground: resets everything.
    pub fn on_phase_change(&mut self, from: FlightPhase, to: FlightPhase);

    /// Returns the current throughput fraction (0.0 to 1.0).
    /// Automatically progresses the ramp based on elapsed time.
    pub fn fraction(&mut self) -> f64;

    /// Returns true if the throttle is actively suppressing or ramping.
    pub fn is_active(&self) -> bool;
}
```

### Coordinator Integration

Changes to `process_telemetry()`:

```rust
// Current:
let phase_changed = self.phase_detector.update(ground_speed_kt, agl_ft);

// New:
let msl_ft = state.altitude; // MSL from XGPS2/ForeFlight telemetry
let phase_changed = self.phase_detector.update(ground_speed_kt, msl_ft);
```

The rest of the coordinator logic (strategy selection, throttle application, backpressure) remains the same. The three-phase model naturally slots into the existing `match phase {}` arms with `Transition` returning `None` (no prefetch plan).

### Configurable Parameters

The transition ramp parameters are user-configurable through the standard INI config pipeline, allowing end users to tune behavior for their aircraft and system.

**INI configuration** (`~/.xearthlayer/config.ini`):

```ini
[prefetch]
# Existing settings...
enabled = true
mode = auto

# Transition ramp settings (new)
takeoff_climb_ft = 1000
takeoff_timeout_secs = 90
landing_hysteresis_secs = 15
ramp_duration_secs = 30
ramp_start_fraction = 0.25
```

**Config pipeline additions:**

| INI Key | Type | Default | Range | Description |
|---------|------|---------|-------|-------------|
| `prefetch.takeoff_climb_ft` | f32 | 1000.0 | 200–5000 | Altitude climb (ft) above takeoff MSL that releases the transition hold |
| `prefetch.takeoff_timeout_secs` | u64 | 90 | 30–300 | Maximum seconds before timeout release if climb threshold not reached |
| `prefetch.landing_hysteresis_secs` | u64 | 15 | 5–60 | Sustained seconds at GS < 40kt before Cruise→Ground transition |
| `prefetch.ramp_duration_secs` | u64 | 30 | 10–120 | Duration of the linear ramp from `ramp_start_fraction` to 1.0 |
| `prefetch.ramp_start_fraction` | f64 | 0.25 | 0.1–0.5 | Starting prefetch fraction when the ramp begins |

**Files touched in config pipeline:**

| File | Change |
|------|--------|
| `config/settings.rs` | Add fields to `PrefetchSettings` struct |
| `config/defaults.rs` | Add `DEFAULT_TAKEOFF_CLIMB_FT`, `DEFAULT_TAKEOFF_TIMEOUT_SECS`, etc. |
| `config/parser.rs` | Parse new INI keys from `[prefetch]` section |
| `config/keys.rs` | Add `ConfigKey` variants with validation (ranges above) |
| `config/writer.rs` | Write new keys to INI output |
| `service/orchestrator_config.rs` | Add fields to `PrefetchConfig`, map from `PrefetchSettings` |
| `prefetch/adaptive/config.rs` | Add fields to `AdaptivePrefetchConfig`, map from `PrefetchConfig` |

**AdaptivePrefetchConfig fields:**

```rust
pub struct AdaptivePrefetchConfig {
    // ... existing fields ...

    /// Altitude climb (feet) above takeoff MSL that releases the transition hold.
    /// Range: 200–5000. Default: 1000.
    pub takeoff_climb_ft: f32,

    /// Maximum duration of the transition hold before timeout release.
    /// Range: 30–300s. Default: 90s.
    pub takeoff_timeout: Duration,

    /// Sustained duration for Cruise→Ground detection (landing deceleration).
    /// Range: 5–60s. Default: 15s.
    pub landing_hysteresis: Duration,

    /// Duration of the post-transition ramp from ramp_start_fraction to 1.0.
    /// Range: 10–120s. Default: 30s.
    pub ramp_duration: Duration,

    /// Starting prefetch fraction when the ramp begins.
    /// Range: 0.1–0.5. Default: 0.25.
    pub ramp_start_fraction: f64,
}
```

**Note:** The existing `from_prefetch_config()` and `from_prefetch_settings()` constructors must be updated to map these new fields explicitly — avoiding the `..Default::default()` pattern that silently drops unmapped fields (a known bug with `lead_distance` and `band_width`).

### Interaction with Existing Throttling

Three throttling mechanisms operate independently and stack multiplicatively:

1. **Circuit breaker** (resource pools > 90%): Blocks prefetch entirely when system is saturated. Unchanged.
2. **Executor backpressure** (50%/80% load): Reduces/defers submission. Unchanged.
3. **TransitionThrottle** (phase-driven): Holds during Transition, ramps on Cruise entry.

During a heavily loaded takeoff: `submitted = plan.tiles × backpressure_factor × throttle_fraction`

At worst during ramp-up with moderate load: `0.25 × 0.5 = 12.5%` effective submission. This is acceptable — by this point the aircraft is climbing and X-Plane's initial burst has settled.

---

## Paper Validation

### Boeing 737-800 Takeoff

| Time | Event | GS (kt) | MSL (ft) | Phase | Throttle |
|------|-------|---------|----------|-------|----------|
| T+0s | Start roll | 0 | 500 | Ground | 1.0 |
| T+10s | GS crosses 40kt | 40 | 500 | → Transition | 0.0 |
| T+18s | Rotate (V1~140kt) | 140 | 500 | Transition | 0.0 |
| T+30s | Climbing | 180 | 1200 | Transition | 0.0 |
| T+35s | MSL > 500 + 1000 = 1500 | 200 | 1500 | → Cruise | 0.25 |
| T+65s | Ramp complete | 250 | 4000 | Cruise | 1.0 |

**Total hold: ~25 seconds.** Much better than the current 45s fixed timer.

### Cessna 172 Takeoff (Mountain Strip, 6000ft)

| Time | Event | GS (kt) | MSL (ft) | Phase | Throttle |
|------|-------|---------|----------|-------|----------|
| T+0s | Start roll | 0 | 6000 | Ground | 1.0 |
| T+5s | GS crosses 40kt | 40 | 6000 | → Transition | 0.0 |
| T+15s | Rotate (~55kt) | 55 | 6000 | Transition | 0.0 |
| T+60s | Climbing slowly | 75 | 6800 | Transition | 0.0 |
| T+90s | Timeout (only +900ft) | 80 | 6900 | → Cruise (timeout) | 0.25 |
| T+120s | Ramp complete | 85 | 7200 | Cruise | 1.0 |

**Total hold: ~85 seconds** (timeout triggered). The C172 at a mountain strip climbs slowly, so the 90s timeout kicks in before +1000ft is reached. This is appropriate — the aircraft is still climbing slowly and nearby tiles are likely still in cache from ground prefetch.

### Helicopter Departure (Vertical Takeoff)

| Time | Event | GS (kt) | MSL (ft) | Phase | Throttle |
|------|-------|---------|----------|-------|----------|
| T+0s | Hover at pad | 0 | 500 | Ground | 1.0 |
| T+10s | Climbs vertically | 5 | 800 | Ground | 1.0 |
| T+30s | Still vertical, GS < 40 | 10 | 1400 | Ground | 1.0 |
| T+60s | Transition to forward flight | 50 | 1600 | → Transition | 0.0 |
| T+61s | MSL > 500 + 1000 = 1500 | 55 | 1600 | → Cruise (immediate) | 0.25 |

**Total hold: ~1 second.** The helicopter was already airborne when it crossed 40kt, so the +1000ft threshold is immediately met. Ground prefetch ran continuously during the vertical climb — correct behavior.

---

## Testing Strategy

### PhaseDetector Unit Tests

| Test | Scenario | Expected |
|------|----------|----------|
| `test_ground_to_transition_at_40kt` | GS ≥ 40kt sustained 2s | Phase = Transition |
| `test_transition_records_msl` | On entering Transition | `transition_msl` = current MSL |
| `test_transition_to_cruise_on_climb` | MSL exceeds baseline + 1000ft | Phase = Cruise |
| `test_transition_to_cruise_on_timeout` | 90s elapsed, MSL < baseline + 1000ft | Phase = Cruise |
| `test_no_direct_ground_to_cruise` | GS ≥ 40kt from Ground | Always goes through Transition |
| `test_cruise_to_ground_sustained` | GS < 40kt sustained 15s | Phase = Ground |
| `test_cruise_to_ground_no_false_trigger` | Brief GS dip then recovery | Stays Cruise |
| `test_helicopter_immediate_release` | Already high MSL when crossing 40kt | Transition→Cruise in < 2 updates |
| `test_transition_msl_none_uses_timeout` | No MSL data (0.0) when entering Transition | Falls back to 90s timeout |

### TransitionThrottle Unit Tests

| Test | Scenario | Expected |
|------|----------|----------|
| `test_transition_phase_returns_zero` | Phase is Transition | `fraction()` = 0.0 |
| `test_cruise_after_transition_ramps` | Just entered Cruise from Transition | `fraction()` starts at 0.25, reaches 1.0 after 30s |
| `test_ground_phase_returns_full` | Phase is Ground | `fraction()` = 1.0 |
| `test_cruise_steady_returns_full` | Cruise for > 30s (not from Transition) | `fraction()` = 1.0 |
| `test_ramp_resets_on_re_transition` | Cruise→Ground→Transition→Cruise | Ramp restarts from 0.25 |
| `test_ramp_linear_at_midpoint` | 15s into ramp | `fraction()` ≈ 0.625 |

### Coordinator Integration Tests

| Test | Scenario | Expected |
|------|----------|----------|
| `test_takeoff_holds_prefetch` | During Transition phase | Zero tiles submitted |
| `test_takeoff_ramp_after_climb` | After Transition→Cruise | Tiles submitted at reduced rate |
| `test_ground_prefetch_unaffected` | Ground strategy during Ground phase | Full rate, no throttle |
| `test_msl_passed_to_phase_detector` | `process_telemetry()` with altitude | MSL propagated to PhaseDetector |

### Configuration Pipeline Tests

| Test | Scenario | Expected |
|------|----------|----------|
| `test_config_key_takeoff_climb_ft` | Get/set via ConfigKey | Round-trips through INI correctly |
| `test_config_key_takeoff_timeout` | Get/set via ConfigKey | Round-trips through INI correctly |
| `test_config_key_validation_ranges` | Out-of-range values | Rejected by ConfigKey validation |
| `test_config_defaults` | Default config | All new fields have expected defaults |
| `test_from_prefetch_config_maps_ramp_settings` | PrefetchConfig → AdaptivePrefetchConfig | New fields propagated (not lost to `..Default::default()`) |
| `test_config_upgrade_adds_new_keys` | Existing INI without new keys | `config upgrade` adds them with defaults |

---

## Files Changed

### Prefetch System

| File | Change |
|------|--------|
| `prefetch/adaptive/phase_detector.rs` | Add `Transition` phase, MSL tracking, +1000ft/90s release, 15s landing hysteresis |
| `prefetch/adaptive/transition_throttle.rs` | Simplify to phase-driven fraction (remove internal timer logic) |
| `prefetch/adaptive/coordinator/core.rs` | Pass MSL to PhaseDetector, handle three-phase model in strategy selection |
| `prefetch/adaptive/config.rs` | Add configurable fields (`takeoff_climb_ft`, `takeoff_timeout`, `landing_hysteresis`, `ramp_duration`, `ramp_start_fraction`) |

### Configuration Pipeline

| File | Change |
|------|--------|
| `config/settings.rs` | Add fields to `PrefetchSettings` |
| `config/defaults.rs` | Add default constants for new settings |
| `config/parser.rs` | Parse new keys from `[prefetch]` INI section |
| `config/keys.rs` | Add `ConfigKey` variants with range validation |
| `config/writer.rs` | Write new keys to INI output |
| `service/orchestrator_config.rs` | Add fields to `PrefetchConfig`, map from settings |
| `docs/configuration.md` | Document new `[prefetch]` settings with descriptions and ranges |

### What Does NOT Change

- **Circuit breaker** — still monitors resource pool utilization
- **Executor backpressure** — still checks `executor_load()`
- **Band calculator / strategies** — geometry unchanged
- **BoundaryPrioritizer** — tile ordering unchanged
- **Four-tier filter** — caching logic unchanged
- **DdsClient / Executor** — submission mechanism unchanged

---

## Scope Boundaries

### In Scope
- Three-phase model (Ground / Transition / Cruise)
- Observation-based takeoff hold (+1000ft MSL / 90s timeout)
- TransitionThrottle simplification (phase-driven)
- PhaseDetector MSL tracking

### Explicitly Deferred
- **Landing/approach detection** — Deferred to scenery prefetch redesign. Landing is a warm-state problem (tiles should be pre-cached from cruise prefetch). The geospatially-aware prefetch system will naturally handle the edge case of landing near a DSF boundary.
- **Startup ramp** — Not needed. Ground strategy starts at full rate, and the circuit breaker handles resource contention during X-Plane's initial scene load.
- **Band geometry changes** — Separate concern addressed in the scenery prefetch redesign.

---

## Issues Addressed

- Partially closes #62 (Prefetch causes stutters during takeoff) — replaces the fixed-timer throttle with an observation-based hold
- Contributes to #58 (Prefetch refinements) — ensures correct phase handling for strategy selection
