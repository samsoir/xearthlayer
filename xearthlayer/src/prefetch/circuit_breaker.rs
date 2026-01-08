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
    /// Previous FUSE job count (for delta rate calculation).
    last_fuse_jobs: u64,
    /// Time of last rate check (for delta rate calculation).
    last_check_time: Instant,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            high_load_start: None,
            half_open_start: None,
            last_fuse_jobs: 0,
            last_check_time: Instant::now(),
        }
    }

    /// Update circuit state based on current FUSE job count.
    ///
    /// Calculates the delta rate (jobs/sec since last check) rather than using
    /// a lifetime average. This allows detecting load spikes even after the
    /// session has been running for a while.
    ///
    /// **IMPORTANT**: Only pass FUSE-originated job count here, NOT prefetch jobs.
    /// This prevents self-fulfilling lockup.
    ///
    /// # Arguments
    ///
    /// * `fuse_jobs_total` - Total FUSE jobs submitted since session start
    ///
    /// # Returns
    ///
    /// `true` if circuit is open (prefetch should be blocked), `false` otherwise.
    pub fn update(&mut self, fuse_jobs_total: u64) -> bool {
        // Calculate delta rate since last check
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_check_time);
        let elapsed_secs = elapsed.as_secs_f64().max(0.001); // Avoid division by zero

        let jobs_delta = fuse_jobs_total.saturating_sub(self.last_fuse_jobs);
        let fuse_jobs_per_second = jobs_delta as f64 / elapsed_secs;

        // Update tracking for next check
        self.last_fuse_jobs = fuse_jobs_total;
        self.last_check_time = now;

        let is_high_load = fuse_jobs_per_second > self.config.threshold_jobs_per_sec;

        // Log rate calculation for debugging (INFO temporarily for diagnosis)
        tracing::info!(
            jobs_total = fuse_jobs_total,
            jobs_delta = jobs_delta,
            elapsed_ms = elapsed.as_millis(),
            rate = format!("{:.1}", fuse_jobs_per_second),
            threshold = self.config.threshold_jobs_per_sec,
            is_high_load = is_high_load,
            state = ?self.state,
            "Circuit breaker rate check"
        );

        match self.state {
            CircuitState::Closed => {
                if is_high_load {
                    // Start tracking sustained high load
                    if self.high_load_start.is_none() {
                        self.high_load_start = Some(Instant::now());
                        tracing::info!(
                            rate = format!("{:.1}", fuse_jobs_per_second),
                            threshold = self.config.threshold_jobs_per_sec,
                            "Circuit breaker: high load detected, starting tracking"
                        );
                    }

                    // Check if high load has been sustained long enough to trip
                    if let Some(start) = self.high_load_start {
                        if start.elapsed() >= self.config.open_duration {
                            self.state = CircuitState::Open;
                            self.high_load_start = None;
                            tracing::info!(
                                rate = format!("{:.1}", fuse_jobs_per_second),
                                sustained_secs = self.config.open_duration.as_secs(),
                                "Circuit breaker OPENED - prefetch paused"
                            );
                        }
                    }
                } else {
                    // Load dropped, reset tracking
                    if self.high_load_start.is_some() {
                        tracing::debug!("Circuit breaker: load dropped, resetting tracking");
                    }
                    self.high_load_start = None;
                }
            }
            CircuitState::Open => {
                if !is_high_load {
                    // Load dropped, transition to half-open to test recovery
                    self.state = CircuitState::HalfOpen;
                    self.half_open_start = Some(Instant::now());
                    tracing::info!(
                        rate = format!("{:.1}", fuse_jobs_per_second),
                        "Circuit breaker: load dropped, transitioning to half-open"
                    );
                }
            }
            CircuitState::HalfOpen => {
                if is_high_load {
                    // Load spiked again, go back to open
                    self.state = CircuitState::Open;
                    self.half_open_start = None;
                    tracing::info!(
                        rate = format!("{:.1}", fuse_jobs_per_second),
                        "Circuit breaker: load spike in half-open, returning to open"
                    );
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

    /// Helper to simulate high load: sleep then update with jobs that exceed threshold.
    /// Returns the new total job count.
    fn simulate_high_load(cb: &mut CircuitBreaker, current_jobs: u64, sleep_ms: u64) -> u64 {
        thread::sleep(Duration::from_millis(sleep_ms));
        // Add enough jobs to exceed 5 jobs/sec threshold
        // e.g., 100ms sleep + 10 jobs = 100 jobs/sec
        let new_jobs = current_jobs + 10;
        cb.update(new_jobs);
        new_jobs
    }

    /// Helper to simulate low load: sleep then update with few jobs (below threshold).
    /// Returns the new total job count.
    fn simulate_low_load(cb: &mut CircuitBreaker, current_jobs: u64, sleep_ms: u64) -> u64 {
        thread::sleep(Duration::from_millis(sleep_ms));
        // Add minimal jobs to stay below 5 jobs/sec threshold
        // e.g., 100ms sleep + 0 jobs = 0 jobs/sec
        cb.update(current_jobs);
        current_jobs
    }

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

        // First update establishes baseline
        cb.update(0);

        // Single high load update shouldn't trip immediately
        let jobs = simulate_high_load(&mut cb, 0, 50);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Load drops before sustained period (resets tracking)
        let jobs = simulate_low_load(&mut cb, jobs, 30);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Another spike - starts tracking again but hasn't sustained
        let _jobs = simulate_high_load(&mut cb, jobs, 50);
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

        // First update establishes baseline
        cb.update(0);

        // Start high load - first spike starts tracking
        let jobs = simulate_high_load(&mut cb, 0, 30);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Sustained high load should trip after open_duration
        let _jobs = simulate_high_load(&mut cb, jobs, 60);
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_transitions_to_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(50),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline
        cb.update(0);

        // Trip the circuit with sustained high load
        let jobs = simulate_high_load(&mut cb, 0, 20);
        let jobs = simulate_high_load(&mut cb, jobs, 40);
        assert_eq!(cb.state(), CircuitState::Open);

        // Load drops - should go to half-open
        let _jobs = simulate_low_load(&mut cb, jobs, 50);
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(cb.is_open()); // Still considered "open" for blocking purposes
    }

    #[test]
    fn test_circuit_breaker_half_open_then_closes() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(40),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline
        cb.update(0);

        // Trip the circuit
        let jobs = simulate_high_load(&mut cb, 0, 20);
        let jobs = simulate_high_load(&mut cb, jobs, 40);
        assert_eq!(cb.state(), CircuitState::Open);

        // Go to half-open
        let _jobs = simulate_low_load(&mut cb, jobs, 50);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // try_close() too early - should fail
        assert!(!cb.try_close());
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Wait for half-open duration
        thread::sleep(Duration::from_millis(50));

        // Now try_close() should succeed
        assert!(cb.try_close());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(!cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_resets_on_load_spike_in_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(100),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline
        cb.update(0);

        // Trip the circuit and go to half-open
        let jobs = simulate_high_load(&mut cb, 0, 20);
        let jobs = simulate_high_load(&mut cb, jobs, 40);
        let jobs = simulate_low_load(&mut cb, jobs, 50);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Load spikes again - should go back to open
        let _jobs = simulate_high_load(&mut cb, jobs, 30);
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
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(30),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline
        cb.update(0);

        // Many updates with low load (no new jobs)
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(10));
            cb.update(0);
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
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(30),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline
        cb.update(0);

        // Closed state with low load - should return false
        thread::sleep(Duration::from_millis(50));
        assert!(!cb.update(0));

        // Build up high load to trip
        let jobs = simulate_high_load(&mut cb, 0, 20);
        let jobs = simulate_high_load(&mut cb, jobs, 40);

        // Open state - should return true
        assert!(cb.is_open());

        // Transition to half-open with low load - should still return true
        let _jobs = simulate_low_load(&mut cb, jobs, 50);
        assert!(cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_delta_rate_calculation() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 10.0, // 10 jobs/sec threshold
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(30),
        };
        let mut cb = CircuitBreaker::new(config);

        // First update establishes baseline at 0 jobs
        cb.update(0);

        // Wait 100ms, then add 5 jobs = 50 jobs/sec (above threshold)
        thread::sleep(Duration::from_millis(100));
        cb.update(5);
        // Should start tracking high load
        assert_eq!(cb.state(), CircuitState::Closed);

        // Wait another 50ms with more jobs to sustain and trip
        thread::sleep(Duration::from_millis(50));
        cb.update(15); // 10 more jobs in 50ms = 200 jobs/sec
        assert_eq!(cb.state(), CircuitState::Open);
    }
}
