//! Background logger for network statistics.
//!
//! Periodically logs download metrics including bytes transferred,
//! average speed, peak speed, and error counts.

use crate::config::format_size;
use crate::log::Logger;
use crate::orchestrator::NetworkStats;
use crate::{log_debug, log_info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Background thread for periodic network statistics logging.
pub struct NetworkStatsLogger {
    /// Handle to the logger thread
    thread_handle: Option<JoinHandle<()>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl NetworkStatsLogger {
    /// Start the network stats logger thread.
    ///
    /// # Arguments
    ///
    /// * `stats` - Shared network stats to read from
    /// * `logger` - Logger for output
    /// * `interval_secs` - Logging interval in seconds
    pub fn start(stats: Arc<NetworkStats>, logger: Arc<dyn Logger>, interval_secs: u64) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let thread_handle = thread::Builder::new()
            .name("network-stats".to_string())
            .spawn(move || {
                Self::run_loop(stats, logger, interval_secs, shutdown_clone);
            })
            .expect("Failed to spawn network stats logger thread");

        Self {
            thread_handle: Some(thread_handle),
            shutdown,
        }
    }

    /// The main logger loop.
    fn run_loop(
        stats: Arc<NetworkStats>,
        logger: Arc<dyn Logger>,
        interval_secs: u64,
        shutdown: Arc<AtomicBool>,
    ) {
        let interval = Duration::from_secs(interval_secs);

        // Use shorter sleep intervals for responsive shutdown
        let check_interval = Duration::from_secs(1);
        let mut elapsed = Duration::ZERO;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                log_debug!(logger, "Network stats logger received shutdown signal");
                break;
            }

            thread::sleep(check_interval);
            elapsed += check_interval;

            if elapsed >= interval {
                elapsed = Duration::ZERO;
                Self::log_stats(&stats, &logger);
            }
        }
    }

    /// Log current network statistics.
    fn log_stats(stats: &NetworkStats, logger: &Arc<dyn Logger>) {
        let snapshot = stats.snapshot();

        // Only log if there's been any activity
        if snapshot.chunks_downloaded == 0 && snapshot.chunks_failed == 0 {
            return;
        }

        // Format speeds
        let avg_speed = format_speed(snapshot.avg_bytes_per_sec);
        let peak_speed = format_speed(snapshot.peak_bytes_per_sec);

        log_info!(
            logger,
            "[NET] Downloaded: {} chunks ({}), avg: {}, peak: {}, retries: {}, errors: {}",
            snapshot.chunks_downloaded,
            format_size(snapshot.bytes_downloaded as usize),
            avg_speed,
            peak_speed,
            snapshot.retries,
            snapshot.chunks_failed
        );
    }

    /// Signal shutdown.
    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Wait for thread to finish.
    fn join(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NetworkStatsLogger {
    fn drop(&mut self) {
        self.shutdown();
        self.join();
    }
}

/// Format a speed in bytes/sec as human-readable string.
fn format_speed(bytes_per_sec: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;

    if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.1} KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_speed_bytes() {
        assert_eq!(format_speed(500.0), "500 B/s");
        assert_eq!(format_speed(0.0), "0 B/s");
    }

    #[test]
    fn test_format_speed_kilobytes() {
        assert_eq!(format_speed(1024.0), "1.0 KB/s");
        assert_eq!(format_speed(5120.0), "5.0 KB/s");
        assert_eq!(format_speed(1536.0), "1.5 KB/s");
    }

    #[test]
    fn test_format_speed_megabytes() {
        assert_eq!(format_speed(1024.0 * 1024.0), "1.0 MB/s");
        assert_eq!(format_speed(10.0 * 1024.0 * 1024.0), "10.0 MB/s");
        assert_eq!(format_speed(1.5 * 1024.0 * 1024.0), "1.5 MB/s");
    }
}
