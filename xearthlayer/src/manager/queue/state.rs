//! The queue's shared state, and the transitions every thread uses.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::job::{CancelResult, Job, JobEvent, JobId, JobOutcome, JobSummary, QueueObserver};
use super::ledger::DiskLedger;
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

/// Work `cancel` took out of the install queue, dropped once unlocked.
type Taken<D> = Option<(JobId, Ready<D>)>;

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
            ledger: DiskLedger::new(0),
            capacity_known: false,
            finished: BTreeMap::new(),
            swapping: false,
            shutting_down: false,
            abandoned: false,
        }
    }

    /// Whether a job for `summary`'s package is already unfinished.
    pub(super) fn holds(&self, summary: &JobSummary) -> bool {
        self.jobs.values().any(|e| {
            e.summary.region == summary.region && e.summary.package_type == summary.package_type
        })
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
    /// finishes it. Only the thread that popped a job finishes it.
    pub(super) fn cancel(&self, id: JobId) -> CancelResult {
        let mut state = self.lock();
        let (result, payload) = self.cancel_locked(&mut state, id);
        drop(state);
        drop(payload); // a downloaded package's temp directory goes outside the lock
        result
    }

    /// Cancel every unfinished job in one critical section, so no job can
    /// reach its swap while the others are being cancelled. With `abandon`,
    /// also stop waiters from waiting for anything but a running swap.
    pub(super) fn cancel_every(&self, abandon: bool) {
        let mut state = self.lock();
        let ids: Vec<JobId> = state.jobs.keys().copied().collect();
        let payloads: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.cancel_locked(&mut state, id).1)
            .collect();
        if abandon {
            state.abandoned = true;
            self.changed.notify_all();
        }
        drop(state);
        drop(payloads); // temp directories go outside the lock
    }

    /// `cancel` under a held lock. Returns the work it took out of the
    /// install queue, for the caller to drop once it has unlocked.
    fn cancel_locked(
        &self,
        state: &mut State<S::Downloaded>,
        id: JobId,
    ) -> (CancelResult, Taken<S::Downloaded>) {
        let Some(entry) = state.jobs.get(&id) else {
            return (CancelResult::Unknown, None);
        };
        match entry.phase {
            Phase::Swapping | Phase::Removing => (CancelResult::TooLate, None),
            Phase::Queued | Phase::Downloaded => {
                // No thread holds it: take it out of its queue here.
                state.downloads.retain(|&queued| queued != id);
                let payload = state
                    .installs
                    .iter()
                    .position(|(queued, _)| *queued == id)
                    .and_then(|at| state.installs.remove(at));
                self.finish(state, id, JobOutcome::Cancelled);
                (CancelResult::Cancelled, payload)
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
        state.ledger.release(id);
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
