//! Carries out the package manager's effects.
//!
//! [`Effects`] owns the session's install queue and a worker thread for the
//! slow reads: the installed packages, the library over HTTP, the `run`
//! check, package metadata and the overlay rebuild. The queue, the
//! [`ActiveQueue`] and dropping the queue stay on the runner's thread; the
//! worker only reads and answers with [`Msg`]s. [`ChannelObserver`] turns the
//! queue's events into `Msg`s on the same channel.

use std::collections::HashMap;
use std::ops::ControlFlow;
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
    after_removal, library_status, rebuild_overlays, LibraryRow, LibraryStatus,
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
    /// The package each remove job removes, until the job finishes.
    removing: HashMap<JobId, PackageKey>,
    custom_scenery_path: Option<PathBuf>,
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
        let custom_scenery_path = settings.custom_scenery_path.clone();
        Self {
            start_queue,
            queue: None,
            active,
            worker: Worker::start(manager, settings, tx.clone()),
            tx,
            removing: HashMap::new(),
            custom_scenery_path,
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
            Effect::Submit(metadata) => self.submit(metadata),
            Effect::Remove(targets) => self.remove(targets),
            Effect::CancelJob(id) => {
                let too_late = self
                    .queue
                    .as_ref()
                    .is_some_and(|queue| queue.cancel(id) == CancelResult::TooLate);
                if too_late {
                    self.send(Msg::Status("too late: finishing".to_string()));
                }
            }
            Effect::CancelAll => {
                if let Some(queue) = &self.queue {
                    queue.canceller().cancel_all();
                }
            }
            Effect::Abandon => {
                if let Some(queue) = &self.queue {
                    queue.canceller().abandon();
                }
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

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// The running queue, started if there is none. A queue that will not
    /// start is answered with `QueueRefused`.
    fn queue(&mut self) -> Option<&InstallQueue<S>> {
        if self.queue.is_none() {
            match (self.start_queue)() {
                Ok(queue) => {
                    self.active.set(queue.canceller());
                    self.queue = Some(queue);
                }
                Err(e) => {
                    self.send(Msg::QueueRefused(e.to_string()));
                    return None;
                }
            }
        }
        self.queue.as_ref()
    }

    /// Submit the installs as one batch. Each job's `Queued` comes from the
    /// observer during `submit_batch`, so `Submitted` follows all of them.
    fn submit(&mut self, metadata: Vec<PackageMetadata>) {
        let names: Vec<String> = metadata
            .iter()
            .map(|m| package_name(&m.title, m.package_type))
            .collect();
        let Some(queue) = self.queue() else {
            return;
        };
        let results = queue.submit_batch(metadata);
        let refused = names
            .into_iter()
            .zip(results)
            .filter_map(|(name, result)| result.err().map(|e| refusal(&name, &e)))
            .collect();
        // A stop asked for before the batch was in the queue found nothing to stop.
        self.active.reapply();
        self.send(Msg::Submitted { refused });
    }

    /// Submit a remove for each target, answered as [`submit`](Self::submit) is.
    fn remove(&mut self, targets: Vec<PackageKey>) {
        let Some(queue) = self.queue() else {
            return;
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
        // Idle, so this joins its threads at once.
        drop(self.queue.take());
    }
}

impl<S: Stages, F> Drop for Effects<S, F> {
    fn drop(&mut self) {
        self.active.clear();
        drop(self.queue.take());
    }
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
        let thread = std::thread::Builder::new()
            .name("xel-packages-worker".into())
            .spawn(move || run_worker(rx, &worker_flags, manager.as_ref(), &settings, &out))
            .map_err(|e| tracing::error!(error = %e, "Package manager: no worker thread"))
            .ok();
        Self {
            tx: Some(tx),
            thread,
            flags,
        }
    }

    fn send(&self, read: Read) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(read);
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

impl Drop for Worker {
    /// The reads still queued are skipped. A rebuild still to do is waited
    /// for, so the overlay folder is never left half built; otherwise the
    /// thread is left to finish its read, which may wait on the network.
    fn drop(&mut self) {
        self.flags.closing.store(true, Ordering::SeqCst);
        drop(self.tx.take());
        if self.flags.rebuilds.load(Ordering::SeqCst) > 0 {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
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
    let send = |msg: Msg| {
        let _ = out.send(msg);
    };
    // The last reads, so each answer can be joined with the other.
    let mut installed: Option<Vec<InstalledPackage>> = None;
    let mut library: Vec<LibraryRow> = Vec::new();
    for read in rx {
        if flags.closing.load(Ordering::SeqCst) && !matches!(read, Read::RebuildOverlays) {
            continue;
        }
        match read {
            Read::Load(load) => {
                // Cleared first, so a request made during this read runs again.
                flags.pending[load as usize].store(false, Ordering::SeqCst);
                match load {
                    Load::Installed => {
                        let store = manager.create_store(&settings.install_dir);
                        match manager.list_packages(&store) {
                            Ok(packages) => {
                                send(Msg::Installed(Ok(local_rows(&packages, &library))));
                                installed = Some(packages);
                            }
                            Err(e) => send(Msg::Installed(Err(e.to_string()))),
                        }
                    }
                    Load::Library => {
                        let Some(url) = &settings.library_url else {
                            send(Msg::LibraryNotConfigured);
                            continue;
                        };
                        match read_library(manager, settings, url) {
                            Ok(rows) => {
                                library = rows.clone();
                                send(Msg::Library(Ok(rows)));
                                // The installed rows' updates come from the library.
                                if let Some(packages) = &installed {
                                    send(Msg::Installed(Ok(local_rows(packages, &library))));
                                }
                            }
                            Err(e) => send(Msg::Library(Err(e.to_string()))),
                        }
                    }
                    Load::Run => send(Msg::RunActive(manager.running_instance())),
                }
            }
            Read::Metadata(entries) => {
                let results = entries
                    .iter()
                    .map(|entry| {
                        manager.fetch_metadata(&entry.metadata_url).map_err(|e| {
                            format!("{}: {e}", package_name(&entry.title, entry.package_type))
                        })
                    })
                    .collect();
                send(Msg::MetadataReady(results));
            }
            Read::RebuildOverlays => {
                let report = rebuild_overlays(
                    manager,
                    &settings.install_dir,
                    settings.custom_scenery_path.as_deref(),
                    settings.disable_overlays,
                    settings.xworld_pro,
                );
                flags.rebuilds.fetch_sub(1, Ordering::SeqCst);
                if let Some(line) = report.outcome.or_else(|| report.lines.first().cloned()) {
                    send(Msg::Status(line));
                }
            }
        }
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
    use std::time::Duration;

    use semver::Version;
    use tempfile::TempDir;
    use xearthlayer::manager::{
        CancelToken, Capacity, DownloadProgress, DownloadProgressCallback, InstallContext,
        InstallQueue, InstallResult, InstallStage, JobEvent, JobId, JobKind, JobOutcome,
        JobSummary, ManagerError, ManagerResult, PackageStatus, QueueObserver, Stages,
    };
    use xearthlayer_package::{LibraryEntry, PackageMetadata, PackageType};

    use super::*;
    use crate::commands::packages::test_support::{install_dir, FakeManager};
    use crate::signals::ActiveQueue;
    use crate::ui::packages::app::{Effect, JobMsg, Msg};

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

    /// Every stage succeeds at once, except a swap held by `swap_gate`.
    #[derive(Clone)]
    struct FakeStages {
        temp: PathBuf,
        packages: PathBuf,
        swap_gate: Option<Arc<Gate>>,
    }

    impl FakeStages {
        fn in_dir(dir: &Path) -> Self {
            Self {
                temp: dir.join("tmp"),
                packages: dir.join("packages"),
                swap_gate: None,
            }
        }
    }

    impl Stages for FakeStages {
        type Prepared = String;
        type Downloaded = String;

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
        ) -> ManagerResult<String> {
            progress(&DownloadProgress {
                parts: Vec::new(),
                total_bytes_downloaded: 1,
                total_bytes: Some(1),
            });
            Ok(title)
        }

        fn install(&self, title: String, ctx: &dyn InstallContext) -> ManagerResult<InstallResult> {
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

    type TestEffects =
        Effects<FakeStages, Box<dyn Fn() -> ManagerResult<InstallQueue<FakeStages>>>>;

    /// Effects whose queue runs `stages`, with `manager` behind the reads.
    fn effects_with(
        stages: FakeStages,
        manager: FakeManager,
        settings: Settings,
    ) -> (TestEffects, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let observer_tx = tx.clone();
        let start: Box<dyn Fn() -> ManagerResult<InstallQueue<FakeStages>>> = Box::new(move || {
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

    /// One line per message, for asserting on order.
    fn describe(msg: &Msg) -> String {
        match msg {
            Msg::Queued(id, summary) => format!("{} queued {}", id.0, summary.region),
            Msg::Submitted { refused } => format!("submitted {refused:?}"),
            Msg::Job(id, job) => format!("{} {job:?}", id.0),
            other => format!("{other:?}"),
        }
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

        let _ = effects.apply(Effect::Submit(vec![metadata("EU-WEST")]));

        let seen: Vec<String> = until(&rx, finished).iter().map(describe).collect();
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

        let _ = effects.apply(Effect::Submit(vec![
            metadata("EU-WEST"),
            metadata("NA-EAST"),
            metadata("EU-WEST"),
        ]));

        let seen: Vec<String> = until(&rx, |msg| matches!(msg, Msg::Submitted { .. }))
            .iter()
            .map(describe)
            .collect();
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
        gate.open();
    }

    #[test]
    fn remove_queues_each_target_and_answers_submitted() {
        let dir = TempDir::new().unwrap();
        let (mut effects, rx) = effects_on(FakeStages::in_dir(dir.path()));

        let _ = effects.apply(Effect::Remove(vec![
            ("EU-WEST".to_string(), PackageType::Ortho),
            ("EU-WEST".to_string(), PackageType::Overlay),
        ]));

        let mut seen: Vec<String> = Vec::new();
        while seen.iter().filter(|l| l.ends_with("Removed")).count() < 2
            || !seen.iter().any(|l| l.starts_with("submitted"))
        {
            seen.push(describe(&next(&rx)));
        }
        assert_eq!(seen[0], "1 queued EU-WEST", "{seen:#?}");
        assert!(seen.contains(&"submitted []".to_string()), "{seen:#?}");
    }

    #[test]
    fn release_queue_frees_the_lock_for_the_next_queue() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let (mut effects, rx) = effects_on(stages.clone());
        let _ = effects.apply(Effect::Submit(vec![metadata("EU-WEST")]));
        until(&rx, finished);

        let _ = effects.apply(Effect::ReleaseQueue);

        let next = InstallQueue::start(stages, Arc::new(Quiet));
        assert!(next.is_ok(), "the lock is still held: {:?}", next.err());
    }

    #[test]
    fn a_held_lock_refuses_the_queue_and_names_the_other_operation() {
        let dir = TempDir::new().unwrap();
        let stages = FakeStages::in_dir(dir.path());
        let _holder = InstallQueue::start(stages.clone(), Arc::new(Quiet)).unwrap();
        let (mut effects, rx) = effects_on(stages);

        let _ = effects.apply(Effect::Submit(vec![metadata("EU-WEST")]));
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
        let _ = effects.apply(Effect::Submit(vec![metadata("EU-WEST")]));
        until(
            &rx,
            |msg| matches!(msg, Msg::Job(_, JobMsg::Stage(stage)) if *stage == "Installing"),
        );

        let _ = effects.apply(Effect::CancelJob(JobId(1)));

        let status = until(&rx, |msg| matches!(msg, Msg::Status(_)));
        assert!(
            matches!(status.last(), Some(Msg::Status(text)) if text == "too late: finishing"),
            "{status:?}"
        );
        gate.open();
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
        let _ = effects.apply(Effect::Submit(vec![
            metadata("EU-WEST"),
            metadata("NA-EAST"),
        ]));
        until(
            &rx,
            |msg| matches!(msg, Msg::Job(_, JobMsg::Stage(stage)) if *stage == "Installing"),
        );

        let _ = effects.apply(Effect::CancelAll);
        gate.open();

        let seen: Vec<String> = until(&rx, |msg| {
            matches!(msg, Msg::Job(JobId(2), JobMsg::Cancelled))
        })
        .iter()
        .map(describe)
        .collect();
        assert!(seen.iter().any(|l| l == "2 Cancelled"), "{seen:#?}");
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

        assert!(matches!(next(&rx), Msg::RunActive(Some(4242))));
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
    fn rebuild_overlays_reports_its_outcome_as_the_status() {
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
            matches!(&msg, Msg::Status(text) if text.contains("disable_overlays")),
            "{msg:?}"
        );
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
}
