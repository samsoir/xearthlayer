//! Carries out the package manager's effects.
//!
//! [`Effects`] owns the session's install queue and a worker thread for the
//! slow reads: the installed packages, the library over HTTP, the `run`
//! check, package metadata and the overlay rebuild. The queue, the
//! [`ActiveQueue`], dropping the queue and the quick `run` check made before
//! each submission (with, while `run` serves, a read of what is installed)
//! stay on the runner's thread; the worker only reads and
//! answers with [`Msg`]s. [`ChannelObserver`] turns the
//! queue's events into `Msg`s on the same channel.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use xearthlayer::manager::{
    CancelResult, InstallQueue, InstalledPackage, Job, JobEvent, JobId, JobOutcome, ManagerError,
    ManagerResult, QueueObserver, Stages,
};
use xearthlayer_package::{LibraryEntry, PackageMetadata, PackageType};

use super::app::{Effect, JobMsg, LocalRow, Msg, PackageKey};
use crate::commands::packages::actions::{
    after_removal, check_removal, check_replacement, library_status, rebuild_overlays, LibraryRow,
    LibraryStatus,
};
use crate::commands::packages::traits::PackageManagerService;
use crate::signals::ActiveQueue;

// ============================================================================
// Queue events
// ============================================================================

/// Sends each queue event as a [`Msg`]. It runs under the queue's lock, so
/// it only sends on an unbounded channel; a closed channel is ignored.
pub struct ChannelObserver(Sender<Msg>);

impl ChannelObserver {
    pub fn new(tx: Sender<Msg>) -> Self {
        Self(tx)
    }
}

impl QueueObserver for ChannelObserver {
    fn on_event(&self, id: JobId, event: &JobEvent<'_>) {
        let _ = self.0.send(message(id, event));
    }
}

/// The owned message for a queue event.
fn message(id: JobId, event: &JobEvent<'_>) -> Msg {
    let job = match event {
        JobEvent::Queued(summary) => return Msg::Queued(id, (*summary).clone()),
        JobEvent::Preparing => JobMsg::Preparing,
        JobEvent::WaitingForSpace { needed, available } => JobMsg::Waiting {
            needed: *needed,
            available: *available,
        },
        JobEvent::Downloading(progress) => JobMsg::Downloading {
            done: progress.total_bytes_downloaded,
            total: progress.total_bytes,
        },
        JobEvent::Downloaded => JobMsg::Downloaded,
        JobEvent::Installing(stage) => JobMsg::Stage(stage.name()),
        JobEvent::Removing => JobMsg::Removing,
        JobEvent::Finished(outcome) => match outcome {
            JobOutcome::Installed(_) => JobMsg::Installed,
            JobOutcome::Removed => JobMsg::Removed,
            JobOutcome::Cancelled => JobMsg::Cancelled,
            JobOutcome::Failed(e) => JobMsg::Failed(e.to_string()),
        },
    };
    Msg::Job(id, job)
}

// ============================================================================
// Effects
// ============================================================================

/// What the reads and the overlay rebuild need from the configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    pub install_dir: PathBuf,
    /// `None` when no `packages.library_url` is configured.
    pub library_url: Option<String>,
    pub custom_scenery_path: Option<PathBuf>,
    pub disable_overlays: bool,
    pub xworld_pro: bool,
}

/// Carries out [`Effect`]s, answering with [`Msg`]s on `tx`.
///
/// `start_queue` starts a queue whose observer sends on the same channel.
pub struct Effects<S: Stages, F> {
    start_queue: F,
    queue: Option<InstallQueue<S>>,
    active: ActiveQueue,
    tx: Sender<Msg>,
    worker: Worker,
    /// Asked again whether `run` is serving when a submission is carried
    /// out: the state's answer may be stale.
    manager: Arc<dyn PackageManagerService>,
    /// The package each remove job removes, until the job finishes.
    removing: HashMap<JobId, PackageKey>,
    install_dir: PathBuf,
    custom_scenery_path: Option<PathBuf>,
    /// The queue was abandoned: on exit, wait only for a running swap.
    abandoned: bool,
}

