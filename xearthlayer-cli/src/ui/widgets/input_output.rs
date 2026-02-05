//! Input/Output widget with 2-column layout.
//!
//! Displays network and disk I/O metrics side-by-side:
//!
//! Layout:
//! ```text
//! ┌─────────────────────┬─────────────────────┐
//! │      NETWORK        │        DISK         │
//! │   [sparkline]       │   [sparkline]       │
//! │   Data Rate: 5.2MB/s│   I/O Rate: R:1MB/s │
//! │   Downloaded: 1.2GB │   Total: R:500MB    │
//! │   Provider: Bing    │                     │
//! │   Error Rate: 0.1%  │                     │
//! └─────────────────────┴─────────────────────┘
//! ```

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::TelemetrySnapshot;

use super::primitives::{format_bytes, format_throughput, Sparkline, SparklineHistory};

/// History tracking for disk I/O metrics.
///
/// Tracks disk read and write throughput for sparkline visualization.
/// Uses cache reads/writes as a proxy for disk I/O activity.
#[derive(Debug, Clone)]
pub struct DiskHistory {
    /// Disk write rate history (bytes/s).
    write_history: SparklineHistory,
    /// Disk read rate history (bytes/s).
    read_history: SparklineHistory,
    /// Previous disk bytes written for delta calculation.
    prev_disk_writes: u64,
    /// Previous disk bytes read for delta calculation.
    prev_disk_reads: u64,
}

impl DiskHistory {
    /// Create new disk history with specified sample count.
    pub fn new(max_samples: usize) -> Self {
        Self {
            write_history: SparklineHistory::new(max_samples),
            read_history: SparklineHistory::new(max_samples),
            prev_disk_writes: 0,
            prev_disk_reads: 0,
        }
    }

    /// Update history with new disk I/O bytes.
    ///
    /// # Arguments
    /// * `disk_writes` - Total bytes written to disk cache
    /// * `disk_reads` - Total bytes read from disk cache
    /// * `sample_interval` - Time interval since last update in seconds
    pub fn update(&mut self, disk_writes: u64, disk_reads: u64, sample_interval: f64) {
        if sample_interval <= 0.0 {
            return;
        }

        let write_delta = disk_writes.saturating_sub(self.prev_disk_writes);
        let write_rate = write_delta as f64 / sample_interval;
        self.prev_disk_writes = disk_writes;
        self.write_history.push(write_rate);

        let read_delta = disk_reads.saturating_sub(self.prev_disk_reads);
        let read_rate = read_delta as f64 / sample_interval;
        self.prev_disk_reads = disk_reads;
        self.read_history.push(read_rate);
    }

    /// Get combined disk I/O sparkline string (reads + writes).
    pub fn io_sparkline(&self) -> String {
        // Combine read and write histories for overall I/O activity
        let combined: Vec<f64> = self
            .read_history
            .values()
            .iter()
            .zip(self.write_history.values().iter())
            .map(|(r, w)| r + w)
            .collect();

        SparklineHistory::from_values(combined).to_sparkline()
    }

    /// Get current total I/O rate (bytes/s).
    pub fn current_io_rate(&self) -> f64 {
        self.read_history.current() + self.write_history.current()
    }

    /// Get current read rate (bytes/s).
    pub fn current_read_rate(&self) -> f64 {
        self.read_history.current()
    }

    /// Get current write rate (bytes/s).
    pub fn current_write_rate(&self) -> f64 {
        self.write_history.current()
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
        let (disk_sparkline, disk_io_rate, disk_read_rate, disk_write_rate) =
            if let Some(disk_history) = self.disk_history {
                (
                    disk_history.io_sparkline(),
                    disk_history.current_io_rate(),
                    disk_history.current_read_rate(),
                    disk_history.current_write_rate(),
                )
            } else {
                ("▁▁▁▁▁▁▁▁▁▁▁▁".to_string(), 0.0, 0.0, 0.0)
            };

        // Total disk I/O this session
        let total_disk_writes = self.snapshot.disk_bytes_written;
        let total_disk_reads = self.snapshot.disk_bytes_read;

        // Split into 2 columns
        let columns =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);

        // Determine colors based on activity
        let network_sparkline_color = if network_rate > 0.0 {
            Color::Green
        } else {
            Color::DarkGray
        };

        let disk_sparkline_color = if disk_io_rate > 0.0 {
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
                ("Provider", self.provider_name.to_string(), Color::Yellow),
                (
                    "Error Rate",
                    format!("{:.1}%", network_error_rate),
                    Self::error_rate_color(network_error_rate),
                ),
            ],
        );

        // Right column: DISK
        // Format read/write rates on one line: "R: 1.2MB/s W: 0.5MB/s"
        let io_rate_str = format!(
            "R:{} W:{}",
            format_throughput(disk_read_rate),
            format_throughput(disk_write_rate)
        );
        let io_rate_color = if disk_io_rate > 0.0 {
            Color::Blue
        } else {
            Color::DarkGray
        };

        // Format read/written totals: "R: 500MB / W: 200MB"
        let io_total_str = format!(
            "R:{} W:{}",
            format_bytes(total_disk_reads),
            format_bytes(total_disk_writes)
        );

        Self::render_column(
            columns[1],
            buf,
            "DISK",
            Color::Blue,
            &disk_sparkline,
            disk_sparkline_color,
            &[
                ("I/O Rate", io_rate_str, io_rate_color),
                ("Total", io_total_str, Color::Cyan),
            ],
        );
    }
}
