//! The frame around the tabs, and the helpers every list shares.

use std::ops::Range;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::ui::packages::app::{AppState, Tab};

/// The tabs in title-bar order, with the digit that opens each.
const TABS: [(Tab, char, &str); 3] = [
    (Tab::Local, '1', "Local"),
    (Tab::Library, '2', "Library"),
    (Tab::Progress, '3', "Progress"),
];

/// ` XEarthLayer Packages   [1 Local]  2 Library  3 Progress (N)`
pub fn title_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut spans = vec![Span::styled(
        " XEarthLayer Packages   ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (i, (tab, digit, name)) in TABS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let mut text = format!("{digit} {name}");
        if *tab == Tab::Progress && state.unfinished() > 0 {
            text.push_str(&format!(" ({})", state.unfinished()));
        }
        if *tab == state.tab() {
            spans.push(Span::styled(
                format!("[{text}]"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(text));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The one-line yellow banner shown while `xearthlayer run` is active. On a
/// terminal too narrow for the whole sentence it gives the part that matters.
pub fn run_banner(frame: &mut Frame, area: Rect, pid: u32) {
    let full = format!(
        " XEarthLayer is running (pid {pid}): installs allowed, updates and deletes disabled"
    );
    let text = if full.chars().count() <= area.width as usize {
        full
    } else {
        format!(" XEarthLayer is running (pid {pid}): updates and deletes disabled")
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Yellow)),
        area,
    );
}

/// The status line above the footer.
pub fn status_line(frame: &mut Frame, area: Rect, state: &AppState) {
    if let Some(status) = state.status() {
        frame.render_widget(
            Paragraph::new(format!(" {status}")).style(Style::default().fg(Color::Gray)),
            area,
        );
    }
}

/// The keys of a tab, as (key, what it does).
pub fn footer_keys(tab: Tab) -> &'static [(&'static str, &'static str)] {
    match tab {
        Tab::Local => &[
            ("Space", "mark"),
            ("a", "all"),
            ("u", "update"),
            ("d", "delete"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Library => &[
            ("Space", "mark"),
            ("a", "all"),
            ("t", "type"),
            ("i", "install"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Progress => &[
            ("c", "cancel"),
            ("C", "cancel all"),
            ("x", "clear done"),
            ("?", "help"),
            ("q", "quit"),
        ],
    }
}

/// The current tab's keys.
pub fn footer(frame: &mut Frame, area: Rect, tab: Tab) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, what)) in footer_keys(tab).iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {what}"),
            Style::default().fg(Color::Gray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Split the body into a list on the left and a details pane on the right.
pub fn split_body(area: Rect, list_percent: u16) -> (Rect, Rect) {
    let [list, details] = Layout::horizontal([
        Constraint::Percentage(list_percent),
        Constraint::Percentage(100 - list_percent),
    ])
    .areas(area);
    (list, details)
}

/// A bordered pane with a title.
pub fn pane(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title.into()))
}

/// The rows of a list of `len` rows that are drawn in `height` lines, keeping
/// `selected` in view. A selection beyond the rows is not an error.
pub fn window(selected: usize, len: usize, height: usize) -> Range<usize> {
    let start = selected
        .saturating_sub(height / 2)
        .min(len.saturating_sub(height));
    start..len.min(start + height)
}

/// Draw a list of `len` rows in a titled pane. `row` builds the line of one
/// row from its index and the pane's inner width; the selected row is
/// highlighted. With no rows, `empty` is shown instead.
pub fn list_pane(
    frame: &mut Frame,
    area: Rect,
    title: String,
    len: usize,
    selected: usize,
    empty: &str,
    row: impl Fn(usize, usize) -> Line<'static>,
) {
    let block = pane(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if len == 0 {
        frame.render_widget(
            Paragraph::new(format!(" {empty}")).style(Style::default().fg(Color::Gray)),
            inner,
        );
        return;
    }
    let width = inner.width as usize;
    let lines: Vec<Line> = window(selected, len, inner.height as usize)
        .map(|i| {
            let line = row(i, width);
            if i == selected {
                line.style(Style::default().bg(Color::DarkGray))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Text centred in an area, one paragraph per `\n`, wrapped to its width.
pub fn centered_text(frame: &mut Frame, area: Rect, text: &str, style: Style) {
    let width = (area.width as usize).max(1);
    let lines: usize = text
        .lines()
        .map(|line| line.chars().count().div_ceil(width).max(1))
        .sum();
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length((lines as u16).min(area.height)),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(text.to_string())
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(style),
        middle,
    );
}

/// A `width` by `height` box in the middle of `area`, shrunk to fit it.
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// `text` cut to `width` columns, with an ellipsis when it was cut.
pub fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        cut.push('…');
    }
    cut
}

/// `text` cut or padded to exactly `width` columns.
pub fn fit(text: &str, width: usize) -> String {
    let cut = clip(text, width);
    format!("{cut:<width$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_keeps_the_selection_in_view() {
        assert_eq!(window(0, 100, 10), 0..10);
        assert_eq!(window(50, 100, 10), 45..55);
        assert_eq!(window(99, 100, 10), 90..100);
    }

    #[test]
    fn window_survives_a_selection_beyond_the_rows() {
        assert_eq!(window(5, 0, 10), 0..0);
        assert_eq!(window(500, 3, 10), 0..3);
        assert_eq!(window(7, 3, 0), 3..3);
    }

    #[test]
    fn fit_cuts_with_an_ellipsis_and_pads() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(clip("abcdef", 0), "");
    }

    #[test]
    fn centered_rect_never_exceeds_the_area() {
        let area = Rect::new(0, 0, 20, 10);
        assert_eq!(centered_rect(area, 100, 100), area);
        assert_eq!(centered_rect(area, 10, 4), Rect::new(5, 3, 10, 4));
    }
}
