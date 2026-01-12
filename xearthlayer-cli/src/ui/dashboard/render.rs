//! Main dashboard rendering.
//!
//! This module contains the top-level layout orchestration and header rendering.

use std::time::Duration;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use xearthlayer::prefetch::PrefetchStatusSnapshot;
use xearthlayer::runtime::HealthSnapshot;
use xearthlayer::telemetry::TelemetrySnapshot;

use super::render_sections::{inner_rect, render_control_plane, render_prefetch};
use super::state::{JobRates, PrewarmProgress};
use super::utils::format_duration;
use crate::ui::widgets::{
    CacheConfig, CacheWidget, ErrorsWidget, NetworkHistory, NetworkWidget, PipelineHistory,
    PipelineWidget,
};

/// Render the main dashboard UI to the frame.
#[allow(clippy::too_many_arguments)]
pub fn render_ui(
    frame: &mut Frame,
    snapshot: &TelemetrySnapshot,
    network_history: &NetworkHistory,
    pipeline_history: &PipelineHistory,
    uptime: Duration,
    cache_config: &CacheConfig,
    prefetch_snapshot: &PrefetchStatusSnapshot,
    control_plane_snapshot: Option<&HealthSnapshot>,
    max_concurrent_jobs: usize,
    job_rates: Option<&JobRates>,
    confirmation_remaining: Option<Duration>,
    prewarm_status: Option<&PrewarmProgress>,
    prewarm_spinner: Option<char>,
) {
    let size = frame.area();

    // Main layout: header, prefetch, control plane, pipeline, chunks, network, cache
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(0)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(5), // Aircraft Position (GPS Status, Position, Prefetch)
            Constraint::Length(5), // Control Plane (needs 4 rows + 1 border)
            Constraint::Length(6), // Pipeline (Tile Pipeline)
            Constraint::Length(3), // Chunk Tasks (moved above Network)
            Constraint::Length(3), // Network
            Constraint::Length(7), // Cache (5 content + 1 border + 1 title)
            Constraint::Min(0),    // Padding
        ])
        .split(size);

    // Header
    render_header(frame, chunks[0], uptime, prewarm_status, prewarm_spinner);

    // Prefetch/Aircraft widget
    render_prefetch(frame, chunks[1], prefetch_snapshot);

    // Control plane widget
    render_control_plane(
        frame,
        chunks[2],
        control_plane_snapshot,
        max_concurrent_jobs,
        job_rates,
    );

    // Pipeline widget (Tile Pipeline)
    let pipeline_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Tile Pipeline ",
            Style::default().fg(Color::Blue),
        ));
    frame.render_widget(pipeline_block, chunks[3]);
    let pipeline_inner = inner_rect(chunks[3], 1, 1);
    frame.render_widget(
        PipelineWidget::new(snapshot).with_history(pipeline_history),
        pipeline_inner,
    );

    // Chunk Tasks widget (moved above Network)
    let chunks_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Chunk Tasks ",
            Style::default().fg(Color::Blue),
        ));
    frame.render_widget(chunks_block, chunks[4]);
    let chunks_inner = inner_rect(chunks[4], 1, 1);
    frame.render_widget(ErrorsWidget::new(snapshot), chunks_inner);

    // Network widget
    let network_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(" Network ", Style::default().fg(Color::Blue)));
    frame.render_widget(network_block, chunks[5]);
    let network_inner = inner_rect(chunks[5], 1, 1);
    frame.render_widget(NetworkWidget::new(snapshot, network_history), network_inner);

    // Cache widget
    let cache_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(" Cache ", Style::default().fg(Color::Blue)));
    frame.render_widget(cache_block, chunks[6]);
    let cache_inner = inner_rect(chunks[6], 1, 1);
    frame.render_widget(
        CacheWidget::new(snapshot).with_config(cache_config.clone()),
        cache_inner,
    );

    // Quit confirmation overlay (if active)
    if let Some(remaining) = confirmation_remaining {
        render_quit_confirmation(frame, size, remaining);
    }
}

/// Render the quit confirmation overlay banner.
pub fn render_quit_confirmation(frame: &mut Frame, area: Rect, remaining: Duration) {
    // Calculate banner position (centered, near top)
    let banner_width = 60u16;
    let banner_height = 5u16;
    let x = area.x + (area.width.saturating_sub(banner_width)) / 2;
    let y = area.y + 4; // Below header

    let banner_area = Rect {
        x,
        y,
        width: banner_width.min(area.width),
        height: banner_height,
    };

    // Clear the background
    let clear_block = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(clear_block, banner_area);

    // Banner with warning styling
    let remaining_secs = remaining.as_secs();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Black))
        .title(Span::styled(
            " ⚠ Confirm Quit ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));

    let text = vec![
        Line::from(vec![Span::styled(
            "Quitting will crash X-Plane if scenery is loaded!",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from(vec![
            Span::styled("Press ", Style::default().fg(Color::White)),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" or ", Style::default().fg(Color::White)),
            Span::styled(
                "q",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to quit, ", Style::default().fg(Color::White)),
            Span::styled(
                "n",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" or ", Style::default().fg(Color::White)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to cancel", Style::default().fg(Color::White)),
            Span::styled(
                format!("  ({}s)", remaining_secs),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(paragraph, banner_area);
}

/// Render the header bar with uptime and prewarm status.
pub fn render_header(
    frame: &mut Frame,
    area: Rect,
    uptime: Duration,
    prewarm_status: Option<&PrewarmProgress>,
    prewarm_spinner: Option<char>,
) {
    let uptime_str = format_duration(uptime);

    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" X-Plane Earth Layer {} ", xearthlayer::VERSION),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left);

    // Build content based on whether prewarm is active
    let content = if let Some(prewarm) = prewarm_status {
        let percent = (prewarm.progress_fraction() * 100.0) as u8;
        let spinner = prewarm_spinner.unwrap_or('⠋');
        Line::from(vec![
            Span::styled(format!("{} ", spinner), Style::default().fg(Color::Yellow)),
            Span::styled("Pre-warming ", Style::default().fg(Color::Yellow)),
            Span::styled(
                &prewarm.icao,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {}/{} ({}%)",
                    prewarm.tiles_loaded, prewarm.total_tiles, percent
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("  │  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Uptime: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&uptime_str, Style::default().fg(Color::White)),
            Span::styled("  │  Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" to quit, ", Style::default().fg(Color::DarkGray)),
            Span::styled("c", Style::default().fg(Color::Yellow)),
            Span::styled(" to cancel prewarm", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("Uptime: ", Style::default().fg(Color::DarkGray)),
            Span::styled(uptime_str, Style::default().fg(Color::White)),
            Span::styled("  │  Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" to quit", Style::default().fg(Color::DarkGray)),
        ])
    };

    let uptime_text = Paragraph::new(content)
        .block(header_block)
        .alignment(ratatui::layout::Alignment::Right);

    frame.render_widget(uptime_text, area);
}
