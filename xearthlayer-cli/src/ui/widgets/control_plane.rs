//! Control plane health widget.
//!
//! Displays control plane status using a 4-column grid layout:
//! STATUS | JOBS | CREATED/COMPLETED | PRESSURE
//! Uses fixed-width columns to prevent layout dancing.
//!
//! Note: Deprecated in v0.3.0 - replaced by ScenerySystemWidget.

#![allow(dead_code)] // Deprecated widget, kept for compatibility

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::runtime::{HealthSnapshot, HealthStatus};

use crate::ui::dashboard::JobRates;

/// Widget displaying control plane health status.
pub struct ControlPlaneWidget<'a> {
    snapshot: &'a HealthSnapshot,
    max_concurrent_jobs: usize,
    job_rates: Option<&'a JobRates>,
}

impl<'a> ControlPlaneWidget<'a> {
    pub fn new(snapshot: &'a HealthSnapshot, max_concurrent_jobs: usize) -> Self {
        Self {
            snapshot,
            max_concurrent_jobs,
            job_rates: None,
        }
    }

    /// Set job rates for display.
    pub fn with_job_rates(mut self, rates: Option<&'a JobRates>) -> Self {
        self.job_rates = rates;
        self
    }

    /// Get color for health status.
    fn status_color(status: HealthStatus) -> Color {
        match status {
            HealthStatus::Healthy => Color::Green,
            HealthStatus::Degraded => Color::Yellow,
            HealthStatus::Recovering => Color::Cyan,
            HealthStatus::Critical => Color::Red,
        }
    }

    /// Format duration since last success.
    fn format_duration(d: std::time::Duration) -> String {
        let secs = d.as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

impl Widget for ControlPlaneWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let status = self.snapshot.status;
        let status_color = Self::status_color(status);

        // Use 4-column grid layout like pipeline
        let columns = Layout::horizontal([
            Constraint::Ratio(1, 4), // STATUS
            Constraint::Ratio(1, 4), // JOBS
            Constraint::Ratio(1, 4), // CREATED/COMPLETED
            Constraint::Ratio(1, 4), // PRESSURE
        ])
        .split(area);

        // Column 1: STATUS
        Self::render_status_column(columns[0], buf, status, status_color);

        // Column 2: JOBS
        Self::render_jobs_column(
            columns[1],
            buf,
            self.snapshot.jobs_in_progress,
            self.max_concurrent_jobs,
            self.snapshot.peak_concurrent_jobs,
        );

        // Column 3: CREATED/COMPLETED rates
        if let Some(rates) = self.job_rates {
            Self::render_rates_column(columns[2], buf, rates);
        }

        // Column 4: PRESSURE
        if let Some(rates) = self.job_rates {
            Self::render_pressure_column(columns[3], buf, rates);
        }

        // Row 2: Secondary metrics (timeout info or last success)
        if area.height > 1 {
            let row2_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            };

            let has_issues = self.snapshot.jobs_recovered > 0
                || self.snapshot.jobs_rejected_capacity > 0
                || self.snapshot.semaphore_timeouts > 0;

            let metrics_line = if has_issues {
                let mut spans = vec![Span::styled(
                    "Timeout: ",
                    Style::default().fg(Color::DarkGray),
                )];

                if self.snapshot.jobs_recovered > 0 {
                    spans.push(Span::styled(
                        format!("{} recovered", self.snapshot.jobs_recovered),
                        Style::default().fg(Color::Yellow),
                    ));
                }

                if self.snapshot.jobs_rejected_capacity > 0 {
                    if self.snapshot.jobs_recovered > 0 {
                        spans.push(Span::raw("  │  "));
                    }
                    spans.push(Span::styled(
                        format!("{} rejected", self.snapshot.jobs_rejected_capacity),
                        Style::default().fg(Color::Red),
                    ));
                }

                if self.snapshot.semaphore_timeouts > 0 {
                    if self.snapshot.jobs_recovered > 0 || self.snapshot.jobs_rejected_capacity > 0
                    {
                        spans.push(Span::raw("  │  "));
                    }
                    spans.push(Span::styled(
                        format!("{} semaphore", self.snapshot.semaphore_timeouts),
                        Style::default().fg(Color::Red),
                    ));
                }

                Line::from(spans)
            } else if let Some(elapsed) = self.snapshot.time_since_last_success {
                Line::from(vec![
                    Span::styled("Last success: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{} ago", Self::format_duration(elapsed)),
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled("  │  Total: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{} jobs", self.snapshot.total_jobs_completed),
                        Style::default().fg(Color::White),
                    ),
                ])
            } else {
                Line::from(vec![Span::styled(
                    "Waiting for first job...",
                    Style::default().fg(Color::DarkGray),
                )])
            };

