//! Console progress for the install queue: one line per job, and part bars
//! for the package downloading.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use xearthlayer::config::format_size;
use xearthlayer::manager::{
    DownloadProgress, JobEvent, JobId, JobOutcome, JobSummary, PartState, QueueObserver,
};

/// Renders queue events to the terminal.
#[derive(Default)]
pub struct ConsoleQueueObserver {
    multi: MultiProgress,
    lines: Mutex<HashMap<JobId, JobLine>>,
    download: Mutex<Option<DownloadBlock>>,
}

/// One job's line, and which one-off changes it has had.
struct JobLine {
    label: String,
    bar: ProgressBar,
    /// The download status was shown. Applied once, not per snapshot.
    downloading: bool,
    /// The line's current look, so a style is set only when it changes:
    /// re-arming the spinner joins the old ticker thread, which must not
    /// happen under the queue's lock.
    look: Look,
}

/// The header, part bars and total of the package downloading now.
struct DownloadBlock {
    id: JobId,
    header: ProgressBar,
    parts: Vec<ProgressBar>,
    footer: ProgressBar,
}

impl DownloadBlock {
    fn clear(self) {
        for bar in std::iter::once(self.header)
            .chain(self.parts)
            .chain(std::iter::once(self.footer))
        {
            bar.finish_and_clear();
        }
    }
}

impl ConsoleQueueObserver {
    /// Clear any download block. Call once the queue is done; finished job
    /// lines stay on screen.
    pub fn finish(&self) {
        if let Some(block) = self.download.lock().unwrap().take() {
            block.clear();
        }
    }

    /// Clear the download block if it belongs to `id`.
    fn clear_block_of(&self, id: JobId) {
        let mut download = self.download.lock().unwrap();
        if download.as_ref().is_some_and(|block| block.id == id) {
            download.take().expect("checked above").clear();
        }
    }

    /// Show `progress` in `id`'s download block, building the block on the
    /// first snapshot. The first snapshot may arrive at any point after
    /// `Preparing`, so the block is built from whatever it carries.
    fn show_download(&self, id: JobId, label: &str, progress: &DownloadProgress) {
        let mut download = self.download.lock().unwrap();
        if download.as_ref().map(|block| block.id) != Some(id) {
            if let Some(other) = download.take() {
                other.clear();
            }
            *download = Some(self.new_block(id, label, progress));
        }
        let block = download.as_ref().expect("built above");
        update_parts(&block.parts, &block.footer, progress);
    }

    fn new_block(&self, id: JobId, label: &str, progress: &DownloadProgress) -> DownloadBlock {
        let header = self.multi.add(ProgressBar::new(0));
        header.set_style(ProgressStyle::with_template("{msg:.bold}").unwrap());
        header.set_message(format!("Downloading {label}"));

        let queued_style = ProgressStyle::with_template("  {prefix:.dim} {msg:.dim}").unwrap();
        let parts = progress
            .parts
            .iter()
            .map(|part| {
                let bar = self.multi.add(ProgressBar::new(0));
                bar.set_style(queued_style.clone());
                bar.set_prefix(part.filename.clone());
                bar.set_message("(queued)");
                bar
            })
            .collect();

        let footer = self.multi.add(ProgressBar::new(0));
        footer.set_style(
            ProgressStyle::with_template(
                "  Total [{bar:25.green/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
            )
            .unwrap()
            .progress_chars("##-"),
        );

        DownloadBlock {
            id,
            header,
            parts,
            footer,
        }
    }
}

