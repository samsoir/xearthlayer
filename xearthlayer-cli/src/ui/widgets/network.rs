//! Network throughput history tracking.
//!
//! Provides `NetworkHistory` for tracking instantaneous throughput over time.
//! Used by `InputOutputWidget` for sparkline visualization.
//!
//! TODO: Refactor to use primitives::SparklineHistory instead of the inline
//! sparkline implementation.

/// Rolling history for sparkline display.
///
/// Tracks instantaneous throughput by computing the delta between
/// consecutive samples rather than using average rates.
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

    /// Generate sparkline characters.
    pub fn sparkline(&self, width: usize) -> String {
        const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

        if self.samples.is_empty() {
            return " ".repeat(width);
        }

        let max_val = self.samples.iter().cloned().fold(0.0f64, f64::max).max(1.0);

        // Take the last `width` samples
        let start = self.samples.len().saturating_sub(width);
        let visible: Vec<f64> = self.samples[start..].to_vec();

        let mut result = String::with_capacity(width);
        for &val in &visible {
            let normalized = (val / max_val * 7.0).round() as usize;
            let idx = normalized.min(7);
            result.push(SPARK_CHARS[idx]);
        }

        // Pad with spaces if we don't have enough samples
        while result.chars().count() < width {
            result.insert(0, ' ');
        }

        result
    }
}
