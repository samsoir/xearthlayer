//! The details pane: the selected row's fields.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use xearthlayer_package::continent_name;

use super::chrome::pane;
use crate::commands::packages::actions::RunActive;

/// One "Name: value" line.
pub fn field(name: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{name}: "), Style::default().fg(Color::Gray)),
        Span::raw(value.into()),
    ])
}

/// "Continent: Africa", if the region code names one.
pub fn continent(region: &str) -> Option<Line<'static>> {
    continent_name(region).map(|name| field("Continent", name))
}

/// A bold heading line.
pub fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// The full refusal that ends a details pane while `run` is serving: a
/// blank line, then the refusal in the banner's colour.
pub fn run_refusal(run: &RunActive) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(Span::styled(
            run.message(),
            Style::default().fg(Color::Yellow),
        )),
    ]
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
