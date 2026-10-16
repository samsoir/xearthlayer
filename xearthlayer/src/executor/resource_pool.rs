//! Resource pools for task concurrency control.
//!
//! This module provides resource pool abstractions for limiting concurrent
//! operations by resource type. Tasks declare which resource type they need,
//! and the executor acquires permits from the appropriate pool before dispatching.
//!
//! # Resource Types
//!
//! - [`ResourceType::Network`]: HTTP connections (pipeline-balanced, ~8–48 concurrent)
//! - [`ResourceType::DiskIO`]: File operations (~64 concurrent)
//! - [`ResourceType::CPU`]: Compute-bound work (~num_cpus concurrent)
//!
//! # Priority Handling
//!
//! Resource pools do NOT handle priority - they are simple semaphore-based
//! capacity limiters. Priority is handled by the job executor's task queue,
//! which orders tasks by priority before dispatching.
//!
//! This separation of concerns is intentional:
//! - Resource pools: "How many concurrent operations of this type?"
//! - Executor queue: "Which task should run next?"
//!
//! # Mode Switching
//!
//! XEL uses mode switching based on X-Plane's burst behavior:
//! - During bursts: Prefetcher pauses, on-demand requests use full capacity
//! - During quiet: Prefetcher runs, uses full capacity for prefetching
//!
//! This eliminates the need for per-request priority within pools.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::executor::{ResourceType, ResourcePools, ResourcePoolConfig};
//!
//! let config = ResourcePoolConfig::default();
//! let pools = ResourcePools::new(config);
//!
//! // Acquire a network permit
//! let permit = pools.acquire(ResourceType::Network).await;
//! // Do network work...
//! drop(permit); // Release the permit
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// =============================================================================
// Resource Pool Configuration Constants
// =============================================================================

/// Network pool capacity multiplier relative to CPU pool size.
///
/// The network pool is sized as a multiple of the CPU pool capacity to keep
/// the download → encode pipeline balanced. A value of 1.5 means downloads
/// slightly outpace encodes, keeping the CPU pipeline fed without excessive
/// WIP buildup.
pub const DEFAULT_NETWORK_CAPACITY_MULTIPLIER: f64 = 1.5;

/// Minimum network pool capacity.
///
/// Ensures meaningful concurrency even on low-core systems.
pub const MIN_NETWORK_CAPACITY: usize = 8;

/// Maximum network pool capacity.
///
/// Caps concurrent download tasks to prevent bandwidth starvation. With an
/// HTTP semaphore of 1024 shared across all downloads, 64 concurrent tiles
/// each get ~16 HTTP connections — enough for sub-second per-tile latency.
pub const MAX_NETWORK_CAPACITY: usize = 64;

/// Default disk I/O pool capacity (SSD profile).
pub const DEFAULT_DISK_IO_CAPACITY: usize = 64;

/// Blocking threads reserved beyond the CPU and disk I/O pools.
///
/// Not every `spawn_blocking` call takes a resource permit. The startup disk
/// scans (`cache/lru_index.rs`, `prefetch/scenery_index.rs`), the GPU encoder
/// worker, storage detection, and — most importantly — the fire-and-forget
/// cache writes in `tasks/build_and_cache_dds.rs` all queue onto the blocking
/// pool ungated. The write backlog is the one that moves: it is what
/// `disk_writes_active` and `mem_cache_writes_active` measure.
///
/// This reserve keeps those off the pooled work's threads. It is a floor, not
/// a bound — an ungated backlog larger than the reserve queues rather than
/// spawning threads, which is the intended behaviour (see issue #227).
pub const BLOCKING_POOL_HEADROOM: usize = 32;

/// Default CPU pool capacity multiplier.
pub const DEFAULT_CPU_CAPACITY_MULTIPLIER: f64 = 1.25;

/// Minimum CPU pool capacity addition.
pub const MIN_CPU_CAPACITY_ADDITION: usize = 2;

// =============================================================================
// Resource Type
// =============================================================================

