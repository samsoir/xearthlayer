//! Phase-driven transition throttle for the adaptive prefetch system.
//!
//! Manages prefetch throughput during flight phase transitions. The throttle
//! responds to phase changes from the `PhaseDetector` and outputs a fraction
//! (0.0 to 1.0) that multiplies the prefetch submission rate.
//!
//! # State Machine
//!
//! ```text
//! Phase: Ground            → fraction = 1.0 (full rate)
//! Phase: Transition        → fraction = 0.0 (fully suppressed)
//! Phase: Cruise (from Tr)  → fraction ramps start_fraction → 1.0
//! Phase: Cruise (steady)   → fraction = 1.0 (full rate)
//! ```
//!
//! The "when to release the hold" decision lives entirely in the `PhaseDetector`.
//! This throttle only controls the ramp-up after the hold is released.

use std::time::{Duration, Instant};

use super::phase_detector::FlightPhase;

/// Default ramp duration (seconds).
const DEFAULT_RAMP_DURATION: Duration = Duration::from_secs(30);

/// Default ramp start fraction.
const DEFAULT_RAMP_START_FRACTION: f64 = 0.25;

/// Computes the ramp fraction for a given elapsed time.
///
/// Returns `None` once the ramp has completed, signalling the caller to fall
/// back to full throughput. Kept free of `Instant` so the interpolation can be
/// tested deterministically without wall-clock sleeps.
fn ramp_fraction(elapsed: Duration, ramp_duration: Duration, start_fraction: f64) -> Option<f64> {
    if elapsed >= ramp_duration {
        return None;
    }
    let t = elapsed.as_secs_f64() / ramp_duration.as_secs_f64();
    Some(start_fraction + t * (1.0 - start_fraction))
}

/// Internal state of the transition throttle.
#[derive(Debug, Clone)]
enum ThrottleState {
    /// No active throttle; prefetch runs at full rate.
    Idle,

    /// Transition phase — fully suppressed.
    Held,

    /// Ramping up after Transition → Cruise.
    Ramping { started_at: Instant },
}

/// Phase-driven transition throttle.
///
/// Suppresses prefetch during the Transition phase and ramps up gradually
/// after the PhaseDetector releases to Cruise. The ramp prevents a burst
/// of prefetch activity that could compete with X-Plane's post-takeoff
/// scene loading.
///
/// # Usage
///
/// The coordinator calls `on_phase_change()` when the `PhaseDetector`
/// transitions, and `fraction()` on each tick to get the current
/// throughput multiplier.
#[derive(Debug)]
pub struct TransitionThrottle {
    state: ThrottleState,
    ramp_duration: Duration,
    ramp_start_fraction: f64,
}

impl TransitionThrottle {
    /// Creates a new throttle in the Idle state with default config.
    pub fn new() -> Self {
        Self {
            state: ThrottleState::Idle,
            ramp_duration: DEFAULT_RAMP_DURATION,
            ramp_start_fraction: DEFAULT_RAMP_START_FRACTION,
        }
    }

    /// Creates a new throttle with custom ramp configuration.
    pub fn with_config(ramp_duration: Duration, ramp_start_fraction: f64) -> Self {
        Self {
            state: ThrottleState::Idle,
            ramp_duration,
            ramp_start_fraction,
        }
    }

    /// Handles a flight phase transition.
    ///
    /// - `→ Transition`: enters hold (fully suppressed)
    /// - `Transition → Cruise`: starts ramp-up
    /// - `→ Ground`: resets to idle (full throughput)
    /// - Direct `Ground → Cruise`: no ramp (shouldn't happen in normal flow)
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

    /// Returns the current throughput fraction (0.0 to 1.0).
    ///
    /// - `0.0` during Transition hold (fully suppressed)
    /// - `start_fraction..1.0` during ramp-up (linear interpolation)
    /// - `1.0` when idle (full throughput)
    ///
    /// This method auto-transitions from Ramping to Idle when the ramp completes.
    pub fn fraction(&mut self) -> f64 {
        match self.state {
            ThrottleState::Idle => 1.0,
            ThrottleState::Held => 0.0,
            ThrottleState::Ramping { started_at } => {
                match ramp_fraction(
                    started_at.elapsed(),
                    self.ramp_duration,
                    self.ramp_start_fraction,
                ) {
                    Some(fraction) => fraction,
                    None => {
                        tracing::debug!("TransitionThrottle: ramp complete");
                        self.state = ThrottleState::Idle;
                        1.0
                    }
                }
            }
        }
    }

