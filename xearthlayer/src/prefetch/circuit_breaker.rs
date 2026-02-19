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
//!
//! # Thread Safety
//!
//! `CircuitBreaker` implements `PrefetchThrottler` and can be used through
//! `Arc<dyn PrefetchThrottler>`. Interior mutability via `Mutex` ensures
//! thread-safe state updates.

use super::load_monitor::FuseLoadMonitor;
use super::throttler::{PrefetchThrottler, ThrottleState};
use crate::executor::ResourcePools;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resource pool utilization threshold that triggers the circuit breaker.
///
/// When any resource pool exceeds this utilization fraction, the circuit breaker
/// counts it as high load — the same as high FUSE request rate.
pub const RESOURCE_SATURATION_THRESHOLD: f64 = 0.9;

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Jobs/second threshold to trip the circuit (default: 50.0).
    pub threshold_jobs_per_sec: f64,
    /// Duration high rate must sustain to trip the circuit (default: 500ms).
    pub open_duration: Duration,
    /// Duration of low activity before trying to close (default: 2s).
    pub half_open_duration: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            threshold_jobs_per_sec: 50.0,
            open_duration: Duration::from_millis(500),
            half_open_duration: Duration::from_secs(2),
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

/// Internal mutable state for the circuit breaker.
#[derive(Debug)]
struct CircuitBreakerInner {
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

impl CircuitBreakerInner {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            high_load_start: None,
            half_open_start: None,
            last_fuse_jobs: 0,
            last_check_time: Instant::now(),
        }
    }
}

