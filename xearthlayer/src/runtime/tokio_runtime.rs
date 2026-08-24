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
//! Sizing the pool from the storage profile bounds both terms.
//!
//! # Provisioning caveat
//!
//! The profile's cap is not currently derived from what the executor's
//! resource pools can grant. Both the CPU pool and the disk I/O pool dispatch
//! through `spawn_blocking`, so their combined capacity is the real worst-case
//! demand. On a 32-core host with the SSD profile:
//!
//! ```text
//! CPU pool       max(ceil(32 * 1.25), 34)  =  40
//! disk I/O pool  DEFAULT_DISK_IO_CAPACITY  =  64
//!                                    total = 104
//! blocking cap   min(32 * 4, 64)           =  64
//! ```
//!
//! Tasks beyond the cap queue rather than fail, and nothing here blocks on
//! another blocking task, so this throttles rather than deadlocks. It is still
//! an under-provision: the pools may grant 104 permits against 64 threads.
//! Whether to raise the profile ceilings to `cpu + disk_io + headroom` is open
//! — see issue #227.

use crate::config::DiskIoProfile;
use tokio::runtime::Runtime;

/// Build the Tokio runtime that hosts the XEarthLayer daemons.
///
/// Worker threads keep Tokio's default (one per core). The blocking pool is
/// capped at [`DiskIoProfile::max_blocking_threads`] rather than the default
/// 512, because the pool's real ceiling is how many concurrent disk
/// operations the storage can absorb, not how many tasks want to run.
pub fn build_service_runtime(profile: DiskIoProfile) -> std::io::Result<Runtime> {
    let max_blocking = profile.max_blocking_threads();

    tracing::info!(
        profile = %profile,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The cap must actually bind the pool.
    ///
    /// Spawns more blocking tasks than the profile allows and asserts that the
    /// number running concurrently settles at exactly the cap. Under Tokio's
    /// default of 512 every task would run, so this fails if the
    /// `max_blocking_threads` call is removed.
    #[test]
    fn blocking_pool_is_capped_at_the_profile_limit() {
        let profile = DiskIoProfile::Hdd;
        let cap = profile.max_blocking_threads();
        let runtime = build_service_runtime(profile).expect("runtime builds");

        let running = Arc::new(AtomicUsize::new(0));
        // Held by every blocking task; dropping it at the end releases them.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

        for _ in 0..cap + 4 {
            let running = Arc::clone(&running);
            let release_rx = Arc::clone(&release_rx);
            runtime.spawn_blocking(move || {
                running.fetch_add(1, Ordering::SeqCst);
                // Park until the sender is dropped. Only the thread holding
                // the lock actually receives; the rest block on the mutex,
                // which is what we want — every task occupies its thread.
                let guard = release_rx.lock().unwrap();
                let _ = guard.recv();
            });
        }

        // Wait for the pool to fill, then confirm it stops there.
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

    /// `enable_all` must stay on — the daemons need the time and I/O drivers.
    #[test]
    fn runtime_has_time_and_io_drivers_enabled() {
        let runtime = build_service_runtime(DiskIoProfile::Ssd).expect("runtime builds");
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
    }

    /// Every profile must produce a usable, non-zero cap.
    #[test]
    fn every_profile_yields_a_bounded_pool() {
        for profile in [
            DiskIoProfile::Hdd,
            DiskIoProfile::Ssd,
            DiskIoProfile::Nvme,
            DiskIoProfile::Auto,
        ] {
            let cap = profile.max_blocking_threads();
            assert!(
                cap >= 1,
                "{:?} must allow at least one blocking thread",
                profile
            );
            assert!(
                cap < 512,
                "{:?} cap of {} must be below tokio's 512 default, or wiring it is pointless",
                profile,
                cap
            );
            build_service_runtime(profile).expect("runtime builds");
        }
    }
}