    /// Returns `true` if the throttle is actively suppressing or ramping.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, ThrottleState::Idle)
    }
}

impl Default for TransitionThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_throttle_is_idle() {
        let throttle = TransitionThrottle::new();
        assert!(!throttle.is_active());
    }

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
        let mut throttle = TransitionThrottle::with_config(Duration::from_secs(30), 0.25);
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
        throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);

        let frac = throttle.fraction();
        assert!(
            frac >= 0.25 && frac < 0.35,
            "Expected ~0.25 at ramp start, got {frac}"
        );
        assert!(throttle.is_active());
    }

    #[test]
    fn test_cruise_steady_returns_full() {
        // Entering Cruise directly (not from Transition) — no ramp
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);
        // Direct Ground→Cruise shouldn't happen in new model but handle gracefully
        assert_eq!(throttle.fraction(), 1.0);
    }

    #[test]
    fn test_ramp_completes_to_full() {
        let mut throttle = TransitionThrottle::with_config(Duration::from_millis(50), 0.25);
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
        throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(throttle.fraction(), 1.0);
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_ramp_resets_on_re_transition() {
        // Long ramp so no plausible scheduling delay advances it meaningfully;
        // this test covers the state machine, not the interpolation.
        let mut throttle = TransitionThrottle::with_config(Duration::from_secs(60), 0.25);

        // First cycle: Transition → Cruise (start ramp)
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
        throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);
        let started = throttle.fraction();
        assert!(started >= 0.25 && started < 0.35);

        // Touch-and-go: Cruise → Ground → Transition → Cruise
        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert_eq!(throttle.fraction(), 1.0); // Ground = full

        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
        assert_eq!(throttle.fraction(), 0.0); // Transition = held

        throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);
        let restart = throttle.fraction();
        assert!(
            restart >= 0.25 && restart < 0.35,
            "Ramp should restart from 0.25, got {restart}"
        );
    }

    #[test]
    fn test_ramp_fraction_is_linear_between_start_and_full() {
        let ramp = Duration::from_millis(100);

        // At t=0: the configured start fraction.
        assert_eq!(ramp_fraction(Duration::ZERO, ramp, 0.25), Some(0.25));

        // At t=0.5: 0.25 + 0.5 * (1.0 - 0.25) = 0.625
        let mid = ramp_fraction(Duration::from_millis(50), ramp, 0.25).unwrap();
        assert!((mid - 0.625).abs() < 1e-9, "expected 0.625, got {mid}");

        // At t=0.75: 0.25 + 0.75 * 0.75 = 0.8125
        let late = ramp_fraction(Duration::from_millis(75), ramp, 0.25).unwrap();
        assert!((late - 0.8125).abs() < 1e-9, "expected 0.8125, got {late}");
    }

    #[test]
    fn test_ramp_fraction_completes_at_and_beyond_duration() {
        let ramp = Duration::from_millis(100);
        assert_eq!(ramp_fraction(ramp, ramp, 0.25), None);
        assert_eq!(ramp_fraction(Duration::from_millis(101), ramp, 0.25), None);
        // A zero-length ramp completes immediately rather than dividing by zero.
        assert_eq!(ramp_fraction(Duration::ZERO, Duration::ZERO, 0.25), None);
    }

    #[test]
    fn test_default_is_idle() {
        let throttle = TransitionThrottle::default();
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_ground_to_ground_is_noop() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Ground);
        // Note: the new impl doesn't have same-phase guard since
        // the coordinator only calls on_phase_change when phase actually changes.
        // But Ground→Ground would just set Idle again, which is fine.
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_cruise_to_ground_during_ramp_resets() {
        let mut throttle = TransitionThrottle::with_config(Duration::from_millis(100), 0.25);
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Transition);
        throttle.on_phase_change(FlightPhase::Transition, FlightPhase::Cruise);
        assert!(throttle.is_active());

        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);
    }
}
