//! The Local tab: installed packages.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    Frame,
};
use xearthlayer::config::format_size;

use super::chrome::{fit, list_pane};
use super::detail::{details_pane, field, heading};
use crate::ui::packages::app::{AppState, LocalRow};

/// Columns around the region: mark, type, version, size and the update mark.
const FIXED_WIDTH: usize = 4 + 1 + 7 + 1 + 8 + 1 + 9 + 2;
/// The version column and its space, left out of a narrow pane.
const VERSION_WIDTH: usize = 9;
/// Below this much room for the region, the version column goes.
const MIN_REGION_WIDTH: usize = 8;

pub fn draw(frame: &mut Frame, list: Rect, details: Rect, state: &AppState) {
    let rows = state.local_rows();
    list_pane(
        frame,
        list,
        format!("Installed ({})", rows.len()),
        rows.len(),
        state.selected(),
        "Nothing installed",
        |i, width| row_line(state, &rows[i], width),
    );
    let lines = rows
        .get(state.selected())
        .map(detail_lines)
        .unwrap_or_default();
    details_pane(frame, details, lines, "Nothing selected");
}

/// `{mark}{region:<22} {type:<7} {version:<8} {size:>9}{ ↑ if update}`, with
/// the region as wide as the pane leaves room for. A pane too narrow for a
/// readable region drops the version (the details pane has it) so the size
/// and the update mark always fit.
fn row_line(state: &AppState, row: &LocalRow, width: usize) -> Line<'static> {
    let mark = if state.is_local_marked(row) {
        "[x] "
    } else {
        "[ ] "
    };
    let compact = width < FIXED_WIDTH + MIN_REGION_WIDTH;
    let fixed = if compact {
        FIXED_WIDTH - VERSION_WIDTH
    } else {
        FIXED_WIDTH
    };
    let region_width = width.saturating_sub(fixed).clamp(4, 22);
    let version = if compact {
        String::new()
    } else {
        format!("{:<8} ", row.version)
    };
    let text = format!(
        "{mark}{} {:<7} {version}{:>9}",
        fit(&row.region, region_width),
        row.package_type.to_string(),
        format_size(row.size_bytes as usize),
    );
    let update = if row.update.is_some() { " ↑" } else { "" };
    Line::from(vec![
        Span::raw(text),
        Span::styled(update, Style::default().fg(Color::Yellow)),
    ])
}

fn detail_lines(row: &LocalRow) -> Vec<Line<'static>> {
    let version = match &row.update {
        Some(update) => format!("{} ({update} available)", row.version),
        None => row.version.to_string(),
    };
    vec![
        heading(row.region.clone()),
        Line::default(),
        field("Type", row.package_type.to_string()),
        field("Version", version),
        field("Size", format_size(row.size_bytes as usize)),
        field("Location", row.path.display().to_string()),
        field("Published", row.published_at.format("%Y-%m-%d").to_string()),
    ]
}
