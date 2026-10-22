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
    lines: Mutex<HashMap<JobId, (String, ProgressBar)>>,
    download: Mutex<Option<DownloadBlock>>,
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
        let label = {
            let mut lines = self.lines.lock().unwrap();
            if let JobEvent::Queued(summary) = event {
                let line = self.multi.add(ProgressBar::new(0));
                line.set_style(ProgressStyle::with_template("{msg}").unwrap());
                lines.insert(id, (label(summary), line));
            }
            let Some((label, line)) = lines.get(&id) else {
                return;
            };
            if matches!(event, JobEvent::Installing(_) | JobEvent::Removing) {
                line.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
                line.enable_steady_tick(Duration::from_millis(100));
            }
            if let Some(text) = status(label, event) {
                line.set_message(text);
            }
            if let JobEvent::Finished(outcome) = event {
                line.finish_with_message(finished_line(label, outcome));
            }
            label.clone()
        };

        match event {
            JobEvent::Downloading(progress) => self.show_download(id, &label, progress),
            JobEvent::Downloaded | JobEvent::Finished(_) => self.clear_block_of(id),
            _ => {}
        }
    }
}

/// "NA-CANADA ortho v2.1.0", or without a version for a remove.
fn label(summary: &JobSummary) -> String {
    match &summary.version {
        Some(version) => format!("{} {} v{version}", summary.region, summary.package_type),
        None => format!("{} {}", summary.region, summary.package_type),
    }
}

/// The status text a job line shows after `event`, or None when the line
/// does not change (download progress) or the line is finished.
fn status(label: &str, event: &JobEvent<'_>) -> Option<String> {
    match event {
        JobEvent::Queued(_) => Some(format!("  {label}: queued")),
        JobEvent::Preparing => Some(format!("  {label}: sizing parts")),
        JobEvent::WaitingForSpace { needed, available } => Some(format!(
            "  {label}: waiting for disk space (needs {}, {} free)",
            format_size(*needed as usize),
            format_size(*available as usize)
        )),
        JobEvent::Downloading(_) => None,
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
            (InstallStage::Reassembling, "reassembling"),
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
    fn download_progress_and_finishing_leave_the_status_alone() {
        let progress = DownloadProgress {
            parts: Vec::new(),
            total_bytes_downloaded: 0,
            total_bytes: None,
        };
        assert_eq!(status(LABEL, &JobEvent::Downloading(&progress)), None);
        assert_eq!(
            status(LABEL, &JobEvent::Finished(&JobOutcome::Removed)),
            None
        );
    }

    #[test]
    fn every_outcome_leaves_its_own_line() {
        let installed = JobOutcome::Installed(InstallResult {
            region: "NA-CANADA".to_string(),
            package_type: PackageType::Ortho,
            version: semver::Version::new(2, 1, 0),
            install_path: PathBuf::from("/scenery/zzXEL_na-canada_ortho"),
            bytes_downloaded: 1,
            files_extracted: 1,
        });
        let cases = [
            (installed, "✓ NA-CANADA ortho v2.1.0 installed"),
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
