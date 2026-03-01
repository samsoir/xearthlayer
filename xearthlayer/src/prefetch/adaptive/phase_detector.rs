//! Flight phase detection for adaptive prefetch.
//!
//! Detects whether the aircraft is on the ground, in a takeoff transition,
//! or in cruise flight to select the appropriate prefetch strategy.
//!
//! # Three-Phase Model
//!
//! ```text
//! Ground → Transition → Cruise
//!                         ↓
//!                       Ground (landing hysteresis)
//! ```
//!
//! # Detection Logic
//!
//! ```text
//! Ground:     GS < 40kt
//! Transition: GS ≥ 40kt, awaiting climb confirmation
//! Cruise:     MSL ≥ takeoff_msl + climb_ft, or timeout elapsed
//! ```
//!
//! Ground speed is the sole phase trigger. AGL is not available from
//! the current telemetry sources (X-Plane UDP / ForeFlight protocol).

use std::time::{Duration, Instant};

use super::config::AdaptivePrefetchConfig;

/// Flight phase for strategy selection.
///
/// The prefetch system uses different strategies based on flight phase:
/// - Ground: Ring-based prefetch around current position
/// - Transition: No prefetch (system resources reserved for X-Plane)
/// - Cruise: Band-based prefetch ahead of track
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlightPhase {
    /// Aircraft is on the ground (taxi, parking, runway).
    ///
    /// Condition: Ground speed < 40kt
    #[default]
    Ground,

    /// Takeoff detected, awaiting climb confirmation (+1000ft MSL or timeout).
    Transition,

    /// Aircraft is in flight (takeoff, cruise, approach).
    ///
    /// Condition: MSL exceeds takeoff baseline + climb threshold, or timeout.
    /// The OR handles slow rotorcraft that are clearly airborne.
    Cruise,
}

impl FlightPhase {
    /// Get a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            FlightPhase::Ground => "ground operations",
            FlightPhase::Transition => "takeoff transition",
            FlightPhase::Cruise => "cruise flight",
        }
    }
}

impl std::fmt::Display for FlightPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlightPhase::Ground => write!(f, "ground"),
            FlightPhase::Transition => write!(f, "transition"),
            FlightPhase::Cruise => write!(f, "cruise"),
        }
    }
}

/// Detects flight phase transitions.
///
/// Monitors ground speed and MSL altitude to determine whether the aircraft
/// is on the ground, in a takeoff transition, or in cruise flight.
///
/// # Hysteresis
///
/// To prevent rapid phase switching during transition (e.g., takeoff roll),
/// the detector requires the new phase conditions to persist for a short
/// duration before confirming the transition.
///
/// # Transition Release
///
/// The Transition→Cruise release bypasses normal hysteresis and happens
/// immediately when either:
/// - MSL exceeds takeoff baseline by `takeoff_climb_ft`
/// - `takeoff_timeout` has elapsed since entering Transition
#[derive(Debug)]
pub struct PhaseDetector {
    /// Current detected phase.
    current_phase: FlightPhase,

    /// Ground speed threshold (knots).
    ground_speed_threshold_kt: f32,

    /// When the current phase was entered.
    phase_entered_at: Instant,

    /// Pending phase transition (if conditions met but not yet confirmed).
    pending_transition: Option<(FlightPhase, Instant)>,

    /// Hysteresis duration (how long conditions must persist).
    pub(crate) hysteresis_duration: Duration,

    /// Altitude climb (feet) above takeoff MSL to release transition hold.
    takeoff_climb_ft: f32,

    /// Maximum time before timeout release if climb threshold not reached.
    takeoff_timeout: Duration,

    /// Sustained duration at ground conditions before Cruise→Ground transition.
    landing_hysteresis: Duration,

    /// MSL altitude recorded when entering Transition phase.
    transition_msl: Option<f32>,
}