/// Circuit breaker for prefetch throttling.
///
/// Monitors FUSE request rate via a `FuseLoadMonitor` and pauses prefetching
/// when X-Plane is actively loading scenery (high request rate sustained).
///
/// Implements `PrefetchThrottler` for use through trait objects.
///
/// # Example
///
/// ```
/// use xearthlayer::prefetch::{
///     CircuitBreaker, CircuitBreakerConfig, FuseLoadMonitor,
///     PrefetchThrottler, SharedFuseLoadMonitor,
/// };
/// use std::sync::Arc;
///
/// let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
/// let circuit_breaker = CircuitBreaker::new(
///     CircuitBreakerConfig::default(),
///     load_monitor,
/// );
///
/// // Use through trait object
/// let throttler: Arc<dyn PrefetchThrottler> = Arc::new(circuit_breaker);
/// if throttler.should_throttle() {
///     // Skip prefetch this cycle
/// }
/// ```
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    load_monitor: Arc<dyn FuseLoadMonitor>,
    /// Optional resource pools for utilization-based trip condition.
    resource_pools: Option<Arc<ResourcePools>>,
    inner: Mutex<CircuitBreakerInner>,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("config", &self.config)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration and load monitor.
    pub fn new(config: CircuitBreakerConfig, load_monitor: Arc<dyn FuseLoadMonitor>) -> Self {
        Self {
            config,
            load_monitor,
            resource_pools: None,
            inner: Mutex::new(CircuitBreakerInner::new()),
        }
    }

    /// Add resource pool monitoring for utilization-based trip condition.
    ///
    /// When set, the circuit breaker also trips if any resource pool exceeds
    /// [`RESOURCE_SATURATION_THRESHOLD`] utilization — even if the FUSE request
    /// rate is below threshold. This catches situations where prefetch saturates
    /// the executor without generating FUSE requests.
    pub fn with_resource_pools(mut self, pools: Arc<ResourcePools>) -> Self {
        self.resource_pools = Some(pools);
        self
    }

    /// Update circuit state based on current load and return whether throttling is active.
    ///
    /// Reads from the load monitor and calculates delta rate (jobs/sec since last check).
    /// This allows detecting load spikes even after the session has been running.
    ///
    /// # Returns
    ///
    /// `true` if circuit is open (prefetch should be blocked), `false` otherwise.
    fn update_and_check(&self) -> bool {
        let fuse_jobs_total = self.load_monitor.total_requests();

        let mut inner = self.inner.lock().unwrap();

        // Calculate delta rate since last check
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_check_time);
        let elapsed_secs = elapsed.as_secs_f64().max(0.001); // Avoid division by zero

        let jobs_delta = fuse_jobs_total.saturating_sub(inner.last_fuse_jobs);
        let fuse_jobs_per_second = jobs_delta as f64 / elapsed_secs;

        // Update tracking for next check
        inner.last_fuse_jobs = fuse_jobs_total;
        inner.last_check_time = now;

        let fuse_high = fuse_jobs_per_second > self.config.threshold_jobs_per_sec;

        // Check resource pool saturation (secondary trigger)
        let (resource_saturated, max_utilization) = match &self.resource_pools {
            Some(pools) => {
                let util = pools.max_utilization();
                (util > RESOURCE_SATURATION_THRESHOLD, util)
            }
            None => (false, 0.0),
        };

        let is_high_load = fuse_high || resource_saturated;

        // Log rate calculation at debug level (high volume - every update)
        tracing::debug!(
            jobs_total = fuse_jobs_total,
            jobs_delta = jobs_delta,
            elapsed_ms = elapsed.as_millis(),
            rate = format!("{:.1}", fuse_jobs_per_second),
            threshold = self.config.threshold_jobs_per_sec,
            resource_utilization = format!("{:.1}%", max_utilization * 100.0),
            is_high_load = is_high_load,
            state = ?inner.state,
            "Circuit breaker rate check"
        );

        match inner.state {
            CircuitState::Closed => {
                if is_high_load {
                    // Start tracking sustained high load
                    if inner.high_load_start.is_none() {
                        inner.high_load_start = Some(Instant::now());
                        tracing::info!(
                            rate = format!("{:.1}", fuse_jobs_per_second),
                            threshold = self.config.threshold_jobs_per_sec,
                            "Circuit breaker: high load detected, starting tracking"
                        );
                    }

                    // Check if high load has been sustained long enough to trip
                    if let Some(start) = inner.high_load_start {
                        if start.elapsed() >= self.config.open_duration {
                            inner.state = CircuitState::Open;
                            inner.high_load_start = None;
                            tracing::info!(
                                rate = format!("{:.1}", fuse_jobs_per_second),
                                sustained_secs = self.config.open_duration.as_secs_f64(),
                                "Circuit breaker OPENED - prefetch paused"
                            );
                        }
                    }
                } else {
                    // Load dropped, reset tracking
                    if inner.high_load_start.is_some() {
                        tracing::debug!("Circuit breaker: load dropped, resetting tracking");
                    }
                    inner.high_load_start = None;
                }
            }
            CircuitState::Open => {
                if !is_high_load {
                    // Load dropped, transition to half-open to test recovery
                    inner.state = CircuitState::HalfOpen;
                    inner.half_open_start = Some(Instant::now());
                    tracing::info!(
                        rate = format!("{:.1}", fuse_jobs_per_second),
                        "Circuit breaker: load dropped, transitioning to half-open"
                    );
                }
            }
            CircuitState::HalfOpen => {
                if is_high_load {
                    // Load spiked again, go back to open
                    inner.state = CircuitState::Open;
                    inner.half_open_start = None;
                    tracing::info!(
                        rate = format!("{:.1}", fuse_jobs_per_second),
                        "Circuit breaker: load spike in half-open, returning to open"
                    );
                } else {
                    // Check if half-open duration has elapsed - auto close
                    if let Some(start) = inner.half_open_start {
                        if start.elapsed() >= self.config.half_open_duration {
                            inner.state = CircuitState::Closed;
                            inner.half_open_start = None;
                            tracing::info!("Circuit breaker CLOSED - prefetch resumed");
                        }
                    }
                }
            }
        }

        Self::is_open_state(inner.state)
    }

    /// Check if the given state is "open" (prefetch blocked).
    fn is_open_state(state: CircuitState) -> bool {
        matches!(state, CircuitState::Open | CircuitState::HalfOpen)
    }

    /// Get the current circuit state.
    pub fn circuit_state(&self) -> CircuitState {
        self.inner.lock().unwrap().state
    }

    /// Check if the circuit is open (prefetch should be blocked).
    pub fn is_open(&self) -> bool {
        Self::is_open_state(self.inner.lock().unwrap().state)
    }

    /// Check if the circuit is closed (prefetch is allowed).
    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().state == CircuitState::Closed
    }
}

impl PrefetchThrottler for CircuitBreaker {
    fn should_throttle(&self) -> bool {
        self.update_and_check()
    }