impl<S, F> Effects<S, F>
where
    S: Stages,
    F: Fn() -> ManagerResult<InstallQueue<S>>,
{
    pub fn new(
        start_queue: F,
        manager: Arc<dyn PackageManagerService>,
        settings: Settings,
        active: ActiveQueue,
        tx: Sender<Msg>,
    ) -> Self {
        let install_dir = settings.install_dir.clone();
        let custom_scenery_path = settings.custom_scenery_path.clone();
        Self {
            start_queue,
            queue: None,
            active,
            worker: Worker::start(Arc::clone(&manager), settings, tx.clone()),
            manager,
            tx,
            removing: HashMap::new(),
            install_dir,
            custom_scenery_path,
            abandoned: false,
        }
    }

    /// Carry out `effect`. `Break` means the runner should leave.
    pub fn apply(&mut self, effect: Effect) -> ControlFlow<()> {
        match effect {
            Effect::LoadInstalled => self.worker.load(Load::Installed),
            Effect::LoadLibrary => self.worker.load(Load::Library),
            Effect::CheckRun => self.worker.load(Load::Run),
            Effect::Install(entries) => self.worker.send(Read::Metadata(entries)),
            Effect::RebuildOverlays => self.worker.rebuild_overlays(),
            Effect::Submit {
                packages,
                replacing,
            } => self.submit(packages, &replacing),
            Effect::Remove(targets) => self.remove(targets),
            Effect::CancelJob(id) => self.stop(Stop::Job(id)),
            Effect::CancelAll => self.stop(Stop::All),
            Effect::Abandon => {
                self.abandoned |= self.queue.is_some();
                self.stop(Stop::Abandon);
            }
            Effect::ReleaseQueue => self.release_queue(),
            Effect::Exit => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }

    /// Act on a message before the state sees it: a removed overlay loses
    /// its per-region symlink.
    pub fn observe(&mut self, msg: &Msg) {
        let Msg::Job(id, job) = msg else {
            return;
        };
        match job {
            JobMsg::Removed => {
                if let Some((region, package_type)) = self.removing.remove(id) {
                    after_removal(&region, package_type, self.custom_scenery_path.as_deref());
                }
            }
            JobMsg::Installed | JobMsg::Cancelled | JobMsg::Failed(_) => {
                self.removing.remove(id);
            }
            _ => {}
        }
    }

    /// An overlay rebuild is still to finish, so dropping these effects
    /// waits for it.
    pub fn rebuild_pending(&self) -> bool {
        self.worker.flags.rebuilds.load(Ordering::SeqCst) > 0
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// Cancel on a thread of its own: cancelling deletes the temp trees of
    /// downloaded jobs, and the runner must keep drawing meanwhile. Done
    /// here if no thread can be made. With no queue there is nothing to stop.
    fn stop(&self, stop: Stop) {
        let Some(queue) = &self.queue else {
            return;
        };
        let canceller = queue.canceller();
        let tx = self.tx.clone();
        let run = move || match stop {
            Stop::Job(id) => {
                if canceller.cancel(id) == CancelResult::TooLate {
                    let _ = tx.send(Msg::Status("too late: finishing".to_string()));
                }
            }
            Stop::All => canceller.cancel_all(),
            Stop::Abandon => canceller.abandon(),
        };
        let spawned = std::thread::Builder::new()
            .name("xel-packages-cancel".into())
            .spawn(run.clone());
        if spawned.is_err() {
            run();
        }
    }

    /// The running queue, started if there is none, or why it will not
    /// start.
    fn queue(&mut self) -> Result<&InstallQueue<S>, String> {
        if self.queue.is_none() {
            let queue = (self.start_queue)().map_err(|e| e.to_string())?;
            self.active.set(queue.canceller());
            self.queue = Some(queue);
        }
        Ok(self.queue.as_ref().expect("started above"))
    }

    /// The packages installed on disk.
    fn installed(&self) -> ManagerResult<Vec<PackageKey>> {
        let store = self.manager.create_store(&self.install_dir);
        let packages = self.manager.list_packages(&store)?;
        Ok(packages
            .iter()
            .map(|p| (p.region().to_string(), p.package_type()))
            .collect())
    }

    /// Submit the installs as one batch. Each job's `Queued` comes from the
    /// observer during `submit_batch`, so `Submitted` follows all of them.
    /// While `run` is serving, the packages in `replacing` and those
    /// installed on disk are left out, as the CLI leaves them out; with
    /// nothing left, the request is refused.
    fn submit(&mut self, mut metadata: Vec<PackageMetadata>, replacing: &[PackageKey]) {
        let mut refused = Vec::new();
        if let Err(run) = check_replacement(self.manager.as_ref()) {
            self.send(Msg::RunActive(Some(run.clone())));
            // The state's rows may be stale, so the disk is read again: only
            // while `run` is serving, when a stale row would matter.
            let installed = match self.installed() {
                Ok(installed) => installed,
                Err(e) => {
                    let why = format!("could not read the installed packages: {e}");
                    self.send(Msg::QueueRefused(why));
                    return;
                }
            };
            let left_out: Vec<PackageMetadata>;
            (left_out, metadata) = metadata.into_iter().partition(|m| {
                replacing
                    .iter()
                    .chain(&installed)
                    .any(|(region, package_type)| {
                        region.eq_ignore_ascii_case(&m.title) && *package_type == m.package_type
                    })
            });
            if !left_out.is_empty() {
                let names: Vec<String> = left_out
                    .iter()
                    .map(|m| package_name(&m.title, m.package_type))
                    .collect();
                let note = format!("{} left out. {}", names.join(", "), run.message());
                if metadata.is_empty() {
                    self.send(Msg::QueueRefused(note));
                    return;
                }
                refused.push(note);
            }
        }
        let names: Vec<String> = metadata
            .iter()
            .map(|m| package_name(&m.title, m.package_type))
            .collect();
        let queue = match self.queue() {
            Ok(queue) => queue,
            Err(why) => {
                // What was left out is still said.
                refused.push(why);
                self.send(Msg::QueueRefused(refused.join("; ")));
                return;
            }
        };
        let results = queue.submit_batch(metadata);
        refused.extend(
            names
                .into_iter()
                .zip(results)
                .filter_map(|(name, result)| result.err().map(|e| refusal(&name, &e))),
        );
        // A stop asked for before the batch was in the queue found nothing to stop.
        self.active.reapply();
        self.send(Msg::Submitted { refused });
    }

    /// Submit a remove for each target, answered as [`submit`](Self::submit) is.
    /// While `run` is serving, the whole request is refused, as the CLI
    /// refuses it.
    fn remove(&mut self, targets: Vec<PackageKey>) {
        if let Err(run) = check_removal(self.manager.as_ref()) {
            self.send(Msg::RunActive(Some(run.clone())));
            self.send(Msg::QueueRefused(run.message()));
            return;
        }
        let queue = match self.queue() {
            Ok(queue) => queue,
            Err(why) => return self.send(Msg::QueueRefused(why)),
        };
        let mut removing = Vec::new();
        let mut refused = Vec::new();
        for (region, package_type) in targets {
            let job = Job::Remove {
                region: region.clone(),
                package_type,
            };
            match queue.submit(job) {
                Ok(id) => removing.push((id, (region, package_type))),
                Err(e) => refused.push(refusal(&package_name(&region, package_type), &e)),
            }
        }
        self.removing.extend(removing);
        self.active.reapply();
        self.send(Msg::Submitted { refused });
    }

    /// Drop the idle queue and with it the lock. The canceller goes first:
    /// a live one keeps the lock held.
    fn release_queue(&mut self) {
        self.active.clear();
        // Idle, so this returns at once: its threads are joined, or, while
        // its sweep still runs, left to finish it.
        drop(self.queue.take());
        self.abandoned = false;
    }
}

impl<S: Stages, F> Drop for Effects<S, F> {
    /// An overlay rebuild still to do is finished first, while the queue
    /// still holds the lock. An abandoned queue is not joined when dropped,
    /// so a swap it is running is waited for here: the process must not end
    /// mid-rename. Nothing else is waited for.
    fn drop(&mut self) {
        self.worker.close();
        self.active.clear();
        if let Some(queue) = self.queue.take() {
            if self.abandoned {
                queue.wait_idle();
            }
        }
    }
}

/// What [`Effects::stop`] asks of the queue.
#[derive(Debug, Clone, Copy)]
enum Stop {
    Job(JobId),
    All,
    Abandon,
}

/// "NA-CANADA ortho"
fn package_name(region: &str, package_type: PackageType) -> String {
    format!("{region} {package_type}")
}

/// Why the queue refused one package, naming it.
fn refusal(name: &str, e: &ManagerError) -> String {
    match e {
        // Its message already names the package.
        ManagerError::AlreadyQueued { .. } => e.to_string(),
        _ => format!("{name}: {e}"),
    }
}

// ============================================================================
// The worker
// ============================================================================

/// A reload that a second request of its kind can share.
#[derive(Debug, Clone, Copy)]
enum Load {
    Installed = 0,
    Library = 1,
    Run = 2,
}

/// Work for the worker thread.
enum Read {
    Load(Load),
    Metadata(Vec<LibraryEntry>),
    RebuildOverlays,
}

#[derive(Default)]
struct WorkerFlags {
    /// The runner has gone: skip the reads still queued.
    closing: AtomicBool,
    /// A load of each kind is waiting to run, so another is not queued.
    pending: [AtomicBool; 3],
    /// Overlay rebuilds asked for and not yet done.
    rebuilds: AtomicUsize,
}

/// One thread doing the slow reads in order.
struct Worker {
    tx: Option<Sender<Read>>,
    thread: Option<JoinHandle<()>>,
    flags: Arc<WorkerFlags>,
    /// Where a read nobody will do is answered.
    out: Sender<Msg>,
}

impl Worker {
    fn start(
        manager: Arc<dyn PackageManagerService>,
        settings: Settings,
        out: Sender<Msg>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let flags = Arc::new(WorkerFlags::default());
        let worker_flags = Arc::clone(&flags);
        let worker_out = out.clone();
        // A failed spawn drops the receiver, so every read is answered
        // with an error by `send`.
        let thread = std::thread::Builder::new()
            .name("xel-packages-worker".into())
            .spawn(move || run_worker(rx, &worker_flags, manager.as_ref(), &settings, &worker_out))
            .map_err(|e| tracing::error!(error = %e, "Package manager: no worker thread"))
            .ok();
        Self {
            tx: Some(tx),
            thread,
            flags,
            out,
        }
    }

    /// A worker whose thread never started.
    #[cfg(test)]
    fn unstarted(out: Sender<Msg>) -> Self {
        let (tx, _) = mpsc::channel();
        Self {
            tx: Some(tx),
            thread: None,
            flags: Arc::new(WorkerFlags::default()),
            out,
        }
    }

    /// Queue `read`. With no thread to do it, answer it now with an error,
    /// so the state is never left waiting.
    fn send(&self, read: Read) {
        let Some(tx) = &self.tx else {
            return;
        };
        if let Err(mpsc::SendError(read)) = tx.send(read) {
            let failed = Failure::of(&read);
            match read {
                Read::Load(load) => {
                    self.flags.pending[load as usize].store(false, Ordering::SeqCst)
                }
                Read::RebuildOverlays => {
                    self.flags.rebuilds.fetch_sub(1, Ordering::SeqCst);
                }
                Read::Metadata(_) => {}
            }
            let _ = self
                .out
                .send(failed.msg("the package manager's worker is not running"));
        }
    }

    fn load(&self, load: Load) {
        if !self.flags.pending[load as usize].swap(true, Ordering::SeqCst) {
            self.send(Read::Load(load));
        }
    }

    fn rebuild_overlays(&self) {
        self.flags.rebuilds.fetch_add(1, Ordering::SeqCst);
        self.send(Read::RebuildOverlays);
    }
}

impl Worker {
    /// Take no more reads and skip those still queued. A rebuild still to
    /// do is waited for, so the overlay folder is never left half built;
    /// otherwise the thread is left to finish its read, which may wait on
    /// the network. Closing again does nothing.
    fn close(&mut self) {
        self.flags.closing.store(true, Ordering::SeqCst);
        drop(self.tx.take());
        if self.flags.rebuilds.load(Ordering::SeqCst) > 0 {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.close();
    }
}

/// The answer to a read that did not happen, built before the read runs.
enum Failure {
    Installed,
    Library,
    Run,
    /// The name of each package whose metadata was asked for.
    Metadata(Vec<String>),
    Overlays,
}

impl Failure {
    fn of(read: &Read) -> Self {
        match read {
            Read::Load(Load::Installed) => Self::Installed,
            Read::Load(Load::Library) => Self::Library,
            Read::Load(Load::Run) => Self::Run,
            Read::Metadata(entries) => Self::Metadata(
                entries
                    .iter()
                    .map(|e| package_name(&e.title, e.package_type))
                    .collect(),
            ),
            Read::RebuildOverlays => Self::Overlays,
        }
    }

    fn msg(self, why: &str) -> Msg {
        match self {
            Self::Installed => Msg::Installed(Err(why.to_string())),
            Self::Library => Msg::Library(Err(why.to_string())),
            Self::Run => Msg::Status(format!("could not check for xearthlayer run: {why}")),
            Self::Metadata(names) => Msg::MetadataReady(
                names
                    .into_iter()
                    .map(|name| Err(format!("{name}: {why}")))
                    .collect(),
            ),
            Self::Overlays => {
                Msg::OverlaysRebuilt(Some(format!("could not rebuild the overlays: {why}")))
            }
        }
    }
}

fn run_worker(
    rx: Receiver<Read>,
    flags: &WorkerFlags,
    manager: &dyn PackageManagerService,
    settings: &Settings,
    out: &Sender<Msg>,
) {
    let mut reads = Reads {
        manager,
        settings,
        out,
        installed: None,
        library: Vec::new(),
    };
    for read in rx {
        if flags.closing.load(Ordering::SeqCst) && !matches!(read, Read::RebuildOverlays) {
            continue;
        }
        if let Read::Load(load) = &read {
            // Cleared first, so a request made during this read runs again.
            flags.pending[*load as usize].store(false, Ordering::SeqCst);
        }
        let rebuild = matches!(read, Read::RebuildOverlays);
        let failure = Failure::of(&read);
        let done = panic::catch_unwind(AssertUnwindSafe(|| reads.run(read)));
        if rebuild {
            flags.rebuilds.fetch_sub(1, Ordering::SeqCst);
        }
        if let Err(payload) = done {
            let why = format!("panicked: {}", panic_text(payload.as_ref()));
            tracing::error!("Package manager: a read {why}");
            let _ = out.send(failure.msg(&why));
        }
    }
}

/// The worker's reads, and what it keeps of the last ones.
struct Reads<'a> {
    manager: &'a dyn PackageManagerService,
    settings: &'a Settings,
    out: &'a Sender<Msg>,
    /// The last installed list and library rows, so each answer can be
    /// joined with the other.
    installed: Option<Vec<InstalledPackage>>,
    library: Vec<LibraryRow>,
}

impl Reads<'_> {
    fn send(&self, msg: Msg) {
        let _ = self.out.send(msg);
    }

    fn run(&mut self, read: Read) {
        let manager = self.manager;
        let settings = self.settings;
        match read {
            Read::Load(Load::Installed) => {
                let store = manager.create_store(&settings.install_dir);
                match manager.list_packages(&store) {
                    Ok(packages) => {
                        self.send(Msg::Installed(Ok(local_rows(&packages, &self.library))));
                        self.installed = Some(packages);
                    }
                    Err(e) => self.send(Msg::Installed(Err(e.to_string()))),
                }
            }
            Read::Load(Load::Library) => {
                let Some(url) = &settings.library_url else {
                    self.send(Msg::LibraryNotConfigured);
                    return;
                };
                match read_library(manager, settings, url) {
                    Ok(rows) => {
                        self.library = rows.clone();
                        self.send(Msg::Library(Ok(rows)));
                        // The installed rows' updates come from the library.
                        if let Some(packages) = &self.installed {
                            self.send(Msg::Installed(Ok(local_rows(packages, &self.library))));
                        }
                    }
                    Err(e) => self.send(Msg::Library(Err(e.to_string()))),
                }
            }
            Read::Load(Load::Run) => self.send(Msg::RunActive(check_removal(manager).err())),
            Read::Metadata(entries) => {
                let results = entries
                    .iter()
                    .map(|entry| {
                        manager.fetch_metadata(&entry.metadata_url).map_err(|e| {
                            format!("{}: {e}", package_name(&entry.title, entry.package_type))
                        })
                    })
                    .collect();
                self.send(Msg::MetadataReady(results));
            }
            Read::RebuildOverlays => {
                let report = rebuild_overlays(
                    manager,
                    &settings.install_dir,
                    settings.custom_scenery_path.as_deref(),
                    settings.disable_overlays,
                    settings.xworld_pro,
                );
                // Answered even with nothing to say: the state shows the
                // rebuild until it is.
                let line = report.outcome.or_else(|| report.lines.first().cloned());
                self.send(Msg::OverlaysRebuilt(line));
            }
        }
    }
}

/// The text of a panic payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// The library and each package's status against what is installed.
fn read_library(
    manager: &dyn PackageManagerService,
    settings: &Settings,
    url: &str,
) -> ManagerResult<Vec<LibraryRow>> {
    let library = manager.fetch_library(url)?;
    let store = manager.create_store(&settings.install_dir);
    library_status(manager, &store, &library)
}

/// The installed packages as rows, each with the library's newer version
/// if it offers one.
fn local_rows(installed: &[InstalledPackage], library: &[LibraryRow]) -> Vec<LocalRow> {
    installed
        .iter()
        .map(|package| {
            let metadata = &package.metadata;
            let update = library
                .iter()
                .find(|row| {
                    row.entry.title.eq_ignore_ascii_case(&metadata.title)
                        && row.entry.package_type == metadata.package_type
                        && matches!(row.status, LibraryStatus::Update { .. })
                })
                .map(|row| row.entry.version.clone());
            LocalRow {
                region: metadata.title.clone(),
                package_type: metadata.package_type,
                version: metadata.package_version.clone(),
                size_bytes: package.size_bytes,
                path: package.path.clone(),
                published_at: metadata.published_at,
                update,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use semver::Version;
    use tempfile::TempDir;
    use xearthlayer::manager::{
        CancelToken, Capacity, DownloadProgress, DownloadProgressCallback, InstallContext,
        InstallQueue, InstallResult, InstallStage, JobEvent, JobId, JobKind, JobOutcome,
        JobSummary, ManagerError, ManagerResult, PackageStatus, QueueObserver, Stages,
    };
    use xearthlayer_package::{LibraryEntry, PackageMetadata, PackageType};

    use super::*;
    use crate::commands::packages::test_support::{install_dir, run_active, FakeManager};
    use crate::signals::ActiveQueue;
    use crate::ui::packages::app::{Action, AppState, Effect, Input, JobMsg, Msg};

    const WAIT: Duration = Duration::from_secs(10);

    // ------------------------------------------------------------------------
    // A small fake queue
    // ------------------------------------------------------------------------

    /// Holds an install inside its swap until opened.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }

        fn pass(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
        }
    }

    /// Opens its gate when dropped, so a failing assertion unwinds instead
    /// of leaving a queue thread held at the gate.
    struct OpenOnDrop(Arc<Gate>);

    impl Drop for OpenOnDrop {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// Every stage succeeds at once, except a swap held by `swap_gate` and
    /// the delete of one package's download held by `drop_gate`.
    #[derive(Clone)]
    struct FakeStages {
        temp: PathBuf,
        packages: PathBuf,
        swap_gate: Option<Arc<Gate>>,
        drop_gate: Option<(&'static str, Arc<Gate>)>,
    }

    impl FakeStages {
        fn in_dir(dir: &Path) -> Self {
            Self {
                temp: dir.join("tmp"),
                packages: dir.join("packages"),
                swap_gate: None,
                drop_gate: None,
            }
        }
    }

    /// A download. Dropping it stands for deleting its temp tree.
    struct Payload {
        title: String,
        gate: Option<Arc<Gate>>,
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.pass();
            }
        }
    }

    impl Stages for FakeStages {
        type Prepared = String;
        type Downloaded = Payload;

        fn temp_dir(&self) -> &Path {
            &self.temp
        }

        fn packages_root(&self) -> &Path {
            &self.packages
        }

        fn capacity(&self) -> ManagerResult<Capacity> {
            Ok(Capacity::Shared { free: u64::MAX / 4 })
        }

        fn prepare(&self, metadata: &PackageMetadata) -> ManagerResult<String> {
            Ok(metadata.title.clone())
        }

        fn download_size(&self, _prepared: &String) -> u64 {
            1
        }

        fn download(
            &self,
            title: String,
            _cancel: &CancelToken,
            progress: DownloadProgressCallback,
        ) -> ManagerResult<Payload> {
            progress(&DownloadProgress {
                parts: Vec::new(),
                total_bytes_downloaded: 1,
                total_bytes: Some(1),
            });
            let gate = match &self.drop_gate {
                Some((held, gate)) if *held == title => Some(Arc::clone(gate)),
                _ => None,
            };
            Ok(Payload { title, gate })
        }

        fn install(
            &self,
            payload: Payload,
            ctx: &dyn InstallContext,
        ) -> ManagerResult<InstallResult> {
            let title = payload.title.clone();
            ctx.stage(InstallStage::Extracting);
            if !ctx.begin_swap() {
                return Err(ManagerError::Cancelled);
            }
            ctx.stage(InstallStage::Installing);
            if let Some(gate) = &self.swap_gate {
                gate.pass();
            }
            ctx.end_swap();
            Ok(InstallResult {
                region: title,
                package_type: PackageType::Ortho,
                version: Version::new(1, 0, 0),
                install_path: PathBuf::new(),
                bytes_downloaded: 1,
                files_extracted: 0,
            })
        }

        fn remove(&self, _region: &str, _package_type: PackageType) -> ManagerResult<()> {
            Ok(())
        }
    }

    /// Ignores every event, for a queue that only holds the lock.
    struct Quiet;

    impl QueueObserver for Quiet {
        fn on_event(&self, _id: JobId, _event: &JobEvent<'_>) {}
    }

    fn settings() -> Settings {
        Settings {
            install_dir: install_dir(),
            library_url: Some("https://example.invalid/library".to_string()),
            custom_scenery_path: None,
            disable_overlays: false,
            xworld_pro: false,
        }
    }

    type StartQueue = Box<dyn Fn() -> ManagerResult<InstallQueue<FakeStages>> + Send>;
    type TestEffects = Effects<FakeStages, StartQueue>;

    /// Effects whose queue runs `stages`, with `manager` behind the reads.
    fn effects_with(
        stages: FakeStages,
        manager: FakeManager,
        settings: Settings,
    ) -> (TestEffects, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let observer_tx = tx.clone();
        let start: StartQueue = Box::new(move || {
            InstallQueue::start(
                stages.clone(),
                Arc::new(ChannelObserver::new(observer_tx.clone())),
            )
        });
        let effects = Effects::new(
            start,
            Arc::new(manager),
            settings,
            ActiveQueue::default(),
            tx,
        );
        (effects, rx)
    }

    fn effects_on(stages: FakeStages) -> (TestEffects, Receiver<Msg>) {
        effects_with(stages, FakeManager::default(), settings())
    }

    fn metadata(title: &str) -> PackageMetadata {
        PackageMetadata {
            spec_version: Version::new(1, 0, 0),
            title: title.to_string(),
            package_version: Version::new(1, 0, 0),
            published_at: chrono::Utc::now(),
            package_type: PackageType::Ortho,
            mountpoint: title.to_string(),
            filename: format!("{title}.tar.gz"),
            parts: Vec::new(),
        }
    }

    fn next(rx: &Receiver<Msg>) -> Msg {
        rx.recv_timeout(WAIT).expect("a message")
    }

    /// Messages until one matches `last`, that one included.
    fn until(rx: &Receiver<Msg>, last: impl Fn(&Msg) -> bool) -> Vec<Msg> {
        let mut seen = Vec::new();
        loop {
            let msg = next(rx);
            let done = last(&msg);
            seen.push(msg);
            if done {
                return seen;
            }
        }
    }

    fn finished(msg: &Msg) -> bool {
        matches!(
            msg,
            Msg::Job(
                _,
                JobMsg::Installed | JobMsg::Removed | JobMsg::Cancelled | JobMsg::Failed(_)
            )
        )
    }

    /// One line per message, for asserting on order. Jobs are numbered
    /// from 1 in the order they are first seen: every queue in the test
    /// process draws its ids from one counter, so their values are not
    /// known in advance.
    #[derive(Default)]
    struct Describe(Vec<JobId>);

    impl Describe {
        fn number(&mut self, id: JobId) -> usize {
            match self.0.iter().position(|seen| *seen == id) {
                Some(i) => i + 1,
                None => {
                    self.0.push(id);
                    self.0.len()
                }
            }
        }

        fn one(&mut self, msg: &Msg) -> String {
            match msg {
                Msg::Queued(id, summary) => {
                    format!("{} queued {}", self.number(*id), summary.region)
                }
                Msg::Submitted { refused } => format!("submitted {refused:?}"),
                Msg::Job(id, job) => format!("{} {job:?}", self.number(*id)),
                other => format!("{other:?}"),
            }
        }

        fn all(&mut self, msgs: &[Msg]) -> Vec<String> {
            msgs.iter().map(|msg| self.one(msg)).collect()
        }
    }

    /// The ids of the jobs queued, in order.
    fn queued_ids(seen: &[Msg]) -> Vec<JobId> {
        seen.iter()
            .filter_map(|msg| match msg {
                Msg::Queued(id, _) => Some(*id),
                _ => None,
            })
            .collect()
    }

    // ------------------------------------------------------------------------
    // ChannelObserver
    // ------------------------------------------------------------------------

    #[test]
    fn the_observer_turns_each_event_into_its_message() {
        let (tx, rx) = mpsc::channel();
        let observer = ChannelObserver::new(tx);
        let id = JobId(7);
        let summary = JobSummary {
            kind: JobKind::Install,
            region: "EU-WEST".to_string(),
            package_type: PackageType::Ortho,
            version: Some(Version::new(1, 0, 0)),
        };
        let progress = DownloadProgress {
            parts: Vec::new(),
            total_bytes_downloaded: 40,
            total_bytes: Some(100),
        };
        let installed = JobOutcome::Installed(InstallResult {
            region: "EU-WEST".to_string(),
            package_type: PackageType::Ortho,
            version: Version::new(1, 0, 0),
            install_path: PathBuf::new(),
            bytes_downloaded: 100,
            files_extracted: 1,
        });
        let failed = JobOutcome::Failed(ManagerError::QueueClosed);
        let events = [
            JobEvent::Queued(&summary),
            JobEvent::Preparing,
            JobEvent::WaitingForSpace {
                needed: 10,
                available: 5,
            },
            JobEvent::Downloading(&progress),
            JobEvent::Downloaded,
            JobEvent::Installing(InstallStage::Extracting),
            JobEvent::Installing(InstallStage::Installing),
            JobEvent::Removing,
            JobEvent::Finished(&installed),
            JobEvent::Finished(&JobOutcome::Removed),
            JobEvent::Finished(&JobOutcome::Cancelled),
            JobEvent::Finished(&failed),
        ];

        for event in &events {
            observer.on_event(id, event);
        }

        let got: Vec<Msg> = rx.try_iter().collect();
        assert_eq!(got.len(), events.len());
        assert!(matches!(&got[0], Msg::Queued(JobId(7), s) if *s == summary));
        let jobs: Vec<&JobMsg> = got[1..]
            .iter()
            .map(|msg| match msg {
                Msg::Job(JobId(7), job) => job,
                other => panic!("expected a job message, got {other:?}"),
            })
            .collect();
        assert!(matches!(jobs[0], JobMsg::Preparing));
        assert!(matches!(
            jobs[1],
            JobMsg::Waiting {
                needed: 10,
                available: 5
            }
        ));
        assert!(matches!(
            jobs[2],
            JobMsg::Downloading {
                done: 40,
                total: Some(100)
            }
        ));
        assert!(matches!(jobs[3], JobMsg::Downloaded));
        assert!(matches!(jobs[4], JobMsg::Stage("Extracting")));
        assert!(matches!(jobs[5], JobMsg::Stage("Installing")));
        assert!(matches!(jobs[6], JobMsg::Removing));
        assert!(matches!(jobs[7], JobMsg::Installed));
        assert!(matches!(jobs[8], JobMsg::Removed));
        assert!(matches!(jobs[9], JobMsg::Cancelled));
        assert!(
            matches!(jobs[10], JobMsg::Failed(e) if e.contains("shutting down")),
            "{:?}",
            jobs[10]
        );
    }

    #[test]
    fn the_observer_ignores_a_closed_channel() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let observer = ChannelObserver::new(tx);

        observer.on_event(JobId(1), &JobEvent::Preparing);
    }

    // ------------------------------------------------------------------------
    // The queue
    // ------------------------------------------------------------------------

    #[test]
    fn submit_starts_a_queue_and_reports_queued_first_then_progress_in_order() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_on(FakeStages::in_dir(dir.path()));

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });

        let seen = Describe::default().all(&until(&rx, finished));
        assert_eq!(seen[0], "1 queued EU-WEST", "{seen:#?}");
        assert!(seen.contains(&"submitted []".to_string()), "{seen:#?}");
        let progress: Vec<&str> = seen
            .iter()
            .filter(|line| !line.starts_with("submitted"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            progress,
            [
                "1 queued EU-WEST",
                "1 Preparing",
                "1 Downloading { done: 1, total: Some(1) }",
                "1 Downloaded",
                "1 Stage(\"Extracting\")",
                "1 Stage(\"Installing\")",
                "1 Installed",
            ]
        );
    }

    #[test]
    fn submitted_comes_after_every_queued_of_its_batch_with_the_refusals() {
        let dir = TempDir::new().unwrap();
        let gate = Arc::new(Gate::default());
        let stages = FakeStages {
            swap_gate: Some(gate.clone()),
            ..FakeStages::in_dir(dir.path())
        };
        let (mut effects, rx) = effects_on(stages);
        // Bound after the effects, so on a failure it opens before their
        // drop joins the queue thread held at it.
        let gate = OpenOnDrop(gate);

        let _ = effects.apply(Effect::Submit {
            packages: vec![
                metadata("EU-WEST"),
                metadata("NA-EAST"),
                metadata("EU-WEST"),
            ],
            replacing: Vec::new(),
        });

        let seen = Describe::default().all(&until(&rx, |msg| matches!(msg, Msg::Submitted { .. })));
        let queued: Vec<&String> = seen.iter().filter(|l| l.contains("queued")).collect();
        assert_eq!(
            queued,
            ["1 queued EU-WEST", "2 queued NA-EAST"],
            "{seen:#?}"
        );
        let submitted = seen.last().unwrap();
        assert!(
            submitted.contains("EU-WEST ortho already has a job in the queue"),
            "{submitted}"
        );
        drop(gate);
    }

    #[test]
    fn remove_queues_each_target_and_answers_submitted() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_on(FakeStages::in_dir(dir.path()));

        let _ = effects.apply(Effect::Remove(vec![
            ("EU-WEST".to_string(), PackageType::Ortho),
            ("EU-WEST".to_string(), PackageType::Overlay),
        ]));

        let mut describe = Describe::default();
        let mut seen: Vec<String> = Vec::new();
        while seen.iter().filter(|l| l.ends_with("Removed")).count() < 2
            || !seen.iter().any(|l| l.starts_with("submitted"))
        {
            seen.push(describe.one(&next(&rx)));
        }
        assert_eq!(seen[0], "1 queued EU-WEST", "{seen:#?}");
        assert!(seen.contains(&"submitted []".to_string()), "{seen:#?}");
    }

    #[test]
    fn release_queue_frees_the_lock_for_the_next_queue() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let (mut effects, rx) = effects_on(stages.clone());
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });
        until(&rx, finished);

        let _ = effects.apply(Effect::ReleaseQueue);

        let next = InstallQueue::start(stages, Arc::new(Quiet));
        assert!(next.is_ok(), "the lock is still held: {:?}", next.err());
    }

    #[test]
    fn a_queue_started_after_a_release_gives_new_ids() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_on(FakeStages::in_dir(dir.path()));
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });
        let first = until(&rx, finished);
        let _ = effects.apply(Effect::ReleaseQueue);

        let _ = effects.apply(Effect::Remove(vec![(
            "EU-WEST".to_string(),
            PackageType::Ortho,
        )]));
        let second = until(&rx, finished);

        let (first, id) = (queued_ids(&first), queued_ids(&second)[0]);
        assert!(!first.contains(&id), "{first:?} and {id:?}");
        assert!(
            matches!(second.last(), Some(Msg::Job(done, JobMsg::Removed)) if *done == id),
            "{second:?}"
        );
    }

    /// Carry out `pending` and every effect the state gives for the
    /// messages that follow, until `done` holds for the state and the
    /// effects given.
    fn drive(
        state: &mut AppState,
        effects: &mut TestEffects,
        rx: &Receiver<Msg>,
        mut pending: Vec<Effect>,
        done: impl Fn(&AppState, &[Effect]) -> bool,
    ) -> Vec<Effect> {
        let mut given = Vec::new();
        loop {
            for effect in pending.drain(..) {
                given.push(effect.clone());
                let _ = effects.apply(effect);
            }
            if done(state, &given) {
                return given;
            }
            let msg = next(rx);
            effects.observe(&msg);
            pending = state.update(Input::Msg(msg), Instant::now());
        }
    }

    fn released(_: &AppState, given: &[Effect]) -> bool {
        given.contains(&Effect::ReleaseQueue)
    }

    /// The state keeps its job rows after the queue is released, so a job
    /// in the next queue must not be mistaken for a finished one.
    #[test]
    fn a_job_queued_after_the_queue_was_released_finishes_and_releases_it_again() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_installed(PackageType::Ortho, &["EU-WEST"]);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());
        let (mut state, load) = AppState::new(false);
        drive(&mut state, &mut effects, &rx, load, |state, _| {
            !state.local_rows().is_empty()
        });
        let delete = |state: &mut AppState| {
            let now = Instant::now();
            let _ = state.update(Input::Action(Action::Delete), now);
            state.update(Input::Action(Action::Confirm), now)
        };

        for _ in 0..2 {
            let remove = delete(&mut state);
            assert!(matches!(&remove[..], [Effect::Remove(_)]), "{remove:?}");
            drive(&mut state, &mut effects, &rx, remove, released);
        }

        assert_eq!(state.jobs().len(), 2);
        assert_eq!(state.unfinished(), 0);
        assert_eq!(state.summary(), "Package manager: 2 removed");
    }

    /// The state's view of `run` may be stale, so acting checks again.
    #[test]
    fn a_remove_while_run_is_active_queues_nothing_and_is_refused() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(stages.clone(), manager, settings());

        let _ = effects.apply(Effect::Remove(vec![
            ("EU-WEST".to_string(), PackageType::Ortho),
            ("NA-EAST".to_string(), PackageType::Ortho),
        ]));

        assert!(matches!(next(&rx), Msg::RunActive(Some(run)) if run.pid == 4242));
        let refused = next(&rx);
        assert!(
            matches!(&refused, Msg::QueueRefused(why) if *why == run_active(4242).message()),
            "{refused:?}"
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(
            InstallQueue::start(stages, Arc::new(Quiet)).is_ok(),
            "no queue was started"
        );
    }

    #[test]
    fn a_submit_while_run_is_active_queues_only_the_new_packages() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST"), metadata("NA-EAST")],
            replacing: vec![("eu-west".to_string(), PackageType::Ortho)],
        });

        let seen = until(&rx, |msg| matches!(msg, Msg::Submitted { .. }));
        assert!(
            matches!(&seen[0], Msg::RunActive(Some(run)) if run.pid == 4242),
            "{seen:?}"
        );
        let queued: Vec<&str> = seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Queued(_, summary) => Some(summary.region.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(queued, ["NA-EAST"]);
        let Some(Msg::Submitted { refused }) = seen.last() else {
            panic!("{seen:?}");
        };
        assert_eq!(
            refused,
            &[format!(
                "EU-WEST ortho left out. {}",
                run_active(4242).message()
            )]
        );
        until(&rx, finished);
    }

    #[test]
    fn a_submit_of_only_replacements_while_run_is_active_is_refused() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: vec![("EU-WEST".to_string(), PackageType::Ortho)],
        });

        assert!(matches!(next(&rx), Msg::RunActive(Some(run)) if run.pid == 4242));
        let refused = next(&rx);
        assert!(
            matches!(&refused, Msg::QueueRefused(why)
                if *why == format!("EU-WEST ortho left out. {}", run_active(4242).message())),
            "{refused:?}"
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// The state's rows may be stale: a package installed on disk is a
    /// replacement whatever its library row says.
    #[test]
    fn a_submit_while_run_is_active_leaves_out_what_is_installed_on_disk() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default()
            .with_running_instance(4242)
            .with_installed(PackageType::Ortho, &["eu-west"]);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST"), metadata("NA-EAST")],
            replacing: Vec::new(),
        });

        let seen = until(&rx, |msg| matches!(msg, Msg::Submitted { .. }));
        let queued: Vec<&str> = seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Queued(_, summary) => Some(summary.region.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(queued, ["NA-EAST"]);
        let Some(Msg::Submitted { refused }) = seen.last() else {
            panic!("{seen:?}");
        };
        assert_eq!(
            refused,
            &[format!(
                "EU-WEST ortho left out. {}",
                run_active(4242).message()
            )]
        );
        until(&rx, finished);
    }

    #[test]
    fn a_queue_that_will_not_start_keeps_the_left_out_note() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let _holder = InstallQueue::start(stages.clone(), Arc::new(Quiet)).unwrap();
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(stages, manager, settings());

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST"), metadata("NA-EAST")],
            replacing: vec![("EU-WEST".to_string(), PackageType::Ortho)],
        });

        assert!(matches!(next(&rx), Msg::RunActive(Some(run)) if run.pid == 4242));
        let refused = next(&rx);
        let left_out = format!("EU-WEST ortho left out. {}; ", run_active(4242).message());
        assert!(
            matches!(&refused, Msg::QueueRefused(why)
                if why.starts_with(&left_out) && why.contains("another package operation")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_submit_while_run_is_active_still_installs_new_packages() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("NA-EAST")],
            replacing: Vec::new(),
        });

        let seen = Describe::default().all(&until(&rx, finished));
        assert!(seen.contains(&"1 Installed".to_string()), "{seen:#?}");
    }

    #[test]
    fn a_held_lock_refuses_the_queue_and_names_the_other_operation() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let _holder = InstallQueue::start(stages.clone(), Arc::new(Quiet)).unwrap();
        let (mut effects, rx) = effects_on(stages);

        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });
        let refused = next(&rx);
        assert!(
            matches!(&refused, Msg::QueueRefused(why) if why.contains("another package operation")),
            "{refused:?}"
        );

        let _ = effects.apply(Effect::Remove(vec![(
            "EU-WEST".to_string(),
            PackageType::Ortho,
        )]));
        assert!(matches!(next(&rx), Msg::QueueRefused(_)));

        // Nothing to release: the queue never started.
        let _ = effects.apply(Effect::ReleaseQueue);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cancelling_a_job_in_its_swap_is_too_late() {
        let dir = TempDir::new().unwrap();
        let gate = Arc::new(Gate::default());
        let stages = FakeStages {
            swap_gate: Some(gate.clone()),
            ..FakeStages::in_dir(dir.path())
        };
        let (mut effects, rx) = effects_on(stages);
        // Bound after the effects, so on a failure it opens before their
        // drop joins the queue thread held at it.
        let gate = OpenOnDrop(gate);
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });
        let seen = until(
            &rx,
            |msg| matches!(msg, Msg::Job(_, JobMsg::Stage(stage)) if *stage == "Installing"),
        );

        let _ = effects.apply(Effect::CancelJob(queued_ids(&seen)[0]));

        let status = until(&rx, |msg| matches!(msg, Msg::Status(_)));
        assert!(
            matches!(status.last(), Some(Msg::Status(text)) if text == "too late: finishing"),
            "{status:?}"
        );
        drop(gate);
        until(&rx, finished);
    }

    #[test]
    fn queue_effects_with_no_queue_do_nothing() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_on(FakeStages::in_dir(dir.path()));

        for effect in [
            Effect::CancelJob(JobId(1)),
            Effect::CancelAll,
            Effect::Abandon,
            Effect::ReleaseQueue,
        ] {
            assert!(effects.apply(effect).is_continue());
        }

        assert!(rx.try_recv().is_err());
        assert!(effects.apply(Effect::Exit).is_break());
    }

    #[test]
    fn cancel_all_cancels_the_queued_jobs() {
        let dir = TempDir::new().unwrap();
        let gate = Arc::new(Gate::default());
        let stages = FakeStages {
            swap_gate: Some(gate.clone()),
            ..FakeStages::in_dir(dir.path())
        };
        let (mut effects, rx) = effects_on(stages);
        // Bound after the effects, so on a failure it opens before their
        // drop joins the queue thread held at it.
        let gate = OpenOnDrop(gate);
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST"), metadata("NA-EAST")],
            replacing: Vec::new(),
        });
        let mut describe = Describe::default();
        describe.all(&until(
            &rx,
            |msg| matches!(msg, Msg::Job(_, JobMsg::Stage(stage)) if *stage == "Installing"),
        ));

        let _ = effects.apply(Effect::CancelAll);

        // The swap is still held: the cancel, applied on its own thread,
        // finishes the queued job without it.
        let seen = describe.all(&until(&rx, finished));
        assert_eq!(seen.last().unwrap(), "2 Cancelled", "{seen:#?}");
        drop(gate);
        let rest = describe.all(&until(&rx, finished));
        assert_eq!(rest.last().unwrap(), "1 Installed", "the swap completes");
    }

    /// EU-WEST held in its swap, and NA-EAST downloaded behind it with the
    /// delete of its download held. Returns the gates, swap first, and the
    /// two jobs' ids.
    fn swapping_with_a_held_download_behind(
        dir: &Path,
    ) -> (
        TestEffects,
        Receiver<Msg>,
        OpenOnDrop,
        OpenOnDrop,
        [JobId; 2],
    ) {
        let swap = Arc::new(Gate::default());
        let delete = Arc::new(Gate::default());
        let stages = FakeStages {
            swap_gate: Some(Arc::clone(&swap)),
            drop_gate: Some(("NA-EAST", Arc::clone(&delete))),
            ..FakeStages::in_dir(dir)
        };
        let (mut effects, rx) = effects_on(stages);
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST"), metadata("NA-EAST")],
            replacing: Vec::new(),
        });
        let (swap, delete) = (OpenOnDrop(swap), OpenOnDrop(delete));
        let mut ids = Vec::new();
        let mut swapping = false;
        let mut downloaded = false;
        while !(swapping && downloaded) {
            match next(&rx) {
                Msg::Queued(id, _) => ids.push(id),
                Msg::Job(id, JobMsg::Stage("Installing")) if id == ids[0] => swapping = true,
                Msg::Job(id, JobMsg::Downloaded) if id == ids[1] => downloaded = true,
                _ => {}
            }
        }
        (effects, rx, swap, delete, [ids[0], ids[1]])
    }

    #[test]
    fn cancel_all_returns_at_once_while_a_download_is_being_deleted() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx, swap, delete, [eu, na]) =
            swapping_with_a_held_download_behind(dir.path());
        // A watchdog: should cancelling block, this opens the delete late
        // and the timing below fails, rather than the test hanging.
        let watchdog = Arc::clone(&delete.0);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            watchdog.open();
        });

        let started = std::time::Instant::now();
        let flow = effects.apply(Effect::CancelAll);

        assert!(flow.is_continue());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancelling held the runner for {:?}",
            started.elapsed()
        );
        // The runner is free: an effect still applies at once.
        let _ = effects.apply(Effect::CheckRun);
        assert!(
            !rx.try_iter()
                .any(|msg| matches!(msg, Msg::Job(id, JobMsg::Cancelled) if id == na)),
            "NA-EAST finishes only once its download is deleted"
        );
        drop(delete);
        until(
            &rx,
            |msg| matches!(msg, Msg::Job(id, JobMsg::Cancelled) if *id == na),
        );
        drop(swap);
        until(&rx, |msg| {
            matches!(msg, Msg::Job(id, _) if *id == eu) && finished(msg)
        });
    }

    #[test]
    fn exit_after_abandon_waits_for_the_swap_and_nothing_else() {
        let dir = TempDir::new().unwrap();
        let (mut effects, _rx, swap, delete, _) = swapping_with_a_held_download_behind(dir.path());

        assert!(effects.apply(Effect::Abandon).is_continue());
        assert!(effects.apply(Effect::Exit).is_break());
        let (tx, dropped) = mpsc::channel();
        std::thread::spawn(move || {
            drop(effects);
            let _ = tx.send(());
        });

        assert!(
            dropped.recv_timeout(Duration::from_millis(300)).is_err(),
            "left while the swap was running"
        );
        drop(swap);
        assert!(
            dropped.recv_timeout(WAIT).is_ok(),
            "waited for more than the swap"
        );
        drop(delete);
    }

    /// The queue holds its lock until the overlay rebuild is done, on the
    /// way out too.
    #[test]
    fn exit_finishes_the_overlay_rebuild_before_releasing_the_queue() {
        let dir = TempDir::new().unwrap();
        let scenery = dir.path().join("Custom Scenery");
        std::fs::create_dir_all(&scenery).unwrap();
        let (release, held) = mpsc::channel();
        let manager = FakeManager::default().holding_create_store(held);
        let stages = FakeStages::in_dir(dir.path());
        let (mut effects, rx) = effects_with(
            stages.clone(),
            manager,
            Settings {
                custom_scenery_path: Some(scenery),
                ..settings()
            },
        );
        let _ = effects.apply(Effect::Submit {
            packages: vec![metadata("EU-WEST")],
            replacing: Vec::new(),
        });
        until(&rx, finished);
        assert!(!effects.rebuild_pending());

        let _ = effects.apply(Effect::RebuildOverlays);
        assert!(effects.rebuild_pending());
        assert!(effects.apply(Effect::Exit).is_break());
        let (tx, dropped) = mpsc::channel();
        std::thread::spawn(move || {
            drop(effects);
            let _ = tx.send(());
        });

        assert!(
            dropped.recv_timeout(Duration::from_millis(200)).is_err(),
            "left before the rebuild was done"
        );
        assert!(
            matches!(
                InstallQueue::start(stages.clone(), Arc::new(Quiet)),
                Err(ManagerError::QueueBusy { .. })
            ),
            "the queue was released before the rebuild was done"
        );
        release.send(()).unwrap();
        assert!(dropped.recv_timeout(WAIT).is_ok());
        assert!(InstallQueue::start(stages, Arc::new(Quiet)).is_ok());
    }

    // ------------------------------------------------------------------------
    // The reads
    // ------------------------------------------------------------------------

    #[test]
    fn load_library_with_no_url_says_not_configured() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_with(
            FakeStages::in_dir(dir.path()),
            FakeManager::default(),
            Settings {
                library_url: None,
                ..settings()
            },
        );

        let _ = effects.apply(Effect::LoadLibrary);

        assert!(matches!(next(&rx), Msg::LibraryNotConfigured));
    }

    #[test]
    fn load_library_reports_a_failed_update_check_as_an_error() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST"])
            .with_failing_update_check();
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::LoadLibrary);

        assert!(matches!(next(&rx), Msg::Library(Err(_))));
    }

    #[test]
    fn the_installed_rows_carry_the_library_update_once_the_library_is_loaded() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default()
            .with_installed(PackageType::Ortho, &["EU-WEST", "NA-EAST"])
            .with_library(PackageType::Ortho, &["EU-WEST", "NA-EAST"])
            .with_status(
                PackageType::Ortho,
                "EU-WEST",
                PackageStatus::UpdateAvailable {
                    installed: Version::new(0, 9, 0),
                    available: Version::new(1, 0, 0),
                },
            )
            .with_status(PackageType::Ortho, "NA-EAST", PackageStatus::UpToDate);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::LoadInstalled);
        let _ = effects.apply(Effect::LoadLibrary);

        let Msg::Installed(Ok(first)) = next(&rx) else {
            panic!("installed rows first");
        };
        assert!(
            first.iter().all(|row| row.update.is_none()),
            "no library yet"
        );
        let Msg::Library(Ok(library)) = next(&rx) else {
            panic!("then the library");
        };
        assert_eq!(library.len(), 2);
        let Msg::Installed(Ok(rows)) = next(&rx) else {
            panic!("then the installed rows again, with updates");
        };
        let updates: Vec<(&str, Option<Version>)> = rows
            .iter()
            .map(|row| (row.region.as_str(), row.update.clone()))
            .collect();
        assert_eq!(
            updates,
            [("EU-WEST", Some(Version::new(1, 0, 0))), ("NA-EAST", None)]
        );
    }

    #[test]
    fn check_run_reports_the_running_instance() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_running_instance(4242);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::CheckRun);

        assert!(matches!(next(&rx), Msg::RunActive(Some(run)) if run.pid == 4242));
    }

    #[test]
    fn install_fetches_each_entry_metadata_in_order() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default().with_library(PackageType::Ortho, &["AF", "EU"]);
        let entries: Vec<LibraryEntry> = manager.fetch_library("").unwrap().entries;
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::Install(entries));

        let Msg::MetadataReady(results) = next(&rx) else {
            panic!("metadata expected");
        };
        let titles: Vec<String> = results.into_iter().map(|r| r.unwrap().title).collect();
        assert_eq!(titles, ["AF", "EU"]);
    }

    #[test]
    fn rebuild_overlays_answers_once_with_its_outcome() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_with(
            FakeStages::in_dir(dir.path()),
            FakeManager::default(),
            Settings {
                disable_overlays: true,
                ..settings()
            },
        );

        let _ = effects.apply(Effect::RebuildOverlays);

        let msg = next(&rx);
        assert!(
            matches!(&msg, Msg::OverlaysRebuilt(Some(text)) if text.contains("disable_overlays")),
            "{msg:?}"
        );
        drop(effects);
        assert!(rx.try_recv().is_err(), "answered once");
    }

    #[test]
    fn a_removed_overlay_loses_its_custom_scenery_symlink() {
        let dir = TempDir::new().unwrap();
        let scenery = dir.path().join("Custom Scenery");
        std::fs::create_dir_all(&scenery).unwrap();
        let link = scenery.join(xearthlayer_package::package_mountpoint(
            "EU-WEST",
            PackageType::Overlay,
        ));
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let (mut effects, rx) = effects_with(
            FakeStages::in_dir(dir.path()),
            FakeManager::default(),
            Settings {
                custom_scenery_path: Some(scenery),
                ..settings()
            },
        );
        let _ = effects.apply(Effect::Remove(vec![(
            "EU-WEST".to_string(),
            PackageType::Overlay,
        )]));

        for msg in until(&rx, finished) {
            effects.observe(&msg);
        }

        assert!(link.symlink_metadata().is_err(), "the symlink is gone");
    }

    #[test]
    fn a_read_that_panics_is_answered_with_an_error_and_the_worker_goes_on() {
        let dir = TempDir::new().unwrap();
        let manager = FakeManager::default()
            .panicking_on_list()
            .with_running_instance(7);
        let (mut effects, rx) = effects_with(FakeStages::in_dir(dir.path()), manager, settings());

        let _ = effects.apply(Effect::LoadInstalled);
        let _ = effects.apply(Effect::CheckRun);

        let msg = next(&rx);
        assert!(
            matches!(&msg, Msg::Installed(Err(why)) if why.contains("panicked")),
            "{msg:?}"
        );
        assert!(matches!(next(&rx), Msg::RunActive(Some(run)) if run.pid == 7));
    }

    #[test]
    fn with_no_worker_every_read_is_answered_with_an_error() {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::unstarted(tx);

        worker.load(Load::Installed);
        worker.load(Load::Library);
        worker.load(Load::Run);
        worker.send(Read::Metadata(vec![LibraryEntry::new(
            "sum",
            PackageType::Ortho,
            "EU-WEST",
            Version::new(1, 0, 0),
            "https://example.invalid/EU-WEST",
        )]));
        worker.rebuild_overlays();

        let got: Vec<Msg> = rx.try_iter().collect();
        assert!(matches!(&got[0], Msg::Installed(Err(_))), "{got:?}");
        assert!(matches!(&got[1], Msg::Library(Err(_))), "{got:?}");
        assert!(matches!(&got[2], Msg::Status(_)), "{got:?}");
        assert!(
            matches!(&got[3], Msg::MetadataReady(results)
                if matches!(&results[..], [Err(why)] if why.starts_with("EU-WEST ortho"))),
            "{got:?}"
        );
        assert!(
            matches!(&got[4], Msg::OverlaysRebuilt(Some(why)) if why.contains("not running")),
            "{got:?}"
        );
        // A later load is answered too, not swallowed as already pending.
        worker.load(Load::Installed);
        assert!(matches!(rx.try_recv(), Ok(Msg::Installed(Err(_)))));
    }
}
