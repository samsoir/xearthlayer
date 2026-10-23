//! The details pane: the selected row's fields.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::chrome::pane;

/// One "Name: value" line.
pub fn field(name: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{name}: "), Style::default().fg(Color::Gray)),
        Span::raw(value.into()),
    ])
}

/// A bold heading line.
pub fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// Draw `lines` in a pane titled "Details", wrapped to its width. With no
/// lines, `empty` says why there is nothing to show.
pub fn details_pane(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, empty: &str) {
    let block = pane("Details");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let paragraph = if lines.is_empty() {
        Paragraph::new(format!(" {empty}")).style(Style::default().fg(Color::Gray))
    } else {
        Paragraph::new(lines)
    };
    frame.render_widget(paragraph.wrap(Wrap { trim: false }), inner);
}
