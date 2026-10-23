//! The queue's shared state, and the transitions every thread uses.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use xearthlayer_package::package_mountpoint;

use super::job::{CancelResult, Job, JobEvent, JobId, JobOutcome, JobSummary, QueueObserver};
use super::ledger::{Capacity, DiskLedger};
use super::lock::QueueLock;
use super::stages::Stages;
use crate::manager::cancel::CancelToken;
use crate::manager::error::ManagerError;

/// Where a job is. The phase decides what cancelling it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Queued,
    Preparing,
    WaitingForSpace,
    Downloading,
    Downloaded,
    Installing,
    Swapping,
    Removing,
    /// Cancelled while downloaded: the cancelling caller is deleting its
    /// temp tree and finishes the job once that is done.
    Cancelling,
}

pub(super) struct Entry {
    pub(super) job: Job,
    pub(super) summary: JobSummary,
    pub(super) token: CancelToken,
    pub(super) phase: Phase,
}

/// Work waiting for the installer.
pub(super) enum Ready<D> {
    Install(D),
    Remove,
}

/// A downloaded package `cancel` took out of the install queue. The
/// caller drops it once unlocked, which deletes its temp tree, and then
/// finishes the job if `finish_after` is set.
struct Taken<D> {
    id: JobId,
    /// Held only to be dropped.
    _payload: D,
    finish_after: bool,
}

pub(super) struct State<D> {
    /// Every unfinished job.
    pub(super) jobs: BTreeMap<JobId, Entry>,
    /// Installs waiting for the downloader, in submission order.
    pub(super) downloads: VecDeque<JobId>,
    /// Work waiting for the installer, in the order it became ready.
    pub(super) installs: VecDeque<(JobId, Ready<D>)>,
    pub(super) ledger: DiskLedger,
    /// Whether the ledger's capacity was read in this busy period.
    pub(super) capacity_known: bool,
    pub(super) finished: BTreeMap<JobId, JobOutcome>,
    pub(super) swapping: bool,
    pub(super) shutting_down: bool,
    /// Waiters stop waiting once no swap runs, and dropping the queue
    /// no longer joins its threads.
    pub(super) abandoned: bool,
}

impl<D> State<D> {
    pub(super) fn new() -> Self {
        Self {
            jobs: BTreeMap::new(),
            downloads: VecDeque::new(),
            installs: VecDeque::new(),
            ledger: DiskLedger::new(Capacity::Shared { free: 0 }),
            capacity_known: false,
            finished: BTreeMap::new(),
            swapping: false,
            shutting_down: false,
            abandoned: false,
        }
    }

    /// Whether a job for `summary`'s package is already unfinished. Jobs
    /// are compared by package folder, as the store keys packages, so a
    /// region code in another case is the same package.
    pub(super) fn holds(&self, summary: &JobSummary) -> bool {
        let folder = |s: &JobSummary| package_mountpoint(&s.region, s.package_type);
        let wanted = folder(summary);
        self.jobs.values().any(|e| folder(&e.summary) == wanted)
    }
}

pub(super) struct Shared<S: Stages> {
    pub(super) stages: S,
    observer: Arc<dyn QueueObserver>,
    state: Mutex<State<S::Downloaded>>,
    pub(super) changed: Condvar,
    pub(super) next_id: AtomicU64,
    /// Held until the last holder of this state goes: normally after both
    /// threads are joined, and when the queue is abandoned, after the
    /// detached threads finish, so no new queue sweeps under them.
    _lock: QueueLock,
}

impl<S: Stages> Shared<S> {
    pub(super) fn new(stages: S, observer: Arc<dyn QueueObserver>, lock: QueueLock) -> Self {
        Self {
            stages,
            observer,
            state: Mutex::new(State::new()),
            changed: Condvar::new(),
            next_id: AtomicU64::new(1),
            _lock: lock,
        }
    }

