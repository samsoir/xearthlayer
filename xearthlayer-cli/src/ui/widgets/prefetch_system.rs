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
            PrefetchMode::Radial => "Radial",
            PrefetchMode::TileBased => "Tile-Based (DSF)",
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
            PrefetchMode::TileBased => Color::Blue,
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

        // Row 1: Status with indicator (3-char indent for alignment)
        let status_line = Line::from(vec![
            Span::styled("   Status:  ", Style::default().fg(Color::DarkGray)),
            Span::styled("● ", Style::default().fg(status_color)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]);

        // Row 2: Mode (3-char indent for alignment)
        let mode_line = Line::from(vec![
            Span::styled("   Mode:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(mode_text, Style::default().fg(mode_color)),
        ]);

        // Only show Status and Mode (removed Loading/Stats line per user feedback)
        let text = vec![status_line, mode_line];
        let paragraph = Paragraph::new(text);
        paragraph.render(area, buf);
    }
}
