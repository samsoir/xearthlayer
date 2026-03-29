# Cache Load Transition Ramp Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the fixed-timer TransitionThrottle with a three-phase model (Ground/Transition/Cruise) that uses observation-based takeoff detection (+1000ft MSL or 90s timeout), and wire 5 new configurable parameters through the full INI→service→adaptive config pipeline.

**Architecture:** The `PhaseDetector` gains a `Transition` phase with MSL tracking. The `TransitionThrottle` simplifies to a phase-driven fraction calculator (no internal timers). Five new user-configurable settings flow through `config/settings.rs` → `config/defaults.rs` → `config/parser.rs` → `config/keys.rs` → `config/writer.rs` → `service/orchestrator_config.rs` → `prefetch/adaptive/config.rs`.

**Tech Stack:** Rust, existing prefetch framework (`AdaptivePrefetchCoordinator`, `PhaseDetector`, `TransitionThrottle`), INI config pipeline

**Design document:** `docs/dev/cache-load-transition-ramp-design.md` (moved from `docs/plans/` in Task 1)

---

## Task 1: Move Design Document to docs/dev/

Promote the design document from planning to final speclet.

**Files:**
- Move: `docs/plans/2026-02-27-cache-load-transition-ramp-design.md` → `docs/dev/cache-load-transition-ramp-design.md`

**Step 1: Move the file**

```bash
git mv docs/plans/2026-02-27-cache-load-transition-ramp-design.md docs/dev/cache-load-transition-ramp-design.md
```

**Step 2: Commit**

```bash
git add docs/dev/cache-load-transition-ramp-design.md
git commit -m "docs(prefetch): move transition ramp design to docs/dev/ as final speclet"
```

---

## Task 2: Add RangeSpec Validators to Config Keys

The design requires range validation for the 5 new settings (e.g., `takeoff_climb_ft` must be 200–5000). The existing `keys.rs` has `PositiveIntegerSpec` and `PositiveNumberSpec` but no range validators.

**Files:**
- Modify: `xearthlayer/src/config/keys.rs`

**Step 1: Write failing tests for the new range specs**

Add to the `#[cfg(test)] mod tests` at the bottom of `keys.rs`:

```rust
#[test]
fn test_integer_range_spec() {
    let spec = IntegerRangeSpec::new(30, 300);
    assert!(spec.is_satisfied_by("30").is_ok());
    assert!(spec.is_satisfied_by("300").is_ok());
    assert!(spec.is_satisfied_by("90").is_ok());
    assert!(spec.is_satisfied_by("29").is_err());
    assert!(spec.is_satisfied_by("301").is_err());
    assert!(spec.is_satisfied_by("abc").is_err());
}

#[test]
fn test_float_range_spec() {
    let spec = FloatRangeSpec::new(0.1, 0.5);
    assert!(spec.is_satisfied_by("0.1").is_ok());
    assert!(spec.is_satisfied_by("0.5").is_ok());
    assert!(spec.is_satisfied_by("0.25").is_ok());
    assert!(spec.is_satisfied_by("0.09").is_err());
    assert!(spec.is_satisfied_by("0.51").is_err());
    assert!(spec.is_satisfied_by("abc").is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer config::keys::tests::test_integer_range_spec -- --no-capture`
Expected: FAIL — `IntegerRangeSpec` not found

**Step 3: Implement the range specs**

Add after `PositiveNumberSpec` implementation (around line 905 in `keys.rs`):

```rust
/// Specification for integer values within a range (inclusive).
struct IntegerRangeSpec {
    min: u64,
    max: u64,
}

impl IntegerRangeSpec {
    fn new(min: u64, max: u64) -> Self {
        Self { min, max }
    }
}

impl ValueSpecification for IntegerRangeSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        let n = value
            .parse::<u64>()
            .map_err(|_| format!("must be an integer between {} and {}", self.min, self.max))?;
        if n >= self.min && n <= self.max {
            Ok(())
        } else {
            Err(format!("must be between {} and {}", self.min, self.max))
        }
    }
}

/// Specification for floating-point values within a range (inclusive).
struct FloatRangeSpec {
    min: f64,
    max: f64,
}

impl FloatRangeSpec {
    fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }
}

impl ValueSpecification for FloatRangeSpec {
    fn is_satisfied_by(&self, value: &str) -> Result<(), String> {
        let n = value
            .parse::<f64>()
            .map_err(|_| format!("must be a number between {} and {}", self.min, self.max))?;
        if n >= self.min && n <= self.max {
            Ok(())
        } else {
            Err(format!("must be between {} and {}", self.min, self.max))
        }
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer config::keys::tests::test_integer_range_spec config::keys::tests::test_float_range_spec -- --no-capture`
Expected: PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/config/keys.rs
git commit -m "feat(config): add IntegerRangeSpec and FloatRangeSpec validators (#62)"
```

---

## Task 3: Add 5 New Settings to Config Pipeline

Wire the 5 new configurable parameters through the full config pipeline. This is one task because the config pipeline files are tightly coupled — changing one without the others causes compile errors.

**Files:**
- Modify: `xearthlayer/src/config/defaults.rs` (add DEFAULT_* constants)
- Modify: `xearthlayer/src/config/settings.rs` (add fields to `PrefetchSettings`)
- Modify: `xearthlayer/src/config/parser.rs` (parse from INI)
- Modify: `xearthlayer/src/config/keys.rs` (add ConfigKey variants + from_str + name + get + set + validate + all)
- Modify: `xearthlayer/src/config/writer.rs` (write to INI)
- Modify: `xearthlayer/src/service/orchestrator_config.rs` (add to `PrefetchConfig`, map from settings)
- Modify: `xearthlayer/src/prefetch/adaptive/config.rs` (add to `AdaptivePrefetchConfig`, map from PrefetchConfig)

**Step 1: Add default constants to `config/defaults.rs`**

Add after `DEFAULT_CALIBRATION_SAMPLE_DURATION` (line ~173):

```rust
/// Default takeoff climb threshold (feet above takeoff MSL).
pub const DEFAULT_TAKEOFF_CLIMB_FT: f32 = 1000.0;