    fn state(&self) -> ThrottleState {
        match self.inner.lock().unwrap().state {
            CircuitState::Closed => ThrottleState::Active,
            CircuitState::Open => ThrottleState::Paused,
            CircuitState::HalfOpen => ThrottleState::Resuming,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::load_monitor::SharedFuseLoadMonitor;
    use super::*;
    use std::thread;

    fn create_test_circuit_breaker(
        config: CircuitBreakerConfig,
    ) -> (CircuitBreaker, Arc<SharedFuseLoadMonitor>) {
        let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
        let cb = CircuitBreaker::new(
            config,
            Arc::clone(&load_monitor) as Arc<dyn FuseLoadMonitor>,
        );
        (cb, load_monitor)
    }

    /// Helper to simulate high load by adding requests to the monitor.
    fn simulate_high_load(monitor: &SharedFuseLoadMonitor, count: u64) {
        for _ in 0..count {
            monitor.record_request();
        }
    }

    #[test]
    fn test_circuit_breaker_initial_state() {
        let (cb, _) = create_test_circuit_breaker(CircuitBreakerConfig::default());
        assert_eq!(cb.circuit_state(), CircuitState::Closed);
        assert!(!cb.is_open());
        assert!(cb.is_closed());
    }

    #[test]
    fn test_circuit_breaker_default_config() {
        let config = CircuitBreakerConfig::default();
        assert_eq!(config.threshold_jobs_per_sec, 50.0);
        assert_eq!(config.open_duration, Duration::from_millis(500));
        assert_eq!(config.half_open_duration, Duration::from_secs(2));
    }

    #[test]
    fn test_circuit_breaker_ignores_transient_spikes() {
        // Short open_duration for testing
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(100),
            half_open_duration: Duration::from_millis(50),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Single high load spike
        thread::sleep(Duration::from_millis(50));
        simulate_high_load(&monitor, 10); // 10 jobs in 50ms = 200 jobs/sec
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Closed);

        // Load drops (no new jobs added, rate falls)
        thread::sleep(Duration::from_millis(100));
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_opens_on_sustained_load() {
        // Very short durations for testing
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(50),
            half_open_duration: Duration::from_millis(50),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Start high load
        thread::sleep(Duration::from_millis(30));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Closed);

        // Sustained high load should trip after open_duration
        thread::sleep(Duration::from_millis(60));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Open);
        assert!(cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_transitions_to_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(50),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Trip the circuit with sustained high load
        thread::sleep(Duration::from_millis(20));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        thread::sleep(Duration::from_millis(40));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Open);

        // Load drops - should go to half-open
        thread::sleep(Duration::from_millis(100));
        cb.should_throttle(); // No new jobs = low rate
        assert_eq!(cb.circuit_state(), CircuitState::HalfOpen);
        assert!(cb.is_open()); // Still considered "open" for blocking purposes
    }

