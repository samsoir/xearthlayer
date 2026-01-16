//! Scenery system widget with 2-column layout.
//!
//! Consolidates FUSE request rates (X-Plane requests) and job executor
//! throughput into a unified "Scenery System" panel.
//!
//! Layout:
//! ```text
//! ┌─────────────────────┬─────────────────────┐
//! │   TILE REQUESTS     │   TILE PROCESSING   │
//! │   [sparkline]       │   [sparkline]       │
//! │   Req/s: 12.5       │   Done/s: 10.2      │
//! │   Pressure: Δ+2/s   │   Active: 8/32      │
//! │   Error Rate: 0.1%  │   Error Rate: 0.2%  │
//! └─────────────────────┴─────────────────────┘
//! ```

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::runtime::HealthSnapshot;
use xearthlayer::telemetry::TelemetrySnapshot;

use super::primitives::{Sparkline, SparklineHistory};

/// History tracking for scenery system metrics.
///
/// Maintains rolling histories for FUSE request rates and job completion
/// rates, enabling sparkline visualization and rate calculations.
#[derive(Debug, Clone)]
pub struct SceneryHistory {
    /// FUSE request rate history.
    request_history: SparklineHistory,
    /// Job completion rate history.
    completion_history: SparklineHistory,
    /// Previous snapshot values for delta calculation.
    prev_fuse_requests: u64,
    prev_jobs_completed: u64,
}

impl SceneryHistory {
    /// Create new scenery history with specified sample count.
    pub fn new(max_samples: usize) -> Self {
        Self {
            request_history: SparklineHistory::new(max_samples),
            completion_history: SparklineHistory::new(max_samples),
            prev_fuse_requests: 0,
            prev_jobs_completed: 0,
        }
    }

    /// Update history with new telemetry snapshot.
    pub fn update(&mut self, snapshot: &TelemetrySnapshot, sample_interval: f64) {
        if sample_interval <= 0.0 {
            return;
        }

        // Calculate deltas
        let fuse_delta = snapshot
            .fuse_jobs_submitted
            .saturating_sub(self.prev_fuse_requests);
        let jobs_delta = snapshot
            .jobs_completed
            .saturating_sub(self.prev_jobs_completed);

        // Calculate rates
        let request_rate = fuse_delta as f64 / sample_interval;
        let completion_rate = jobs_delta as f64 / sample_interval;

        // Store current values for next delta
        self.prev_fuse_requests = snapshot.fuse_jobs_submitted;
        self.prev_jobs_completed = snapshot.jobs_completed;

        // Push to histories
        self.request_history.push(request_rate);
        self.completion_history.push(completion_rate);
    }

    /// Get request rate sparkline string.
    pub fn request_sparkline(&self) -> String {
        self.request_history.to_sparkline()
    }

    /// Get completion rate sparkline string.
    pub fn completion_sparkline(&self) -> String {
        self.completion_history.to_sparkline()
    }

    /// Get current request rate (requests/s).
    pub fn current_request_rate(&self) -> f64 {
        self.request_history.current()
    }

    /// Get current completion rate (jobs/s).
    pub fn current_completion_rate(&self) -> f64 {
        self.completion_history.current()
    }

    /// Calculate pressure (requests - completions per second).
    ///
    /// Positive pressure indicates more requests than completions,
    /// meaning the queue is growing.
    pub fn pressure(&self) -> f64 {
        self.current_request_rate() - self.current_completion_rate()
    }
}

/// Widget displaying scenery system status with 2 columns.
///
/// Left column shows TILE REQUESTS (FUSE interface activity).
/// Right column shows TILE PROCESSING (job executor throughput).
pub struct ScenerySystemWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    health: Option<&'a HealthSnapshot>,
    history: Option<&'a SceneryHistory>,
    max_concurrent_jobs: usize,
}

impl<'a> ScenerySystemWidget<'a> {
    /// Create a new scenery system widget.
    pub fn new(snapshot: &'a TelemetrySnapshot, max_concurrent_jobs: usize) -> Self {
        Self {
            snapshot,
            health: None,
            history: None,
            max_concurrent_jobs,
        }
    }

    /// Set health snapshot for additional metrics.
    pub fn with_health(mut self, health: Option<&'a HealthSnapshot>) -> Self {
        self.health = health;
        self
    }

    /// Set history for sparkline display.
    pub fn with_history(mut self, history: &'a SceneryHistory) -> Self {
        self.history = Some(history);
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
}

impl Widget for ScenerySystemWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Get sparklines and rates from history
        let (request_sparkline, request_rate, completion_sparkline, completion_rate, pressure) =
            if let Some(history) = self.history {
                (
                    history.request_sparkline(),
                    history.current_request_rate(),
                    history.completion_sparkline(),
                    history.current_completion_rate(),
                    history.pressure(),
                )
            } else {
                (
                    "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                    0.0,
                    "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                    0.0,
                    0.0,
                )
            };

        // Get job metrics
        let jobs_active = self.health.map(|h| h.jobs_in_progress).unwrap_or(0);

        // Calculate error rates
        let request_error_rate = if self.snapshot.fuse_jobs_submitted > 0 {
            (self.snapshot.chunks_failed as f64 / self.snapshot.fuse_jobs_submitted as f64) * 100.0
        } else {
            0.0
        };

        let job_error_rate = if self.snapshot.jobs_submitted > 0 {
            (self.snapshot.jobs_failed as f64 / self.snapshot.jobs_submitted as f64) * 100.0
        } else {
            0.0
        };

        // Split into 2 columns
        let columns =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);

        // Determine colors based on activity and thresholds
        let pressure_color = if pressure > 10.0 {
            Color::Red
        } else if pressure > 5.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        let request_sparkline_color = if request_rate > 0.0 {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let completion_sparkline_color = if completion_rate > 0.0 {
            Color::Green
        } else {
            Color::DarkGray
        };

        // Left column: TILE REQUESTS
        Self::render_column(
            columns[0],
            buf,
            "TILE REQUESTS",
            Color::Cyan,
            &request_sparkline,
            request_sparkline_color,
            &[
                ("Req/s", format!("{:.1}", request_rate), Color::Cyan),
                ("Pressure", format!("Δ{:+.0}/s", pressure), pressure_color),
                (
                    "Error Rate",
                    format!("{:.1}%", request_error_rate),
                    Self::error_rate_color(request_error_rate),
                ),
            ],
        );

        // Right column: TILE PROCESSING
        Self::render_column(
            columns[1],
            buf,
            "TILE PROCESSING",
            Color::Green,
            &completion_sparkline,
            completion_sparkline_color,
            &[
                ("Done/s", format!("{:.1}", completion_rate), Color::Green),
                (
                    "Active",
                    format!("{}/{}", jobs_active, self.max_concurrent_jobs),
                    Color::Cyan,
                ),
                (
                    "Error Rate",
                    format!("{:.1}%", job_error_rate),
                    Self::error_rate_color(job_error_rate),
                ),
            ],
        );
    }
}

impl ScenerySystemWidget<'_> {
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
