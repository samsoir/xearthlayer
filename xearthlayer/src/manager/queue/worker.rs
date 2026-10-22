//! The downloader and installer threads.

use std::sync::Arc;

use xearthlayer_package::PackageMetadata;

use super::job::{Job, JobEvent, JobId, JobOutcome};
use super::ledger::Admission;
use super::stages::{InstallContext, Stages};
use super::state::{outcome_of, Phase, Ready, Shared};
use crate::manager::cancel::CancelToken;
use crate::manager::download::DownloadProgressCallback;
use crate::manager::error::ManagerError;
use crate::manager::installer::InstallStage;

pub(super) fn run_downloader<S: Stages>(shared: Arc<Shared<S>>) {
    while let Some((id, metadata, token)) = next_download(&shared) {
        download_one(&shared, id, &metadata, &token);
    }
}

fn next_download<S: Stages>(shared: &Shared<S>) -> Option<(JobId, PackageMetadata, CancelToken)> {
    let mut state = shared.lock();
    loop {
        if state.shutting_down {
            return None;
        }
        if let Some(id) = state.downloads.pop_front() {
            let entry = state
                .jobs
                .get_mut(&id)
                .expect("a queued download has an entry");
            entry.phase = Phase::Preparing;
            let Job::Install(metadata) = &entry.job else {
                unreachable!("only installs are queued for download")
            };
            let next = (id, metadata.clone(), entry.token.clone());
            shared.emit(id, JobEvent::Preparing);
            return Some(next);
        }
        state = shared.wait(state);
    }
}

fn download_one<S: Stages>(
    shared: &Arc<Shared<S>>,
    id: JobId,
    metadata: &PackageMetadata,
    token: &CancelToken,
) {
    let prepared = match shared.stages.prepare(metadata) {
        Ok(prepared) => prepared,
        Err(e) => return shared.finish_now(id, outcome_of(e)),
    };
    let size = shared.stages.download_size(&prepared);
    if !admit(shared, id, size, token) {
        return; // `prepared` drops here, outside the lock
    }
    match shared
        .stages
        .download(prepared, token, progress_callback(shared, id))
    {
        Ok(downloaded) => {
            let mut state = shared.lock();
            // Once shutting down, the installer may already have drained its
            // queue and left, so a download that finished anyway ends here.
            if state.shutting_down {
                shared.finish(&mut state, id, JobOutcome::Cancelled);
            } else if let Some(entry) = state.jobs.get_mut(&id) {
                entry.phase = Phase::Downloaded;
                shared.emit(id, JobEvent::Downloaded);
                state.installs.push_back((id, Ready::Install(downloaded)));
                shared.changed.notify_all();
                return;
            }
            drop(state);
            drop(downloaded); // its temp directory goes outside the lock
        }
        Err(e) => shared.finish_now(id, outcome_of(e)),
    }
}

/// Wait until the ledger admits `size` bytes. False when the job finished
/// instead: it can never fit, the free space could not be read, or it was
/// cancelled.
fn admit<S: Stages>(shared: &Shared<S>, id: JobId, size: u64, token: &CancelToken) -> bool {
    let mut state = shared.lock();
    if !state.capacity_known {
        match shared.stages.available_bytes() {
            Ok(free) => {
                state.ledger.set_capacity(free);
                state.capacity_known = true;
            }
            Err(e) => {
                shared.finish(&mut state, id, JobOutcome::Failed(e));
                return false;
            }
        }
    }
    loop {
        if token.is_cancelled() || state.shutting_down {
            shared.finish(&mut state, id, JobOutcome::Cancelled);
            return false;
        }
        match state.ledger.admit(id, size) {
            Admission::Admitted => {
                if let Some(entry) = state.jobs.get_mut(&id) {
                    entry.phase = Phase::Downloading;
                }
                return true;
            }
            Admission::Never { required } => {
                let available = state.ledger.capacity();
                let error = ManagerError::InsufficientDiskSpace {
                    path: shared.stages.temp_dir().to_path_buf(),
                    required_bytes: required,
                    available_bytes: available,
                };
                shared.finish(&mut state, id, JobOutcome::Failed(error));
                return false;
            }
            Admission::Wait { needed, available } => {
                let entry = state
                    .jobs
                    .get_mut(&id)
                    .expect("an admitting job has an entry");
                if entry.phase != Phase::WaitingForSpace {
                    entry.phase = Phase::WaitingForSpace;
                    shared.emit(id, JobEvent::WaitingForSpace { needed, available });
                }
                state = shared.wait(state);
            }
        }
    }
}