impl QueueObserver for ConsoleQueueObserver {
    fn on_event(&self, id: JobId, event: &JobEvent<'_>) {
        if crate::signals::terminal_gone() {
            self.multi
                .set_draw_target(indicatif::ProgressDrawTarget::hidden());
            return;
        }
        let label = {
            let mut lines = self.lines.lock().unwrap();
            if let JobEvent::Queued(summary) = event {
                let bar = self.multi.add(ProgressBar::new(0));
                bar.set_style(Look::Plain.style());
                let line = JobLine {
                    label: label(summary),
                    bar,
                    downloading: false,
                    look: Look::Plain,
                };
                lines.insert(id, line);
            }
            let Some(line) = lines.get_mut(&id) else {
                return;
            };
            // A download's status goes up on its first snapshot, not every 100 ms.
            let repeated_snapshot = match event {
                JobEvent::Downloading(_) => std::mem::replace(&mut line.downloading, true),
                _ => false,
            };
            let look = look_after(event);
            if look != line.look {
                // A finished line drops the spinner, so it starts at column 0.
                line.look = look;
                line.bar.set_style(look.style());
                if look == Look::Spinner {
                    line.bar.enable_steady_tick(Duration::from_millis(100));
                }
            }
            if let Some(text) = status(&line.label, event).filter(|_| !repeated_snapshot) {
                line.bar.set_message(text);
            }
            if let JobEvent::Finished(outcome) = event {
                line.bar
                    .finish_with_message(finished_line(&line.label, outcome));
            }
            line.label.clone()
        };

        match event {
            JobEvent::Downloading(progress) => self.show_download(id, &label, progress),
            JobEvent::Downloaded | JobEvent::Finished(_) => self.clear_block_of(id),
            _ => {}
        }
    }
}

/// How a job line is drawn. The colour lives in the template, so status
/// text stays plain and is dropped with colour when output is not a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Plain,
    /// Downloaded, waiting to install: light blue.
    Downloaded,
    Spinner,
    /// Installed or removed: green, check mark included.
    Done,
}

impl Look {
    fn template(self) -> &'static str {
        match self {
            Look::Plain => "{msg}",
            Look::Downloaded => "{msg:.blue.bright}",
            Look::Spinner => "{spinner:.cyan} {msg}",
            Look::Done => "{msg:.green}",
        }
    }

    fn style(self) -> ProgressStyle {
        ProgressStyle::with_template(self.template()).unwrap()
    }
}

/// The look a job line takes on after `event`.
fn look_after(event: &JobEvent<'_>) -> Look {
    match event {
        JobEvent::Downloaded => Look::Downloaded,
        JobEvent::Installing(_) | JobEvent::Removing => Look::Spinner,
        JobEvent::Finished(JobOutcome::Installed(_) | JobOutcome::Removed) => Look::Done,
        _ => Look::Plain,
    }
}

/// "NA-CANADA ortho v2.1.0", or without a version for a remove.
pub(crate) fn label(summary: &JobSummary) -> String {
    match &summary.version {
        Some(version) => format!("{} {} v{version}", summary.region, summary.package_type),
        None => format!("{} {}", summary.region, summary.package_type),
    }
}

/// The status text a job line shows after `event`, or None when the line
/// is finished. A download's status is shown on its first snapshot only.
fn status(label: &str, event: &JobEvent<'_>) -> Option<String> {
    match event {
        JobEvent::Queued(_) => Some(format!("  {label}: queued")),
        JobEvent::Preparing => Some(format!("  {label}: sizing parts")),
        JobEvent::WaitingForSpace { needed, available } => Some(format!(
            "  {label}: waiting for disk space (needs {}, {} free)",
            format_size(*needed as usize),
            format_size(*available as usize)
        )),
        // The arrow sits in the spinner's column, like the check mark later.
        JobEvent::Downloading(_) => Some(format!("↓ {label}: downloading")),
        JobEvent::Downloaded => Some(format!("  {label}: downloaded, waiting to install")),
        JobEvent::Installing(stage) => Some(format!("{label}: {}", stage.name().to_lowercase())),
        JobEvent::Removing => Some(format!("{label}: removing")),
        JobEvent::Finished(_) => None,
    }
}

