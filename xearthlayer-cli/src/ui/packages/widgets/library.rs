//! The Library tab: what the package library offers.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    Frame,
};

use super::chrome::{centered_text, fit, list_pane};
use super::detail::{details_pane, field, heading};
use crate::commands::packages::actions::{LibraryRow, LibraryStatus};
use crate::ui::packages::app::{AppState, LibraryState, TypeFilter};

/// Columns around the region and the status: mark, type, version and spaces.
const FIXED_WIDTH: usize = 4 + 1 + 7 + 1 + 8 + 1;
/// The widest status, "update 2.1.0 → 2.2.0", and the room it needs.
const FULL_STATUS_WIDTH: usize = 20;
const COMPACT_STATUS_WIDTH: usize = 9;
/// The region is at least this wide before the status is shortened.
const MIN_REGION_WIDTH: usize = 12;
/// The version column and its space, left out of a very narrow pane.
const VERSION_WIDTH: usize = 9;

pub fn draw(frame: &mut Frame, list: Rect, details: Rect, state: &AppState) {
    match state.library() {
        LibraryState::Loaded(_) => {}
        LibraryState::Loading => {
            let whole = list.union(details);
            centered_text(
                frame,
                whole,
                "Loading the library...",
                Style::default().fg(Color::Gray),
            );
            return;
        }
        LibraryState::Failed(error) => {
            let whole = list.union(details);
            centered_text(
                frame,
                whole,
                &format!("Could not load the library: {error}\n\nPress r to retry"),
                Style::default().fg(Color::Red),
            );
            return;
        }
        LibraryState::NotConfigured => {
            let whole = list.union(details);
            centered_text(
                frame,
                whole,
                "No package library is configured.\n\nSet one with: xearthlayer config set packages.library_url <url>",
                Style::default().fg(Color::Yellow),
            );
            return;
        }
    }

    let rows = state.library_rows();
    list_pane(
        frame,
        list,
        format!("Library: {} ({})", filter_name(state.filter()), rows.len()),
        rows.len(),
        state.selected(),
        "No packages match the filter",
        |i, width| row_line(state, rows[i], width),
    );
    // The selection is not clamped while the library is away, so it may
    // point past the rows.
    let lines = rows
        .get(state.selected())
        .map(|row| detail_lines(state, row))
        .unwrap_or_default();
    details_pane(frame, details, lines, "Nothing selected");
}

fn filter_name(filter: TypeFilter) -> &'static str {
    match filter {
        TypeFilter::All => "all",
        TypeFilter::Ortho => "ortho",
        TypeFilter::Overlay => "overlay",
    }
}

/// `[x]`/`[ ]`, region, type, version, then the status.
fn row_line(state: &AppState, row: &LibraryRow, width: usize) -> Line<'static> {
    let mark = if state.is_library_marked(row) {
        "[x] "
    } else {
        "[ ] "
    };
    let full = width.saturating_sub(FIXED_WIDTH + FULL_STATUS_WIDTH) >= MIN_REGION_WIDTH;
    let status_width = if full {
        FULL_STATUS_WIDTH
    } else {
        COMPACT_STATUS_WIDTH
    };
    // Too narrow even for the short status: the version goes, the details
    // pane has it.
    let compact = width < FIXED_WIDTH + status_width + 8;
    let fixed = if compact {
        FIXED_WIDTH - VERSION_WIDTH
    } else {
        FIXED_WIDTH
    };
    let region_width = width.saturating_sub(fixed + status_width).clamp(4, 22);
    let version = if compact {
        String::new()
    } else {
        format!("{:<8} ", row.entry.version)
    };
    let (status, color) = status_text(&row.status, &row.entry.version, full);
    Line::from(vec![
        Span::raw(format!(
            "{mark}{} {:<7} {version}",
            fit(&row.entry.title, region_width),
            row.entry.package_type.to_string(),
        )),
        Span::styled(status, Style::default().fg(color)),
    ])
}

/// "installed", "update 2.1.0 → 2.2.0" (or "update ↑" when short), or blank.
fn status_text(status: &LibraryStatus, library: &semver::Version, full: bool) -> (String, Color) {
    match status {
        LibraryStatus::NotInstalled => (String::new(), Color::Reset),
        LibraryStatus::UpToDate => ("installed".to_string(), Color::Green),
        LibraryStatus::Update { installed } if full => {
            (format!("update {installed} → {library}"), Color::Yellow)
        }
        LibraryStatus::Update { .. } => ("update ↑".to_string(), Color::Yellow),
    }
}

fn detail_lines(state: &AppState, row: &LibraryRow) -> Vec<Line<'static>> {
    let installed = match &row.status {
        LibraryStatus::NotInstalled => "not installed".to_string(),
        LibraryStatus::UpToDate => row.entry.version.to_string(),
        LibraryStatus::Update { installed } => installed.to_string(),
    };
    let mut lines = vec![
        heading(row.entry.title.clone()),
        Line::default(),
        field("Type", row.entry.package_type.to_string()),
        field("Library version", row.entry.version.to_string()),
        field("Installed version", installed),
    ];
    if row.entry.package_type == xearthlayer_package::PackageType::Ortho {
        let follows = if state.overlay_follows(row) {
            "yes"
        } else {
            "no"
        };
        lines.push(field("Overlay follows", follows));
    }
    lines
}
