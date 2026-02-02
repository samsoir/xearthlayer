//! Time-related utility functions.
//!
//! This module provides helpers for working with time types, particularly
//! conversions between different time representations.

use std::time::{Instant, SystemTime};

/// Convert a `SystemTime` to an `Instant`.
///
/// This is approximate since `Instant` doesn't have a fixed epoch.
/// The conversion calculates elapsed time from the `SystemTime` to now,
/// then subtracts that from the current `Instant`.
///
/// # Arguments
///
/// * `system_time` - The system time to convert
///
/// # Returns
///
/// `Some(Instant)` if the conversion succeeds, `None` if the resulting
/// instant would be before the process start (underflow).
///
/// # Example
///
/// ```
/// use std::time::SystemTime;
/// use xearthlayer::time::system_time_to_instant;
///
/// let file_mtime = SystemTime::now();
/// if let Some(instant) = system_time_to_instant(file_mtime) {
///     println!("File was modified {:?} ago", instant.elapsed());
/// }
/// ```
pub fn system_time_to_instant(system_time: SystemTime) -> Option<Instant> {
    let now_system = SystemTime::now();
    let now_instant = Instant::now();

    match now_system.duration_since(system_time) {
        Ok(elapsed) => now_instant.checked_sub(elapsed),
        Err(_) => Some(now_instant), // Future time, use now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn system_time_to_instant_now() {
        let now = SystemTime::now();
        let instant = system_time_to_instant(now);

        assert!(instant.is_some());
        // Should be very close to now
        assert!(instant.unwrap().elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn system_time_to_instant_past() {
        let past = SystemTime::now() - Duration::from_secs(60);
        let instant = system_time_to_instant(past);

        assert!(instant.is_some());
        // Should be about 60 seconds ago
        let elapsed = instant.unwrap().elapsed();
        assert!(elapsed >= Duration::from_secs(59));
        assert!(elapsed <= Duration::from_secs(61));
    }

    #[test]
    fn system_time_to_instant_future() {
        let future = SystemTime::now() + Duration::from_secs(60);
        let instant = system_time_to_instant(future);

        // Future times should return now (not fail)
        assert!(instant.is_some());
        assert!(instant.unwrap().elapsed() < Duration::from_millis(100));
    }
}