/// Resource types that tasks can require.
///
/// Tasks declare their resource type so the scheduler can dispatch them
/// to the appropriate pool. This prevents resource exhaustion by limiting
/// concurrent operations of each type.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum ResourceType {
    /// Network I/O (HTTP connections).
    ///
    /// Default capacity: `clamp(ceil(cpu_capacity * 1.5), 8, 48)`
    /// Pipeline-balanced with CPU pool to prevent bandwidth starvation.
    Network,

    /// Disk I/O (file read/write).
    ///
    /// Capacity varies by storage type (HDD: 4, SSD: 64, NVMe: 256).
    DiskIO,

    /// CPU-bound work (encoding, assembly).
    ///
    /// Default capacity: `max(num_cpus * 1.25, num_cpus + 2)`
    CPU,
}

impl std::fmt::Display for ResourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network => write!(f, "Network"),
            Self::DiskIO => write!(f, "DiskIO"),
            Self::CPU => write!(f, "CPU"),
        }
    }
}

// =============================================================================
// Resource Pool (single pool for one resource type)
// =============================================================================

/// A semaphore-backed pool for a single resource type.
///
/// This is a simple capacity limiter - it does not handle priority.
/// Priority is managed by the executor's task queue.
#[derive(Debug)]
pub struct ResourcePool {
    resource_type: ResourceType,
    semaphore: Arc<Semaphore>,
    capacity: usize,
    in_flight: Arc<AtomicUsize>,
    peak_in_flight: AtomicUsize,
}

impl ResourcePool {
    /// Creates a new resource pool with the given capacity.
    pub fn new(resource_type: ResourceType, capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            resource_type,
            semaphore: Arc::new(Semaphore::new(capacity)),
            capacity,
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    /// Acquires a permit, waiting if none available.
    pub async fn acquire(&self) -> ResourcePermit {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed unexpectedly");

        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_peak(current);

        ResourcePermit {
            _permit: permit,
            in_flight: Arc::clone(&self.in_flight),
            resource_type: self.resource_type,
        }
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self) -> Option<ResourcePermit> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;

        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_peak(current);

        Some(ResourcePermit {
            _permit: permit,
            in_flight: Arc::clone(&self.in_flight),
            resource_type: self.resource_type,
        })
    }

    /// Updates the peak counter if current exceeds it.
    fn update_peak(&self, current: usize) {
        let mut peak = self.peak_in_flight.load(Ordering::Relaxed);
        while current > peak {
            match self.peak_in_flight.compare_exchange_weak(
                peak,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }
    }

    /// Returns the resource type for this pool.
    pub fn resource_type(&self) -> ResourceType {
        self.resource_type
    }

    /// Returns the total capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of available permits.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Returns the current number of in-flight operations.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Returns the peak number of concurrent operations observed.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    /// Resets the peak counter.
    pub fn reset_peak(&self) {
        self.peak_in_flight.store(0, Ordering::Relaxed);
    }

    /// Returns the current utilization as a fraction from 0.0 to 1.0.
    ///
    /// A value of 1.0 means the pool is fully saturated (all permits in use).
    pub fn utilization(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.in_flight() as f64 / self.capacity as f64
    }
}

// =============================================================================
// Resource Permit
// =============================================================================

/// A permit from a resource pool.
///
/// While this permit is held, it counts against the pool's capacity.
/// The permit is automatically released when dropped.
///
/// The permit is owned (no lifetime parameter) so it can be moved across
/// function boundaries and into spawned futures. This prevents the double
/// permit acquisition bug where `find_dispatchable_task()` consumed a permit
/// that was dropped on return, then `spawn_task()` acquired a second one.
pub struct ResourcePermit {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicUsize>,
    resource_type: ResourceType,
}

impl ResourcePermit {
    /// Returns the resource type this permit is for.
    pub fn resource_type(&self) -> ResourceType {
        self.resource_type
    }
}

impl Drop for ResourcePermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for ResourcePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourcePermit")
            .field("resource_type", &self.resource_type)
            .finish()
    }
}

