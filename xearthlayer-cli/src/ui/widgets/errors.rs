//! Error summary widget.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

/// Widget displaying error statistics.
pub struct ErrorsWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
}

impl<'a> ErrorsWidget<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self { snapshot }
    }
}

impl Widget for ErrorsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::NONE);

        let chunks_total = self.snapshot.chunks_downloaded + self.snapshot.chunks_failed;
        let chunk_fail_rate = if chunks_total > 0 {
            (self.snapshot.chunks_failed as f64 / chunks_total as f64) * 100.0
        } else {
            0.0
        };

        let chunk_color = if self.snapshot.chunks_failed > 0 && chunk_fail_rate > 1.0 {
            Color::Red
        } else if chunk_fail_rate > 0.1 {
            Color::Yellow
        } else {
            Color::Green
        };

        let line = Line::from(vec![
            Span::styled("  Chunks:  ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{} failed", self.snapshot.chunks_failed),
                Style::default().fg(chunk_color),
            ),
            Span::styled(
                format!(" ({:.2}%)", chunk_fail_rate),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("  │  Retries: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", self.snapshot.chunks_retried),
                Style::default().fg(Color::White),
            ),
            Span::styled("  │  Timeouts: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", self.snapshot.jobs_timed_out),
                Style::default().fg(if self.snapshot.jobs_timed_out > 0 {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
            Span::styled("  │  Errors: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", self.snapshot.jobs_failed),
                Style::default().fg(if self.snapshot.jobs_failed > 0 {
                    Color::Red
                } else {
                    Color::Green
                }),
            ),
        ]);

        let paragraph = Paragraph::new(vec![line]).block(block);
        paragraph.render(area, buf);
    }
}
