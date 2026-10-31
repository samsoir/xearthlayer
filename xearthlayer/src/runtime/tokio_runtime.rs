//! Construction of the process-wide Tokio runtime that hosts the daemons.
//!
//! Tokio's default `max_blocking_threads` is 512. The blocking pool carries
//! DDS encoding and disk I/O, so under sustained prefetch load it saturates:
//! flight logs show 581 threads (32 workers + base + 512 blocking) present in
//! 103 of 730 samples across a 12-hour run.
//!
//! That costs memory twice over. Each thread carries a 2 MiB stack — 2.21
//! MB/thread measured against pool excursions, r = 0.939. More importantly,
//! glibc binds threads to arenas under contention and each arena keeps its own
//! top-chunk high-water mark, which is never returned to the OS. Every
//! excursion to 581 threads ratcheted the arena by ~420 MB, and the arena
//! reached 6,166 MB while holding only 572 MB of live data. See issue #227.
//!
//! Sizing the pool from the executor's pool capacities bounds both terms.
//!
//! # Sizing
//!
//! The cap comes from the executor's own pool capacities, not from the
//! storage profile. Both the CPU pool and the disk I/O pool dispatch through
//! `spawn_blocking`, so their sum is the worst-case simultaneous demand; the
//! network pool is excluded because it gates async HTTP on worker threads.
//! A reserve covers the `spawn_blocking` calls that take no permit: startup
//! scans, the GPU worker, and the fire-and-forget cache writes.
//!
//! The demand and prefetch lanes each have their own CPU and disk I/O pools
//! (#296), so the caller passes
//! [`LaneSizing::blocking_threads_required`](crate::executor::LaneSizing::blocking_threads_required),
//! which sums both.
//!
//! Sizing from the storage profile instead would under-provision: the SSD
//! profile yields 64 against the same 104 of pooled demand.

use tokio::runtime::Runtime;

/// Build the Tokio runtime that hosts the XEarthLayer daemons.
///
/// Worker threads keep tokio's default (one per core). The blocking pool is
/// capped at `max_blocking` rather than the default 512; production passes
/// [`LaneSizing::blocking_threads_required`](crate::executor::LaneSizing::blocking_threads_required).
pub fn build_service_runtime(max_blocking: usize) -> std::io::Result<Runtime> {
    tracing::info!(
        max_blocking_threads = max_blocking,
        "Building service Tokio runtime"
    );

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(max_blocking)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{LaneSizing, ResourcePoolConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The cap must actually bind the pool.
    ///
    /// Spawns more blocking tasks than the config allows and asserts that the
    /// number running concurrently settles at exactly the cap. Under tokio's
    /// default of 512 every task would run, so this fails if the
    /// `max_blocking_threads` call is removed.
    #[test]
    fn blocking_pool_is_capped_at_the_derived_limit() {
        // Small explicit capacities keep the test fast and independent of the
        // host's core count.
        let cap = ResourcePoolConfig::new(4, 2, 2).blocking_threads_required();
        assert_pool_is_capped_at(cap);
    }

    /// The production cap covers both lanes and must bind just the same.
    #[test]
    fn blocking_pool_honours_a_lane_sizing_cap() {
        let sizing = LaneSizing::derive(ResourcePoolConfig::new(4, 2, 2), 0.5);
        let cap = sizing.blocking_threads_required();
        assert!(cap > ResourcePoolConfig::new(4, 2, 2).blocking_threads_required());
        assert_pool_is_capped_at(cap);
    }

    fn assert_pool_is_capped_at(cap: usize) {
        let runtime = build_service_runtime(cap).expect("runtime builds");

        let running = Arc::new(AtomicUsize::new(0));
        // Held by every blocking task; dropping the sender releases them.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

        for _ in 0..cap + 4 {
            let running = Arc::clone(&running);
            let release_rx = Arc::clone(&release_rx);
            runtime.spawn_blocking(move || {
                running.fetch_add(1, Ordering::SeqCst);
                // Park until the sender is dropped. Only the task holding the
                // lock receives; the rest block on the mutex, which is what we
                // want — every task occupies its thread.
                let guard = release_rx.lock().unwrap();
                let _ = guard.recv();
            });
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        while running.load(Ordering::SeqCst) < cap && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            running.load(Ordering::SeqCst),
            cap,
            "pool should fill to exactly the {} thread cap",
            cap
        );

        // Settle: the extra tasks must stay queued rather than spawning threads.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            running.load(Ordering::SeqCst),
            cap,
            "queued tasks must not exceed the {} thread cap",
            cap
        );

        drop(release_tx);
    }

    /// The cap must cover what the pools can grant.
    ///
    /// This is the anti-drift guard: raising `DEFAULT_DISK_IO_CAPACITY` or the
    /// CPU multiplier without revisiting the blocking pool would let the pools
    /// issue more permits than there are threads to serve them. It runs against
    /// the default config, which is what `RuntimeConfig::default()` gives the
    /// executor in production.
    #[test]
    fn derived_cap_covers_what_the_pools_can_grant() {
        let pools = ResourcePoolConfig::default();
        let demand = pools.cpu + pools.disk_io;

        assert!(
            pools.blocking_threads_required() > demand,
            "blocking cap {} must exceed pooled demand {} (cpu {} + disk_io {})",
            pools.blocking_threads_required(),
            demand,
            pools.cpu,
            pools.disk_io
        );
    }

    /// The cap must stay meaningfully below tokio's default, or the fix does
    /// nothing. The bound is deliberately loose — it catches a pool config that
    /// has grown past the point where capping still helps.
    #[test]
    fn derived_cap_stays_below_the_tokio_default() {
        let cap = ResourcePoolConfig::default().blocking_threads_required();
        assert!(
            cap < 512,
            "cap of {} must be below tokio's 512 default, or wiring it is pointless",
            cap
        );
    }

    /// `enable_all` must stay on — the daemons need the time and I/O drivers.
    #[test]
    fn runtime_has_time_and_io_drivers_enabled() {
        let runtime =
            build_service_runtime(ResourcePoolConfig::default().blocking_threads_required())
                .expect("runtime builds");
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
    }
}