// =============================================================================
// Resource Pool Configuration
// =============================================================================

/// Configuration for resource pool capacities.
#[derive(Clone, Debug)]
pub struct ResourcePoolConfig {
    /// Network pool capacity (HTTP connections).
    pub network: usize,

    /// Disk I/O pool capacity.
    pub disk_io: usize,

    /// CPU pool capacity.
    pub cpu: usize,
}

impl Default for ResourcePoolConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);

        let cpu_capacity = ((cpus as f64 * DEFAULT_CPU_CAPACITY_MULTIPLIER).ceil() as usize)
            .max(cpus + MIN_CPU_CAPACITY_ADDITION);

        Self {
            network: ((cpu_capacity as f64 * DEFAULT_NETWORK_CAPACITY_MULTIPLIER).ceil() as usize)
                .clamp(MIN_NETWORK_CAPACITY, MAX_NETWORK_CAPACITY),
            disk_io: DEFAULT_DISK_IO_CAPACITY,
            cpu: cpu_capacity,
        }
    }
}

impl ResourcePoolConfig {
    /// Blocking threads needed to service this configuration.
    ///
    /// The CPU and disk I/O pools both dispatch through `spawn_blocking`, so
    /// their combined capacity is the worst-case simultaneous demand for
    /// blocking threads. The network pool is excluded: it gates async HTTP,
    /// which runs on worker threads.
    ///
    /// Used to size the Tokio blocking pool in
    /// [`crate::runtime::build_service_runtime`]. Sizing it any smaller lets
    /// the pools grant more permits than there are threads to serve them;
    /// leaving it at tokio's 512 default ratchets the glibc arena on every
    /// thread excursion. See issue #227.
    pub fn blocking_threads_required(&self) -> usize {
        self.cpu + self.disk_io + BLOCKING_POOL_HEADROOM
    }

    /// Creates a configuration with the given capacities.
    pub fn new(network: usize, disk_io: usize, cpu: usize) -> Self {
        Self {
            network,
            disk_io,
            cpu,
        }
    }
}

// =============================================================================
// Resource Pools Collection
// =============================================================================

/// Collection of all resource pools.
///
/// Provides a unified interface for acquiring permits based on resource type.
/// Each pool is a simple semaphore - priority is handled by the executor's
/// task queue, not by the pools.
pub struct ResourcePools {
    network: ResourcePool,
    disk_io: ResourcePool,
    cpu: ResourcePool,
}

impl ResourcePools {
    /// Creates new resource pools with the given configuration.
    pub fn new(config: ResourcePoolConfig) -> Self {
        Self {
            network: ResourcePool::new(ResourceType::Network, config.network),
            disk_io: ResourcePool::new(ResourceType::DiskIO, config.disk_io),
            cpu: ResourcePool::new(ResourceType::CPU, config.cpu),
        }
    }

    /// Creates resource pools with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ResourcePoolConfig::default())
    }

    /// Returns the pool for the given resource type.
    pub fn get(&self, resource_type: ResourceType) -> &ResourcePool {
        match resource_type {
            ResourceType::Network => &self.network,
            ResourceType::DiskIO => &self.disk_io,
            ResourceType::CPU => &self.cpu,
        }
    }

    /// Acquires a permit from the specified resource pool.
    ///
    /// This will wait if the pool is at capacity.
    pub async fn acquire(&self, resource_type: ResourceType) -> ResourcePermit {
        self.get(resource_type).acquire().await
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self, resource_type: ResourceType) -> Option<ResourcePermit> {
        self.get(resource_type).try_acquire()
    }

    /// Returns the available permits for the given resource type.
    pub fn available(&self, resource_type: ResourceType) -> usize {
        self.get(resource_type).available()
    }

    /// Returns the total capacity for the given resource type.
    pub fn capacity(&self, resource_type: ResourceType) -> usize {
        self.get(resource_type).capacity()
    }

    /// Returns the current in-flight count for the given resource type.
    pub fn in_flight(&self, resource_type: ResourceType) -> usize {
        self.get(resource_type).in_flight()
    }

    /// Returns the network pool.
    pub fn network(&self) -> &ResourcePool {
        &self.network
    }

    /// Returns the disk I/O pool.
    pub fn disk_io(&self) -> &ResourcePool {
        &self.disk_io
    }

    /// Returns the CPU pool.
    pub fn cpu(&self) -> &ResourcePool {
        &self.cpu
    }

    /// Returns the maximum utilization across all pools (0.0 to 1.0).
    ///
    /// Useful for circuit breaker decisions — if any single pool is near
    /// saturation, the system is under stress.
    pub fn max_utilization(&self) -> f64 {
        self.network
            .utilization()
            .max(self.disk_io.utilization())
            .max(self.cpu.utilization())
    }
}