            Paragraph::new(metrics_line).render(row2_area, buf);
        }
    }
}

impl ControlPlaneWidget<'_> {
    fn render_status_column(area: Rect, buf: &mut Buffer, status: HealthStatus, color: Color) {
        // Row 1: Label
        let label = Line::from(Span::styled(
            format!("{:^width$}", "STATUS", width = area.width as usize),
            Style::default().fg(Color::DarkGray),
        ));
        if area.height >= 1 {
            Paragraph::new(label).render(Rect { height: 1, ..area }, buf);
        }

        // Row 2: Value (empty line for alignment)
        // Row 3: Status value
        if area.height >= 3 {
            let value = Line::from(Span::styled(
                format!(
                    "{:^width$}",
                    status.as_str().to_uppercase(),
                    width = area.width as usize
                ),
                Style::default().fg(color),
            ));
            Paragraph::new(value).render(
                Rect {
                    x: area.x,
                    y: area.y + 2,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }

    fn render_jobs_column(area: Rect, buf: &mut Buffer, current: usize, max: usize, peak: usize) {
        // Row 1: Label
        let label = Line::from(Span::styled(
            format!("{:^width$}", "JOBS", width = area.width as usize),
            Style::default().fg(Color::DarkGray),
        ));
        if area.height >= 1 {
            Paragraph::new(label).render(Rect { height: 1, ..area }, buf);
        }

        // Row 3: Value
        if area.height >= 3 {
            let value_str = format!("{}/{}", current, max);
            let peak_str = format!("(peak: {})", peak);
            let value = Line::from(vec![
                Span::styled(
                    format!("{:^width$}", value_str, width = area.width as usize / 2),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(peak_str, Style::default().fg(Color::DarkGray)),
            ]);
            Paragraph::new(value).render(
                Rect {
                    x: area.x,
                    y: area.y + 2,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }

    fn render_rates_column(area: Rect, buf: &mut Buffer, rates: &JobRates) {
        // Row 1: Label
        let label = Line::from(Span::styled(
            format!(
                "{:^width$}",
                "CREATED / COMPLETED",
                width = area.width as usize
            ),
            Style::default().fg(Color::DarkGray),
        ));
        if area.height >= 1 {
            Paragraph::new(label).render(Rect { height: 1, ..area }, buf);
        }

        // Row 3: Values
        if area.height >= 3 {
            let created_str = format!("{:.0}/s", rates.submitted_per_sec);
            let completed_str = format!("{:.0}/s", rates.completed_per_sec);
            let value = Line::from(vec![
                Span::styled(
                    format!("{:>width$}", created_str, width = area.width as usize / 3),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    format!("{:^width$}", "/", width = 3),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<width$}", completed_str, width = area.width as usize / 3),
                    Style::default().fg(Color::Cyan),
                ),
            ]);
            Paragraph::new(value).render(
                Rect {
                    x: area.x,
                    y: area.y + 2,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }

    fn render_pressure_column(area: Rect, buf: &mut Buffer, rates: &JobRates) {
        let pressure = rates.pressure();
        let pressure_color = if pressure > 10.0 {
            Color::Red
        } else if pressure > 5.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        // Row 1: Label
        let label = Line::from(Span::styled(
            format!("{:^width$}", "PRESSURE", width = area.width as usize),
            Style::default().fg(Color::DarkGray),
        ));
        if area.height >= 1 {
            Paragraph::new(label).render(Rect { height: 1, ..area }, buf);
        }

        // Row 3: Value
        if area.height >= 3 {
            let value = Line::from(Span::styled(
                format!(
                    "{:^width$}",
                    format!("Δ {:+.0}/s", pressure),
                    width = area.width as usize
                ),
                Style::default().fg(pressure_color),
            ));
            Paragraph::new(value).render(
                Rect {
                    x: area.x,
                    y: area.y + 2,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }
}