impl PhaseDetector {
    /// Create a new phase detector with the given configuration.
    pub fn new(config: &AdaptivePrefetchConfig) -> Self {
        Self {
            current_phase: FlightPhase::Ground,
            ground_speed_threshold_kt: config.ground_speed_threshold_kt,
            phase_entered_at: Instant::now(),
            pending_transition: None,
            hysteresis_duration: Duration::from_secs(2),
            takeoff_climb_ft: config.takeoff_climb_ft,
            takeoff_timeout: config.takeoff_timeout,
            landing_hysteresis: config.landing_hysteresis,
            transition_msl: None,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(&AdaptivePrefetchConfig::default())
    }

    /// Get the current flight phase.
    pub fn current_phase(&self) -> FlightPhase {
        self.current_phase
    }

    /// Get the MSL altitude recorded at takeoff (Transition entry).
    ///
    /// Returns `Some(msl_ft)` when in Transition phase, `None` otherwise.
    pub fn transition_msl(&self) -> Option<f32> {
        self.transition_msl
    }

    /// Update phase detection with new telemetry data.
    ///
    /// # Arguments
    ///
    /// * `ground_speed_kt` - Current ground speed in knots
    /// * `msl_ft` - Current altitude above mean sea level in feet
    ///
    /// # Returns
    ///
    /// `true` if the phase changed, `false` otherwise.
    pub fn update(&mut self, ground_speed_kt: f32, msl_ft: f32) -> bool {
        // If currently in Transition, check for immediate release (no hysteresis)
        if self.current_phase == FlightPhase::Transition {
            return self.check_transition_release(ground_speed_kt, msl_ft);
        }

        let detected_phase = self.detect_phase(ground_speed_kt);

        if detected_phase == self.current_phase {
            // Same phase, clear any pending transition
            self.pending_transition = None;
            return false;
        }

        // Determine target phase based on current state
        let target_phase = if self.current_phase == FlightPhase::Ground {
            // Ground → always goes to Transition first (never directly to Cruise)
            FlightPhase::Transition
        } else {
            // Cruise → Ground (detected_phase must be Ground here)
            detected_phase
        };

        let now = Instant::now();

        // Select hysteresis duration based on transition direction
        let required_hysteresis = if self.current_phase == FlightPhase::Cruise {
            self.landing_hysteresis
        } else {
            self.hysteresis_duration
        };

        // Check if we have a pending transition to this phase
        if let Some((pending_phase, started_at)) = self.pending_transition {
            if pending_phase == target_phase {
                // Same pending phase, check if hysteresis duration passed
                if now.duration_since(started_at) >= required_hysteresis {
                    return self.commit_transition(target_phase, ground_speed_kt, msl_ft);
                }
                // Still waiting for hysteresis
                return false;
            }
        }

        // Start a new pending transition
        self.pending_transition = Some((target_phase, now));
        false
    }

    /// Check if Transition phase should release to Cruise or revert to Ground.
    ///
    /// This bypasses normal hysteresis — release is immediate when conditions are met.
    fn check_transition_release(&mut self, ground_speed_kt: f32, msl_ft: f32) -> bool {
        // Check if we've climbed enough above takeoff MSL
        if let Some(takeoff_msl) = self.transition_msl {
            if msl_ft - takeoff_msl >= self.takeoff_climb_ft {
                return self.commit_transition(FlightPhase::Cruise, ground_speed_kt, msl_ft);
            }
        }

        // Check if timeout has elapsed
        if self.phase_entered_at.elapsed() >= self.takeoff_timeout {
            return self.commit_transition(FlightPhase::Cruise, ground_speed_kt, msl_ft);
        }

        // Check if we've returned to ground conditions (aborted takeoff)
        let detected = self.detect_phase(ground_speed_kt);
        if detected == FlightPhase::Ground {
            // Use standard hysteresis for reverting to ground
            let now = Instant::now();
            if let Some((pending_phase, started_at)) = self.pending_transition {
                if pending_phase == FlightPhase::Ground {
                    if now.duration_since(started_at) >= self.hysteresis_duration {
                        return self.commit_transition(
                            FlightPhase::Ground,
                            ground_speed_kt,
                            msl_ft,
                        );
                    }
                    return false;
                }
            }
            self.pending_transition = Some((FlightPhase::Ground, now));
            return false;
        }

        // Clear any pending ground reversion if we're airborne again
        self.pending_transition = None;
        false
    }

    /// Commit a phase transition, updating all state.
    fn commit_transition(
        &mut self,
        new_phase: FlightPhase,
        ground_speed_kt: f32,
        msl_ft: f32,
    ) -> bool {
        let old_phase = self.current_phase;
        self.current_phase = new_phase;
        self.phase_entered_at = Instant::now();
        self.pending_transition = None;

        // Track MSL on entering Transition
        if new_phase == FlightPhase::Transition {
            self.transition_msl = Some(msl_ft);
        } else {
            self.transition_msl = None;
        }

        tracing::info!(
            from = %old_phase,
            to = %new_phase,
            ground_speed_kt = ground_speed_kt,
            msl_ft = msl_ft,
            "Flight phase transition"
        );

        true
    }

    /// Detect phase based on ground speed (without hysteresis).
    ///
    /// Returns Ground or Cruise based on raw ground speed condition. The
    /// `update()` method interprets Cruise as Transition when coming from Ground.
    fn detect_phase(&self, ground_speed_kt: f32) -> FlightPhase {
        if ground_speed_kt < self.ground_speed_threshold_kt {
            FlightPhase::Ground
        } else {
            FlightPhase::Cruise
        }
    }

    /// How long the current phase has been active.
    pub fn phase_duration(&self) -> Duration {
        self.phase_entered_at.elapsed()
    }

    /// Force a specific phase (for testing).
    #[cfg(test)]
    pub fn set_phase(&mut self, phase: FlightPhase) {
        self.current_phase = phase;
        self.phase_entered_at = Instant::now();
        self.pending_transition = None;
        self.transition_msl = None;
    }
}

impl Default for PhaseDetector {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flight_phase_display() {
        assert_eq!(format!("{}", FlightPhase::Ground), "ground");
        assert_eq!(format!("{}", FlightPhase::Transition), "transition");
        assert_eq!(format!("{}", FlightPhase::Cruise), "cruise");
    }

