//! Background daemon for disk cache garbage collection.
//!
//! The daemon runs in a separate thread and periodically checks if the disk
//! cache exceeds its size limit, evicting least-recently-used entries as needed.

use crate::cache::disk::DiskCache;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Background daemon for disk cache garbage collection.
///
/// Periodically checks the disk cache size and evicts LRU entries when
/// the cache exceeds its configured maximum size.
///
/// The daemon runs in a separate thread and can be cleanly shut down
/// by calling `shutdown()` or dropping the `DiskCacheDaemon` instance.
pub struct DiskCacheDaemon {
    /// Handle to the daemon thread
    thread_handle: Option<JoinHandle<()>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl DiskCacheDaemon {
    /// Start a new disk cache daemon.
    ///
    /// # Arguments
    ///
    /// * `cache` - Arc to the disk cache to manage
    /// * `interval_secs` - How often to check the cache size (in seconds)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::cache::{DiskCache, DiskCacheDaemon};
    /// use std::sync::Arc;
    ///
    /// let cache = Arc::new(DiskCache::new("/tmp/cache".into(), 20_000_000_000)?);
    /// let daemon = DiskCacheDaemon::start(cache, 60);
    /// // Daemon runs in background...
    /// // daemon.shutdown(); // or just drop it
    /// ```
    pub fn start(cache: Arc<DiskCache>, interval_secs: u64) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let thread_handle = thread::Builder::new()
            .name("disk-cache-gc".to_string())
            .spawn(move || {
                Self::run_loop(cache, interval_secs, shutdown_clone);
            })
            .expect("Failed to spawn disk cache daemon thread");

        info!("Disk cache daemon started (interval: {}s)", interval_secs);

        Self {
            thread_handle: Some(thread_handle),
            shutdown,
        }
    }

    /// The main daemon loop.
    fn run_loop(cache: Arc<DiskCache>, interval_secs: u64, shutdown: Arc<AtomicBool>) {
        let interval = Duration::from_secs(interval_secs);

        // Use shorter sleep intervals to check shutdown more frequently
        let check_interval = Duration::from_secs(1);
        let mut elapsed = Duration::ZERO;

        loop {
            // Check for shutdown
            if shutdown.load(Ordering::Relaxed) {
                debug!("Disk cache daemon received shutdown signal");
                break;
            }

            // Sleep in short intervals to allow responsive shutdown
            thread::sleep(check_interval);
            elapsed += check_interval;

            // Run eviction check at the configured interval
            if elapsed >= interval {
                elapsed = Duration::ZERO;

                let size_before = cache.size_bytes();
                let max_size = cache.max_size_bytes();

                if size_before > max_size {
                    debug!(
                        "Disk cache over limit: {} MB / {} MB, running eviction",
                        size_before / (1024 * 1024),
                        max_size / (1024 * 1024)
                    );

                    if let Err(e) = cache.evict_if_over_limit() {
                        warn!("Disk cache eviction failed: {}", e);
                    }
                } else {
                    debug!(
                        "Disk cache within limit: {} MB / {} MB",
                        size_before / (1024 * 1024),
                        max_size / (1024 * 1024)
                    );
                }
            }
        }

        debug!("Disk cache daemon stopped");
    }

    /// Signal the daemon to shut down.
    ///
    /// This is non-blocking. The daemon will stop at its next check interval.
    /// Call `join()` after this to wait for the thread to finish.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Wait for the daemon thread to finish.
    ///
    /// Should be called after `shutdown()` to ensure clean termination.
    pub fn join(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            if let Err(e) = handle.join() {
                warn!("Disk cache daemon thread panicked: {:?}", e);
            }
        }
    }

    /// Check if the daemon is still running.
    pub fn is_running(&self) -> bool {
        self.thread_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }
}

impl Drop for DiskCacheDaemon {
    fn drop(&mut self) {
        self.shutdown();
        self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::types::CacheKey;
    use crate::coord::TileCoord;
    use crate::dds::DdsFormat;
    use tempfile::TempDir;

    fn create_test_key(col: u32) -> CacheKey {
        CacheKey::new(
            "test",
            DdsFormat::BC1,
            TileCoord {
                row: 100,
                col,
                zoom: 15,
            },
        )
    }

    #[test]
    fn test_daemon_starts_and_stops() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Arc::new(DiskCache::new(temp_dir.path().to_path_buf(), 10_000_000).unwrap());

        let daemon = DiskCacheDaemon::start(cache, 1);
        assert!(daemon.is_running());

        // Give it a moment to start
        thread::sleep(Duration::from_millis(100));
        assert!(daemon.is_running());

        // Shutdown
        daemon.shutdown();
        thread::sleep(Duration::from_secs(2));
        assert!(!daemon.is_running());
    }

    #[test]
    fn test_daemon_drop_triggers_shutdown() {
        let temp_dir = TempDir::new().unwrap();
        let cache = Arc::new(DiskCache::new(temp_dir.path().to_path_buf(), 10_000_000).unwrap());

        {
            let _daemon = DiskCacheDaemon::start(cache.clone(), 1);
            // Daemon is running
        }
        // Daemon dropped, should have shut down

        // Give it a moment
        thread::sleep(Duration::from_millis(100));

        // Cache should still be accessible
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn test_daemon_evicts_when_over_limit() {
        let temp_dir = TempDir::new().unwrap();
        // Small cache limit: 5KB
        let cache = Arc::new(DiskCache::new(temp_dir.path().to_path_buf(), 5_000).unwrap());

        // Add entries that exceed the limit
        let data = vec![0u8; 2_000]; // 2KB each
        for i in 1..=5 {
            cache.put_sync(create_test_key(i), data.clone()).unwrap();
            thread::sleep(Duration::from_millis(10)); // Ensure different mtimes
        }

        // Cache should be over limit (10KB > 5KB)
        assert!(cache.size_bytes() > cache.max_size_bytes());

        // Start daemon with short interval
        let daemon = DiskCacheDaemon::start(cache.clone(), 1);

        // Wait for daemon to run eviction
        thread::sleep(Duration::from_secs(3));

        // Cache should be under limit now
        assert!(
            cache.size_bytes() <= cache.max_size_bytes(),
            "Cache size {} should be <= max {}",
            cache.size_bytes(),
            cache.max_size_bytes()
        );

        daemon.shutdown();
    }
}
