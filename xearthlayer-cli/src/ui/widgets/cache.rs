//! Cache status widgets showing memory and disk cache utilization.
//!
//! Displays cache statistics in a compact 3-column 2-line format:
//! ```text
//! Memory [████░░░░] 1.2/2.0 GB | DDS Disk [████░░░░] 6.5/12.0 GB | Chunks [██░░░░░░] 2.1/8.0 GB
//! 89.2%  │ 1.5M hits │ 42K miss  │ 93.6%   │ 17.3K hits │ 1.2K miss │ 21.7% │ 5.0K hits │ 18K miss
//! ```

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::TelemetrySnapshot;

use super::primitives::{format_bytes, format_count, ProgressBar, ProgressBarStyle};

/// Configuration for cache display.
#[derive(Clone)]
pub struct CacheConfig {
    /// Maximum memory cache size in bytes.
    pub memory_max_bytes: usize,
    /// Maximum DDS disk cache size in bytes.
    pub dds_disk_max_bytes: usize,
    /// Maximum chunk disk cache size in bytes.
    pub chunk_disk_max_bytes: usize,
    /// Whether the disk tiers run this session (`false` with `--no-cache`).
    pub disk_caching: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_max_bytes: 2 * 1024 * 1024 * 1024,
            dds_disk_max_bytes: 12 * 1024 * 1024 * 1024,
            chunk_disk_max_bytes: 8 * 1024 * 1024 * 1024,
            disk_caching: true,
        }
    }
}