/// Default takeoff timeout (seconds).
pub const DEFAULT_TAKEOFF_TIMEOUT_SECS: u64 = 90;

/// Default landing hysteresis (seconds at GS < 40kt before Cruise→Ground).
pub const DEFAULT_LANDING_HYSTERESIS_SECS: u64 = 15;

/// Default ramp duration (seconds for linear ramp from start fraction to 1.0).
pub const DEFAULT_RAMP_DURATION_SECS: u64 = 30;

/// Default ramp start fraction (initial prefetch fraction when ramp begins).
pub const DEFAULT_RAMP_START_FRACTION: f64 = 0.25;
```

Update `PrefetchSettings` initialization in `ConfigFile::default()` (line ~286-298) to add:

```rust
prefetch: PrefetchSettings {
    // ... existing fields ...
    calibration_sample_duration: DEFAULT_CALIBRATION_SAMPLE_DURATION,
    takeoff_climb_ft: DEFAULT_TAKEOFF_CLIMB_FT,
    takeoff_timeout_secs: DEFAULT_TAKEOFF_TIMEOUT_SECS,
    landing_hysteresis_secs: DEFAULT_LANDING_HYSTERESIS_SECS,
    ramp_duration_secs: DEFAULT_RAMP_DURATION_SECS,
    ramp_start_fraction: DEFAULT_RAMP_START_FRACTION,
},
```

**Step 2: Add fields to `PrefetchSettings` in `config/settings.rs`**

Add after `calibration_sample_duration` (line ~202):

```rust
    /// Altitude climb (feet) above takeoff MSL that releases the transition hold.
    /// Default: 1000.0. Range: 200–5000.
    pub takeoff_climb_ft: f32,
    /// Maximum seconds before timeout release if climb threshold not reached.
    /// Default: 90. Range: 30–300.
    pub takeoff_timeout_secs: u64,
    /// Sustained seconds at GS < 40kt before Cruise→Ground transition.
    /// Default: 15. Range: 5–60.
    pub landing_hysteresis_secs: u64,
    /// Duration (seconds) of the linear ramp from ramp_start_fraction to 1.0.
    /// Default: 30. Range: 10–120.
    pub ramp_duration_secs: u64,
    /// Starting prefetch fraction when the ramp begins.
    /// Default: 0.25. Range: 0.1–0.5.
    pub ramp_start_fraction: f64,
```

**Step 3: Add parsing to `config/parser.rs`**

Add after the `calibration_sample_duration` parsing block (line ~378), before the `}` closing `[prefetch]`:

```rust
        // Transition ramp settings
        if let Some(v) = section.get("takeoff_climb_ft") {
            config.prefetch.takeoff_climb_ft =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "takeoff_climb_ft".to_string(),
                    value: v.to_string(),
                    reason: "must be a number between 200 and 5000 (feet)".to_string(),
                })?;
        }
        if let Some(v) = section.get("takeoff_timeout_secs") {
            config.prefetch.takeoff_timeout_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "takeoff_timeout_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be an integer between 30 and 300 (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("landing_hysteresis_secs") {
            config.prefetch.landing_hysteresis_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "landing_hysteresis_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be an integer between 5 and 60 (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("ramp_duration_secs") {
            config.prefetch.ramp_duration_secs =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "ramp_duration_secs".to_string(),
                    value: v.to_string(),
                    reason: "must be an integer between 10 and 120 (seconds)".to_string(),
                })?;
        }
        if let Some(v) = section.get("ramp_start_fraction") {
            config.prefetch.ramp_start_fraction =
                v.parse().map_err(|_| ConfigFileError::InvalidValue {
                    section: "prefetch".to_string(),
                    key: "ramp_start_fraction".to_string(),
                    value: v.to_string(),
                    reason: "must be a number between 0.1 and 0.5".to_string(),
                })?;
        }