impl std::fmt::Debug for ResourcePools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourcePools")
            .field(
                "network",
                &format_args!("{}/{}", self.network.in_flight(), self.network.capacity()),
            )
            .field(
                "disk_io",
                &format_args!("{}/{}", self.disk_io.in_flight(), self.disk_io.capacity()),
            )
            .field(
                "cpu",
                &format_args!("{}/{}", self.cpu.in_flight(), self.cpu.capacity()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_type_display() {
        assert_eq!(format!("{}", ResourceType::Network), "Network");
        assert_eq!(format!("{}", ResourceType::DiskIO), "DiskIO");
        assert_eq!(format!("{}", ResourceType::CPU), "CPU");
    }

    #[test]
    fn test_resource_pool_creation() {
        let pool = ResourcePool::new(ResourceType::Network, 10);
        assert_eq!(pool.resource_type(), ResourceType::Network);
        assert_eq!(pool.capacity(), 10);
        assert_eq!(pool.available(), 10);
        assert_eq!(pool.in_flight(), 0);
        assert_eq!(pool.peak_in_flight(), 0);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn test_resource_pool_zero_capacity() {
        ResourcePool::new(ResourceType::CPU, 0);
    }

    #[tokio::test]
    async fn test_resource_pool_acquire_release() {
        let pool = ResourcePool::new(ResourceType::Network, 2);

        let permit1 = pool.acquire().await;
        assert_eq!(pool.in_flight(), 1);
        assert_eq!(pool.available(), 1);

        let permit2 = pool.acquire().await;
        assert_eq!(pool.in_flight(), 2);
        assert_eq!(pool.available(), 0);

        drop(permit1);
        assert_eq!(pool.in_flight(), 1);
        assert_eq!(pool.available(), 1);

        drop(permit2);
        assert_eq!(pool.in_flight(), 0);
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn test_resource_pool_try_acquire() {
        let pool = ResourcePool::new(ResourceType::CPU, 1);

        let permit1 = pool.try_acquire();
        assert!(permit1.is_some());
        assert_eq!(pool.in_flight(), 1);

        let permit2 = pool.try_acquire();
        assert!(permit2.is_none());

        drop(permit1);
        assert_eq!(pool.in_flight(), 0);

        let permit3 = pool.try_acquire();
        assert!(permit3.is_some());
    }

    #[tokio::test]
    async fn test_resource_pool_peak_tracking() {
        let pool = ResourcePool::new(ResourceType::DiskIO, 10);

        assert_eq!(pool.peak_in_flight(), 0);

        let _p1 = pool.acquire().await;
        let _p2 = pool.acquire().await;
        let _p3 = pool.acquire().await;

        assert_eq!(pool.peak_in_flight(), 3);

        drop(_p3);
        drop(_p2);

        assert_eq!(pool.peak_in_flight(), 3); // Peak unchanged
        assert_eq!(pool.in_flight(), 1);

        pool.reset_peak();
        assert_eq!(pool.peak_in_flight(), 0);
    }

    #[test]
    fn test_resource_pool_config_default() {
        let config = ResourcePoolConfig::default();
        assert!(config.network > 0);
        assert!(config.network >= MIN_NETWORK_CAPACITY);
        assert!(config.network <= MAX_NETWORK_CAPACITY);
        assert!(config.disk_io > 0);
        assert!(config.cpu > 0);
    }

    #[test]
    fn test_network_pool_balanced_with_cpu() {
        let config = ResourcePoolConfig::default();

        // Network pool should be approximately 1.5x CPU pool, clamped to [8, 64]
        let expected = ((config.cpu as f64 * DEFAULT_NETWORK_CAPACITY_MULTIPLIER).ceil() as usize)
            .clamp(MIN_NETWORK_CAPACITY, MAX_NETWORK_CAPACITY);
        assert_eq!(
            config.network, expected,
            "Network pool ({}) should be pipeline-balanced with CPU pool ({})",
            config.network, config.cpu
        );
    }

    #[test]
    fn test_resource_pool_config_custom() {
        let config = ResourcePoolConfig::new(128, 32, 8);
        assert_eq!(config.network, 128);
        assert_eq!(config.disk_io, 32);
        assert_eq!(config.cpu, 8);
    }

    #[test]
    fn test_resource_pools_creation() {
        let pools = ResourcePools::with_defaults();

        assert!(pools.capacity(ResourceType::Network) > 0);
        assert!(pools.capacity(ResourceType::DiskIO) > 0);
        assert!(pools.capacity(ResourceType::CPU) > 0);

        assert_eq!(pools.in_flight(ResourceType::Network), 0);
        assert_eq!(pools.in_flight(ResourceType::DiskIO), 0);
        assert_eq!(pools.in_flight(ResourceType::CPU), 0);
    }

    #[tokio::test]
    async fn test_resource_pools_acquire() {
        let config = ResourcePoolConfig::new(2, 2, 2);
        let pools = ResourcePools::new(config);

        let permit1 = pools.acquire(ResourceType::Network).await;
        assert_eq!(pools.in_flight(ResourceType::Network), 1);

        let permit2 = pools.acquire(ResourceType::CPU).await;
        assert_eq!(pools.in_flight(ResourceType::CPU), 1);
        assert_eq!(pools.in_flight(ResourceType::Network), 1); // Unchanged

        drop(permit1);
        drop(permit2);

        assert_eq!(pools.in_flight(ResourceType::Network), 0);
        assert_eq!(pools.in_flight(ResourceType::CPU), 0);
    }

    #[test]
    fn test_resource_pools_try_acquire() {
        let config = ResourcePoolConfig::new(1, 1, 1);
        let pools = ResourcePools::new(config);

        let permit1 = pools.try_acquire(ResourceType::Network);
        assert!(permit1.is_some());

        let permit2 = pools.try_acquire(ResourceType::Network);
        assert!(permit2.is_none());

        drop(permit1);

        let permit3 = pools.try_acquire(ResourceType::Network);
        assert!(permit3.is_some());
    }

    #[test]
    fn test_resource_pools_get() {
        let pools = ResourcePools::with_defaults();

        assert_eq!(
            pools.get(ResourceType::Network).resource_type(),
            ResourceType::Network
        );
        assert_eq!(
            pools.get(ResourceType::DiskIO).resource_type(),
            ResourceType::DiskIO
        );
        assert_eq!(
            pools.get(ResourceType::CPU).resource_type(),
            ResourceType::CPU
        );
    }

    #[test]
    fn test_resource_pools_accessors() {
        let pools = ResourcePools::with_defaults();

        assert_eq!(pools.network().resource_type(), ResourceType::Network);
        assert_eq!(pools.disk_io().resource_type(), ResourceType::DiskIO);
        assert_eq!(pools.cpu().resource_type(), ResourceType::CPU);
    }

    #[test]
    fn test_resource_pools_debug() {
        let pools = ResourcePools::with_defaults();
        let debug = format!("{:?}", pools);
        assert!(debug.contains("ResourcePools"));
        assert!(debug.contains("network"));
        assert!(debug.contains("cpu"));
    }

    #[test]
    fn test_resource_permit_debug() {
        let pool = ResourcePool::new(ResourceType::Network, 1);
        let permit = pool.try_acquire().unwrap();
        let debug = format!("{:?}", permit);
        assert!(debug.contains("ResourcePermit"));
        assert!(debug.contains("Network"));
    }

    #[test]
    fn test_resource_permit_is_owned() {
        // Permits can be moved across function boundaries (no lifetime tie to pool).
        fn acquire_and_return(pool: &ResourcePool) -> ResourcePermit {
            pool.try_acquire().unwrap()
        }

        let pool = ResourcePool::new(ResourceType::Network, 2);
        let permit = acquire_and_return(&pool);

        // Permit is alive — pool shows 1 in-flight
        assert_eq!(pool.in_flight(), 1);
        assert_eq!(pool.available(), 1);

        drop(permit);

        // Permit released — pool shows 0 in-flight
        assert_eq!(pool.in_flight(), 0);
        assert_eq!(pool.available(), 2);
    }

    #[tokio::test]
    async fn test_dispatch_uses_single_permit() {
        // Simulate the dispatch pattern: try_acquire in one scope, use in another.
        let pool = ResourcePool::new(ResourceType::CPU, 2);

        // Phase 1: find_dispatchable_task acquires the permit
        let permit = pool.try_acquire().unwrap();
        assert_eq!(pool.in_flight(), 1);

        // Phase 2: spawn_task holds the same permit (no second acquire)
        let handle = tokio::spawn(async move {
            let _permit = permit; // Moved into spawned future
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            // permit drops here
        });

        // While task is running, only 1 permit consumed (not 2)
        assert_eq!(pool.in_flight(), 1);

        handle.await.unwrap();

        // After task completes, permit is released
        assert_eq!(pool.in_flight(), 0);
        assert_eq!(pool.available(), 2);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Utilization tests (Phase 6)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_pool_utilization_empty() {
        let pool = ResourcePool::new(ResourceType::CPU, 4);
        assert!((pool.utilization() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pool_utilization_partial() {
        let pool = ResourcePool::new(ResourceType::CPU, 4);
        let _p1 = pool.try_acquire().unwrap();
        let _p2 = pool.try_acquire().unwrap();
        assert!((pool.utilization() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pool_utilization_full() {
        let pool = ResourcePool::new(ResourceType::CPU, 2);
        let _p1 = pool.try_acquire().unwrap();
        let _p2 = pool.try_acquire().unwrap();
        assert!((pool.utilization() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pools_max_utilization() {
        let config = ResourcePoolConfig {
            network: 10,
            disk_io: 10,
            cpu: 4,
            ..Default::default()
        };
        let pools = ResourcePools::new(config);

        // Saturate CPU pool (smallest) to 75%
        let _p1 = pools.try_acquire(ResourceType::CPU).unwrap();
        let _p2 = pools.try_acquire(ResourceType::CPU).unwrap();
        let _p3 = pools.try_acquire(ResourceType::CPU).unwrap();
        // 3/4 = 0.75

        assert!((pools.max_utilization() - 0.75).abs() < f64::EPSILON);

        // Network and DiskIO are at 0% — CPU is the max
        assert!((pools.network().utilization() - 0.0).abs() < f64::EPSILON);
        assert!((pools.disk_io().utilization() - 0.0).abs() < f64::EPSILON);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // No prefetch quota
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn prefetch_priority_may_use_the_whole_pool() {
        let pools = ResourcePools::new(ResourcePoolConfig::new(4, 4, 4));
        let permits: Vec<_> = (0..4)
            .map(|_| pools.try_acquire(ResourceType::CPU).unwrap())
            .collect();
        assert_eq!(
            permits.len(),
            4,
            "a lane's pool serves one priority class; no quota"
        );
    }
}
