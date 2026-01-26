//! Main dashboard rendering.
//!
//! This module contains the top-level layout orchestration and header rendering.
//!
//! ## Layout v0.3.0
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ Header (3 lines)                                        │
//! ├─────────────────────────────────────────────────────────┤
//! │ Aircraft Position (4 lines)                             │
//! ├─────────────────────────────────────────────────────────┤
//! │ Prefetch System (4 lines)                               │
//! ├─────────────────────────────────────────────────────────┤
//! │ Scenery System (8 lines) - 2-column                     │
//! │ TILE REQUESTS        │ TILE PROCESSING                  │
//! ├─────────────────────────────────────────────────────────┤
//! │ Input/Output (8 lines) - 2-column                       │
//! │ NETWORK              │ DISK                             │
//! ├─────────────────────────────────────────────────────────┤
//! │ Caches (6 lines)                                        │
//! └─────────────────────────────────────────────────────────┘
//! ```

use std::time::Duration;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use xearthlayer::metrics::TelemetrySnapshot;
use xearthlayer::prefetch::PrefetchStatusSnapshot;
use xearthlayer::runtime::HealthSnapshot;

use super::render_sections::inner_rect;
use super::state::PrewarmProgress;
use super::utils::format_duration;
use crate::ui::widgets::{
    AircraftPositionWidget, CacheConfig, CacheWidgetCompact, DiskHistory, InputOutputWidget,
    NetworkHistory, PrefetchSystemWidget, SceneryHistory, ScenerySystemWidget,
};
use xearthlayer::aircraft_position::AircraftPositionStatus;

/// Render the main dashboard UI to the frame.
///
/// # Layout v0.3.0
///
/// The new layout consolidates the old 8-section layout into 6 sections
/// with side-by-side panels for better information density.
#[allow(clippy::too_many_arguments)]
pub fn render_ui(
    frame: &mut Frame,
    snapshot: &TelemetrySnapshot,
    network_history: &NetworkHistory,
    scenery_history: &SceneryHistory,
    disk_history: &DiskHistory,
    provider_name: &str,
    uptime: Duration,
    cache_config: &CacheConfig,
    prefetch_snapshot: &PrefetchStatusSnapshot,
    aircraft_position_status: &AircraftPositionStatus,
    control_plane_snapshot: Option<&HealthSnapshot>,
    max_concurrent_jobs: usize,
    confirmation_remaining: Option<Duration>,
    prewarm_status: Option<&PrewarmProgress>,
    prewarm_spinner: Option<char>,
) {
    let size = frame.area();

    // New v0.3.0 layout: 6 sections
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(0)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(5), // Aircraft Position (3 content lines + border + margin)
            Constraint::Length(4), // Prefetch System
            Constraint::Length(8), // Scenery System (2-column)
            Constraint::Length(8), // Input/Output (2-column)
            Constraint::Length(6), // Caches
            Constraint::Min(0),    // Padding
        ])
        .split(size);

    // 1. Header
    render_header(frame, chunks[0], uptime, prewarm_status, prewarm_spinner);

    // 2. Aircraft Position (now using APT module)
    render_aircraft_position(frame, chunks[1], aircraft_position_status);

    // 3. Prefetch System (new panel)
    render_prefetch_system(frame, chunks[2], prefetch_snapshot);

    // 4. Scenery System (replaces Control Plane + Pipeline)
    let scenery_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Scenery System ",
            Style::default().fg(Color::Blue),
        ));
    frame.render_widget(scenery_block, chunks[3]);
    let scenery_inner = inner_rect(chunks[3], 1, 1);
    frame.render_widget(
        ScenerySystemWidget::new(snapshot, max_concurrent_jobs)
            .with_health(control_plane_snapshot)
            .with_history(scenery_history),
        scenery_inner,
    );

    // 5. Input/Output (replaces Network + Chunk Tasks)
    let io_block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Input / Output ",
            Style::default().fg(Color::Blue),
        ));
    frame.render_widget(io_block, chunks[4]);
    let io_inner = inner_rect(chunks[4], 1, 1);
    frame.render_widget(
        InputOutputWidget::new(snapshot, network_history, provider_name)
            .with_disk_history(disk_history),
        io_inner,
    );

    // 6. Caches (compact format)
    let cache_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(" Caches ", Style::default().fg(Color::Blue)));
    frame.render_widget(cache_block, chunks[5]);
    let cache_inner = inner_rect(chunks[5], 1, 1);
    frame.render_widget(
        CacheWidgetCompact::new(snapshot).with_config(cache_config.clone()),
        cache_inner,
    );

    // Quit confirmation overlay (if active)
    if let Some(remaining) = confirmation_remaining {
        render_quit_confirmation(frame, size, remaining);
    }
}

/// Render the aircraft position section using APT module.
///
/// Shows position, accuracy, and provider connection status.
/// Layout matches the wireframe:
/// ```text
/// Position : 9.99°E, 53.63°N | Hdg: 090° | GS: 489kt | Alt: 33532ft
/// Accuracy : 10m (GPS)
/// GPS: * Connected
/// ```
fn render_aircraft_position(frame: &mut Frame, area: Rect, status: &AircraftPositionStatus) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Aircraft Position ",
            Style::default().fg(Color::Magenta),
        ));

    frame.render_widget(block, area);
    let inner = inner_rect(area, 1, 1);

    frame.render_widget(AircraftPositionWidget::new(status), inner);
}

/// Render the prefetch system panel (new in v0.3.0).
fn render_prefetch_system(frame: &mut Frame, area: Rect, prefetch: &PrefetchStatusSnapshot) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Prefetch System ",
            Style::default().fg(Color::Yellow),
        ));

    frame.render_widget(block, area);
    let inner = inner_rect(area, 1, 1);

    frame.render_widget(
        PrefetchSystemWidget::new(prefetch.prefetch_mode, prefetch.detailed_stats.as_ref()),
        inner,
    );
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
        if prewarm.is_complete() {
            // Show completion message
            let (message, color) = if prewarm.was_cancelled {
                ("Pre-warming cancelled", Color::Red)
            } else {
                ("Pre-warming complete!", Color::Green)
            };
            Line::from(vec![
                Span::styled("✓ ", Style::default().fg(color)),
                Span::styled(message, Style::default().fg(color)),
                Span::styled(" (", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    &prewarm.icao,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" - {} tiles)", prewarm.tiles_loaded),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("  │  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Uptime: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&uptime_str, Style::default().fg(Color::White)),
                Span::styled("  │  Press ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::Yellow)),
                Span::styled(" to quit", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            // Show progress
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
                    Style::default().fg(Color::White),
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
        }
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