```

**Step 4: Add ConfigKey variants and wire all 7 match arms in `config/keys.rs`**

Add to the `ConfigKey` enum (after line ~88, after `PrefetchCalibrationSampleDuration`):

```rust
    // Prefetch transition ramp settings
    PrefetchTakeoffClimbFt,
    PrefetchTakeoffTimeoutSecs,
    PrefetchLandingHysteresisSecs,
    PrefetchRampDurationSecs,
    PrefetchRampStartFraction,
```

Add to `FromStr` impl (after line ~180, after `calibration_sample_duration` match):

```rust
            "prefetch.takeoff_climb_ft" => Ok(ConfigKey::PrefetchTakeoffClimbFt),
            "prefetch.takeoff_timeout_secs" => Ok(ConfigKey::PrefetchTakeoffTimeoutSecs),
            "prefetch.landing_hysteresis_secs" => Ok(ConfigKey::PrefetchLandingHysteresisSecs),
            "prefetch.ramp_duration_secs" => Ok(ConfigKey::PrefetchRampDurationSecs),
            "prefetch.ramp_start_fraction" => Ok(ConfigKey::PrefetchRampStartFraction),
```

Add to `name()` method (after line ~267, after `calibration_sample_duration` match):

```rust
            ConfigKey::PrefetchTakeoffClimbFt => "prefetch.takeoff_climb_ft",
            ConfigKey::PrefetchTakeoffTimeoutSecs => "prefetch.takeoff_timeout_secs",
            ConfigKey::PrefetchLandingHysteresisSecs => "prefetch.landing_hysteresis_secs",
            ConfigKey::PrefetchRampDurationSecs => "prefetch.ramp_duration_secs",
            ConfigKey::PrefetchRampStartFraction => "prefetch.ramp_start_fraction",
```

Add to `get()` method (after line ~398, after `calibration_sample_duration` match):

```rust
            ConfigKey::PrefetchTakeoffClimbFt => {
                config.prefetch.takeoff_climb_ft.to_string()
            }
            ConfigKey::PrefetchTakeoffTimeoutSecs => {
                config.prefetch.takeoff_timeout_secs.to_string()
            }
            ConfigKey::PrefetchLandingHysteresisSecs => {
                config.prefetch.landing_hysteresis_secs.to_string()
            }
            ConfigKey::PrefetchRampDurationSecs => {
                config.prefetch.ramp_duration_secs.to_string()
            }
            ConfigKey::PrefetchRampStartFraction => {
                config.prefetch.ramp_start_fraction.to_string()
            }
