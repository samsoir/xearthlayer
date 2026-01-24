//! Network throughput history tracking.
//!
//! Provides `NetworkHistory` for tracking instantaneous throughput over time.
//! Used by `InputOutputWidget` for sparkline visualization.

use super::primitives::SparklineHistory;

/// Rolling history for sparkline display.
///
/// Tracks instantaneous throughput by computing the delta between
/// consecutive samples rather than using average rates.
///
/// Uses [`SparklineHistory`] for rendering to avoid code duplication.
pub struct NetworkHistory {
    /// Instantaneous bytes per second samples (most recent last).
    samples: Vec<f64>,
    /// Maximum samples to keep.
    max_samples: usize,
    /// Last total bytes downloaded (for delta calculation).
    last_bytes_downloaded: u64,
    /// Current instantaneous throughput.
    current_bps: f64,
}

impl NetworkHistory {
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: Vec::with_capacity(max_samples),
            max_samples,
            last_bytes_downloaded: 0,
            current_bps: 0.0,
        }
    }

    /// Update with new telemetry snapshot.
    ///
    /// Calculates instantaneous throughput as the delta in bytes downloaded
    /// since the last sample, divided by the sample interval.
    pub fn update(&mut self, bytes_downloaded: u64, sample_interval_secs: f64) {
        // Calculate bytes downloaded since last sample
        let bytes_delta = bytes_downloaded.saturating_sub(self.last_bytes_downloaded);
        self.last_bytes_downloaded = bytes_downloaded;

        // Calculate instantaneous throughput (bytes per second)
        let instant_bps = if sample_interval_secs > 0.0 {
            bytes_delta as f64 / sample_interval_secs
        } else {
            0.0
        };

        self.current_bps = instant_bps;

        // Add to history
        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(instant_bps);
    }

    /// Get the current instantaneous throughput.
    pub fn current(&self) -> f64 {
        self.current_bps
    }

    /// Generate sparkline characters with fixed width.
    ///
    /// Delegates to [`SparklineHistory::render_sparkline_fixed_width`] for
    /// consistent sparkline rendering across all widgets.
    pub fn sparkline(&self, width: usize) -> String {
        SparklineHistory::render_sparkline_fixed_width(&self.samples, width)
    }
}