/// The line a finished job leaves behind.
fn finished_line(label: &str, outcome: &JobOutcome) -> String {
    match outcome {
        JobOutcome::Installed(_) => format!("✓ {label} installed"),
        JobOutcome::Removed => format!("✓ {label} removed"),
        JobOutcome::Cancelled => format!("- {label} cancelled"),
        // The handler prints the error itself; repeating it here would show it twice.
        JobOutcome::Failed(_) => format!("✗ {label} failed"),
    }
}

/// Drive one package's part bars and its total from a progress snapshot.
fn update_parts(bars: &[ProgressBar], footer: &ProgressBar, progress: &DownloadProgress) {
    let part_style = ProgressStyle::with_template(
        "  {prefix:.dim} [{bar:25.cyan/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
    )
    .unwrap()
    .progress_chars("##-");
    let fail_style = ProgressStyle::with_template("  {prefix:.red} {msg:.red}").unwrap();
    let retry_style = ProgressStyle::with_template("  {prefix:.yellow} {msg:.yellow}").unwrap();

    for part in &progress.parts {
        let Some(bar) = bars.get(part.index) else {
            continue;
        };
        // Skip already-finished bars to avoid duplicate rendering
        if bar.is_finished() {
            continue;
        }
        match &part.state {
            PartState::Queued => {}
            PartState::Downloading => {
                if let Some(total) = part.total_bytes {
                    bar.set_length(total);
                }
                bar.set_style(part_style.clone());
                bar.set_position(part.bytes_downloaded);
            }
            PartState::Done => {
                // Clear completed bars so the list scrolls naturally
                bar.finish_and_clear();
            }
            PartState::Failed { reason, .. } => {
                bar.set_style(fail_style.clone());
                bar.abandon_with_message(format!("(failed: {})", reason));
            }
            PartState::Retrying { attempt } => {
                bar.set_style(retry_style.clone());
                bar.set_message(format!("(retry {}/3)", attempt));
            }
        }
    }
    if let Some(total) = progress.total_bytes {
        footer.set_length(total);
    }
    footer.set_position(progress.total_bytes_downloaded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use xearthlayer::manager::{InstallResult, InstallStage, JobKind, ManagerError};
    use xearthlayer_package::PackageType;

    fn summary(version: Option<semver::Version>) -> JobSummary {
        JobSummary {
            kind: if version.is_some() {
                JobKind::Install
            } else {
                JobKind::Remove
            },
            region: "NA-CANADA".to_string(),
            package_type: PackageType::Ortho,
            version,
        }
    }

    const LABEL: &str = "NA-CANADA ortho v2.1.0";

    #[test]
    fn an_install_is_labelled_with_its_version() {
        assert_eq!(
            label(&summary(Some(semver::Version::new(2, 1, 0)))),
            "NA-CANADA ortho v2.1.0"
        );
    }

    #[test]
    fn a_remove_is_labelled_without_a_version() {
        let mut overlay = summary(None);
        overlay.package_type = PackageType::Overlay;
        assert_eq!(label(&overlay), "NA-CANADA overlay");
    }

    #[test]
    fn each_event_before_the_install_shows_its_status_indented() {
        let queued = summary(Some(semver::Version::new(2, 1, 0)));
        let cases = [
            (
                JobEvent::Queued(&queued),
                "  NA-CANADA ortho v2.1.0: queued",
            ),
            (
                JobEvent::Preparing,
                "  NA-CANADA ortho v2.1.0: sizing parts",
            ),
            (
                JobEvent::Downloaded,
                "  NA-CANADA ortho v2.1.0: downloaded, waiting to install",
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(
                status(LABEL, &event).as_deref(),
                Some(expected),
                "{event:?}"
            );
        }
    }

    #[test]
    fn waiting_for_space_shows_both_sizes() {
        let event = JobEvent::WaitingForSpace {
            needed: 3 * 1024 * 1024 * 1024,
            available: 2 * 1024 * 1024 * 1024,
        };
        assert_eq!(
            status(LABEL, &event).as_deref(),
            Some(&*format!(
                "  {LABEL}: waiting for disk space (needs {}, {} free)",
                format_size(3 * 1024 * 1024 * 1024),
                format_size(2 * 1024 * 1024 * 1024)
            ))
        );
    }

    #[test]
    fn each_install_stage_shows_in_lower_case() {
        for (stage, name) in [
            (InstallStage::Extracting, "extracting"),
            (InstallStage::Installing, "installing"),
        ] {
            assert_eq!(
                status(LABEL, &JobEvent::Installing(stage)),
                Some(format!("{LABEL}: {name}"))
            );
        }
    }

    #[test]
    fn a_remove_shows_removing() {
        assert_eq!(
            status("NA-CANADA ortho", &JobEvent::Removing).as_deref(),
            Some("NA-CANADA ortho: removing")
        );
    }

    #[test]
    fn a_download_shows_downloading_and_finishing_leaves_the_status_alone() {
        let progress = DownloadProgress {
            parts: Vec::new(),
            total_bytes_downloaded: 0,
            total_bytes: None,
        };
        assert_eq!(
            status(LABEL, &JobEvent::Downloading(&progress)).as_deref(),
            Some("↓ NA-CANADA ortho v2.1.0: downloading")
        );
        assert_eq!(
            status(LABEL, &JobEvent::Finished(&JobOutcome::Removed)),
            None
        );
    }

    fn installed() -> JobOutcome {
        JobOutcome::Installed(InstallResult {
            region: "NA-CANADA".to_string(),
            package_type: PackageType::Ortho,
            version: semver::Version::new(2, 1, 0),
            install_path: PathBuf::from("/scenery/zzXEL_na-canada_ortho"),
            bytes_downloaded: 1,
            files_extracted: 1,
        })
    }

    #[test]
    fn a_downloaded_package_waiting_to_install_turns_light_blue() {
        assert_eq!(look_after(&JobEvent::Downloaded), Look::Downloaded);
        assert!(Look::Downloaded.template().contains("{msg:.blue.bright}"));
    }

    #[test]
    fn installing_and_removing_spin() {
        for event in [
            JobEvent::Installing(InstallStage::Extracting),
            JobEvent::Removing,
        ] {
            assert_eq!(look_after(&event), Look::Spinner, "{event:?}");
        }
    }

    #[test]
    fn an_installed_or_removed_package_turns_green() {
        for outcome in [installed(), JobOutcome::Removed] {
            assert_eq!(
                look_after(&JobEvent::Finished(&outcome)),
                Look::Done,
                "{outcome:?}"
            );
        }
        assert!(Look::Done.template().contains("{msg:.green}"));
    }

    #[test]
    fn a_cancelled_or_failed_package_stays_plain() {
        for outcome in [
            JobOutcome::Cancelled,
            JobOutcome::Failed(ManagerError::Cancelled),
        ] {
            assert_eq!(
                look_after(&JobEvent::Finished(&outcome)),
                Look::Plain,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn earlier_events_keep_the_plain_look() {
        let queued = summary(Some(semver::Version::new(2, 1, 0)));
        let progress = DownloadProgress {
            parts: Vec::new(),
            total_bytes_downloaded: 0,
            total_bytes: None,
        };
        for event in [
            JobEvent::Queued(&queued),
            JobEvent::Preparing,
            JobEvent::WaitingForSpace {
                needed: 1,
                available: 0,
            },
            JobEvent::Downloading(&progress),
        ] {
            assert_eq!(look_after(&event), Look::Plain, "{event:?}");
        }
    }

    #[test]
    fn every_outcome_leaves_its_own_line() {
        let cases = [
            (installed(), "✓ NA-CANADA ortho v2.1.0 installed"),
            (JobOutcome::Removed, "✓ NA-CANADA ortho v2.1.0 removed"),
            (JobOutcome::Cancelled, "- NA-CANADA ortho v2.1.0 cancelled"),
            (
                JobOutcome::Failed(ManagerError::Cancelled),
                "✗ NA-CANADA ortho v2.1.0 failed",
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(finished_line(LABEL, &outcome), expected);
        }
    }
}
