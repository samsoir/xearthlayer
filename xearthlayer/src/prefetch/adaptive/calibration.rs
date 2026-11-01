//! The throughput estimate that sizes prefetch plans.
//!
//! A throughput calibrator once lived here to choose a prefetch mode, but
//! nothing fed it samples and nothing read its result, so it was removed in
//! #277, and the mode itself in #340. `PerformanceCalibration` survives as the
//! fixed estimate `PrefetchPlan` uses for time budgeting.

use std::time::Instant;

/// Throughput estimate used to budget prefetch plans.
///
/// In production this is always [`PerformanceCalibration::default_opportunistic`];
/// nothing measures throughput to replace it.
#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceCalibration {
    /// Tiles generated per second (sustained throughput).
    pub throughput_tiles_per_sec: f64,

    /// Average time to generate one tile (milliseconds).
    pub avg_tile_generation_ms: u64,

    /// Standard deviation of generation time (milliseconds).
    ///
    /// High variance indicates inconsistent performance, which may
    /// affect prefetch timing reliability.
    pub tile_generation_stddev_ms: u64,

    /// Confidence level (0.0 - 1.0) based on sample size and variance.
    ///
    /// Higher confidence means more reliable calibration:
    /// - > 0.9: Excellent (many samples, low variance)
    /// - 0.7-0.9: Good (sufficient samples)
    /// - 0.5-0.7: Fair (limited samples or high variance)
    /// - < 0.5: Poor (calibration may be unreliable)
    pub confidence: f64,

    /// When calibration was performed.
    pub calibrated_at: Instant,

    /// Baseline throughput from initial calibration.
    ///
    /// Used for rolling recalibration to detect degradation/recovery.
    pub baseline_throughput: f64,

    /// Number of samples used in calibration.
    pub sample_count: usize,
}

impl PerformanceCalibration {
    /// The fixed estimate the coordinator plans with.
    pub fn default_opportunistic() -> Self {
        Self {
            throughput_tiles_per_sec: 15.0,
            avg_tile_generation_ms: 67,
            tile_generation_stddev_ms: 20,
            confidence: 0.5,
            calibrated_at: Instant::now(),
            baseline_throughput: 15.0,
            sample_count: 0,
        }
    }

    /// Estimate time to complete a batch of tiles (milliseconds).
    ///
    /// Uses average generation time plus one standard deviation for safety.
    pub fn estimate_batch_time_ms(&self, tile_count: usize) -> u64 {
        if self.throughput_tiles_per_sec <= 0.0 {
            return u64::MAX;
        }

        let tiles_per_ms = self.throughput_tiles_per_sec / 1000.0;
        let base_time = tile_count as f64 / tiles_per_ms;

        // Add one stddev for safety margin
        (base_time + self.tile_generation_stddev_ms as f64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calibration_estimate_batch_time() {
        let cal = PerformanceCalibration {
            throughput_tiles_per_sec: 20.0, // 20 tiles/sec = 50ms/tile
            avg_tile_generation_ms: 50,
            tile_generation_stddev_ms: 10,
            confidence: 0.9,
            calibrated_at: Instant::now(),
            baseline_throughput: 20.0,
            sample_count: 100,
        };

        // 100 tiles at 20/sec = 5000ms, plus 10ms stddev = 5010ms
        let estimate = cal.estimate_batch_time_ms(100);
        assert!(estimate >= 5000);
        assert!(estimate <= 5100);
    }

    #[test]
    fn test_default_opportunistic_calibration() {
        let cal = PerformanceCalibration::default_opportunistic();
        assert!(cal.confidence < 1.0);
    }
}
