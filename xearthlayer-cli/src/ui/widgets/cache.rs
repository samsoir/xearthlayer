//! Cache status widgets showing memory and disk cache utilization.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

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

/// Widget displaying cache statistics.
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

    fn format_bytes(bytes: usize) -> String {
        if bytes >= 1_000_000_000 {
            format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
        } else if bytes >= 1_000_000 {
            format!("{:.1} MB", bytes as f64 / 1_000_000.0)
        } else if bytes >= 1_000 {
            format!("{:.1} KB", bytes as f64 / 1_000.0)
        } else {
            format!("{} B", bytes)
        }
    }

    fn progress_bar(current: usize, max: usize, width: usize) -> String {
        let ratio = if max > 0 {
            (current as f64 / max as f64).min(1.0)
        } else {
            0.0
        };
        let filled = (ratio * width as f64).round() as usize;
        let empty = width.saturating_sub(filled);
        format!("{}{}", "█".repeat(filled), "░".repeat(empty))
    }
}

impl Widget for CacheWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        // Memory cache line
        let memory_current = self.snapshot.memory_cache_size_bytes as usize;
        let memory_bar = Self::progress_bar(memory_current, self.config.memory_max_bytes, 16);
        let memory_hits = self.snapshot.memory_cache_hits;
        let memory_misses = self.snapshot.memory_cache_misses;
        let memory_hit_rate = self.snapshot.memory_cache_hit_rate * 100.0;
        let memory_tiles = memory_hits + memory_misses; // Approximate tile count

        let memory_line = Line::from(vec![
            Span::styled("  Memory Cache:  ", Style::default().fg(Color::White)),
            Span::styled(memory_bar, Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} / {}",
                    Self::format_bytes(memory_current),
                    Self::format_bytes(self.config.memory_max_bytes)
                ),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("  ({} tiles)", memory_tiles),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let memory_stats_line = Line::from(vec![
            Span::raw("                 "),
            Span::styled("Hit rate: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", memory_hit_rate),
                Style::default().fg(if memory_hit_rate > 80.0 {
                    Color::Green
                } else if memory_hit_rate > 50.0 {
                    Color::Yellow
                } else {
                    Color::Red
                }),
            ),
            Span::styled("  │  Hits: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", memory_hits),
                Style::default().fg(Color::White),
            ),
            Span::styled("  │  Misses: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", memory_misses),
                Style::default().fg(Color::White),
            ),
        ]);

        // Disk cache line
        let disk_current = self.snapshot.disk_cache_size_bytes as usize;
        let disk_bar = Self::progress_bar(disk_current, self.config.disk_max_bytes, 16);
        let disk_hits = self.snapshot.disk_cache_hits;
        let disk_misses = self.snapshot.disk_cache_misses;
        let disk_hit_rate = self.snapshot.disk_cache_hit_rate * 100.0;
        let disk_chunks = disk_hits + disk_misses;

        let disk_line = Line::from(vec![
            Span::raw(""),
            Span::styled("  Disk Cache:    ", Style::default().fg(Color::White)),
            Span::styled(disk_bar, Style::default().fg(Color::Blue)),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} / {}",
                    Self::format_bytes(disk_current),
                    Self::format_bytes(self.config.disk_max_bytes)
                ),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(
                format!("  ({} chunks)", disk_chunks),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let disk_stats_line = Line::from(vec![
            Span::raw("                 "),
            Span::styled("Hit rate: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", disk_hit_rate),
                Style::default().fg(if disk_hit_rate > 80.0 {
                    Color::Green
                } else if disk_hit_rate > 50.0 {
                    Color::Yellow
                } else {
                    Color::Red
                }),
            ),
            Span::styled("  │  Hits: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", disk_hits), Style::default().fg(Color::White)),
            Span::styled("  │  Misses: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", disk_misses),
                Style::default().fg(Color::White),
            ),
        ]);

        let text = vec![
            memory_line,
            memory_stats_line,
            Line::raw(""),
            disk_line,
            disk_stats_line,
        ];

        let paragraph = Paragraph::new(text).block(block);
        paragraph.render(area, buf);
    }
}
