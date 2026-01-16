//! Cache status widgets showing memory and disk cache utilization.
//!
//! Displays cache statistics in a compact format:
//! ```text
//! Memory: [████████░░] 1.2/2.0 GB (65%) | Hit: 89.2%
//! Disk:   [██████░░░░] 6.5/20.0 GB (32%) | Hit: 72.5%
//! ```
//!
//! Note: CacheWidget is deprecated - use CacheWidgetCompact instead.

#![allow(dead_code)] // Legacy CacheWidget kept for compatibility

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

use super::primitives::{format_bytes, format_bytes_usize, ProgressBar, ProgressBarStyle};

/// Configuration for cache display.
#[derive(Clone)]
pub struct CacheConfig {
    /// Maximum memory cache size in bytes.
    pub memory_max_bytes: usize,
    /// Maximum disk cache size in bytes.
    pub disk_max_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_max_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
            disk_max_bytes: 20 * 1024 * 1024 * 1024,  // 20 GB
        }
    }
}

/// Widget displaying cache statistics in compact format.
///
/// Shows memory and disk cache on separate lines with:
/// - Progress bar with fractional block precision
/// - Current/max size
/// - Utilization percentage
/// - Hit rate
pub struct CacheWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    config: CacheConfig,
}

impl<'a> CacheWidget<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self {
            snapshot,
            config: CacheConfig::default(),
        }
    }

    pub fn with_config(mut self, config: CacheConfig) -> Self {
        self.config = config;
        self
    }

    /// Get color for hit rate based on threshold.
    fn hit_rate_color(rate: f64) -> Color {
        if rate > 80.0 {
            Color::Green
        } else if rate > 50.0 {
            Color::Yellow
        } else {
            Color::Red
        }
    }

    /// Build a compact cache line with all metrics.
    fn build_cache_line(
        label: &str,
        current_bytes: u64,
        max_bytes: u64,
        hit_rate: f64,
        bar_color: Color,
    ) -> Line<'static> {
        let utilization = if max_bytes > 0 {
            (current_bytes as f64 / max_bytes as f64) * 100.0
        } else {
            0.0
        };

        let progress_bar = ProgressBar::from_u64(current_bytes, max_bytes, 10)
            .bar_style(ProgressBarStyle::Fractional)
            .to_string();

        Line::from(vec![
            Span::styled(format!("  {}: ", label), Style::default().fg(Color::White)),
            Span::styled(
                format!("[{}]", progress_bar),
                Style::default().fg(bar_color),
            ),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{}/{}",
                    format_bytes(current_bytes),
                    format_bytes(max_bytes)
                ),
                Style::default().fg(bar_color),
            ),
            Span::styled(
                format!(" ({:.0}%)", utilization),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled("Hit: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", hit_rate),
                Style::default().fg(Self::hit_rate_color(hit_rate)),
            ),
        ])
    }
}

impl Widget for CacheWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        // Memory cache metrics
        let memory_current = self.snapshot.memory_cache_size_bytes;
        let memory_max = self.config.memory_max_bytes as u64;
        let memory_hit_rate = self.snapshot.memory_cache_hit_rate * 100.0;

        // Disk cache metrics
        let disk_current = self.snapshot.disk_cache_size_bytes;
        let disk_max = self.config.disk_max_bytes as u64;
        let disk_hit_rate = self.snapshot.disk_cache_hit_rate * 100.0;

        let memory_line = Self::build_cache_line(
            "Memory",
            memory_current,
            memory_max,
            memory_hit_rate,
            Color::Cyan,
        );

        let disk_line =
            Self::build_cache_line("Disk  ", disk_current, disk_max, disk_hit_rate, Color::Blue);

        let text = vec![memory_line, disk_line];

        let paragraph = Paragraph::new(text).block(block);
        paragraph.render(area, buf);
    }
}

/// Compact cache widget for the new dashboard layout.
///
/// An even more compact version that fits in 4 lines with additional
/// stats like hits/misses on a second row per cache type.
pub struct CacheWidgetCompact<'a> {
    snapshot: &'a TelemetrySnapshot,
    config: CacheConfig,
}

impl<'a> CacheWidgetCompact<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self {
            snapshot,
            config: CacheConfig::default(),
        }
    }

    pub fn with_config(mut self, config: CacheConfig) -> Self {
        self.config = config;
        self
    }
}

impl Widget for CacheWidgetCompact<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        // Memory cache
        let memory_current = self.snapshot.memory_cache_size_bytes;
        let memory_max = self.config.memory_max_bytes as u64;
        let memory_hit_rate = self.snapshot.memory_cache_hit_rate * 100.0;
        let memory_hits = self.snapshot.memory_cache_hits;
        let memory_misses = self.snapshot.memory_cache_misses;

        let memory_bar = ProgressBar::from_u64(memory_current, memory_max, 10)
            .bar_style(ProgressBarStyle::Fractional)
            .to_string();

        let memory_line1 = Line::from(vec![
            Span::styled("  Memory: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("[{}]", memory_bar),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{}/{}",
                    format_bytes(memory_current),
                    format_bytes_usize(self.config.memory_max_bytes)
                ),
                Style::default().fg(Color::Cyan),
            ),
        ]);

        let memory_line2 = Line::from(vec![
            Span::raw("           "),
            Span::styled("Hit: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", memory_hit_rate),
                Style::default().fg(CacheWidget::hit_rate_color(memory_hit_rate)),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} hits", memory_hits),
                Style::default().fg(Color::Green),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} miss", memory_misses),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        // Disk cache
        let disk_current = self.snapshot.disk_cache_size_bytes;
        let disk_max = self.config.disk_max_bytes as u64;
        let disk_hit_rate = self.snapshot.disk_cache_hit_rate * 100.0;
        let disk_hits = self.snapshot.disk_cache_hits;
        let disk_misses = self.snapshot.disk_cache_misses;

        let disk_bar = ProgressBar::from_u64(disk_current, disk_max, 10)
            .bar_style(ProgressBarStyle::Fractional)
            .to_string();

        let disk_line1 = Line::from(vec![
            Span::styled("  Disk:   ", Style::default().fg(Color::White)),
            Span::styled(format!("[{}]", disk_bar), Style::default().fg(Color::Blue)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{}/{}",
                    format_bytes(disk_current),
                    format_bytes_usize(self.config.disk_max_bytes)
                ),
                Style::default().fg(Color::Blue),
            ),
        ]);

        let disk_line2 = Line::from(vec![
            Span::raw("           "),
            Span::styled("Hit: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", disk_hit_rate),
                Style::default().fg(CacheWidget::hit_rate_color(disk_hit_rate)),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} hits", disk_hits),
                Style::default().fg(Color::Green),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} miss", disk_misses),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let text = vec![memory_line1, memory_line2, disk_line1, disk_line2];

        let paragraph = Paragraph::new(text).block(block);
        paragraph.render(area, buf);
    }
}
