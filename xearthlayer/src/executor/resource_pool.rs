//! Resource pools for task concurrency control.
//!
//! This module provides resource pool abstractions for limiting concurrent
//! operations by resource type. Tasks declare which resource type they need,
//! and the executor acquires permits from the appropriate pool before dispatching.
//!
//! # Resource Types
//!
//! - [`ResourceType::Network`]: HTTP connections (~256 concurrent)
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

use crate::config::DiskIoProfile;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// =============================================================================
// Resource Pool Configuration Constants
// =============================================================================

/// Default network pool capacity (HTTP connections).
pub const DEFAULT_NETWORK_CAPACITY: usize = 256;

/// Default disk I/O pool capacity (SSD profile).
pub const DEFAULT_DISK_IO_CAPACITY: usize = 64;

/// Default CPU pool capacity multiplier.
pub const DEFAULT_CPU_CAPACITY_MULTIPLIER: f64 = 1.25;

/// Minimum CPU pool capacity addition.
pub const MIN_CPU_CAPACITY_ADDITION: usize = 2;

/// Disk I/O capacity for HDD profile.
pub const DISK_IO_CAPACITY_HDD: usize = 4;

/// Disk I/O capacity for SSD profile.
pub const DISK_IO_CAPACITY_SSD: usize = 64;

/// Disk I/O capacity for NVMe profile.
pub const DISK_IO_CAPACITY_NVME: usize = 256;

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
    /// Default capacity: `min(num_cpus * 16, 256)`
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
    in_flight: AtomicUsize,
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
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    /// Acquires a permit, waiting if none available.
    pub async fn acquire(&self) -> ResourcePermit<'_> {
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
            in_flight: &self.in_flight,
            resource_type: self.resource_type,
        }
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self) -> Option<ResourcePermit<'_>> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;

        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_peak(current);

        Some(ResourcePermit {
            _permit: permit,
            in_flight: &self.in_flight,
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
}

// =============================================================================
// Resource Permit
// =============================================================================

/// A permit from a resource pool.
///
/// While this permit is held, it counts against the pool's capacity.
/// The permit is automatically released when dropped.
pub struct ResourcePermit<'a> {
    _permit: OwnedSemaphorePermit,
    in_flight: &'a AtomicUsize,
    resource_type: ResourceType,
}

impl<'a> ResourcePermit<'a> {
    /// Returns the resource type this permit is for.
    pub fn resource_type(&self) -> ResourceType {
        self.resource_type
    }
}

impl Drop for ResourcePermit<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for ResourcePermit<'_> {
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

        Self {
            network: (cpus * 16).min(DEFAULT_NETWORK_CAPACITY),
            disk_io: DEFAULT_DISK_IO_CAPACITY,
            cpu: ((cpus as f64 * DEFAULT_CPU_CAPACITY_MULTIPLIER).ceil() as usize)
                .max(cpus + MIN_CPU_CAPACITY_ADDITION),
        }
    }
}

impl ResourcePoolConfig {
    /// Creates a configuration with the given capacities.
    pub fn new(network: usize, disk_io: usize, cpu: usize) -> Self {
        Self {
            network,
            disk_io,
            cpu,
        }
    }

    /// Creates a configuration with disk I/O capacity based on storage profile.
    pub fn with_disk_io_profile(mut self, profile: DiskIoProfile) -> Self {
        self.disk_io = match profile {
            DiskIoProfile::Auto => DEFAULT_DISK_IO_CAPACITY, // Assume SSD
            DiskIoProfile::Hdd => DISK_IO_CAPACITY_HDD,
            DiskIoProfile::Ssd => DISK_IO_CAPACITY_SSD,
            DiskIoProfile::Nvme => DISK_IO_CAPACITY_NVME,
        };
        self
    }
}

impl From<&crate::config::ExecutorSettings> for ResourcePoolConfig {
    fn from(settings: &crate::config::ExecutorSettings) -> Self {
        Self {
            network: settings.network_concurrent,
            disk_io: settings.disk_io_concurrent,
            cpu: settings.cpu_concurrent,
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
    pub async fn acquire(&self, resource_type: ResourceType) -> ResourcePermit<'_> {
        self.get(resource_type).acquire().await
    }

    /// Tries to acquire a permit without waiting.
    ///
    /// Returns `None` if no permits are available.
    pub fn try_acquire(&self, resource_type: ResourceType) -> Option<ResourcePermit<'_>> {
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
        assert!(config.disk_io > 0);
        assert!(config.cpu > 0);
    }

    #[test]
    fn test_resource_pool_config_custom() {
        let config = ResourcePoolConfig::new(128, 32, 8);
        assert_eq!(config.network, 128);
        assert_eq!(config.disk_io, 32);
        assert_eq!(config.cpu, 8);
    }

    #[test]
    fn test_resource_pool_config_with_disk_profile() {
        let config = ResourcePoolConfig::default().with_disk_io_profile(DiskIoProfile::Hdd);
        assert_eq!(config.disk_io, DISK_IO_CAPACITY_HDD);

        let config = ResourcePoolConfig::default().with_disk_io_profile(DiskIoProfile::Nvme);
        assert_eq!(config.disk_io, DISK_IO_CAPACITY_NVME);
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
}