    /// The state lock. A panicking thread must not wedge the others, so a
    /// poisoned lock is taken over.
    pub(super) fn lock(&self) -> MutexGuard<'_, State<S::Downloaded>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn wait<'a>(
        &self,
        guard: MutexGuard<'a, State<S::Downloaded>>,
    ) -> MutexGuard<'a, State<S::Downloaded>> {
        self.changed.wait(guard).unwrap_or_else(|p| p.into_inner())
    }

    /// Report an event. Callers hold the lock, so one job's events can
    /// never be reordered between threads.
    /// An observer that panics is logged and otherwise ignored: unwinding
    /// here would leave the job unfinished with the lock poisoned.
    pub(super) fn emit(&self, id: JobId, event: JobEvent<'_>) {
        let call = panic::catch_unwind(AssertUnwindSafe(|| self.observer.on_event(id, &event)));
        if let Err(payload) = call {
            tracing::error!(
                job = id.0,
                "install queue observer panicked: {}",
                panic_message(payload.as_ref())
            );
        }
    }

    /// Cancel `id`. A job no thread holds (queued, or downloaded and
    /// waiting for the installer) is taken out of its queue and finished
    /// here; one a thread holds only has its token raised, and that thread
    /// finishes it. Only the thread that took a job finishes it. A
    /// downloaded job is finished only after its temp tree is deleted, so
    /// a waiter that returns can rely on it being gone.
    pub(super) fn cancel(&self, id: JobId) -> CancelResult {
        let mut state = self.lock();
        let (result, taken) = self.cancel_locked(&mut state, id, true);
        drop(state);
        self.clean_up(taken.into_iter().collect());
        result
    }

    /// Cancel every unfinished job in one critical section, so no job can
    /// reach its swap while the others are being cancelled. With `abandon`,
    /// also stop waiters from waiting for anything but a running swap, and
    /// finish downloaded jobs at once rather than after their cleanup.
    pub(super) fn cancel_every(&self, abandon: bool) {
        let mut state = self.lock();
        let ids: Vec<JobId> = state.jobs.keys().copied().collect();
        let taken: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.cancel_locked(&mut state, id, !abandon).1)
            .collect();
        if abandon {
            state.abandoned = true;
            self.changed.notify_all();
        }
        drop(state);
        if abandon {
            // Nothing waits for these deletes, so neither does the caller.
            drop_elsewhere(taken);
        } else {
            self.clean_up(taken);
        }
    }

    /// Drop what `cancel_locked` took, outside the lock, then finish the
    /// jobs it left `Cancelling`.
    fn clean_up(&self, taken: Vec<Taken<S::Downloaded>>) {
        let deferred: Vec<JobId> = taken
            .iter()
            .filter(|t| t.finish_after)
            .map(|t| t.id)
            .collect();
        drop(taken); // temp directories go outside the lock
        if deferred.is_empty() {
            return;
        }
        let mut state = self.lock();
        for id in deferred {
            self.finish(&mut state, id, JobOutcome::Cancelled);
        }
    }

    /// `cancel` under a held lock. Returns a downloaded package it took out
    /// of the install queue, for the caller to drop once it has unlocked.
    /// With `defer`, that job is left `Cancelling` for the caller to finish
    /// after the drop; without it, the job is finished here.
    fn cancel_locked(
        &self,
        state: &mut State<S::Downloaded>,
        id: JobId,
        defer: bool,
    ) -> (CancelResult, Option<Taken<S::Downloaded>>) {
        let Some(entry) = state.jobs.get(&id) else {
            return (CancelResult::Unknown, None);
        };
        match entry.phase {
            Phase::Swapping | Phase::Removing => (CancelResult::TooLate, None),
            // Already cancelled; its canceller finishes it.
            Phase::Cancelling => (CancelResult::Cancelled, None),
            Phase::Queued | Phase::Downloaded => {
                // No thread holds it: take it out of its queue here.
                state.downloads.retain(|&queued| queued != id);
                let ready = state
                    .installs
                    .iter()
                    .position(|(queued, _)| *queued == id)
                    .and_then(|at| state.installs.remove(at));
                let taken = match ready {
                    Some((_, Ready::Install(payload))) => Some(Taken {
                        id,
                        _payload: payload,
                        finish_after: defer,
                    }),
                    Some((_, Ready::Remove)) | None => None,
                };
                if taken.as_ref().is_some_and(|t| t.finish_after) {
                    if let Some(entry) = state.jobs.get_mut(&id) {
                        entry.phase = Phase::Cancelling;
                    }
                } else {
                    self.finish(state, id, JobOutcome::Cancelled);
                }
                (CancelResult::Cancelled, taken)
            }
            Phase::Preparing | Phase::WaitingForSpace | Phase::Downloading | Phase::Installing => {
                entry.token.cancel();
                self.changed.notify_all(); // wakes a downloader waiting for space
                (CancelResult::Cancelled, None)
            }
        }
    }

    /// Record `id`'s outcome and wake every waiter. Callers hold the lock.
    pub(super) fn finish(&self, state: &mut State<S::Downloaded>, id: JobId, outcome: JobOutcome) {
        let removed = state.jobs.remove(&id);
        debug_assert!(removed.is_some(), "job {} finished twice", id.0);
        match &outcome {
            JobOutcome::Installed(_) => state.ledger.installed(id),
            _ => state.ledger.release(id),
        }
        self.emit(id, JobEvent::Finished(&outcome));
        state.finished.insert(id, outcome);
        if state.jobs.is_empty() {
            state.capacity_known = false;
        }
        self.changed.notify_all();
    }

    pub(super) fn finish_now(&self, id: JobId, outcome: JobOutcome) {
        let mut state = self.lock();
        self.finish(&mut state, id, outcome);
    }
}

/// Drop `taken` on a thread of its own, or here if none can be made.
/// Abandoned jobs are already finished, so nothing is left to do after.
fn drop_elsewhere<D: Send + 'static>(taken: Vec<Taken<D>>) {
    if taken.is_empty() {
        return;
    }
    debug_assert!(taken.iter().all(|t| !t.finish_after));
    // A spawn that fails drops the closure, and with it `taken`, here.
    let _ = std::thread::Builder::new()
        .name("xel-queue-discard".into())
        .spawn(move || drop(taken));
}

/// A stage's error as a job outcome.
pub(super) fn outcome_of(error: ManagerError) -> JobOutcome {
    match error {
        ManagerError::Cancelled => JobOutcome::Cancelled,
        e => JobOutcome::Failed(e),
    }
}

/// The text of a panic payload, for logs and `ManagerError::Panicked`.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}
