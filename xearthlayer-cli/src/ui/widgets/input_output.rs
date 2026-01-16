//! Input/Output widget with 2-column layout.
//!
//! Displays network and disk I/O metrics side-by-side:
//!
//! Layout:
//! ```text
//! ┌─────────────────────┬─────────────────────┐
//! │      NETWORK        │        DISK         │
//! │   [sparkline]       │   [sparkline]       │
//! │   Data Rate: 5.2MB/s│   Throughput: 12MB/s│
//! │   Downloaded: 1.2GB │   Written: 800MB    │
//! │   Error Rate: 0.1%  │   Provider: Bing    │
//! └─────────────────────┴─────────────────────┘
//! ```

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

use super::primitives::{format_bytes, format_throughput, Sparkline, SparklineHistory};

/// History tracking for disk I/O metrics.
///
/// Tracks disk write throughput for sparkline visualization.
/// Uses cache writes as a proxy for disk I/O activity.
#[derive(Debug, Clone)]
pub struct DiskHistory {
    /// Disk write rate history (bytes/s).
    write_history: SparklineHistory,
    /// Previous disk bytes written for delta calculation.
    prev_disk_writes: u64,
}

impl DiskHistory {
    /// Create new disk history with specified sample count.
    pub fn new(max_samples: usize) -> Self {
        Self {
            write_history: SparklineHistory::new(max_samples),
            prev_disk_writes: 0,
        }
    }

    /// Update history with new disk write bytes.
    ///
    /// # Arguments
    /// * `disk_writes` - Total bytes written to disk cache
    /// * `sample_interval` - Time interval since last update in seconds
    pub fn update(&mut self, disk_writes: u64, sample_interval: f64) {
        if sample_interval <= 0.0 {
            return;
        }

        let delta = disk_writes.saturating_sub(self.prev_disk_writes);
        let rate = delta as f64 / sample_interval;

        self.prev_disk_writes = disk_writes;
        self.write_history.push(rate);
    }

    /// Get disk write sparkline string.
    pub fn write_sparkline(&self) -> String {
        self.write_history.to_sparkline()
    }

    /// Get current write rate (bytes/s).
    pub fn current_write_rate(&self) -> f64 {
        self.write_history.current()
    }

    /// Get peak write rate (bytes/s).
    #[allow(dead_code)]
    pub fn peak_write_rate(&self) -> f64 {
        self.write_history.peak()
    }
}

/// Widget displaying network and disk I/O in 2 columns.
pub struct InputOutputWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    network_history: &'a super::network::NetworkHistory,
    disk_history: Option<&'a DiskHistory>,
    provider_name: &'a str,
}

impl<'a> InputOutputWidget<'a> {
    /// Create a new I/O widget.
    pub fn new(
        snapshot: &'a TelemetrySnapshot,
        network_history: &'a super::network::NetworkHistory,
        provider_name: &'a str,
    ) -> Self {
        Self {
            snapshot,
            network_history,
            disk_history: None,
            provider_name,
        }
    }

    /// Set disk history for sparkline display.
    pub fn with_disk_history(mut self, history: &'a DiskHistory) -> Self {
        self.disk_history = Some(history);
        self
    }

    /// Render a single metrics column.
    fn render_column(
        area: Rect,
        buf: &mut Buffer,
        title: &str,
        title_color: Color,
        sparkline_data: &str,
        sparkline_color: Color,
        metrics: &[(&str, String, Color)],
    ) {
        // Row 1: Title (centered)
        if area.height >= 1 {
            let title_line = Line::from(Span::styled(
                format!("{:^width$}", title, width = area.width as usize),
                Style::default().fg(title_color),
            ));
            Paragraph::new(title_line).render(Rect { height: 1, ..area }, buf);
        }

        // Row 2: Sparkline (centered)
        if area.height >= 2 {
            Sparkline::new(sparkline_data)
                .fg(sparkline_color)
                .centered()
                .render(
                    Rect {
                        x: area.x,
                        y: area.y + 1,
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
        }

        // Rows 3+: Metrics
        for (i, (label, value, color)) in metrics.iter().enumerate() {
            let row = 2 + i as u16;
            if area.height > row {
                let metric_line = Line::from(vec![
                    Span::styled(
                        format!("  {}: ", label),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(value.clone(), Style::default().fg(*color)),
                ]);
                Paragraph::new(metric_line).render(
                    Rect {
                        x: area.x,
                        y: area.y + row,
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
            }
        }
    }

    /// Get color for error rate based on threshold.
    fn error_rate_color(rate: f64) -> Color {
        if rate > 5.0 {
            Color::Red
        } else if rate > 1.0 {
            Color::Yellow
        } else {
            Color::Green
        }
    }
}

impl Widget for InputOutputWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Network metrics
        let network_sparkline = self.network_history.sparkline(12);
        let network_rate = self.network_history.current();
        let total_downloaded = self.snapshot.bytes_downloaded;

        // Calculate network error rate
        let network_error_rate = if self.snapshot.chunks_downloaded > 0 {
            (self.snapshot.chunks_failed as f64 / self.snapshot.chunks_downloaded as f64) * 100.0
        } else {
            0.0
        };

        // Disk metrics
        let (disk_sparkline, disk_rate) = if let Some(disk_history) = self.disk_history {
            (
                disk_history.write_sparkline(),
                disk_history.current_write_rate(),
            )
        } else {
            ("▁▁▁▁▁▁▁▁▁▁▁▁".to_string(), 0.0)
        };

        // Total disk writes (using disk cache size as proxy)
        let total_disk_writes = self.snapshot.disk_cache_size_bytes;

        // Split into 2 columns
        let columns =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);

        // Determine colors based on activity
        let network_sparkline_color = if network_rate > 0.0 {
            Color::Green
        } else {
            Color::DarkGray
        };

        let disk_sparkline_color = if disk_rate > 0.0 {
            Color::Blue
        } else {
            Color::DarkGray
        };

        // Left column: NETWORK
        Self::render_column(
            columns[0],
            buf,
            "NETWORK",
            Color::Green,
            &network_sparkline,
            network_sparkline_color,
            &[
                (
                    "Data Rate",
                    format_throughput(network_rate),
                    if network_rate > 0.0 {
                        Color::Green
                    } else {
                        Color::DarkGray
                    },
                ),
                ("Downloaded", format_bytes(total_downloaded), Color::Cyan),
                (
                    "Error Rate",
                    format!("{:.1}%", network_error_rate),
                    Self::error_rate_color(network_error_rate),
                ),
            ],
        );

        // Right column: DISK
        Self::render_column(
            columns[1],
            buf,
            "DISK",
            Color::Blue,
            &disk_sparkline,
            disk_sparkline_color,
            &[
                (
                    "Throughput",
                    format_throughput(disk_rate),
                    if disk_rate > 0.0 {
                        Color::Blue
                    } else {
                        Color::DarkGray
                    },
                ),
                ("Written", format_bytes(total_disk_writes), Color::Cyan),
                ("Provider", self.provider_name.to_string(), Color::Yellow),
            ],
        );
    }
}
