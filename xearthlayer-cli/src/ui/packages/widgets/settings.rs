//! The Settings tab: the `[packages]` settings of `config.ini` (#336).

use std::time::Instant;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    Frame,
};

use super::chrome::{centered_text, fit, list_pane, pane, spinner};
use super::detail::{details_pane, field, heading};
use crate::ui::packages::app::{AppState, Editor, SettingRow, SettingsState};

/// The widest setting name, `auto_install_overlays`, and a space.
const NAME_WIDTH: usize = 22;

pub fn draw(frame: &mut Frame, list: Rect, details: Rect, state: &AppState, now: Instant) {
    let rows = match state.settings() {
        SettingsState::Loaded(rows) => rows,
        other => {
            let block = pane("Settings");
            let inner = block.inner(list);
            frame.render_widget(block, list);
            let text = match other {
                SettingsState::Failed(e) => format!("Could not read the configuration: {e}"),
                _ => format!(
                    "{} Reading the configuration…",
                    spinner(state.started(), now)
                ),
            };
            centered_text(frame, inner, &text, Style::default().fg(Color::Gray));
            details_pane(frame, details, Vec::new(), "");
            return;
        }
    };
    let selected = state.selected();
    let read_only = state.settings_refusal();
    list_pane(
        frame,
        list,
        "Settings ([packages] in config.ini)".to_string(),
        rows.len(),
        selected,
        "No settings",
        |i, width| {
            let editor = state.editor().filter(|_| i == selected);
            row_line(&rows[i], editor, read_only.is_some(), width)
        },
    );
    let lines = rows
        .get(selected)
        .map(|row| detail_lines(row, read_only.as_deref()))
        .unwrap_or_default();
    details_pane(frame, details, lines, "Nothing selected");
}

/// `{name:<22} {value}`, the value dimmed when it is the default, and the
/// editor's text with a block cursor while it is edited.
fn row_line(
    row: &SettingRow,
    editor: Option<&Editor>,
    read_only: bool,
    width: usize,
) -> Line<'static> {
    let name = format!("{} ", fit(row.key.key_name(), NAME_WIDTH - 1));
    let name = format!("{name:<NAME_WIDTH$}");
    let room = width.saturating_sub(NAME_WIDTH).max(1);
    let mut spans = vec![Span::styled(
        name,
        Style::default().fg(if read_only {
            Color::DarkGray
        } else {
            Color::Gray
        }),
    )];
    if let Some(editor) = editor {
        spans.extend(editing_spans(editor, room));
        return Line::from(spans);
    }
    let (text, style) = if row.value.is_empty() {
        let default = row.default.as_deref().unwrap_or("not set");
        (format!("({default})"), Style::default().fg(Color::DarkGray))
    } else {
        let style = if read_only {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        (row.value.clone(), style)
    };
    spans.push(Span::styled(fit(&text, room), style));
    Line::from(spans)
}

/// The edited text, scrolled so the cursor stays in `room` columns, with the
/// character under the cursor (or a space at the end) reversed.
fn editing_spans(editor: &Editor, room: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = editor.buffer.chars().collect();
    let start = editor.cursor.saturating_sub(room.saturating_sub(1));
    let before: String = chars[start..editor.cursor].iter().collect();
    let under = chars.get(editor.cursor).copied().unwrap_or(' ');
    let after: String = chars
        .iter()
        .skip(editor.cursor + 1)
        .take(room.saturating_sub(before.chars().count() + 1))
        .collect();
    let text = Style::default().fg(Color::Cyan);
    vec![
        Span::styled(before, text),
        Span::styled(under.to_string(), text.add_modifier(Modifier::REVERSED)),
        Span::styled(after, text),
    ]
}

fn detail_lines(row: &SettingRow, read_only: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = vec![heading(row.key.name()), Line::default()];
    for paragraph in &row.help {
        lines.push(Line::from(paragraph.clone()));
    }
    if row.value.is_empty() {
        if let Some(default) = &row.default {
            lines.push(Line::default());
            lines.push(field("Unset, so", default.clone()));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Enter edits, Enter again saves, Esc cancels",
        Style::default().fg(Color::Gray),
    )));
    if let Some(reason) = read_only {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            reason.to_string(),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
}