```

Add to `set_unchecked()` method (after line ~584, after `calibration_sample_duration` match):

```rust
            ConfigKey::PrefetchTakeoffClimbFt => {
                config.prefetch.takeoff_climb_ft = value.parse().unwrap();
            }
            ConfigKey::PrefetchTakeoffTimeoutSecs => {
                config.prefetch.takeoff_timeout_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchLandingHysteresisSecs => {
                config.prefetch.landing_hysteresis_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchRampDurationSecs => {
                config.prefetch.ramp_duration_secs = value.parse().unwrap();
            }
            ConfigKey::PrefetchRampStartFraction => {
                config.prefetch.ramp_start_fraction = value.parse().unwrap();
            }
```

Add to `specification()` method (after line ~721, after `calibration_sample_duration` match):

```rust
            ConfigKey::PrefetchTakeoffClimbFt => Box::new(IntegerRangeSpec::new(200, 5000)),
            ConfigKey::PrefetchTakeoffTimeoutSecs => Box::new(IntegerRangeSpec::new(30, 300)),
            ConfigKey::PrefetchLandingHysteresisSecs => Box::new(IntegerRangeSpec::new(5, 60)),
            ConfigKey::PrefetchRampDurationSecs => Box::new(IntegerRangeSpec::new(10, 120)),
            ConfigKey::PrefetchRampStartFraction => Box::new(FloatRangeSpec::new(0.1, 0.5)),
```

Add to `all()` list (after `PrefetchCalibrationSampleDuration` in the array):

```rust
            ConfigKey::PrefetchTakeoffClimbFt,
            ConfigKey::PrefetchTakeoffTimeoutSecs,
            ConfigKey::PrefetchLandingHysteresisSecs,
            ConfigKey::PrefetchRampDurationSecs,
            ConfigKey::PrefetchRampStartFraction,
```

**Step 5: Add to writer template in `config/writer.rs`**

Add after `calibration_sample_duration = {}` (line ~196), before `[control_plane]`:

```
; Transition ramp settings (takeoff phase management)
; Altitude climb (feet) above takeoff MSL to release transition hold (default: 1000, range: 200-5000)
takeoff_climb_ft = {}
; Maximum seconds before timeout release if climb not reached (default: 90, range: 30-300)
takeoff_timeout_secs = {}
; Sustained seconds at GS < 40kt before landing detection (default: 15, range: 5-60)
landing_hysteresis_secs = {}
; Duration (secs) of linear ramp from start fraction to full rate (default: 30, range: 10-120)
ramp_duration_secs = {}
; Starting prefetch fraction when ramp begins (default: 0.25, range: 0.1-0.5)
ramp_start_fraction = {}
```

Add the corresponding format args in the `format!()` call (after `calibration_sample_duration` arg, line ~294):

```rust
        config.prefetch.takeoff_climb_ft,
        config.prefetch.takeoff_timeout_secs,
        config.prefetch.landing_hysteresis_secs,
        config.prefetch.ramp_duration_secs,
        config.prefetch.ramp_start_fraction,
```

**Step 6: Add to `PrefetchConfig` in `service/orchestrator_config.rs`**

Add fields to `PrefetchConfig` struct (after `calibration_sample_duration` at line ~101):

```rust
    /// Takeoff climb threshold (feet above takeoff MSL).
    pub takeoff_climb_ft: f32,

    /// Takeoff timeout (seconds).
    pub takeoff_timeout_secs: u64,

    /// Landing hysteresis (seconds).
    pub landing_hysteresis_secs: u64,

    /// Ramp duration (seconds).
    pub ramp_duration_secs: u64,

    /// Ramp start fraction.
    pub ramp_start_fraction: f64,
```

Add mapping in `RuntimeConfig::from_config()` (after `calibration_sample_duration` at line ~189):

```rust
            takeoff_climb_ft: config.prefetch.takeoff_climb_ft,
            takeoff_timeout_secs: config.prefetch.takeoff_timeout_secs,
            landing_hysteresis_secs: config.prefetch.landing_hysteresis_secs,
            ramp_duration_secs: config.prefetch.ramp_duration_secs,
            ramp_start_fraction: config.prefetch.ramp_start_fraction,
```

**Step 7: Add to `AdaptivePrefetchConfig` in `prefetch/adaptive/config.rs`**

Add fields to `AdaptivePrefetchConfig` struct (after `track_stability_duration` at line ~122):

```rust
    /// Altitude climb (feet) above takeoff MSL that releases the transition hold.
    /// Range: 200–5000. Default: 1000.
    pub takeoff_climb_ft: f32,

    /// Maximum duration of the transition hold before timeout release.
    pub takeoff_timeout: Duration,

    /// Sustained duration for Cruise→Ground detection (landing deceleration).
    pub landing_hysteresis: Duration,

    /// Duration of the post-transition ramp from ramp_start_fraction to 1.0.
    pub ramp_duration: Duration,

    /// Starting prefetch fraction when the ramp begins.
    /// Range: 0.1–0.5. Default: 0.25.
    pub ramp_start_fraction: f64,
```

Add defaults in `Default::default()` (after `track_stability_duration` at line ~142):

```rust
            takeoff_climb_ft: 1000.0,
            takeoff_timeout: Duration::from_secs(90),
            landing_hysteresis: Duration::from_secs(15),
            ramp_duration: Duration::from_secs(30),
            ramp_start_fraction: 0.25,
```

Update `from_prefetch_config()` (line ~173-188) to explicitly map the new fields instead of using `..Default::default()`:

```rust
    pub fn from_prefetch_config(config: &PrefetchConfig) -> Self {
        let mode = config.mode.parse().unwrap_or(PrefetchMode::Auto);

        Self {
            enabled: config.enabled,
            mode,
            max_tiles_per_cycle: config.max_tiles_per_cycle as u32,
            calibration: CalibrationConfig {
                aggressive_threshold: config.calibration_aggressive_threshold,
                opportunistic_threshold: config.calibration_opportunistic_threshold,
                sample_duration: Duration::from_secs(config.calibration_sample_duration),
                ..Default::default()
            },
            takeoff_climb_ft: config.takeoff_climb_ft,
            takeoff_timeout: Duration::from_secs(config.takeoff_timeout_secs),
            landing_hysteresis: Duration::from_secs(config.landing_hysteresis_secs),
            ramp_duration: Duration::from_secs(config.ramp_duration_secs),
            ramp_start_fraction: config.ramp_start_fraction,
            ..Default::default()
        }
    }
```

Apply the same pattern to `from_prefetch_settings()` (line ~153-168).

**Step 8: Write config pipeline tests**

Add to `config/keys.rs` tests:

```rust
#[test]
fn test_config_key_takeoff_climb_ft_round_trip() {
    let mut config = ConfigFile::default();
    let key = ConfigKey::PrefetchTakeoffClimbFt;
    assert_eq!(key.get(&config), "1000"); // default
    key.set(&mut config, "2000").unwrap();
    assert_eq!(key.get(&config), "2000");
}

#[test]
fn test_config_key_ramp_start_fraction_validation() {
    let key = ConfigKey::PrefetchRampStartFraction;
    assert!(key.validate("0.25").is_ok());
    assert!(key.validate("0.1").is_ok());
    assert!(key.validate("0.5").is_ok());
    assert!(key.validate("0.05").is_err()); // below range
    assert!(key.validate("0.6").is_err());  // above range
}
```

Add to `prefetch/adaptive/config.rs` tests:

```rust
#[test]
fn test_default_config_transition_ramp() {
    let config = AdaptivePrefetchConfig::default();
    assert_eq!(config.takeoff_climb_ft, 1000.0);
    assert_eq!(config.takeoff_timeout, Duration::from_secs(90));
    assert_eq!(config.landing_hysteresis, Duration::from_secs(15));
    assert_eq!(config.ramp_duration, Duration::from_secs(30));
    assert_eq!(config.ramp_start_fraction, 0.25);
}
```

**Step 9: Run all tests**

Run: `cargo test -p xearthlayer config:: prefetch::adaptive::config`
Expected: PASS

**Step 10: Commit**

```bash
git add xearthlayer/src/config/ xearthlayer/src/service/orchestrator_config.rs xearthlayer/src/prefetch/adaptive/config.rs
git commit -m "feat(config): add transition ramp settings to config pipeline (#62)

Wire 5 new configurable parameters through the full INI config pipeline:
- takeoff_climb_ft (200-5000, default 1000)
- takeoff_timeout_secs (30-300, default 90)
- landing_hysteresis_secs (5-60, default 15)
- ramp_duration_secs (10-120, default 30)
- ramp_start_fraction (0.1-0.5, default 0.25)"
```

---

## Task 4: Add Transition Phase to PhaseDetector (TDD)

The core behavioral change: Ground→Transition→Cruise instead of Ground→Cruise.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/phase_detector.rs`

**Step 1: Update FlightPhase enum**

Add `Transition` variant:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlightPhase {
    #[default]
    Ground,
    /// Takeoff detected, awaiting climb confirmation (+1000ft MSL or timeout).
    Transition,
    Cruise,
}
```

Update `description()` and `Display` impl to handle `Transition`:

```rust
FlightPhase::Transition => "takeoff transition",
// Display:
FlightPhase::Transition => write!(f, "transition"),
```

**Step 2: Write failing tests for the three-phase model**

```rust
#[test]
fn test_ground_to_transition_at_40kt() {
    let mut detector = PhaseDetector::with_defaults();
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    // Start on ground
    assert_eq!(detector.current_phase(), FlightPhase::Ground);

    // GS crosses 40kt
    detector.update(100.0, 0.0, 500.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    let changed = detector.update(100.0, 0.0, 500.0);

    assert!(changed);
    assert_eq!(detector.current_phase(), FlightPhase::Transition);
}

#[test]
fn test_transition_records_msl() {
    let mut detector = PhaseDetector::with_defaults();
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    detector.update(100.0, 0.0, 500.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    detector.update(100.0, 0.0, 500.0);

    assert_eq!(detector.current_phase(), FlightPhase::Transition);
    assert_eq!(detector.transition_msl(), Some(500.0));
}

#[test]
fn test_transition_to_cruise_on_climb() {
    let config = AdaptivePrefetchConfig {
        takeoff_climb_ft: 1000.0,
        ..Default::default()
    };
    let mut detector = PhaseDetector::new(&config);
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    // Enter Transition at MSL=500
    detector.update(100.0, 0.0, 500.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    detector.update(100.0, 0.0, 500.0);
    assert_eq!(detector.current_phase(), FlightPhase::Transition);

    // Climb to MSL=1500 (baseline + 1000)
    let changed = detector.update(200.0, 0.0, 1500.0);
    assert!(changed);
    assert_eq!(detector.current_phase(), FlightPhase::Cruise);
}

#[test]
fn test_transition_to_cruise_on_timeout() {
    let config = AdaptivePrefetchConfig {
        takeoff_climb_ft: 1000.0,
        takeoff_timeout: Duration::from_millis(50),
        ..Default::default()
    };
    let mut detector = PhaseDetector::new(&config);
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    // Enter Transition at MSL=6000
    detector.update(50.0, 0.0, 6000.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    detector.update(50.0, 0.0, 6000.0);
    assert_eq!(detector.current_phase(), FlightPhase::Transition);

    // Wait for timeout (not enough climb)
    std::thread::sleep(std::time::Duration::from_millis(60));
    let changed = detector.update(80.0, 0.0, 6200.0);
    assert!(changed);
    assert_eq!(detector.current_phase(), FlightPhase::Cruise);
}

#[test]
fn test_no_direct_ground_to_cruise() {
    let mut detector = PhaseDetector::with_defaults();
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    // Cross 40kt — should go to Transition, not Cruise
    detector.update(100.0, 0.0, 500.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    detector.update(100.0, 0.0, 500.0);

    assert_eq!(detector.current_phase(), FlightPhase::Transition);
    assert_ne!(detector.current_phase(), FlightPhase::Cruise);
}

#[test]
fn test_cruise_to_ground_sustained_15s() {
    let config = AdaptivePrefetchConfig {
        landing_hysteresis: Duration::from_millis(50),
        ..Default::default()
    };
    let mut detector = PhaseDetector::new(&config);
    detector.set_phase(FlightPhase::Cruise);

    // GS drops below 40kt
    detector.update(10.0, 0.0, 500.0);
    std::thread::sleep(std::time::Duration::from_millis(60));
    let changed = detector.update(10.0, 0.0, 500.0);

    assert!(changed);
    assert_eq!(detector.current_phase(), FlightPhase::Ground);
}

#[test]
fn test_helicopter_immediate_release() {
    let config = AdaptivePrefetchConfig {
        takeoff_climb_ft: 1000.0,
        ..Default::default()
    };
    let mut detector = PhaseDetector::new(&config);
    detector.hysteresis_duration = std::time::Duration::from_millis(10);

    // Helicopter already at MSL=1600, crosses 40kt GS
    detector.update(50.0, 0.0, 1600.0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    detector.update(50.0, 0.0, 1600.0);
    // Enters Transition at MSL=1600, baseline=1600

    // Still at MSL=1600 — climb is 0, not enough
    assert_eq!(detector.current_phase(), FlightPhase::Transition);

    // Next update at MSL=2700 (climbed 1100 > 1000)
    let changed = detector.update(55.0, 0.0, 2700.0);
    assert!(changed);
    assert_eq!(detector.current_phase(), FlightPhase::Cruise);
}
```

**Step 3: Run tests to verify they fail**

Run: `cargo test -p xearthlayer prefetch::adaptive::phase_detector -- --no-capture`
Expected: FAIL — `FlightPhase::Transition` not found, `transition_msl()` not found, wrong update signature

**Step 4: Implement the three-phase PhaseDetector**

Key changes to `PhaseDetector`:
1. Add `takeoff_climb_ft: f32` and `takeoff_timeout: Duration` fields (from config)
2. Add `landing_hysteresis: Duration` field (for Cruise→Ground, replaces the 2s default)
3. Add `transition_msl: Option<f32>` field
4. Add `pub fn transition_msl(&self) -> Option<f32>` accessor
5. Change `update()` signature to `update(&mut self, ground_speed_kt: f32, agl_ft: f32, msl_ft: f32) -> bool`
6. In `detect_phase()`, return `FlightPhase::Transition` when GS ≥ 40kt from Ground (instead of Cruise)
7. Add Transition→Cruise check: `(climbed >= takeoff_climb_ft) || (elapsed >= takeoff_timeout)`
8. Use `landing_hysteresis` for Cruise→Ground hysteresis (instead of the base 2s)
9. Record `transition_msl = Some(msl_ft)` when entering Transition

**Step 5: Run tests to verify they pass**

Run: `cargo test -p xearthlayer prefetch::adaptive::phase_detector -- --no-capture`
Expected: PASS

**Step 6: Fix existing tests that break due to the new update() signature**

The existing tests call `detector.update(gs, agl)` — add a third arg `msl_ft: 0.0` to all existing test calls. The existing `test_phase_detector_hysteresis` and `test_phase_detector_cancelled_transition` tests will need updating to expect `Transition` instead of `Cruise` on the initial phase change.

**Step 7: Run full test suite**

Run: `cargo test -p xearthlayer -- --no-capture`
Expected: PASS (or identify further call sites that need updating)

**Step 8: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/phase_detector.rs
git commit -m "feat(prefetch): add Transition phase with MSL-based release (#62)

Three-phase model: Ground → Transition → Cruise
- Transition triggers at GS ≥ 40kt (same as before)
- Releases when MSL exceeds baseline + takeoff_climb_ft or timeout
- Cruise→Ground uses configurable landing_hysteresis"
```

---

## Task 5: Simplify TransitionThrottle to Phase-Driven (TDD)

Replace the internal-timer state machine with a phase-driven fraction calculator.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/transition_throttle.rs`

**Step 1: Write failing tests for the new phase-driven behavior**

Replace all existing tests with:

```rust
#[test]
fn test_transition_phase_returns_zero() {
    let mut throttle = TransitionThrottle::new();
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    assert_eq!(throttle.fraction(), 0.0);
    assert!(throttle.is_active());
}

#[test]
fn test_ground_phase_returns_full() {
    let mut throttle = TransitionThrottle::new();
    assert_eq!(throttle.fraction(), 1.0);
    assert!(!throttle.is_active());
}

#[test]
fn test_cruise_after_transition_starts_ramp() {
    let mut throttle = TransitionThrottle::with_config(
        Duration::from_secs(30),
        0.25,
    );
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);

    let frac = throttle.fraction();
    assert!(frac >= 0.25 && frac < 0.35, "Expected ~0.25 at ramp start, got {frac}");
    assert!(throttle.is_active());
}

#[test]
fn test_cruise_steady_returns_full() {
    // Entering Cruise directly (not from Transition) — no ramp
    let mut throttle = TransitionThrottle::new();
    // Simulate starting already in Cruise
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);
    // This is a direct Ground→Cruise which shouldn't happen in the new model,
    // but the throttle should handle it gracefully (no ramp)
    assert_eq!(throttle.fraction(), 1.0);
}

#[test]
fn test_ramp_completes_to_full() {
    let mut throttle = TransitionThrottle::with_config(
        Duration::from_millis(50),
        0.25,
    );
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);

    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(throttle.fraction(), 1.0);
    assert!(!throttle.is_active());
}

#[test]
fn test_ramp_resets_on_re_transition() {
    let mut throttle = TransitionThrottle::with_config(
        Duration::from_millis(100),
        0.25,
    );

    // First cycle: Transition → Cruise (start ramp)
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);
    std::thread::sleep(Duration::from_millis(50));
    let mid_ramp = throttle.fraction();
    assert!(mid_ramp > 0.25 && mid_ramp < 1.0);

    // Touch-and-go: Cruise → Ground → Transition → Cruise
    throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
    assert_eq!(throttle.fraction(), 1.0); // Ground = full

    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    assert_eq!(throttle.fraction(), 0.0); // Transition = held

    throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);
    let restart = throttle.fraction();
    assert!(restart >= 0.25 && restart < 0.35, "Ramp should restart from 0.25, got {restart}");
}

#[test]
fn test_ramp_linear_at_midpoint() {
    let mut throttle = TransitionThrottle::with_config(
        Duration::from_millis(100),
        0.25,
    );
    throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
    throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);

    std::thread::sleep(Duration::from_millis(50));
    let frac = throttle.fraction();
    // At t=0.5: 0.25 + 0.5 * (1.0 - 0.25) = 0.625
    let expected = 0.625;
    assert!(
        (frac - expected).abs() < 0.1,
        "Expected ~{expected} at midpoint, got {frac}"
    );
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer prefetch::adaptive::transition_throttle -- --no-capture`
Expected: FAIL — `FlightPhase::Transition` not handled, `with_config` not found

**Step 3: Rewrite TransitionThrottle**

Replace the implementation with:

```rust
use std::time::{Duration, Instant};
use super::phase_detector::FlightPhase;

/// Default ramp duration.
const DEFAULT_RAMP_DURATION: Duration = Duration::from_secs(30);

/// Default ramp start fraction.
const DEFAULT_RAMP_START_FRACTION: f64 = 0.25;

#[derive(Debug, Clone)]
enum ThrottleState {
    /// No throttle active.
    Idle,
    /// Transition phase — fully suppressed.
    Held,
    /// Ramping up after Transition → Cruise.
    Ramping { started_at: Instant },
}

pub struct TransitionThrottle {
    state: ThrottleState,
    ramp_duration: Duration,
    ramp_start_fraction: f64,
}

impl TransitionThrottle {
    pub fn new() -> Self {
        Self {
            state: ThrottleState::Idle,
            ramp_duration: DEFAULT_RAMP_DURATION,
            ramp_start_fraction: DEFAULT_RAMP_START_FRACTION,
        }
    }

    pub fn with_config(ramp_duration: Duration, ramp_start_fraction: f64) -> Self {
        Self {
            state: ThrottleState::Idle,
            ramp_duration,
            ramp_start_fraction,
        }
    }

    pub fn on_phase_change(&mut self, _from: FlightPhase, to: FlightPhase) {
        match to {
            FlightPhase::Transition => {
                tracing::info!("TransitionThrottle: entering hold (transition phase)");
                self.state = ThrottleState::Held;
            }
            FlightPhase::Cruise => {
                if matches!(self.state, ThrottleState::Held) {
                    tracing::info!(
                        ramp_secs = self.ramp_duration.as_secs(),
                        start_fraction = format!("{:.0}%", self.ramp_start_fraction * 100.0),
                        "TransitionThrottle: starting ramp-up"
                    );
                    self.state = ThrottleState::Ramping {
                        started_at: Instant::now(),
                    };
                }
                // If not coming from Held (e.g., direct Ground→Cruise), stay Idle
            }
            FlightPhase::Ground => {
                if self.is_active() {
                    tracing::info!("TransitionThrottle: reset to idle (ground phase)");
                }
                self.state = ThrottleState::Idle;
            }
        }
    }

    pub fn fraction(&mut self) -> f64 {
        match self.state {
            ThrottleState::Idle => 1.0,
            ThrottleState::Held => 0.0,
            ThrottleState::Ramping { started_at } => {
                let elapsed = started_at.elapsed();
                if elapsed >= self.ramp_duration {
                    tracing::debug!("TransitionThrottle: ramp complete");
                    self.state = ThrottleState::Idle;
                    1.0
                } else {
                    let t = elapsed.as_secs_f64() / self.ramp_duration.as_secs_f64();
                    self.ramp_start_fraction + t * (1.0 - self.ramp_start_fraction)
                }
            }
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, ThrottleState::Idle)
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer prefetch::adaptive::transition_throttle -- --no-capture`
Expected: PASS

**Step 5: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/transition_throttle.rs
git commit -m "refactor(prefetch): simplify TransitionThrottle to phase-driven fraction (#62)

Replace internal timer state machine with phase-driven behavior:
- Transition phase → fraction 0.0 (held)
- Cruise from Transition → ramp from config start_fraction to 1.0
- Ground → immediate reset to idle
- Configurable ramp_duration and ramp_start_fraction via with_config()"
```

---

## Task 6: Update Coordinator Integration

Wire the new three-phase model and simplified throttle into the coordinator.

**Files:**
- Modify: `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`

**Step 1: Update `process_telemetry()` to pass MSL**

Change line ~616 from:
```rust
let agl_ft = 0.0;
```
to:
```rust
let msl_ft = state.altitude;
let agl_ft = 0.0;
```

And change the `phase_detector.update()` call (line ~622-623) to pass `msl_ft`:
```rust
let phase_changed = self.phase_detector.update(ground_speed_kt, agl_ft, msl_ft);
```

**Step 2: Handle Transition phase in strategy selection**

In `update()` method (line ~358-386), add `FlightPhase::Transition` arm:

```rust
let plan = match phase {
    FlightPhase::Ground => {
        self.status.active_strategy = self.ground_strategy.name();
        self.ground_strategy.calculate_prefetch(
            position, track, &calibration, &self.cached_tiles,
        )
    }
    FlightPhase::Transition => {
        // Sterile cockpit — no prefetch during takeoff transition
        tracing::debug!("Skipping prefetch - transition phase (sterile cockpit)");
        return None;
    }
    FlightPhase::Cruise => {
        if !self.turn_detector.is_stable() {
            tracing::debug!(
                turn_state = ?self.turn_detector.state(),
                "Skipping cruise prefetch - track not stable"
            );
            return None;
        }
        self.status.active_strategy = self.cruise_strategy.name();
        self.cruise_strategy.calculate_prefetch(
            position, track, &calibration, &self.cached_tiles,
        )
    }
};
```

**Step 3: Initialize TransitionThrottle with config values**

In the coordinator constructor (around line ~168), change:
```rust
let transition_throttle = TransitionThrottle::new();
```
to:
```rust
let transition_throttle = TransitionThrottle::with_config(
    config.ramp_duration,
    config.ramp_start_fraction,
);
```

**Step 4: Update existing coordinator tests**

Update any tests that call `phase_detector.update(gs, agl)` to use the three-arg form `phase_detector.update(gs, agl, msl)`.

Update integration tests that expect `Ground→Cruise` to expect `Ground→Transition→Cruise`.

**Step 5: Write integration tests**

```rust
#[test]
fn test_takeoff_holds_prefetch() {
    // Setup coordinator, trigger Ground→Transition
    // Verify execute() returns 0 during Transition phase
}

#[test]
fn test_takeoff_ramp_after_climb() {
    // Trigger Transition→Cruise, verify execute() returns reduced count
}

#[test]
fn test_ground_prefetch_unaffected() {
    // Verify Ground phase runs at full rate (fraction = 1.0)
}
```

**Step 6: Run full test suite**

Run: `cargo test -p xearthlayer -- --no-capture`
Expected: PASS

**Step 7: Commit**

```bash
git add xearthlayer/src/prefetch/adaptive/coordinator/
git commit -m "feat(prefetch): integrate three-phase model into coordinator (#62)

- Pass MSL altitude to PhaseDetector
- Handle Transition phase (no prefetch, sterile cockpit)
- Initialize TransitionThrottle with config ramp parameters"
```

---

## Task 7: Update Documentation

**Files:**
- Modify: `docs/configuration.md`

**Step 1: Add transition ramp settings to documentation**

In the `[prefetch]` settings table (around line ~338), add:

```markdown
| `takeoff_climb_ft` | float | `1000` | Altitude climb (ft) above takeoff MSL to release transition hold (200-5000) |
| `takeoff_timeout_secs` | integer | `90` | Maximum seconds before timeout release (30-300) |
| `landing_hysteresis_secs` | integer | `15` | Sustained seconds at GS < 40kt before landing detection (5-60) |
| `ramp_duration_secs` | integer | `30` | Duration of linear ramp to full prefetch rate (10-120) |
| `ramp_start_fraction` | float | `0.25` | Starting prefetch fraction when ramp begins (0.1-0.5) |
```

Add a "Transition Ramp" subsection after the circuit breaker documentation explaining the three-phase model.

Update the INI example sections to include the new settings.

Update the `xearthlayer config list` reference table.

**Step 2: Commit**

```bash
git add docs/configuration.md
git commit -m "docs(config): document transition ramp settings (#62)"
```

---

## Task 8: Run Pre-Commit and Final Verification

**Step 1: Run the full pre-commit suite**

Run: `make pre-commit`
Expected: All format, lint, and test checks pass

**Step 2: Fix any issues**

If clippy or tests fail, fix and re-run.

**Step 3: Final commit if needed**

```bash
git commit -m "fix(prefetch): address clippy/test issues from transition ramp (#62)"
```

---

## Task Summary

| Task | Description | Dependencies |
|------|-------------|--------------|
| 1 | Move design doc to `docs/dev/` | None |
| 2 | Add RangeSpec validators | None |
| 3 | Wire 5 settings through config pipeline | Task 2 |
| 4 | Add Transition phase to PhaseDetector (TDD) | Task 3 (needs config fields) |
| 5 | Simplify TransitionThrottle (TDD) | Task 4 (needs FlightPhase::Transition) |
| 6 | Update coordinator integration | Tasks 4, 5 |
| 7 | Update documentation | Task 3 |
| 8 | Pre-commit verification | All |
