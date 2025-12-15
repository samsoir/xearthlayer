//! Network throughput widget with sparkline.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

/// Rolling history for sparkline display.
///
/// Tracks instantaneous throughput by computing the delta between
/// consecutive samples rather than using average rates.
pub struct NetworkHistory {
    /// Instantaneous bytes per second samples (most recent last).
    samples: Vec<f64>,
    /// Maximum samples to keep.
    max_samples: usize,
    /// Peak instantaneous throughput observed.
    peak_bps: f64,
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
            peak_bps: 0.0,
            last_bytes_downloaded: 0,
            current_bps: 0.0,
        }
    }

    /// Update with new telemetry snapshot.
    ///
    /// Calculates instantaneous throughput as the delta in bytes downloaded
    /// since the last sample, divided by the sample interval (assumed ~100ms).
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

        // Update peak
        if instant_bps > self.peak_bps {
            self.peak_bps = instant_bps;
        }
    }

    /// Get the current instantaneous throughput.
    pub fn current(&self) -> f64 {
        self.current_bps
    }

    /// Get the peak throughput.
    pub fn peak(&self) -> f64 {
        self.peak_bps
    }

    /// Generate sparkline characters.
    fn sparkline(&self, width: usize) -> String {
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

/// Widget displaying network throughput.
pub struct NetworkWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    history: &'a NetworkHistory,
}

impl<'a> NetworkWidget<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot, history: &'a NetworkHistory) -> Self {
        Self { snapshot, history }
    }

    fn format_throughput(bps: f64) -> String {
        if bps >= 1_000_000_000.0 {
            format!("{:.1} GB/s", bps / 1_000_000_000.0)
        } else if bps >= 1_000_000.0 {
            format!("{:.1} MB/s", bps / 1_000_000.0)
        } else if bps >= 1_000.0 {
            format!("{:.1} KB/s", bps / 1_000.0)
        } else {
            format!("{:.0} B/s", bps)
        }
    }
}

impl Widget for NetworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        let sparkline = self.history.sparkline(12);
        // Use instantaneous throughput from history, not average from snapshot
        let current_bps = self.history.current();
        let current = Self::format_throughput(current_bps);
        let peak = Self::format_throughput(self.history.peak());

        // Color based on activity level
        let throughput_color = if current_bps > 0.0 {
            Color::Green
        } else {
            Color::DarkGray
        };

        let line = Line::from(vec![
            Span::styled("  Network:  ", Style::default().fg(Color::White)),
            Span::styled(sparkline, Style::default().fg(throughput_color)),
            Span::raw("  "),
            Span::styled(
                format!("{:>10}", current),
                Style::default().fg(throughput_color),
            ),
            Span::styled(
                format!(" (peak: {})", peak),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("   "),
            Span::styled("Chunks: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{:.1}/s", self.snapshot.chunks_per_second),
                Style::default().fg(if self.snapshot.chunks_per_second > 0.0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
        ]);

        let paragraph = Paragraph::new(vec![line]).block(block);
        paragraph.render(area, buf);
    }
}
