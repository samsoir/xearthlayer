//! The dialogs over the tabs: help, and the confirmations.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};
use xearthlayer::config::format_size;

use super::chrome::{centered_rect, clip, pane};
use super::detail::heading;
use crate::ui::packages::app::Modal;

/// Every key, by where it works.
const KEY_SECTIONS: [(&str, &[&str]); 5] = [
    (
        "Everywhere",
        &[
            "1 2 3 4, Tab, Shift+Tab   switch tabs",
            "Up Down j k PgUp PgDn Home End   move",
            "r refresh   ? help   q, Esc, Ctrl+C quit",
            "u, d and i act on the marked rows, else the selected row",
        ],
    ),
    ("Local", &["Space mark   a mark all   u update   d delete"]),
    (
        "Library",
        &["Space mark   a mark all   t type filter   i install"],
    ),
    ("Progress", &["c cancel   C cancel all   x clear finished"]),
    (
        "Settings",
        &["Enter edit, Enter save, Esc cancel   Space flips true/false"],
    ),
];

/// How the package manager works, in four lines.
const HOW_IT_WORKS: [&str; 4] = [
    "Installs and removals queue in the background: see Progress.",
    "Queuing never switches tabs. Finished jobs refresh the lists.",
    "While xearthlayer run is active, updates and deletes are refused.",
    "Overlays follow their ortho; the overlay folder rebuilds when idle.",
];

/// Widest a dialog grows.
const MAX_WIDTH: u16 = 76;

/// Draw whichever dialog is open, over the frame.
pub fn draw(frame: &mut Frame, modal: &Modal) {
    let area = frame.area();
    match modal {
        Modal::Help => help(frame, area),
        Modal::ConfirmQuit { running, .. } => {
            let question = match running {
                0 => "Quit the package manager?".to_string(),
                1 => "1 job running. Quit and cancel it?".to_string(),
                n => format!("{n} jobs running. Quit and cancel them?"),
            };
            confirm(frame, area, "Quit", question, Vec::new(), None, "y/q quit");
        }
        Modal::ConfirmDelete {
            targets,
            total_bytes,
        } => {
            let question = format!(
                "Delete {} ({})?",
                plural(targets.len(), "package"),
                format_size(*total_bytes as usize)
            );
            let items = targets
                .iter()
                .map(|(region, package_type)| format!("  {region} {package_type}"))
                .collect();
            confirm(frame, area, "Delete", question, items, None, "y confirm");
        }
        Modal::ConfirmInstall {
            entries, skipped, ..
        } => {
            let question = format!("Install {}?", plural(entries.len(), "package"));
            let items = entries
                .iter()
                .map(|entry| {
                    format!(
                        "  {} {} v{}",
                        entry.title, entry.package_type, entry.version
                    )
                })
                .collect();
            let note = (!skipped.is_empty())
                .then(|| format!("Left out, already up to date: {}", skipped.join(", ")));
            confirm(frame, area, "Install", question, items, note, "y confirm");
        }
        Modal::ConfirmCancelAll => confirm(
            frame,
            area,
            "Cancel all",
            "Cancel every unfinished job?".to_string(),
            Vec::new(),
            None,
            "y confirm",
        ),
    }
}

fn help(frame: &mut Frame, area: Rect) {
    let mut lines = Vec::new();
    for (section, keys) in KEY_SECTIONS {
        // A tab's keys fit on one line after its name, which keeps the
        // dialog inside an 80 by 24 terminal with four tabs.
        if let [only] = keys {
            if section != "Everywhere" {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{section:<10}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(only.to_string()),
                ]));
                continue;
            }
        }
        lines.push(heading(section));
        lines.extend(keys.iter().map(|keys| Line::from(format!("  {keys}"))));
    }
    lines.push(Line::default());
    lines.push(heading("How it works"));
    lines.extend(
        HOW_IT_WORKS
            .iter()
            .map(|line| Line::from(format!("  {line}"))),
    );
    lines.push(Line::default());
    lines.push(hint("? or Esc closes"));
    show(frame, area, "Help", lines);
}

/// A confirmation: the question, the items it concerns, an optional note,
/// then how to answer it.
///
/// Every line is cut to the box rather than wrapped, so the height is known:
/// the question and the answer keys always fit, and the items give up lines
/// to "... and N more" when there are too many for the terminal.
fn confirm(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    question: String,
    items: Vec<String>,
    note: Option<String>,
    yes: &str,
) {
    let width = dialog_width(area);
    let inner = (width as usize).saturating_sub(2);
    // Borders, the question, a blank line, a blank line and the answer keys.
    let mut fixed = 2 + 1 + 1 + 1;
    if !items.is_empty() {
        fixed += 1;
    }
    if note.is_some() {
        fixed += 2;
    }
    let room = (area.height as usize).saturating_sub(fixed);

    let mut lines = vec![Line::from(clip(&question, inner))];
    if !items.is_empty() {
        lines.push(Line::default());
        let shown = if items.len() <= room {
            items.len()
        } else {
            // One line goes to "... and N more".
            room.saturating_sub(1)
        };
        lines.extend(
            items[..shown]
                .iter()
                .map(|item| Line::from(clip(item, inner))),
        );
        if shown < items.len() {
            lines.push(Line::from(format!(
                "  ... and {} more",
                items.len() - shown
            )));
        }
    }
    if let Some(note) = note {
        lines.push(Line::default());
        lines.push(Line::from(clip(&note, inner)));
    }
    lines.push(Line::default());
    lines.push(hint(&clip(&format!("{yes}   n or Esc cancel"), inner)));
    show(frame, area, title, lines);
}

fn hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::Cyan),
    ))
}

/// "1 package", "3 packages"
fn plural(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// How wide a dialog is in `area`.
fn dialog_width(area: Rect) -> u16 {
    MAX_WIDTH.min(area.width.saturating_sub(4))
}

/// Clear a centred box and draw the lines in it, as tall as they need.
fn show(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let width = dialog_width(area);
    let height = (lines.len() as u16).saturating_add(2);
    let rect = centered_rect(area, width, height);
    let block = pane(title);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_counts() {
        assert_eq!(plural(1, "package"), "1 package");
        assert_eq!(plural(3, "package"), "3 packages");
    }
}
