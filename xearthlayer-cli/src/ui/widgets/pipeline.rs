//! Pipeline stage visualization widget.
//!
//! Shows the flow of tiles through pipeline stages:
//! FUSE → DOWNLOAD → ASSEMBLE → ENCODE → CACHE → DONE

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

/// Widget displaying pipeline stage status.
pub struct PipelineWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
}

impl<'a> PipelineWidget<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self { snapshot }
    }

    /// Create a progress bar string (filled/empty blocks).
    fn progress_bar(active: usize, max_display: usize) -> String {
        let filled = active.min(max_display);
        let empty = max_display.saturating_sub(filled);
        format!("{}{}", "█".repeat(filled), "░".repeat(empty))
    }
}

impl Widget for PipelineWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        // Calculate stage metrics
        // FUSE waiting = jobs_submitted - jobs_completed - jobs_failed - jobs_active
        let fuse_waiting = self
            .snapshot
            .jobs_submitted
            .saturating_sub(self.snapshot.jobs_completed)
            .saturating_sub(self.snapshot.jobs_failed)
            .saturating_sub(self.snapshot.jobs_active as u64);

        let download_active = self.snapshot.downloads_active;
        let encode_active = self.snapshot.encodes_active;
        let completed = self.snapshot.jobs_completed;

        // Use fixed-width columns for perfect alignment
        // Column widths: FUSE=8, arrow=5, DOWNLOAD=12, arrow=5, ASSEMBLE=12, arrow=5, ENCODE=10, arrow=5, CACHE=9, arrow=5, DONE=10
        //
        // Layout (each stage centered in its column):
        //   FUSE   ──►   DOWNLOAD   ──►   ASSEMBLE   ──►   ENCODE   ──►   CACHE   ──►    DONE
        //    0           ░░░░   0          ░░░░   0         ░░░░  0        ░░░░  0          0
        //   wait          active            active           active        active      completed

        // Pipeline flow line with arrows - use fixed column widths
        let flow_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:^6}", "FUSE"), Style::default().fg(Color::Cyan)),
            Span::raw(" ──► "),
            Span::styled(
                format!("{:^10}", "DOWNLOAD"),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" ──► "),
            Span::styled(
                format!("{:^10}", "ASSEMBLE"),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(" ──► "),
            Span::styled(
                format!("{:^10}", "ENCODE"),
                Style::default().fg(Color::Blue),
            ),
            Span::raw(" ──► "),
            Span::styled(
                format!("{:^10}", "CACHE"),
                Style::default().fg(Color::Green),
            ),
            Span::raw(" ──► "),
            Span::styled(format!("{:^10}", "DONE"), Style::default().fg(Color::White)),
        ]);

        // Counts line - each column same width as header
        let counts_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:^6}", fuse_waiting),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("      "), // arrow spacer
            Span::styled(
                format!(
                    "{} {:>3}",
                    Self::progress_bar(download_active, 4),
                    download_active
                ),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!(
                    "{} {:>3}",
                    Self::progress_bar(self.snapshot.jobs_active, 4),
                    self.snapshot.jobs_active
                ),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!(
                    "{} {:>3}",
                    Self::progress_bar(encode_active, 4),
                    encode_active
                ),
                Style::default().fg(Color::Blue),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!("{} {:>3}", Self::progress_bar(0, 4), 0),
                Style::default().fg(Color::Green),
            ),
            Span::raw("      "), // arrow spacer
            Span::styled(
                format!("{:^10}", completed),
                Style::default().fg(Color::White),
            ),
        ]);

        // Labels line - centered in each column
        let labels_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:^6}", "wait"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("      "), // arrow spacer
            Span::styled(
                format!("{:^10}", "active"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!("{:^10}", "active"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!("{:^10}", "active"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("     "), // arrow spacer
            Span::styled(
                format!("{:^10}", "active"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("      "), // arrow spacer
            Span::styled(
                format!("{:^10}", "completed"),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let text = vec![
            Line::raw(""),
            flow_line,
            counts_line,
            labels_line,
            Line::raw(""),
        ];

        let paragraph = Paragraph::new(text).block(block);
        paragraph.render(area, buf);
    }
}