fn progress_callback<S: Stages>(shared: &Arc<Shared<S>>, id: JobId) -> DownloadProgressCallback {
    let shared = Arc::clone(shared);
    Box::new(move |progress| {
        let state = shared.lock();
        if state
            .jobs
            .get(&id)
            .is_some_and(|e| e.phase == Phase::Downloading)
        {
            shared.emit(id, JobEvent::Downloading(progress));
        }
    })
}

pub(super) fn run_installer<S: Stages>(shared: Arc<Shared<S>>) {
    while let Some((id, ready, token)) = next_install(&shared) {
        match ready {
            Ready::Install(downloaded) => install_one(&shared, id, downloaded, token),
            Ready::Remove => remove_one(&shared, id, token),
        }
    }
}

/// Take the next ready job and mark it as the installer's, all under one
/// lock: `cancel` only removes queued work it finds in `installs`.
fn next_install<S: Stages>(
    shared: &Shared<S>,
) -> Option<(JobId, Ready<S::Downloaded>, CancelToken)> {
    let mut state = shared.lock();
    loop {
        if let Some((id, ready)) = state.installs.pop_front() {
            let entry = state.jobs.get_mut(&id).expect("a ready job has an entry");
            entry.phase = match ready {
                Ready::Install(_) => Phase::Installing,
                Ready::Remove => Phase::Removing,
            };
            return Some((id, ready, entry.token.clone()));
        }
        if state.shutting_down {
            return None;
        }
        state = shared.wait(state);
    }
}

fn install_one<S: Stages>(
    shared: &Shared<S>,
    id: JobId,
    downloaded: S::Downloaded,
    token: CancelToken,
) {
    if token.is_cancelled() {
        shared.finish_now(id, JobOutcome::Cancelled);
        drop(downloaded); // its temp directory goes outside the lock
        return;
    }
    let context = Context { shared, id, token };
    let result = shared.stages.install(downloaded, &context);
    let mut state = shared.lock();
    state.swapping = false;
    let outcome = match result {
        Ok(installed) => JobOutcome::Installed(installed),
        Err(e) => outcome_of(e),
    };
    shared.finish(&mut state, id, outcome);
}

fn remove_one<S: Stages>(shared: &Shared<S>, id: JobId, token: CancelToken) {
    let (region, package_type) = {
        let mut state = shared.lock();
        if token.is_cancelled() {
            shared.finish(&mut state, id, JobOutcome::Cancelled);
            return;
        }
        shared.emit(id, JobEvent::Removing);
        let summary = &state.jobs[&id].summary;
        (summary.region.clone(), summary.package_type)
    };
    let result = shared.stages.remove(&region, package_type);
    let outcome = match result {
        Ok(()) => JobOutcome::Removed,
        Err(e) => outcome_of(e),
    };
    shared.finish_now(id, outcome);
}

/// What an install sees of its queue.
struct Context<'a, S: Stages> {
    shared: &'a Shared<S>,
    id: JobId,
    token: CancelToken,
}

impl<S: Stages> InstallContext for Context<'_, S> {
    fn stage(&self, stage: InstallStage) {
        let _state = self.shared.lock();
        self.shared.emit(self.id, JobEvent::Installing(stage));
    }

    fn cancel_token(&self) -> &CancelToken {
        &self.token
    }

    fn begin_swap(&self) -> bool {
        let mut state = self.shared.lock();
        if self.token.is_cancelled() {
            return false;
        }
        if let Some(entry) = state.jobs.get_mut(&self.id) {
            entry.phase = Phase::Swapping;
        }
        state.swapping = true;
        true
    }
}
