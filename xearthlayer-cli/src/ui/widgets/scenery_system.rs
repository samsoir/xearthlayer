//! Scenery system widget with 3-column layout.
//!
//! Consolidates FUSE request rates (X-Plane requests), active tile queue,
//! and job executor throughput into a unified "Scenery System" panel.
//!
//! Layout:
//! ```text
//! ┌────────────────┬────────────────────────────┬────────────────┐
//! │ TILE REQUESTS  │        QUEUE (3/12)        │ TILE PROCESSING│
//! │ [sparkline]    │ 140E,35S ████████░░ 50%    │ [sparkline]    │
//! │ Req/s: 12.5    │ 140E,36S ████░░░░░░ 25%    │ Done/s: 10.2   │
//! │ Pressure: Δ+2/s│ 140E,37S ░░░░░░░░░░  0%    │ Active: 8/32   │
//! │ Error Rate: 0% │                            │ Error Rate: 0% │
//! └────────────────┴────────────────────────────┴────────────────┘
//! ```

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::TelemetrySnapshot;
use xearthlayer::runtime::{HealthSnapshot, TileProgressEntry};

use super::primitives::{Sparkline, SparklineHistory};

/// Characters for progress bar rendering (used in QUEUE column).
const PROGRESS_FULL: char = '█';
const PROGRESS_EMPTY: char = '░';

/// Width of the progress bar in characters.
const PROGRESS_BAR_WIDTH: usize = 10;

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

/// Widget displaying scenery system status with 3 columns.
///
/// Left column shows TILE REQUESTS (FUSE interface activity).
/// Middle column shows QUEUE (active tile progress).
/// Right column shows TILE PROCESSING (job executor throughput).
pub struct ScenerySystemWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    health: Option<&'a HealthSnapshot>,
    history: Option<&'a SceneryHistory>,
    tile_progress: &'a [TileProgressEntry],
}

impl<'a> ScenerySystemWidget<'a> {
    /// Create a new scenery system widget.
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self {
            snapshot,
            health: None,
            history: None,
            tile_progress: &[],
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

    /// Set tile progress entries for QUEUE column.
    pub fn with_tile_progress(mut self, entries: &'a [TileProgressEntry]) -> Self {
        self.tile_progress = entries;
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

        // Split into 3 columns: TILE REQUESTS | QUEUE | TILE PROCESSING
        let columns = Layout::horizontal([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(area);

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

        // Middle column: QUEUE (active tile progress)
        Self::render_queue_column(columns[1], buf, self.tile_progress);

        // Right column: TILE PROCESSING
        Self::render_column(
            columns[2],
            buf,
            "TILE PROCESSING",
            Color::Green,
            &completion_sparkline,
            completion_sparkline_color,
            &[
                ("Done/s", format!("{:.1}", completion_rate), Color::Green),
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

    /// Render the QUEUE column showing active tile progress.
    ///
    /// Coalesces tiles by 1-degree coordinate and sorts by progress (highest first).
    fn render_queue_column(area: Rect, buf: &mut Buffer, entries: &[TileProgressEntry]) {
        // Coalesce entries by formatted coordinate (1-degree buckets)
        let coalesced = Self::coalesce_entries(entries);

        // Row 1: Title (centered, no depth indicator needed with coalesced feedback)
        if area.height >= 1 {
            let title_line = Line::from(Span::styled(
                format!("{:^width$}", "QUEUE", width = area.width as usize),
                Style::default().fg(Color::Yellow),
            ));
            Paragraph::new(title_line).render(Rect { height: 1, ..area }, buf);
        }

        // Row 2: Empty line for spacing between header and content
        // Row 3+: Tile progress entries
        if coalesced.is_empty() {
            // Show placeholder when no tiles are being processed (with spacing)
            if area.height >= 3 {
                let placeholder = Line::from(Span::styled(
                    format!(
                        "{:^width$}",
                        "No tiles in progress",
                        width = area.width as usize
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
                Paragraph::new(placeholder).render(
                    Rect {
                        x: area.x,
                        y: area.y + 2, // Skip row 1 (header) and row 2 (spacing)
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
            }
            return;
        }

        // Show up to 4 coalesced entries, sorted by progress (highest first = about to complete)
        for (i, (coord, count, avg_percent)) in coalesced.iter().take(4).enumerate() {
            let row = 2 + i as u16; // Start at row 2 (after header + spacing)
            if area.height <= row {
                break;
            }

            let progress_bar = Self::render_progress_bar(*avg_percent);
            let color = Self::progress_color(*avg_percent);

            // Format: "140E,35S ████░░ 50%" or "140E,35S(3) ██░░ 25%" if multiple tiles
            let coord_display = if *count > 1 {
                format!("{}({})", coord, count)
            } else {
                coord.clone()
            };

            let line = Line::from(vec![
                Span::styled(
                    format!(" {:<11}", coord_display),
                    Style::default().fg(Color::White),
                ),
                Span::styled(progress_bar, Style::default().fg(color)),
                Span::styled(format!(" {:>3}%", avg_percent), Style::default().fg(color)),
            ]);

            Paragraph::new(line).render(
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

    /// Coalesce tile entries by 1-degree coordinate.
    ///
    /// Groups tiles that share the same formatted coordinate (e.g., "140E,35S")
    /// and returns (coord, count, avg_progress) sorted by progress descending.
    fn coalesce_entries(entries: &[TileProgressEntry]) -> Vec<(String, usize, u8)> {
        use std::collections::HashMap;

        // Group by formatted coordinate
        let mut groups: HashMap<String, Vec<u8>> = HashMap::new();
        for entry in entries {
            let coord = entry.format_coordinate();
            let percent = entry.progress_percent();
            groups.entry(coord).or_default().push(percent);
        }

        // Convert to (coord, count, avg_percent) and sort by progress descending
        let mut result: Vec<_> = groups
            .into_iter()
            .map(|(coord, percents)| {
                let count = percents.len();
                let avg = percents.iter().map(|&p| p as u32).sum::<u32>() / count as u32;
                (coord, count, avg as u8)
            })
            .collect();

        // Sort by progress descending (highest progress = about to complete = at top)
        result.sort_by(|a, b| b.2.cmp(&a.2));

        result
    }

    /// Render a progress bar string.
    fn render_progress_bar(percent: u8) -> String {
        let filled = ((percent as usize * PROGRESS_BAR_WIDTH) / 100).min(PROGRESS_BAR_WIDTH);
        let empty = PROGRESS_BAR_WIDTH - filled;

        format!(
            "{}{}",
            PROGRESS_FULL.to_string().repeat(filled),
            PROGRESS_EMPTY.to_string().repeat(empty)
        )
    }

    /// Get color based on progress percentage.
    fn progress_color(percent: u8) -> Color {
        match percent {
            0..=25 => Color::Yellow,
            26..=75 => Color::Cyan,
            _ => Color::Green,
        }
    }
}
