//! Dashboard section rendering.
//!
//! This module contains rendering functions for specific dashboard sections:
//! - Aircraft position / prefetch status
//! - Control plane metrics

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use xearthlayer::pipeline::control_plane::HealthSnapshot;
use xearthlayer::prefetch::{GpsStatus, PrefetchMode, PrefetchStatusSnapshot};

use super::state::JobRates;
use crate::ui::widgets::ControlPlaneWidget;

/// Render the control plane status section.
pub fn render_control_plane(
    frame: &mut Frame,
    area: Rect,
    snapshot: Option<&HealthSnapshot>,
    max_concurrent_jobs: usize,
    job_rates: Option<&JobRates>,
) {
    let control_plane_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Control Plane ",
            Style::default().fg(Color::Blue),
        ));

    frame.render_widget(control_plane_block, area);
    let inner = inner_rect(area, 1, 1);

    if let Some(health_snapshot) = snapshot {
        frame.render_widget(
            ControlPlaneWidget::new(health_snapshot, max_concurrent_jobs).with_job_rates(job_rates),
            inner,
        );
    } else {
        // No control plane configured - show placeholder
        let text = vec![Line::from(vec![Span::styled(
            "Control plane not configured",
            Style::default().fg(Color::DarkGray),
        )])];
        let paragraph = Paragraph::new(text);
        frame.render_widget(paragraph, inner);
    }
}

/// Render the aircraft position section.
pub fn render_prefetch(frame: &mut Frame, area: Rect, prefetch: &PrefetchStatusSnapshot) {
    let prefetch_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Aircraft Position ",
            Style::default().fg(Color::Magenta),
        ));

    frame.render_widget(prefetch_block, area);
    let inner = inner_rect(area, 1, 1);

    // GPS Status line with colored indicator
    let (gps_indicator, gps_text, gps_color) = match prefetch.gps_status {
        GpsStatus::Connected => ("●", "Connected", Color::Green),
        GpsStatus::Acquiring => ("●", "Acquiring...", Color::Yellow),
        GpsStatus::Inferred => ("●", "Inferred", Color::Red),
    };

    let gps_line = Line::from(vec![
        Span::styled("GPS Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} ", gps_indicator),
            Style::default().fg(gps_color),
        ),
        Span::styled(gps_text, Style::default().fg(gps_color)),
    ]);

    // Position line
    let position_line = if prefetch.aircraft.is_some() {
        Line::from(vec![
            Span::styled("Position:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(prefetch.aircraft_line(), Style::default().fg(Color::Green)),
        ])
    } else {
        Line::from(vec![
            Span::styled("Position:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Waiting for X-Plane telemetry...",
                Style::default().fg(Color::Yellow),
            ),
        ])
    };

    // Prefetch mode line
    let mode_color = match prefetch.prefetch_mode {
        PrefetchMode::Telemetry => Color::Green,
        PrefetchMode::FuseInference => Color::Yellow,
        PrefetchMode::Radial => Color::Cyan,
        PrefetchMode::Idle => Color::DarkGray,
        PrefetchMode::CircuitOpen => Color::Magenta, // Paused due to high X-Plane load
    };

    // Build the prefetch line with detailed stats if available
    let prefetch_line = if let Some(ref stats) = prefetch.detailed_stats {
        // Format: "Prefetch: ● Mode | 45/cyc | ↑120 ⊘5"
        let activity_indicator = if stats.is_active { "●" } else { "○" };
        let activity_color = if stats.is_active {
            Color::Green
        } else {
            Color::DarkGray
        };

        Line::from(vec![
            Span::styled("Prefetch:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ", activity_indicator),
                Style::default().fg(activity_color),
            ),
            Span::styled(
                format!("{}", prefetch.prefetch_mode),
                Style::default().fg(mode_color),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/cyc", stats.tiles_submitted_last_cycle),
                Style::default().fg(Color::White),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑", Style::default().fg(Color::Green)),
            Span::styled(
                format!("{}", stats.cache_hits),
                Style::default().fg(Color::Green),
            ),
            Span::styled(" ", Style::default()),
            Span::styled("⊘", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.ttl_skipped),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else {
        // Fallback to simple display when no detailed stats
        Line::from(vec![
            Span::styled("Prefetch:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", prefetch.prefetch_mode),
                Style::default().fg(mode_color),
            ),
        ])
    };

    let text = vec![gps_line, position_line, prefetch_line];
    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Calculate inner rect with margins.
pub fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        height: area.height.saturating_sub(margin_y * 2),
    }
}
