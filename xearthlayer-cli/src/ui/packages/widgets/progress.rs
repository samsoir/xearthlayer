//! The Progress tab: one row per job, drawn as the console shows them.

use std::time::Instant;

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    Frame,
};
use xearthlayer::config::format_size;
use xearthlayer::manager::JobKind;

use super::chrome::{fit, list_pane};
use super::detail::{details_pane, field, heading};
use crate::ui::dashboard::state::SPINNER_FRAMES;
use crate::ui::packages::app::{AppState, JobLook, JobRow};

/// Columns of a download gauge.
const GAUGE_WIDTH: usize = 10;
/// Room for a gauge with its bytes, "██████░░░░ 1.2 GB / 3.4 GB".
const DOWNLOAD_TAIL_WIDTH: usize = GAUGE_WIDTH + 1 + 8 + 3 + 8;
/// A label is never cut below this to make room for a longer status.
const MIN_LABEL_WIDTH: usize = 10;
const SPINNER_FRAME_MILLIS: u128 = 100;

pub fn draw(frame: &mut Frame, list: Rect, details: Rect, state: &AppState, now: Instant) {
    let jobs = state.jobs();
    let longest = jobs.iter().map(|job| job.label.chars().count()).max();
    list_pane(
        frame,
        list,
        format!("Jobs ({})", jobs.len()),
        jobs.len(),
        state.selected(),
        "No jobs yet. Install packages from the Library tab.",
        |i, width| {
            let label_width = width
                .saturating_sub(3 + DOWNLOAD_TAIL_WIDTH)
                .max(MIN_LABEL_WIDTH)
                .min(longest.unwrap_or(0));
            row_line(&jobs[i], width, label_width, spinner(state.started(), now))
        },
    );
    let lines = jobs
        .get(state.selected())
        .map(detail_lines)
        .unwrap_or_default();
    details_pane(frame, details, lines, "No job selected");
}

/// The spinner frame at `now`, counted from when the state was made.
fn spinner(started: Instant, now: Instant) -> char {
    let frame = now.saturating_duration_since(started).as_millis() / SPINNER_FRAME_MILLIS;
    SPINNER_FRAMES[frame as usize % SPINNER_FRAMES.len()]
}

/// A gauge of `done` out of `total`: "██████░░░░".
fn gauge(done: u64, total: u64) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((done.min(total) as u128 * GAUGE_WIDTH as u128) / total as u128) as usize
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(GAUGE_WIDTH - filled))
}

fn size(bytes: u64) -> String {
    format_size(bytes as usize)
}

/// "waiting for disk space (needs 3 GB, 2 GB free)"
fn waiting_text(needed: u64, available: u64) -> String {
    format!(
        "waiting for disk space (needs {}, {} free)",
        size(needed),
        size(available)
    )
}

/// An install stage or "Removing", as the console shows it.
fn stage_text(stage: &str) -> String {
    stage.to_lowercase()
}

/// What a finished job did.
fn done_text(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Install => "installed",
        JobKind::Remove => "removed",
    }
}

/// One job's row: an icon, the label and the status.
fn row_line(job: &JobRow, width: usize, label_width: usize, spinner: char) -> Line<'static> {
    let plain = Style::default();
    let (icon, style, tail) = match &job.look {
        JobLook::Queued => ('·', plain.fg(Color::Gray), "queued".to_string()),
        JobLook::Preparing => ('·', plain.fg(Color::Gray), "preparing".to_string()),
        JobLook::Waiting { needed, available } => {
            let full = waiting_text(*needed, *available);
            // The numbers are in the details pane when the row is too narrow.
            let tail = if width >= 2 + MIN_LABEL_WIDTH + 1 + full.chars().count() {
                full
            } else {
                "waiting for disk space".to_string()
            };
            ('·', plain.fg(Color::Yellow), tail)
        }
        JobLook::Downloading { done, total } => {
            let tail = match total {
                Some(total) if width >= 2 + MIN_LABEL_WIDTH + 1 + DOWNLOAD_TAIL_WIDTH => {
                    format!(
                        "{} {} / {}",
                        gauge(*done, *total),
                        size(*done),
                        size(*total)
                    )
                }
                Some(total) => format!("{} / {}", size(*done), size(*total)),
                None => size(*done),
            };
            ('↓', plain.fg(Color::Cyan), tail)
        }
        JobLook::Downloaded => (
            '·',
            plain.fg(Color::LightBlue),
            "downloaded, waiting to install".to_string(),
        ),
        JobLook::Working(stage) => (spinner, plain.fg(Color::Cyan), stage_text(stage)),
        JobLook::Done => ('✓', plain.fg(Color::Green), done_text(job.kind).to_string()),
        JobLook::Failed(_) => ('✗', plain.fg(Color::Red), "failed".to_string()),
        JobLook::Cancelled => ('-', plain.fg(Color::DarkGray), "cancelled".to_string()),
    };
    // A long status takes room from the label rather than being cut.
    let label_width = label_width
        .min(width.saturating_sub(3 + tail.chars().count()))
        .max(MIN_LABEL_WIDTH.min(label_width));
    Line::from(Span::styled(
        format!("{icon} {} {tail}", fit(&job.label, label_width)),
        style,
    ))
}

fn state_text(job: &JobRow) -> String {
    match &job.look {
        JobLook::Queued => "queued".to_string(),
        JobLook::Preparing => "preparing".to_string(),
        JobLook::Waiting { .. } => "waiting for disk space".to_string(),
        JobLook::Downloading { .. } => "downloading".to_string(),
        JobLook::Downloaded => "downloaded, waiting to install".to_string(),
        JobLook::Working(stage) => stage_text(stage),
        JobLook::Done => done_text(job.kind).to_string(),
        JobLook::Failed(_) => "failed".to_string(),
        JobLook::Cancelled => "cancelled".to_string(),
    }
}

fn detail_lines(job: &JobRow) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(job.label.clone()),
        Line::default(),
        field("State", state_text(job)),
    ];
    match &job.look {
        JobLook::Waiting { needed, available } => {
            lines.push(field("Needs", size(*needed)));
            lines.push(field("Free", size(*available)));
        }
        JobLook::Downloading {
            done,
            total: Some(total),
        } => {
            let percent = if *total == 0 { 0 } else { done * 100 / total };
            lines.push(field(
                "Downloaded",
                format!("{} of {} ({percent}%)", size(*done), size(*total)),
            ));
        }
        JobLook::Downloading { done, total: None } => {
            lines.push(field("Downloaded", size(*done)));
        }
        JobLook::Failed(error) => {
            lines.push(Line::default());
            lines.push(field("Error", error.clone()));
        }
        _ => {}
    }
    lines
}
