//! The dialogs over the tabs: help, and the confirmations.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};
use xearthlayer::config::format_size;
use xearthlayer_package::LibraryEntry;

use super::chrome::{centered_rect, pane};
use super::detail::heading;
use crate::ui::packages::app::{Modal, PackageKey};

/// Every key, by where it works.
const KEY_SECTIONS: [(&str, &[&str]); 4] = [
    (
        "Everywhere",
        &[
            "1 2 3, Tab, Shift+Tab   switch tabs",
            "Up Down j k PgUp PgDn Home End   move",
            "r refresh   ? help   q, Esc, Ctrl+C quit",
        ],
    ),
    ("Local", &["Space mark   a mark all   u update   d delete"]),
    (
        "Library",
        &["Space mark   a mark all   t type filter   i install"],
    ),
    ("Progress", &["c cancel   C cancel all   x clear finished"]),
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
            confirm(frame, area, "Quit", vec![Line::from(question)], "y/q quit");
        }
        Modal::ConfirmDelete {
            targets,
            total_bytes,
        } => delete(frame, area, targets, *total_bytes),
        Modal::ConfirmInstall { entries, skipped } => install(frame, area, entries, skipped),
        Modal::ConfirmCancelAll => confirm(
            frame,
            area,
            "Cancel all",
            vec![Line::from("Cancel every unfinished job?")],
            "y confirm",
        ),
    }
}

fn help(frame: &mut Frame, area: Rect) {
    let mut lines = Vec::new();
    for (section, keys) in KEY_SECTIONS {
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

fn delete(frame: &mut Frame, area: Rect, targets: &[PackageKey], total_bytes: u64) {
    let mut lines = vec![Line::from(format!(
        "Delete {} ({})?",
        plural(targets.len(), "package"),
        format_size(total_bytes as usize)
    ))];
    lines.push(Line::default());
    lines.extend(
        targets
            .iter()
            .map(|(region, package_type)| Line::from(format!("  {region} {package_type}"))),
    );
    confirm(frame, area, "Delete", lines, "y confirm");
}

fn install(frame: &mut Frame, area: Rect, entries: &[LibraryEntry], skipped: &[String]) {
    let mut lines = vec![Line::from(format!(
        "Install {}?",
        plural(entries.len(), "package")
    ))];
    lines.push(Line::default());
    lines.extend(entries.iter().map(|entry| {
        Line::from(format!(
            "  {} {} v{}",
            entry.title, entry.package_type, entry.version
        ))
    }));
    if !skipped.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(format!(
            "Left out, already up to date: {}",
            skipped.join(", ")
        )));
    }
    confirm(frame, area, "Install", lines, "y confirm");
}

/// A confirmation: the question, then how to answer it.
fn confirm(frame: &mut Frame, area: Rect, title: &str, mut lines: Vec<Line<'static>>, yes: &str) {
    lines.push(Line::default());
    lines.push(hint(&format!("{yes}   n or Esc cancel")));
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

/// Clear a centred box and draw the lines in it, as tall as they need.
fn show(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let width = MAX_WIDTH.min(area.width.saturating_sub(4));
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