/// Render a disk tier that does not exist this session (`--no-cache`).
fn render_tier_off(buf: &mut Buffer, area: Rect, label: &str) {
    if area.height < 2 || area.width < 10 {
        return;
    }
    let line1 = Line::from(vec![
        Span::styled(format!(" {} ", label), Style::default().fg(Color::White)),
        Span::styled("off", Style::default().fg(Color::DarkGray)),
    ]);
    let line2 = Line::from(Span::styled(
        " --no-cache",
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(vec![line1, line2]).render(area, buf);
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

/// Data for rendering a single cache tier column.
struct CacheTierData<'a> {
    label: &'a str,
    color: Color,
    current_bytes: u64,
    max_bytes: u64,
    hit_rate: f64,
    hits: u64,
    misses: u64,
}

/// Render a single cache tier in its column (2 lines).
///
/// Line 1: `Label [████░░░░] size/max`
/// Line 2: `hit% │ Nhits │ Nmiss`
fn render_cache_tier(buf: &mut Buffer, area: Rect, data: &CacheTierData<'_>) {
    let CacheTierData {
        label,
        color,
        current_bytes,
        max_bytes,
        hit_rate,
        hits,
        misses,
    } = *data;
    if area.height < 2 || area.width < 10 {
        return;
    }

    let bar_width = 8;
    let progress_bar = ProgressBar::from_u64(current_bytes, max_bytes, bar_width)
        .bar_style(ProgressBarStyle::Fractional)
        .to_string();

    let line1 = Line::from(vec![
        Span::styled(format!(" {} ", label), Style::default().fg(Color::White)),
        Span::styled(format!("[{}]", progress_bar), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(
            format!(
                "{}/{}",
                format_bytes(current_bytes),
                format_bytes(max_bytes)
            ),
            Style::default().fg(color),
        ),
    ]);

    let hit_color = hit_rate_color(hit_rate);
    let line2 = Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{:.1}%", hit_rate), Style::default().fg(hit_color)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} hits", format_count(hits)),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} miss", format_count(misses)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let paragraph = Paragraph::new(vec![line1, line2]);
    paragraph.render(area, buf);
}

/// Compact cache widget for the dashboard layout.
///
/// Renders three cache tiers (Memory, DDS Disk, Chunks) in a 3-column 2-line layout.
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
        use ratatui::layout::{Constraint, Direction, Layout};

        if area.height < 2 {
            return;
        }

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
            ])
            .split(area);

        // Memory tier — FUSE-only counts/rate so the widget reflects
        // X-Plane's actual cache experience rather than aggregate totals
        // diluted by prefetch/prewarm traffic (see #171).
        render_cache_tier(
            buf,
            columns[0],
            &CacheTierData {
                label: "Memory",
                color: Color::Magenta,
                current_bytes: self.snapshot.memory_cache_size_bytes,
                max_bytes: self.config.memory_max_bytes as u64,
                hit_rate: self.snapshot.fuse_memory_cache_hit_rate * 100.0,
                hits: self.snapshot.fuse_memory_cache_hits,
                misses: self.snapshot.fuse_memory_cache_misses,
            },
        );

        // Without disk caching (`--no-cache`) the disk tiers do not exist:
        // say so rather than draw a budget nothing is using (#400).
        if !self.config.disk_caching {
            render_tier_off(buf, columns[1], "DDS Disk");
            render_tier_off(buf, columns[2], "Chunks");
            return;
        }

        // DDS Disk tier — FUSE-only counts/rate (see #171).
        render_cache_tier(
            buf,
            columns[1],
            &CacheTierData {
                label: "DDS Disk",
                color: Color::Blue,
                current_bytes: self.snapshot.dds_disk_cache_size_bytes,
                max_bytes: self.config.dds_disk_max_bytes as u64,
                hit_rate: self.snapshot.fuse_dds_disk_cache_hit_rate * 100.0,
                hits: self.snapshot.fuse_dds_disk_cache_hits,
                misses: self.snapshot.fuse_dds_disk_cache_misses,
            },
        );

        // Chunks tier
        render_cache_tier(
            buf,
            columns[2],
            &CacheTierData {
                label: "Chunks",
                color: Color::Cyan,
                current_bytes: self.snapshot.chunk_disk_cache_size_bytes,
                max_bytes: self.config.chunk_disk_max_bytes as u64,
                hit_rate: self.snapshot.chunk_disk_cache_hit_rate * 100.0,
                hits: self.snapshot.chunk_disk_cache_hits,
                misses: self.snapshot.chunk_disk_cache_misses,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    fn render_compact_to_string(snapshot: &TelemetrySnapshot) -> String {
        let area = Rect::new(0, 0, 120, 2);
        let mut buf = Buffer::empty(area);
        CacheWidgetCompact::new(snapshot).render(area, &mut buf);

        let mut output = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = &buf[(x, y)];
                output.push_str(cell.symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn test_three_tier_labels_present() {
        let snapshot = TelemetrySnapshot::default();
        let output = render_compact_to_string(&snapshot);
        assert!(output.contains("Memory"), "Should contain Memory label");
        assert!(output.contains("DDS Disk"), "Should contain DDS Disk label");
        assert!(output.contains("Chunks"), "Should contain Chunks label");
    }

    /// #400: with `--no-cache` the disk tiers do not exist, so the widget
    /// must not draw a size bar or budget for them.
    #[test]
    fn disk_tiers_show_off_without_disk_caching() {
        let snapshot = TelemetrySnapshot::default();
        let area = Rect::new(0, 0, 120, 2);
        let mut buf = Buffer::empty(area);
        CacheWidgetCompact::new(&snapshot)
            .with_config(CacheConfig {
                disk_caching: false,
                ..CacheConfig::default()
            })
            .render(area, &mut buf);
        let output: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();

        assert!(output.contains("Memory"), "{output}");
        assert!(output.contains("DDS Disk off"), "{output}");
        assert!(output.contains("Chunks off"), "{output}");
        assert!(output.contains("--no-cache"), "{output}");
        // The default budgets, 12 GiB and 8 GiB, as `format_bytes` writes them.
        assert!(!output.contains("12.88 GB"), "no DDS budget: {output}");
        assert!(!output.contains("8.59 GB"), "no chunk budget: {output}");
    }

    #[test]
    fn test_dds_disk_hits_formatted() {
        let snapshot = TelemetrySnapshot {
            fuse_dds_disk_cache_hits: 17_300,
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("17.3K hits"),
            "DDS disk hits should be formatted, got:\n{}",
            output
        );
    }

    #[test]
    fn test_chunk_disk_misses_formatted() {
        let snapshot = TelemetrySnapshot {
            chunk_disk_cache_misses: 226_000,
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("226.0K miss"),
            "Chunk disk misses should be formatted, got:\n{}",
            output
        );
    }

    #[test]
    fn test_memory_hits_formatted() {
        let snapshot = TelemetrySnapshot {
            fuse_memory_cache_hits: 1_500_000,
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("1.5M hits"),
            "Memory hits should be formatted, got:\n{}",
            output
        );
    }

    #[test]
    fn test_memory_tier_uses_fuse_only_rate() {
        // Regression for #171: widget must render the FUSE-only memory hit rate
        // to show X-Plane's actual cache experience, not the aggregate polluted
        // by prefetch traffic.
        let snapshot = TelemetrySnapshot {
            // Aggregate: looks like ~40% hit rate (dominated by prefetch misses)
            memory_cache_hits: 40,
            memory_cache_misses: 60,
            memory_cache_hit_rate: 0.40,
            // FUSE only: X-Plane actually sees 90% hit rate
            fuse_memory_cache_hits: 90,
            fuse_memory_cache_misses: 10,
            fuse_memory_cache_hit_rate: 0.90,
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("90.0%"),
            "Memory tier should render FUSE rate 90.0%, got:\n{}",
            output
        );
        assert!(
            !output.contains("40.0%"),
            "Memory tier must NOT render aggregate rate 40.0%, got:\n{}",
            output
        );
        assert!(
            output.contains("90 hits"),
            "Memory tier should render FUSE hits (90), got:\n{}",
            output
        );
    }

    #[test]
    fn test_dds_disk_tier_uses_fuse_only_rate() {
        // Regression for #171: widget must render the FUSE-only DDS disk hit
        // rate, not the aggregate polluted by prefetch traffic.
        let snapshot = TelemetrySnapshot {
            // Aggregate: dominated by prefetch misses
            dds_disk_cache_hits: 200,
            dds_disk_cache_misses: 800,
            dds_disk_cache_hit_rate: 0.20,
            // FUSE only: 80% hit rate
            fuse_dds_disk_cache_hits: 80,
            fuse_dds_disk_cache_misses: 20,
            fuse_dds_disk_cache_hit_rate: 0.80,
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("80.0%"),
            "DDS disk tier should render FUSE rate 80.0%, got:\n{}",
            output
        );
        assert!(
            !output.contains("20.0%"),
            "DDS disk tier must NOT render aggregate rate 20.0%, got:\n{}",
            output
        );
        assert!(
            output.contains("80 hits"),
            "DDS disk tier should render FUSE hits (80), got:\n{}",
            output
        );
    }

    #[test]
    fn test_chunk_tier_still_uses_aggregate() {
        // Chunks are fine as aggregate (all chunk reads flow through one path,
        // no origin discrimination needed). This pins that behavior.
        let snapshot = TelemetrySnapshot {
            chunk_disk_cache_hits: 5000,
            chunk_disk_cache_misses: 1000,
            chunk_disk_cache_hit_rate: 5000.0 / 6000.0, // ~83.3%
            ..Default::default()
        };
        let output = render_compact_to_string(&snapshot);
        assert!(
            output.contains("83.3%"),
            "Chunks tier should render aggregate rate 83.3%, got:\n{}",
            output
        );
        assert!(
            output.contains("5.0K hits"),
            "Chunks tier should render aggregate hit count, got:\n{}",
            output
        );
    }
}
