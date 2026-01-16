//! Prefetch system status widget.
//!
//! Displays prefetch system status including:
//! - Status (Active/Paused) with color indicator
//! - Mode (tile based / radial / heading-aware)
//! - Loading tiles list (up to 10 tile coordinates)

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::prefetch::{CircuitState, DetailedPrefetchStats, PrefetchMode};

/// Widget displaying prefetch system status.
pub struct PrefetchSystemWidget<'a> {
    mode: PrefetchMode,
    stats: Option<&'a DetailedPrefetchStats>,
}

impl<'a> PrefetchSystemWidget<'a> {
    /// Create a new prefetch system widget.
    pub fn new(mode: PrefetchMode, stats: Option<&'a DetailedPrefetchStats>) -> Self {
        Self { mode, stats }
    }

    /// Format tile coordinate as compact string (e.g., "+102+40").
    fn format_tile_coord(lat: i32, lon: i32) -> String {
        let lat_sign = if lat >= 0 { "+" } else { "" };
        let lon_sign = if lon >= 0 { "+" } else { "" };
        format!("{}{}{}{}", lat_sign, lat, lon_sign, lon)
    }

    /// Get the status text and color based on current state.
    fn status_info(&self) -> (&'static str, Color) {
        if let Some(stats) = self.stats {
            // Check circuit breaker state
            if let Some(circuit_state) = &stats.circuit_state {
                match circuit_state {
                    CircuitState::Open => return ("Paused", Color::Magenta),
                    CircuitState::HalfOpen => return ("Recovering", Color::Yellow),
                    CircuitState::Closed => {} // Continue to check other conditions
                }
            }

            // Check activity
            if stats.is_active {
                ("Active", Color::Green)
            } else {
                ("Idle", Color::DarkGray)
            }
        } else {
            match self.mode {
                PrefetchMode::CircuitOpen => ("Paused", Color::Magenta),
                PrefetchMode::Idle => ("Idle", Color::DarkGray),
                _ => ("Active", Color::Green),
            }
        }
    }

    /// Get mode display text.
    fn mode_text(&self) -> &'static str {
        match self.mode {
            PrefetchMode::Telemetry => "Heading-Aware (Telemetry)",
            PrefetchMode::FuseInference => "Heading-Aware (Inferred)",
            PrefetchMode::Radial => "Tile Based (Radial)",
            PrefetchMode::Idle => "Idle",
            PrefetchMode::CircuitOpen => "Paused (High Load)",
        }
    }

    /// Get mode color.
    fn mode_color(&self) -> Color {
        match self.mode {
            PrefetchMode::Telemetry => Color::Green,
            PrefetchMode::FuseInference => Color::Yellow,
            PrefetchMode::Radial => Color::Cyan,
            PrefetchMode::Idle => Color::DarkGray,
            PrefetchMode::CircuitOpen => Color::Magenta,
        }
    }
}

impl Widget for PrefetchSystemWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (status_text, status_color) = self.status_info();
        let mode_text = self.mode_text();
        let mode_color = self.mode_color();

        // Row 1: Status with indicator
        let status_line = Line::from(vec![
            Span::styled("Status:  ", Style::default().fg(Color::DarkGray)),
            Span::styled("● ", Style::default().fg(status_color)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]);

        // Row 2: Mode
        let mode_line = Line::from(vec![
            Span::styled("Mode:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(mode_text, Style::default().fg(mode_color)),
        ]);

        // Row 3: Loading tiles (or stats summary if no loading tiles)
        let loading_line = if let Some(stats) = self.stats {
            if !stats.loading_tiles.is_empty() {
                // Show loading tiles
                let tiles_str: Vec<String> = stats
                    .loading_tiles
                    .iter()
                    .take(10) // Limit to 10 for display
                    .map(|(lat, lon)| Self::format_tile_coord(*lat, *lon))
                    .collect();
                let tiles_display = tiles_str.join(", ");
                let more = if stats.loading_tiles.len() > 10 {
                    format!(" +{} more", stats.loading_tiles.len() - 10)
                } else {
                    String::new()
                };

                Line::from(vec![
                    Span::styled("Loading: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(tiles_display, Style::default().fg(Color::Yellow)),
                    Span::styled(more, Style::default().fg(Color::DarkGray)),
                ])
            } else {
                // Show stats summary when no tiles loading
                Line::from(vec![
                    Span::styled("Stats:   ", Style::default().fg(Color::DarkGray)),
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
                    Span::styled(" hits", Style::default().fg(Color::DarkGray)),
                    Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled("⊘", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("{}", stats.ttl_skipped),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(" skipped", Style::default().fg(Color::DarkGray)),
                ])
            }
        } else {
            Line::from(vec![
                Span::styled("Loading: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Waiting for prefetch data...",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        };

        let text = vec![status_line, mode_line, loading_line];
        let paragraph = Paragraph::new(text);
        paragraph.render(area, buf);
    }
}
