//! A live queue of package installs and removes.
//!
//! Downloading is network bound and installing is disk bound, so the queue
//! runs them on two threads and lets downloads run ahead of installs for as
//! long as the disk has room. See `ledger` for that rule.

mod job;
mod ledger;
mod stages;
mod state;
mod worker;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use xearthlayer_package::PackageMetadata;

use crate::manager::cancel::CancelToken;
use crate::manager::error::{ManagerError, ManagerResult};
use crate::manager::installer::InstallResult;

pub use job::{Job, JobEvent, JobId, JobKind, JobOutcome, JobSummary, QueueObserver};
pub use stages::{InstallContext, Stages};

/// Downloads queued packages one after another while installing them in
/// order on a second thread. See the module documentation.
pub struct InstallQueue<S: Stages> {
    shared: Arc<state::Shared<S>>,
    threads: Vec<JoinHandle<()>>,
}

impl<S: Stages> InstallQueue<S> {
    pub fn start(stages: S, observer: Arc<dyn QueueObserver>) -> ManagerResult<Self> {
        let shared = Arc::new(state::Shared::new(stages, observer));
        let spawn = |name: &str, body: fn(Arc<state::Shared<S>>)| {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name(name.to_string())
                .spawn(move || body(shared))
                .expect("spawn an install queue thread")
        };
        let threads = vec![
            spawn("xel-queue-download", worker::run_downloader),
            spawn("xel-queue-install", worker::run_installer),
        ];
        Ok(Self { shared, threads })
    }

    /// Queue `job`. Refused if its package already has an unfinished job.
    pub fn submit(&self, job: Job) -> ManagerResult<JobId> {
        let summary = job.summary();
        let mut state = self.shared.lock();
        if state.shutting_down {
            return Err(ManagerError::QueueClosed);
        }
        if state.holds(&summary) {
            return Err(ManagerError::AlreadyQueued {
                region: summary.region,
                package_type: summary.package_type.to_string(),
            });
        }
        let id = JobId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        self.shared.emit(id, JobEvent::Queued(&summary));
        let kind = summary.kind;
        state.jobs.insert(
            id,
            state::Entry {
                job,
                summary,
                token: CancelToken::new(),
                phase: state::Phase::Queued,
            },
        );
        match kind {
            JobKind::Install => state.downloads.push_back(id),
            JobKind::Remove => state.installs.push_back((id, state::Ready::Remove)),
        }
        self.shared.changed.notify_all();
        Ok(id)
    }

    /// Block until every job in `ids` has finished, then claim their
    /// outcomes in the order given. An id that is unknown or already
    /// claimed yields `Cancelled`.
    pub fn wait_for(&self, ids: &[JobId]) -> Vec<JobOutcome> {
        let mut state = self.shared.lock();
        while ids.iter().any(|id| state.jobs.contains_key(id)) {
            state = self.shared.wait(state);
        }
        ids.iter()
            .map(|id| state.finished.remove(id).unwrap_or(JobOutcome::Cancelled))
            .collect()
    }

    /// Block until no job is unfinished.
    pub fn wait_idle(&self) {
        let mut state = self.shared.lock();
        while !state.jobs.is_empty() {
            state = self.shared.wait(state);
        }
    }

    /// Claim every outcome not yet claimed.
    pub fn take_finished(&self) -> BTreeMap<JobId, JobOutcome> {
        std::mem::take(&mut self.shared.lock().finished)
    }

    /// Install `packages` and return one result per package, in order.
    pub fn install_batch(
        &self,
        packages: Vec<PackageMetadata>,
    ) -> Vec<ManagerResult<InstallResult>> {
        let submitted: Vec<ManagerResult<JobId>> = packages
            .into_iter()
            .map(|m| self.submit(Job::Install(m)))
            .collect();
        let ids: Vec<JobId> = submitted
            .iter()
            .filter_map(|r| r.as_ref().ok().copied())
            .collect();
        let mut outcomes = self.wait_for(&ids).into_iter();
        submitted
            .into_iter()
            .map(|submitted| {
                submitted?;
                match outcomes.next().expect("one outcome per submitted job") {
                    JobOutcome::Installed(installed) => Ok(installed),
                    JobOutcome::Cancelled => Err(ManagerError::Cancelled),
                    JobOutcome::Failed(e) => Err(e),
                    JobOutcome::Removed => unreachable!("an install never ends as a remove"),
                }
            })
            .collect()
    }
}

impl<S: Stages> Drop for InstallQueue<S> {
    /// Cancel everything, let a running swap finish, and join both threads.
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.shutting_down = true;
            for entry in state.jobs.values() {
                entry.token.cancel();
            }
            let queued: Vec<JobId> = state.downloads.drain(..).collect();
            for id in queued {
                self.shared.finish(&mut state, id, JobOutcome::Cancelled);
            }
            self.shared.changed.notify_all();
        }
        for thread in self.threads.drain(..) {
            let name = thread
                .thread()
                .name()
                .unwrap_or("install queue")
                .to_string();
            if let Err(payload) = thread.join() {
                tracing::error!(
                    "{name} thread panicked: {}",
                    state::panic_message(payload.as_ref())
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
