//! Transition throttle for suppressing prefetch after flight phase changes.
//!
//! When the aircraft transitions from Ground to Cruise (takeoff), prefetch
//! is suppressed for a grace period to avoid competing with X-Plane's scene
//! loading, then gradually ramped up over a configurable duration.
//!
//! # State Machine
//!
//! ```text
//! Ground->Cruise         grace elapsed        ramp elapsed
//!    Idle ---------> GracePeriod ---------> RampingUp ---------> Idle
//!      ^                  |                     |
//!      |   Cruise->Ground |  Cruise->Ground     |
//!      +------------------+---------------------+
//! ```
//!
//! During `GracePeriod`, `fraction()` returns `0.0` (fully suppressed).
//! During `RampingUp`, `fraction()` linearly interpolates from
//! [`RAMP_START_FRACTION`] to `1.0`.

use std::time::{Duration, Instant};

use super::phase_detector::FlightPhase;

/// Duration after Ground->Cruise during which prefetch is fully suppressed.
pub const GRACE_PERIOD_DURATION: Duration = Duration::from_secs(45);

/// Duration of the linear ramp from [`RAMP_START_FRACTION`] to 1.0.
pub const RAMP_DURATION: Duration = Duration::from_secs(30);

/// Starting fraction when the ramp begins (25% throughput).
pub const RAMP_START_FRACTION: f64 = 0.25;

/// Internal state of the transition throttle.
#[derive(Debug, Clone)]
enum ThrottleState {
    /// No active throttle; prefetch runs at full rate.
    Idle,

    /// Prefetch is fully suppressed after a Ground->Cruise transition.
    GracePeriod {
        /// When the grace period started.
        started_at: Instant,
    },

    /// Prefetch is ramping up from [`RAMP_START_FRACTION`] to 1.0.
    RampingUp {
        /// When the ramp started.
        started_at: Instant,
    },
}

/// Suppresses prefetch after Ground->Cruise transitions to avoid competing
/// with X-Plane's scene loading during takeoff.
///
/// The throttle goes through three phases:
/// 1. **Grace period** (45s): Fully suppressed (`fraction() == 0.0`)
/// 2. **Ramp-up** (30s): Linear ramp from 25% to 100%
/// 3. **Idle**: Full throughput (`fraction() == 1.0`)
///
/// A Cruise->Ground transition (landing) immediately resets to Idle.
#[derive(Debug)]
pub struct TransitionThrottle {
    state: ThrottleState,
}

impl TransitionThrottle {
    /// Creates a new throttle in the Idle state.
    pub fn new() -> Self {
        Self {
            state: ThrottleState::Idle,
        }
    }

    /// Handles a flight phase transition.
    ///
    /// - Ground -> Cruise: enters grace period (suppresses prefetch)
    /// - Cruise -> Ground: resets to idle (full throughput)
    /// - Same phase: no-op
    pub fn on_phase_change(&mut self, from: FlightPhase, to: FlightPhase) {
        if from == to {
            return;
        }

        match (from, to) {
            (FlightPhase::Ground, FlightPhase::Cruise)
            | (FlightPhase::Ground, FlightPhase::Transition) => {
                tracing::info!(
                    "TransitionThrottle: {from}->{to}, entering grace period ({:.0}s)",
                    GRACE_PERIOD_DURATION.as_secs_f64()
                );
                self.state = ThrottleState::GracePeriod {
                    started_at: Instant::now(),
                };
            }
            (_, FlightPhase::Ground) => {
                tracing::info!("TransitionThrottle: {from}->Ground, resetting to idle");
                self.state = ThrottleState::Idle;
            }
            _ => {}
        }
    }