    #[test]
    fn test_circuit_breaker_half_open_then_closes() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(40),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Trip the circuit
        thread::sleep(Duration::from_millis(20));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        thread::sleep(Duration::from_millis(40));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Open);

        // Go to half-open
        thread::sleep(Duration::from_millis(100));
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::HalfOpen);

        // Wait for half-open duration + check
        thread::sleep(Duration::from_millis(50));
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Closed);
        assert!(!cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_resets_on_load_spike_in_half_open() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(100),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Trip the circuit and go to half-open
        thread::sleep(Duration::from_millis(20));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        thread::sleep(Duration::from_millis(40));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        thread::sleep(Duration::from_millis(100));
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::HalfOpen);

        // Load spikes again - should go back to open
        thread::sleep(Duration::from_millis(30));
        simulate_high_load(&monitor, 10);
        cb.should_throttle();
        assert_eq!(cb.circuit_state(), CircuitState::Open);
    }

    #[test]
    fn test_circuit_breaker_low_load_never_trips() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(30),
        };
        let (cb, _monitor) = create_test_circuit_breaker(config);

        // First check establishes baseline
        cb.should_throttle();

        // Many checks with no new jobs (low load)
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(10));
            cb.should_throttle();
        }

        assert_eq!(cb.circuit_state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_state_display_status() {
        assert_eq!(CircuitState::Closed.display_status(), "Active");
        assert_eq!(CircuitState::Open.display_status(), "Paused");
        assert_eq!(CircuitState::HalfOpen.display_status(), "Resuming...");
    }

    #[test]
    fn test_prefetch_throttler_trait() {
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(50),
            half_open_duration: Duration::from_millis(50),
        };
        let (cb, monitor) = create_test_circuit_breaker(config);

        // Use through trait
        let throttler: &dyn PrefetchThrottler = &cb;

        // Initial state - not throttling
        throttler.should_throttle();
        assert_eq!(throttler.state(), ThrottleState::Active);

        // Start high load period
        thread::sleep(Duration::from_millis(30));
        simulate_high_load(&monitor, 10); // 10 jobs in 30ms = 333 jobs/sec
        throttler.should_throttle();

        // Continue high load to exceed open_duration (50ms)
        thread::sleep(Duration::from_millis(60));
        simulate_high_load(&monitor, 10);

        // This call opens the circuit (sustained high load exceeded threshold)
        let is_throttling = throttler.should_throttle();
        assert!(is_throttling); // Should return true since we're throttling

        // Check state WITHOUT calling should_throttle() again
        // (calling it again would trigger state machine update and transition
        // to HalfOpen since no new jobs arrived between calls)
        assert_eq!(throttler.state(), ThrottleState::Paused);
    }

    #[test]
    fn test_circuit_breaker_arc_trait_object() {
        let config = CircuitBreakerConfig::default();
        let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
        let cb = CircuitBreaker::new(config, load_monitor);

        // Can be used as Arc<dyn PrefetchThrottler>
        let throttler: Arc<dyn PrefetchThrottler> = Arc::new(cb);
        assert!(!throttler.should_throttle());
        assert_eq!(throttler.state(), ThrottleState::Active);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Resource saturation tests (Phase 6)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_circuit_breaker_trips_on_resource_saturation() {
        use crate::executor::{ResourcePoolConfig, ResourcePools, ResourceType};

        // Create resource pools with small capacity so we can saturate them
        let pool_config = ResourcePoolConfig {
            network: 4,
            disk_io: 4,
            cpu: 4,
            ..Default::default()
        };
        let pools = Arc::new(ResourcePools::new(pool_config));

        // Acquire all network permits to push utilization to 100%
        let mut permits = Vec::new();
        for _ in 0..4 {
            if let Some(p) = pools.try_acquire(ResourceType::Network) {
                permits.push(p);
            }
        }
        assert!(
            pools.max_utilization() > RESOURCE_SATURATION_THRESHOLD,
            "Pools should be saturated"
        );

        // Create circuit breaker with resource pools and impossibly high FUSE
        // threshold so ONLY the resource saturation path can trip it
        let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
        let cb = CircuitBreaker::new(
            CircuitBreakerConfig {
                threshold_jobs_per_sec: 5000.0, // Impossibly high FUSE threshold
                open_duration: Duration::from_millis(30),
                half_open_duration: Duration::from_millis(50),
            },
            Arc::clone(&load_monitor) as Arc<dyn FuseLoadMonitor>,
        )
        .with_resource_pools(Arc::clone(&pools));

        // First check establishes baseline
        cb.should_throttle();

        // Sustained resource saturation should trip the circuit
        thread::sleep(Duration::from_millis(40));
        let is_throttling = cb.should_throttle();
        assert!(
            is_throttling,
            "Should throttle when resource pools are saturated"
        );
        assert_eq!(cb.circuit_state(), CircuitState::Open);

        // Drop permits — pools drain
        drop(permits);

        // Load drops, should transition to half-open
        thread::sleep(Duration::from_millis(50));
        cb.should_throttle();
        assert_eq!(
            cb.circuit_state(),
            CircuitState::HalfOpen,
            "Should transition to half-open when pools drain"
        );

        // Wait for half-open to close
        thread::sleep(Duration::from_millis(60));
        cb.should_throttle();
        assert_eq!(
            cb.circuit_state(),
            CircuitState::Closed,
            "Should close after half-open duration"
        );

        // Suppress unused variable warning
        drop(cb);
    }

    #[test]
    fn test_circuit_breaker_still_trips_on_fuse_rate() {
        // Verify the existing FUSE rate behavior is unchanged
        let config = CircuitBreakerConfig {
            threshold_jobs_per_sec: 5.0,
            open_duration: Duration::from_millis(30),
            half_open_duration: Duration::from_millis(50),
        };
        let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
        let cb = CircuitBreaker::new(
            config,
            Arc::clone(&load_monitor) as Arc<dyn FuseLoadMonitor>,
        );
        // No resource pools set — only FUSE rate matters

        cb.should_throttle();

        thread::sleep(Duration::from_millis(20));
        simulate_high_load(&load_monitor, 10);
        cb.should_throttle();

        thread::sleep(Duration::from_millis(40));
        simulate_high_load(&load_monitor, 10);
        cb.should_throttle();

        assert_eq!(
            cb.circuit_state(),
            CircuitState::Open,
            "Should still trip on FUSE rate without resource pools"
        );
    }

    #[test]
    fn test_circuit_breaker_thread_safe() {
        let config = CircuitBreakerConfig::default();
        let load_monitor = Arc::new(SharedFuseLoadMonitor::new());
        let cb = Arc::new(CircuitBreaker::new(
            config,
            Arc::clone(&load_monitor) as Arc<dyn FuseLoadMonitor>,
        ));

        let mut handles = vec![];

        // Spawn threads that all call should_throttle
        for _ in 0..4 {
            let cb = Arc::clone(&cb);
            let monitor = Arc::clone(&load_monitor);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    monitor.record_request();
                    cb.should_throttle();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have recorded 400 requests
        assert_eq!(load_monitor.total_requests(), 400);
    }
}
