//! The jobs a queue runs and the events it reports.

use xearthlayer_package::{PackageMetadata, PackageType};

use crate::manager::download::DownloadProgress;
use crate::manager::error::ManagerError;
use crate::manager::installer::{InstallResult, InstallStage};

/// Identifies one job for the life of its queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

/// Work submitted to the queue.
#[derive(Debug, Clone)]
pub enum Job {
    /// Download and install this package.
    Install(PackageMetadata),
    /// Remove an installed package.
    Remove {
        region: String,
        package_type: PackageType,
    },
}

impl Job {
    /// What this job is about, for reporting and duplicate detection.
    pub fn summary(&self) -> JobSummary {
        match self {
            Self::Install(m) => JobSummary {
                kind: JobKind::Install,
                region: m.title.clone(),
                package_type: m.package_type,
                version: Some(m.package_version.clone()),
            },
            Self::Remove {
                region,
                package_type,
            } => JobSummary {
                kind: JobKind::Remove,
                region: region.clone(),
                package_type: *package_type,
                version: None,
            },
        }
    }
}

/// Whether a job installs or removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Install,
    Remove,
}

/// The package a job concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    pub kind: JobKind,
    pub region: String,
    pub package_type: PackageType,
    /// The version being installed; `None` for a remove.
    pub version: Option<semver::Version>,
}

/// Something that happened to a job.
#[derive(Debug)]
pub enum JobEvent<'a> {
    /// The job was accepted.
    Queued(&'a JobSummary),
    /// The downloader picked the job up and is validating and sizing it.
    Preparing,
    /// The download waits until enough temp space is free.
    WaitingForSpace { needed: u64, available: u64 },
    /// A download progress snapshot.
    Downloading(&'a DownloadProgress),
    /// The download finished; the job waits for the installer.
    Downloaded,
    /// The installer reached `stage`. `Installing(InstallStage::Installing)`
    /// is the swap: it always runs to completion.
    Installing(InstallStage),
    /// The installer is removing the package.
    Removing,
    /// The job is over.
    Finished(&'a JobOutcome),
}

/// How a job ended.
#[derive(Debug)]
pub enum JobOutcome {
    Installed(InstallResult),
    Removed,
    Cancelled,
    Failed(ManagerError),
}

/// What asking to cancel a job achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelResult {
    /// It will finish as cancelled.
    Cancelled,
    /// It is swapping or removing, which always completes.
    TooLate,
    /// No unfinished job has this id.
    Unknown,
}

/// Receives every job event.
pub trait QueueObserver: Send + Sync {
    /// Called while the queue's lock is held, from the submitting thread,
    /// the downloader or the installer. It must not block and must never
    /// call back into the queue.
    fn on_event(&self, id: JobId, event: &JobEvent<'_>);
}
