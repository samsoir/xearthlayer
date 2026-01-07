//! Circuit breaker for prefetch throttling.
//!
//! Monitors FUSE request rate to detect when X-Plane is actively loading scenery.
//! When high load is detected, the circuit "opens" to pause prefetching and
//! avoid competing with X-Plane for bandwidth.
//!
//! # State Machine
//!
//! ```text
//! Closed --[fuse_rate > threshold for open_duration]--> Open
//! Open --[fuse_rate < threshold]--> HalfOpen
//! HalfOpen --[half_open_duration elapsed + try_close()]--> Closed
//! HalfOpen --[fuse_rate > threshold]--> Open (reset)
//! ```
//!
//! # Critical Design Decision
//!
//! The circuit breaker only monitors FUSE-originated jobs (where `is_prefetch: false`),
//! NOT prefetch jobs. This prevents a self-fulfilling lockup where prefetch jobs
//! trigger the breaker that pauses prefetching.

use std::time::{Duration, Instant};

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Jobs/second threshold to trip the circuit (default: 5.0).
    pub threshold_jobs_per_sec: f64,
    /// Duration high rate must sustain to trip the circuit (default: 5s).
    pub open_duration: Duration,
    /// Duration of low activity before trying to close (default: 5s).
    pub half_open_duration: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_secs(5),
            half_open_duration: Duration::from_secs(5),
        }
    }
}

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed - prefetch is active (normal operation).
    Closed,
    /// Circuit is open - prefetch is blocked (high X-Plane load detected).
    Open,
    /// Circuit is half-open - testing if safe to resume prefetch.
    HalfOpen,
}

impl CircuitState {
    /// User-friendly display string (NOT circuit breaker jargon).
    ///
    /// Returns terminology suitable for end-user TUI display.
    pub fn display_status(&self) -> &'static str {
        match self {
            CircuitState::Closed => "Active",
            CircuitState::Open => "Paused",
            CircuitState::HalfOpen => "Resuming...",
        }
    }
}