    /// Returns the current throughput fraction (0.0 to 1.0).
    ///
    /// - `0.0` during grace period (fully suppressed)
    /// - `0.25..1.0` during ramp-up (linear interpolation)
    /// - `1.0` when idle (full throughput)
    ///
    /// This method auto-transitions between states based on elapsed time.
    pub fn fraction(&mut self) -> f64 {
        match self.state {
            ThrottleState::Idle => 1.0,
            ThrottleState::GracePeriod { started_at } => {
                if started_at.elapsed() >= GRACE_PERIOD_DURATION {
                    tracing::debug!(
                        "TransitionThrottle: grace period elapsed, entering ramp-up ({:.0}s)",
                        RAMP_DURATION.as_secs_f64()
                    );
                    self.state = ThrottleState::RampingUp {
                        started_at: Instant::now(),
                    };
                    RAMP_START_FRACTION
                } else {
                    0.0
                }
            }
            ThrottleState::RampingUp { started_at } => {
                let elapsed = started_at.elapsed();
                if elapsed >= RAMP_DURATION {
                    tracing::debug!("TransitionThrottle: ramp complete, returning to idle");
                    self.state = ThrottleState::Idle;
                    1.0
                } else {
                    let t = elapsed.as_secs_f64() / RAMP_DURATION.as_secs_f64();
                    RAMP_START_FRACTION + t * (1.0 - RAMP_START_FRACTION)
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
    fn test_ground_to_cruise_enters_grace_period() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);

        assert!(throttle.is_active());
        assert_eq!(throttle.fraction(), 0.0);
    }

    #[test]
    fn test_cruise_to_ground_resets_to_idle() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Cruise);
        assert!(throttle.is_active());

        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);
    }

    #[test]
    fn test_same_phase_is_noop() {
        let mut throttle = TransitionThrottle::new();
        throttle.on_phase_change(FlightPhase::Ground, FlightPhase::Ground);
        assert!(!throttle.is_active());

        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Cruise);
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_grace_period_transitions_to_ramp() {
        let mut throttle = TransitionThrottle::new();
        // Manually set a grace period that started long ago
        throttle.state = ThrottleState::GracePeriod {
            started_at: Instant::now() - GRACE_PERIOD_DURATION - Duration::from_millis(1),
        };

        let frac = throttle.fraction();
        // Should have auto-transitioned to RampingUp, returning RAMP_START_FRACTION
        assert_eq!(frac, RAMP_START_FRACTION);
        assert!(throttle.is_active());
    }

    #[test]
    fn test_ramp_completes_to_idle() {
        let mut throttle = TransitionThrottle::new();
        // Manually set a ramp that started long ago
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now() - RAMP_DURATION - Duration::from_millis(1),
        };

        let frac = throttle.fraction();
        assert_eq!(frac, 1.0);
        assert!(!throttle.is_active());
    }

    #[test]
    fn test_ramp_fraction_is_linear() {
        let mut throttle = TransitionThrottle::new();
        // Set ramp at 50% elapsed
        let half_ramp = RAMP_DURATION / 2;
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now() - half_ramp,
        };

        let frac = throttle.fraction();
        // At t=0.5: 0.25 + 0.5 * (1.0 - 0.25) = 0.25 + 0.375 = 0.625
        let expected = RAMP_START_FRACTION + 0.5 * (1.0 - RAMP_START_FRACTION);
        assert!(
            (frac - expected).abs() < 0.05,
            "Expected ~{expected}, got {frac}"
        );
    }

    #[test]
    fn test_ramp_fraction_at_start() {
        let mut throttle = TransitionThrottle::new();
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now(),
        };

        let frac = throttle.fraction();
        // At t=0: should be very close to RAMP_START_FRACTION
        assert!(
            (frac - RAMP_START_FRACTION).abs() < 0.01,
            "Expected ~{RAMP_START_FRACTION}, got {frac}"
        );
    }

    #[test]
    fn test_cruise_to_ground_during_ramp_resets() {
        let mut throttle = TransitionThrottle::new();
        throttle.state = ThrottleState::RampingUp {
            started_at: Instant::now(),
        };
        assert!(throttle.is_active());

        throttle.on_phase_change(FlightPhase::Cruise, FlightPhase::Ground);
        assert!(!throttle.is_active());
        assert_eq!(throttle.fraction(), 1.0);
    }

    #[test]
    fn test_default_is_idle() {
        let throttle = TransitionThrottle::default();
        assert!(!throttle.is_active());
    }
}
