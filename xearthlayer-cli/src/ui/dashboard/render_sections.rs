//! Dashboard section rendering.
//!
//! This module contains rendering functions for specific dashboard sections:
//! - Control plane metrics
//!
//! Note: Some functions here are legacy (v0.2.x) and kept for reference.

#![allow(dead_code)] // Legacy render functions kept for reference

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use xearthlayer::runtime::HealthSnapshot;

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

/// Calculate inner rect with margins.
pub fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        height: area.height.saturating_sub(margin_y * 2),
    }
}