/// Circuit breaker for prefetch throttling.
///
/// Monitors FUSE request rate and pauses prefetching when X-Plane is actively
/// loading scenery (high request rate sustained for a period).
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: CircuitState,
    /// When high load was first detected (for sustained load calculation).
    high_load_start: Option<Instant>,
    /// When we entered half-open state (for cooloff calculation).
    half_open_start: Option<Instant>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            high_load_start: None,
            half_open_start: None,
        }
    }

    /// Update circuit state based on current FUSE jobs/second rate.
    ///
    /// **IMPORTANT**: Only pass FUSE-originated job rate here, NOT prefetch jobs.
    /// This prevents self-fulfilling lockup.
    ///
    /// # Returns
    ///
    /// `true` if circuit is open (prefetch should be blocked), `false` otherwise.
    pub fn update(&mut self, fuse_jobs_per_second: f64) -> bool {
        let is_high_load = fuse_jobs_per_second > self.config.threshold_jobs_per_sec;

        match self.state {
            CircuitState::Closed => {
                if is_high_load {
                    // Start tracking sustained high load
                    if self.high_load_start.is_none() {
                        self.high_load_start = Some(Instant::now());
                    }

                    // Check if high load has been sustained long enough to trip
                    if let Some(start) = self.high_load_start {
                        if start.elapsed() >= self.config.open_duration {
                            self.state = CircuitState::Open;
                            self.high_load_start = None;
                        }
                    }
                } else {
                    // Load dropped, reset tracking
                    self.high_load_start = None;
                }
            }
            CircuitState::Open => {
                if !is_high_load {
                    // Load dropped, transition to half-open to test recovery
                    self.state = CircuitState::HalfOpen;
                    self.half_open_start = Some(Instant::now());
                }
            }
            CircuitState::HalfOpen => {
                if is_high_load {
                    // Load spiked again, go back to open
                    self.state = CircuitState::Open;
                    self.half_open_start = None;
                }
                // If still low load, stay in half-open until try_close() is called
            }
        }

        self.is_open()
    }

    /// Attempt to close the circuit.
    ///
    /// Called by the prefetcher to test if it's safe to resume prefetching.
    /// Only succeeds if:
    /// - Currently in HalfOpen state
    /// - Half-open duration has elapsed
    ///
    /// # Returns
    ///
    /// `true` if circuit was successfully closed, `false` otherwise.
    pub fn try_close(&mut self) -> bool {
        if self.state != CircuitState::HalfOpen {
            return false;
        }

        if let Some(start) = self.half_open_start {
            if start.elapsed() >= self.config.half_open_duration {
                self.state = CircuitState::Closed;
                self.half_open_start = None;
                return true;
            }
        }

        false
    }

    /// Get the current circuit state.
    pub fn state(&self) -> CircuitState {
        self.state
    }

    /// Check if the circuit is open (prefetch should be blocked).
    pub fn is_open(&self) -> bool {
        matches!(self.state, CircuitState::Open | CircuitState::HalfOpen)
    }

    /// Check if the circuit is closed (prefetch is allowed).
    pub fn is_closed(&self) -> bool {
        self.state == CircuitState::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_circuit_breaker_initial_state() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(!cb.is_open());
        assert!(cb.is_closed());
    }

    #[test]
    fn test_circuit_breaker_default_config() {
        let config = CircuitBreakerConfig::default();
        assert_eq!(config.threshold_jobs_per_sec, 5.0);
        assert_eq!(config.open_duration, Duration::from_secs(5));
        assert_eq!(config.half_open_duration, Duration::from_secs(5));
    }

    #[test]
    fn test_circuit_breaker_ignores_transient_spikes() {
        // Short open_duration for testing
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(100),
            half_open_duration: Duration::from_millis(50),
        };
        let mut cb = CircuitBreaker::new(config);

        // Single high load update shouldn't trip immediately
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Load drops before sustained period
        cb.update(2.0);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Another spike
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_opens_on_sustained_load() {
        // Very short durations for testing
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(50),
            half_open_duration: Duration::from_millis(50),
        };
        let mut cb = CircuitBreaker::new(config);

        // Start high load
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Wait for sustained period
        thread::sleep(Duration::from_millis(60));

        // Update again with high load - should trip
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_transitions_to_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(10),
            half_open_duration: Duration::from_millis(50),
        };
        let mut cb = CircuitBreaker::new(config);

        // Trip the circuit
        cb.update(10.0);
        thread::sleep(Duration::from_millis(20));
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Open);

        // Load drops - should go to half-open
        cb.update(2.0);
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(cb.is_open()); // Still considered "open" for blocking purposes
    }

    #[test]
    fn test_circuit_breaker_half_open_then_closes() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(10),
            half_open_duration: Duration::from_millis(30),
        };
        let mut cb = CircuitBreaker::new(config);

        // Trip the circuit
        cb.update(10.0);
        thread::sleep(Duration::from_millis(20));
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Open);

        // Go to half-open
        cb.update(2.0);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // try_close() too early - should fail
        assert!(!cb.try_close());
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Wait for half-open duration
        thread::sleep(Duration::from_millis(40));

        // Now try_close() should succeed
        assert!(cb.try_close());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(!cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_resets_on_load_spike_in_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(10),
            half_open_duration: Duration::from_millis(100),
        };
        let mut cb = CircuitBreaker::new(config);

        // Trip the circuit and go to half-open
        cb.update(10.0);
        thread::sleep(Duration::from_millis(20));
        cb.update(10.0);
        cb.update(2.0);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Load spikes again - should go back to open
        cb.update(10.0);
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_circuit_breaker_try_close_only_works_in_half_open() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig::default());

        // Can't close when already closed
        assert!(!cb.try_close());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_low_load_never_trips() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(10),
            half_open_duration: Duration::from_millis(10),
        };
        let mut cb = CircuitBreaker::new(config);

        // Many updates with low load
        for _ in 0..100 {
            cb.update(2.0);
        }

        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_state_display_status() {
        assert_eq!(CircuitState::Closed.display_status(), "Active");
        assert_eq!(CircuitState::Open.display_status(), "Paused");
        assert_eq!(CircuitState::HalfOpen.display_status(), "Resuming...");
    }

    #[test]
    fn test_circuit_breaker_update_returns_is_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(10),
            half_open_duration: Duration::from_millis(10),
        };
        let mut cb = CircuitBreaker::new(config);

        // Closed state - should return false
        assert!(!cb.update(2.0));

        // Trip the circuit
        cb.update(10.0);
        thread::sleep(Duration::from_millis(20));

        // Open state - should return true
        assert!(cb.update(10.0));

        // Half-open state - should still return true (blocking)
        assert!(cb.update(2.0));
    }
}