    #[test]
    fn test_flight_phase_description() {
        assert_eq!(FlightPhase::Ground.description(), "ground operations");
        assert_eq!(FlightPhase::Transition.description(), "takeoff transition");
        assert_eq!(FlightPhase::Cruise.description(), "cruise flight");
    }

    #[test]
    fn test_phase_detector_initial_state() {
        let detector = PhaseDetector::with_defaults();
        assert_eq!(detector.current_phase(), FlightPhase::Ground);
    }

    #[test]
    fn test_phase_detector_ground_conditions() {
        let detector = PhaseDetector::with_defaults();

        // Below GS threshold = ground
        assert_eq!(detector.detect_phase(20.0), FlightPhase::Ground);
        assert_eq!(detector.detect_phase(0.0), FlightPhase::Ground);
        assert_eq!(detector.detect_phase(39.0), FlightPhase::Ground);
    }

    #[test]
    fn test_phase_detector_airborne_conditions() {
        let detector = PhaseDetector::with_defaults();

        // Above GS threshold = airborne (raw detection returns Cruise)
        assert_eq!(detector.detect_phase(100.0), FlightPhase::Cruise);
        assert_eq!(detector.detect_phase(41.0), FlightPhase::Cruise); // Just above threshold
        assert_eq!(detector.detect_phase(40.0), FlightPhase::Cruise); // At threshold
    }

    #[test]
    fn test_phase_detector_hysteresis() {
        let mut detector = PhaseDetector::with_defaults();
        detector.hysteresis_duration = std::time::Duration::from_millis(10);

        // Start on ground
        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        // First update with airborne conditions - should NOT transition yet
        let changed = detector.update(100.0, 0.0);
        assert!(!changed);
        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        // Wait for hysteresis
        std::thread::sleep(std::time::Duration::from_millis(15));

        // Second update - should transition to Transition (not directly to Cruise)
        let changed = detector.update(100.0, 0.0);
        assert!(changed);
        assert_eq!(detector.current_phase(), FlightPhase::Transition);
    }

    #[test]
    fn test_phase_detector_cancelled_transition() {
        let mut detector = PhaseDetector::with_defaults();
        detector.hysteresis_duration = std::time::Duration::from_millis(50);

        // Start on ground
        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        // Start transition to airborne
        detector.update(100.0, 0.0);
        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        // Before hysteresis completes, go back to ground conditions
        detector.update(10.0, 0.0);
        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        // Wait and update with ground conditions - should stay ground
        std::thread::sleep(std::time::Duration::from_millis(60));
        let changed = detector.update(10.0, 0.0);
        assert!(!changed);
        assert_eq!(detector.current_phase(), FlightPhase::Ground);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Three-phase model tests
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_ground_to_transition_at_40kt() {
        let mut detector = PhaseDetector::with_defaults();
        detector.hysteresis_duration = std::time::Duration::from_millis(10);

        assert_eq!(detector.current_phase(), FlightPhase::Ground);

        detector.update(100.0, 500.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        let changed = detector.update(100.0, 500.0);

        assert!(changed);
        assert_eq!(detector.current_phase(), FlightPhase::Transition);
    }

    #[test]
    fn test_transition_records_msl() {
        let mut detector = PhaseDetector::with_defaults();
        detector.hysteresis_duration = std::time::Duration::from_millis(10);

        detector.update(100.0, 500.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        detector.update(100.0, 500.0);

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

        detector.update(100.0, 500.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        detector.update(100.0, 500.0);
        assert_eq!(detector.current_phase(), FlightPhase::Transition);

        let changed = detector.update(200.0, 1500.0);
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

        detector.update(50.0, 6000.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        detector.update(50.0, 6000.0);
        assert_eq!(detector.current_phase(), FlightPhase::Transition);

        std::thread::sleep(std::time::Duration::from_millis(60));
        let changed = detector.update(80.0, 6200.0);
        assert!(changed);
        assert_eq!(detector.current_phase(), FlightPhase::Cruise);
    }

    #[test]
    fn test_no_direct_ground_to_cruise() {
        let mut detector = PhaseDetector::with_defaults();
        detector.hysteresis_duration = std::time::Duration::from_millis(10);

        detector.update(100.0, 500.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        detector.update(100.0, 500.0);

        assert_eq!(detector.current_phase(), FlightPhase::Transition);
        assert_ne!(detector.current_phase(), FlightPhase::Cruise);
    }

    #[test]
    fn test_cruise_to_ground_sustained() {
        let config = AdaptivePrefetchConfig {
            landing_hysteresis: Duration::from_millis(50),
            ..Default::default()
        };
        let mut detector = PhaseDetector::new(&config);
        detector.set_phase(FlightPhase::Cruise);

        detector.update(10.0, 500.0);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let changed = detector.update(10.0, 500.0);

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

        detector.update(50.0, 1600.0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        detector.update(50.0, 1600.0);

        assert_eq!(detector.current_phase(), FlightPhase::Transition);

        let changed = detector.update(55.0, 2700.0);
        assert!(changed);
        assert_eq!(detector.current_phase(), FlightPhase::Cruise);
    }
}
