//! Queue flow tests, driven by gated fake stages.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use xearthlayer_package::{PackageMetadata, PackageType};

use super::*;
use crate::manager::{
    CancelToken, DownloadProgressCallback, InstallResult, InstallStage, ManagerError, ManagerResult,
};

/// A latch a fake stage waits at, so the test decides when it proceeds.
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

    /// Wait until opened. False if `cancel` fires first.
    fn pass(&self, cancel: &CancelToken) -> bool {
        let mut open = self.open.lock().unwrap();
        while !*open {
            if cancel.is_cancelled() {
                return false;
            }
            open = self
                .cv
                .wait_timeout(open, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        true
    }
}

/// Opens its gate when dropped, so a failing assertion unwinds instead of
/// leaving a queue thread blocked at the gate while the queue joins it.
struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[derive(Default)]
struct Fake {
    dir: Option<TempDir>,
    packages: PathBuf,
    free: AtomicU64,
    /// Free bytes on separate temp and packages filesystems, when set;
    /// otherwise both folders share `free`.
    split: Mutex<Option<(u64, u64)>>,
    sizes: Mutex<HashMap<String, u64>>,
    hold_download: Mutex<HashMap<String, Arc<Gate>>>,
    hold_install: Mutex<HashMap<String, Arc<Gate>>>,
    hold_swap: Mutex<HashMap<String, Arc<Gate>>>,
    /// Holds what follows the swap, like deleting the replaced version.
    hold_after_swap: Mutex<HashMap<String, Arc<Gate>>>,
    /// Holds a remove, like deleting a large package.
    hold_remove: Mutex<HashMap<String, Arc<Gate>>>,
    /// Holds dropping a downloaded package, like deleting its temp tree.
    hold_drop: Mutex<HashMap<String, Arc<Gate>>>,
    /// Installs that never call `end_swap`.
    skip_end_swap: Mutex<HashSet<String>>,
    fail_download: Mutex<HashSet<String>>,
    /// Downloads and installs that run on even once cancelled, like a
    /// download whose last byte arrives as the queue shuts down, or an
    /// install stuck in a long temp-directory delete.
    ignore_cancel: Mutex<HashSet<String>>,
    /// Title to the stage that panics for it.
    panic_in: Mutex<HashMap<String, &'static str>>,
    /// How many times the queue read the free space.
    free_reads: AtomicU64,
    /// Holds reading the free space, like statvfs on a slow filesystem.
    hold_free_read: Mutex<Option<Arc<Gate>>>,
    log: Mutex<Vec<String>>,
}

impl Fake {
    fn record(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
    fn maybe_panic(&self, title: &str, stage: &str) {
        let armed = self.panic_in.lock().unwrap().get(title) == Some(&stage);
        if armed {
            panic!("{stage} blew up for {title}");
        }
    }
    fn logged(&self, line: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l == line)
    }
    fn gate(map: &Mutex<HashMap<String, Arc<Gate>>>, title: &str) -> Option<Arc<Gate>> {
        map.lock().unwrap().get(title).cloned()
    }
    fn hold(map: &Mutex<HashMap<String, Arc<Gate>>>, title: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        map.lock()
            .unwrap()
            .insert(title.to_string(), Arc::clone(&gate));
        gate
    }
}

#[derive(Clone)]
struct FakeStages(Arc<Fake>);

/// A prepared or downloaded package. Dropping it stands for deleting its
/// temp tree.
struct Payload {
    title: String,
    fake: Arc<Fake>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        if let Some(gate) = Fake::gate(&self.fake.hold_drop, &self.title) {
            self.fake.record(format!("dropping {}", self.title));
            gate.pass(&CancelToken::new());
            self.fake.record(format!("dropped {}", self.title));
        }
    }
}

impl FakeStages {
    fn new(free: u64) -> Self {
        let dir = TempDir::new().unwrap();
        let fake = Fake {
            packages: dir.path().join("packages"),
            dir: Some(dir),
            ..Fake::default()
        };
        fake.free.store(free, Ordering::SeqCst);
        Self(Arc::new(fake))
    }
    /// Temp and packages folders on separate filesystems.
    fn split(temp: u64, packages: u64) -> Self {
        let stages = Self::new(0);
        *stages.0.split.lock().unwrap() = Some((temp, packages));
        stages
    }
    fn size(&self, title: &str, bytes: u64) -> &Self {
        self.0
            .sizes
            .lock()
            .unwrap()
            .insert(title.to_string(), bytes);
        self
    }
}

impl Stages for FakeStages {
    type Prepared = (Payload, u64);
    type Downloaded = Payload;

    fn temp_dir(&self) -> &Path {
        self.0.dir.as_ref().unwrap().path()
    }

    fn packages_root(&self) -> &Path {
        &self.0.packages
    }

    fn capacity(&self) -> ManagerResult<Capacity> {
        self.0.free_reads.fetch_add(1, Ordering::SeqCst);
        let held = self.0.hold_free_read.lock().unwrap().clone();
        if let Some(gate) = held {
            gate.pass(&CancelToken::new());
        }
        Ok(match *self.0.split.lock().unwrap() {
            Some((temp, packages)) => Capacity::Split { temp, packages },
            None => Capacity::Shared {
                free: self.0.free.load(Ordering::SeqCst),
            },
        })
    }

    fn prepare(&self, metadata: &PackageMetadata) -> ManagerResult<(Payload, u64)> {
        let size = *self
            .0
            .sizes
            .lock()
            .unwrap()
            .get(&metadata.title)
            .unwrap_or(&1);
        self.0.record(format!("prepared {}", metadata.title));
        let payload = Payload {
            title: metadata.title.clone(),
            fake: Arc::clone(&self.0),
        };
        Ok((payload, size))
    }

    fn download_size(&self, prepared: &(Payload, u64)) -> u64 {
        prepared.1
    }

    fn download(
        &self,
        (payload, _): (Payload, u64),
        cancel: &CancelToken,
        _progress: DownloadProgressCallback,
    ) -> ManagerResult<Payload> {
        let title = payload.title.clone();
        self.0.record(format!("download started {title}"));
        self.0.maybe_panic(&title, "download");
        let cancel = if self.0.ignore_cancel.lock().unwrap().contains(&title) {
            &CancelToken::new()
        } else {
            cancel
        };
        if let Some(gate) = Fake::gate(&self.0.hold_download, &title) {
            if !gate.pass(cancel) {
                return Err(ManagerError::Cancelled);
            }
        }
        if self.0.fail_download.lock().unwrap().contains(&title) {
            return Err(ManagerError::DownloadFailed {
                url: title,
                reason: "connection reset".into(),
            });
        }
        self.0.record(format!("downloaded {title}"));
        Ok(payload)
    }

    fn install(&self, payload: Payload, ctx: &dyn InstallContext) -> ManagerResult<InstallResult> {
        let title = payload.title.clone();
        ctx.stage(InstallStage::Extracting);
        let cancel = if self.0.ignore_cancel.lock().unwrap().contains(&title) {
            &CancelToken::new()
        } else {
            ctx.cancel_token()
        };
        if let Some(gate) = Fake::gate(&self.0.hold_install, &title) {
            if !gate.pass(cancel) {
                return Err(ManagerError::Cancelled);
            }
        }
        if !ctx.begin_swap() {
            return Err(ManagerError::Cancelled);
        }
        ctx.stage(InstallStage::Installing);
        self.0.maybe_panic(&title, "install");
        if let Some(gate) = Fake::gate(&self.0.hold_swap, &title) {
            gate.pass(&CancelToken::new()); // the swap ignores cancellation
        }
        self.0.record(format!("swapped {title}")); // before end_swap can wake a waiter
        if !self.0.skip_end_swap.lock().unwrap().contains(&title) {
            ctx.end_swap();
        }
        if let Some(gate) = Fake::gate(&self.0.hold_after_swap, &title) {
            gate.pass(&CancelToken::new()); // nor does the cleanup after it
        }
        self.0.record(format!("installed {title}"));
        drop(payload);
        Ok(InstallResult {
            region: title,
            package_type: PackageType::Ortho,
            version: semver::Version::new(1, 0, 0),
            install_path: PathBuf::new(),
            bytes_downloaded: 0,
            files_extracted: 0,
        })
    }

    fn remove(&self, region: &str, _package_type: PackageType) -> ManagerResult<()> {
        if let Some(gate) = Fake::gate(&self.0.hold_remove, region) {
            gate.pass(&CancelToken::new()); // a started remove ignores cancellation
        }
        self.0.record(format!("removed {region}"));
        Ok(())
    }
}

/// Every event as (job, text).
#[derive(Default)]
struct Events(Mutex<Vec<(JobId, String)>>);

impl Events {
    fn of(&self, id: JobId) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(j, _)| *j == id)
            .map(|(_, e)| e.clone())
            .collect()
    }
}

impl QueueObserver for Events {
    fn on_event(&self, id: JobId, event: &JobEvent<'_>) {
        let text = match event {
            JobEvent::Queued(s) => format!("queued {}", s.region),
            JobEvent::Preparing => "preparing".into(),
            JobEvent::WaitingForSpace { .. } => "waiting".into(),
            JobEvent::Downloading(_) => "downloading".into(),
            JobEvent::Downloaded => "downloaded".into(),
            JobEvent::Installing(stage) => format!("installing {}", stage.name()),
            JobEvent::Removing => "removing".into(),
            JobEvent::Finished(o) => format!("finished {}", outcome_name(o)),
        };
        self.0.lock().unwrap().push((id, text));
    }
}

fn outcome_name(outcome: &JobOutcome) -> &'static str {
    match outcome {
        JobOutcome::Installed(_) => "installed",
        JobOutcome::Removed => "removed",
        JobOutcome::Cancelled => "cancelled",
        JobOutcome::Failed(_) => "failed",
    }
}

fn meta(title: &str) -> PackageMetadata {
    PackageMetadata {
        spec_version: semver::Version::new(1, 0, 0),
        title: title.to_string(),
        package_version: semver::Version::new(1, 0, 0),
        published_at: chrono::Utc::now(),
        package_type: PackageType::Ortho,
        mountpoint: xearthlayer_package::package_mountpoint(title, PackageType::Ortho),
        parts: vec![],
        filename: format!("{title}.tar.gz"),
    }
}

/// Poll `condition` for up to 5 s.
fn eventually(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start(stages: &FakeStages) -> (InstallQueue<FakeStages>, Arc<Events>) {
    let events = Arc::new(Events::default());
    let queue = InstallQueue::start(stages.clone(), events.clone()).unwrap();
    (queue, events)
}

#[test]
fn a_second_queue_on_the_same_temp_directory_is_refused() {
    let stages = FakeStages::new(u64::MAX);
    let (_queue, _) = start(&stages);
    let second = InstallQueue::start(stages.clone(), Arc::new(Events::default()));
    assert!(matches!(second, Err(ManagerError::QueueBusy { .. })));
}

/// A UI keeps its job rows across queues, so an id never repeats.
#[test]
fn ids_never_repeat_across_queues() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _) = start(&stages);
    let first = queue.submit(Job::Install(meta("A"))).unwrap();
    queue.wait_idle();
    drop(queue);

    let (queue, _) = start(&stages);
    let second = queue.submit(Job::Install(meta("A"))).unwrap();
    queue.wait_idle();
    assert_ne!(first, second);
}

#[test]
fn starting_a_queue_sweeps_what_a_killed_run_left_behind() {
    let stages = FakeStages::new(u64::MAX);
    let leftover = stages.temp_dir().join("install_eu_ortho_1.0.0");
    std::fs::create_dir_all(&leftover).unwrap();
    let (_queue, _) = start(&stages);
    assert!(!leftover.exists());
}

/// Item 1: the network never waits for the disk.
#[test]
fn downloads_run_ahead_while_an_install_is_held() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, _) = start(&stages);
    for title in ["A", "B", "C", "D"] {
        queue.submit(Job::Install(meta(title))).unwrap();
    }

    eventually("B, C and D downloaded", || {
        ["B", "C", "D"]
            .iter()
            .all(|t| stages.0.logged(&format!("downloaded {t}")))
    });
    assert!(!stages.0.logged("installed A"));

    held.open();
    queue.wait_idle();
    let installs: Vec<String> = stages
        .0
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("installed"))
        .cloned()
        .collect();
    assert_eq!(
        installs,
        ["installed A", "installed B", "installed C", "installed D"]
    );
}

#[test]
fn a_download_waits_for_space_and_starts_when_an_install_frees_it() {
    // A's and B's parts fill 90 of the 100 temp bytes; C's 60 fit once A
    // has installed and its parts are gone.
    let stages = FakeStages::split(100, 10_000);
    stages.size("A", 50).size("B", 40).size("C", 60);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    let ids: Vec<JobId> = ["A", "B", "C"]
        .iter()
        .map(|t| queue.submit(Job::Install(meta(t))).unwrap())
        .collect();

    eventually("C waiting", || {
        events.of(ids[2]).contains(&"waiting".to_string())
    });
    assert!(!stages.0.logged("downloaded C"));

    held.open();
    queue.wait_idle();
    assert!(stages.0.logged("installed C"));
}

#[test]
fn a_package_that_cannot_fit_alone_fails_and_the_rest_continue() {
    let stages = FakeStages::new(199);
    stages.size("A", 50).size("B", 10);
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B")]);

    match &results[0] {
        Err(ManagerError::InsufficientDiskSpace {
            required_bytes,
            available_bytes,
            ..
        }) => {
            assert_eq!((*required_bytes, *available_bytes), (200, 199));
        }
        other => panic!("expected InsufficientDiskSpace, got {other:?}"),
    }
    assert!(results[1].is_ok(), "{:?}", results[1]);
    assert!(
        !stages.0.logged("downloaded A"),
        "nothing downloads for a package that cannot fit"
    );
}

#[test]
fn a_package_the_packages_disk_cannot_hold_fails_before_downloading_and_the_rest_install() {
    let stages = FakeStages::split(1_000, 200);
    stages.size("A", 50).size("B", 50);
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B")]);

    assert!(results[0].is_ok(), "{:?}", results[0]);
    match &results[1] {
        Err(ManagerError::InsufficientDiskSpace {
            path,
            required_bytes,
            available_bytes,
        }) => {
            assert_eq!(path, stages.packages_root());
            assert_eq!((*required_bytes, *available_bytes), (300, 200));
        }
        other => panic!("expected InsufficientDiskSpace, got {other:?}"),
    }
    assert!(!stages.0.logged("downloaded B"));
}

/// A tree installed earlier in the busy period still takes up the packages
/// disk, so a package submitted after it installs is charged for it.
#[test]
fn a_tree_installed_earlier_in_the_busy_period_stays_charged() {
    let stages = FakeStages::split(1_000, 200);
    stages.size("A", 50).size("B", 1).size("C", 50);
    // B keeps the queue busy, so the capacity is not read again. A and B
    // go in under one lock, so A cannot finish while the queue is idle.
    let held = Fake::hold(&stages.0.hold_download, "B");
    let (queue, _) = start(&stages);
    let _open = OpenOnDrop(Arc::clone(&held));
    let ids = queue.submit_batch(vec![meta("A"), meta("B")]);
    let a = *ids[0].as_ref().unwrap();
    assert!(matches!(
        queue.wait_for(&[a])[..],
        [JobOutcome::Installed(_)]
    ));

    let c = queue.submit(Job::Install(meta("C"))).unwrap();
    held.open();

    // 150 installed + B's 3 + C's 150 = 303.
    match &queue.wait_for(&[c])[..] {
        [JobOutcome::Failed(ManagerError::InsufficientDiskSpace {
            path,
            required_bytes,
            available_bytes,
        })] => {
            assert_eq!(path, stages.packages_root());
            assert_eq!((*required_bytes, *available_bytes), (303, 200));
        }
        other => panic!("expected InsufficientDiskSpace, got {other:?}"),
    }
    assert!(!stages.0.logged("downloaded C"));
    assert_eq!(
        stages.0.free_reads.load(Ordering::SeqCst),
        1,
        "one busy period"
    );
}

#[test]
fn a_remove_overtakes_a_download_waiting_for_space() {
    // A's parts (50) plus B's (50) are one more than the temp disk holds.
    let stages = FakeStages::split(99, 10_000);
    stages.size("A", 50).size("B", 50);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("B waiting", || {
        events.of(b).contains(&"waiting".to_string())
    });
    let removal = queue
        .submit(Job::Remove {
            region: "EU".into(),
            package_type: PackageType::Ortho,
        })
        .unwrap();

    held.open();
    queue.wait_idle();
    let log = stages.0.log.lock().unwrap().clone();
    let at = |line: &str| log.iter().position(|l| l == line).unwrap();
    assert!(at("removed EU") < at("installed B"), "{log:?}");
    assert_eq!(events.of(removal).last().unwrap(), "finished removed");
}

#[test]
fn a_second_job_for_the_same_package_is_refused() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, _) = start(&stages);
    queue.submit(Job::Install(meta("A"))).unwrap();

    let again = queue.submit(Job::Install(meta("A")));
    let remove = queue.submit(Job::Remove {
        region: "A".into(),
        package_type: PackageType::Ortho,
    });

    assert!(
        matches!(again, Err(ManagerError::AlreadyQueued { .. })),
        "{again:?}"
    );
    assert!(
        matches!(remove, Err(ManagerError::AlreadyQueued { .. })),
        "{remove:?}"
    );
    held.open();
}

#[test]
fn a_job_reports_its_events_in_order() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, events) = start(&stages);
    let id = queue.submit(Job::Install(meta("A"))).unwrap();
    queue.wait_idle();

    assert_eq!(
        events.of(id),
        [
            "queued A",
            "preparing",
            "downloaded",
            "installing Extracting",
            "installing Installing",
            "finished installed"
        ]
    );
}

#[test]
fn install_batch_returns_results_in_order_and_isolates_failures() {
    let stages = FakeStages::new(u64::MAX);
    stages.0.fail_download.lock().unwrap().insert("B".into());
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B"), meta("C")]);

    assert_eq!(results[0].as_ref().unwrap().region, "A");
    assert!(matches!(
        results[1],
        Err(ManagerError::DownloadFailed { .. })
    ));
    assert_eq!(results[2].as_ref().unwrap().region, "C");
}

#[test]
fn free_space_is_read_again_after_the_queue_goes_idle() {
    let stages = FakeStages::new(100);
    stages.size("A", 50).size("B", 50);
    let (queue, _) = start(&stages);
    assert!(
        queue.install_batch(vec![meta("A")])[0].is_err(),
        "200 > 100"
    );

    stages.0.free.store(1_000, Ordering::SeqCst);
    assert!(queue.install_batch(vec![meta("B")])[0].is_ok());
}

#[test]
fn take_finished_hands_over_each_outcome_once() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _) = start(&stages);
    let id = queue.submit(Job::Install(meta("A"))).unwrap();
    queue.wait_idle();

    let finished = queue.take_finished();
    assert!(matches!(finished.get(&id), Some(JobOutcome::Installed(_))));
    assert!(queue.take_finished().is_empty());
}

#[test]
fn dropping_the_queue_cancels_queued_work_and_returns() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_download, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("A downloading", || {
        events.of(a).contains(&"preparing".to_string())
    });

    drop(queue); // must not hang: the held download sees its token

    assert_eq!(events.of(a).last().unwrap(), "finished cancelled");
    assert_eq!(events.of(b).last().unwrap(), "finished cancelled");
    drop(held);
}

#[test]
fn a_download_that_finishes_during_shutdown_still_reports_cancelled() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_download, "A");
    stages.0.ignore_cancel.lock().unwrap().insert("A".into());
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    // Past admission and inside the download, so only the gate holds it.
    eventually("A downloading", || stages.0.logged("download started A"));

    let dropper = std::thread::spawn(move || drop(queue));
    // B finishes inside `drop`, after the queue is marked as shutting down,
    // and the installer leaves as soon as it sees that.
    eventually("B cancelled", || {
        events.of(b).last().map(String::as_str) == Some("finished cancelled")
    });
    held.open();
    dropper.join().unwrap();

    assert!(stages.0.logged("downloaded A"));
    assert_eq!(events.of(a).last().unwrap(), "finished cancelled");
}

fn assert_panicked(result: &ManagerResult<InstallResult>, expected: &str) {
    match result {
        Err(ManagerError::Panicked { stage, message }) => {
            assert_eq!(*stage, expected);
            assert!(message.contains("blew up"), "{message}");
        }
        other => panic!("expected Panicked in {expected}, got {other:?}"),
    }
}

#[test]
fn a_panicking_install_fails_its_job_and_the_queue_carries_on() {
    let stages = FakeStages::new(u64::MAX);
    stages
        .0
        .panic_in
        .lock()
        .unwrap()
        .insert("A".into(), "install");
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B")]);

    assert_panicked(&results[0], "install");
    assert_eq!(results[1].as_ref().unwrap().region, "B");
    assert!(
        !queue.shared.lock().swapping,
        "a panic mid-swap clears the flag"
    );
    queue.submit(Job::Install(meta("C"))).unwrap();
    queue.wait_idle();
    assert!(stages.0.logged("installed C"));
}

#[test]
fn a_panicking_download_fails_its_job_and_the_next_download_runs() {
    let stages = FakeStages::new(u64::MAX);
    stages
        .0
        .panic_in
        .lock()
        .unwrap()
        .insert("A".into(), "download");
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B")]);

    assert_panicked(&results[0], "download");
    assert_eq!(results[1].as_ref().unwrap().region, "B");
    queue.wait_idle();
}

/// Panics on the events every job passes through on its way to finishing.
struct PanickingObserver;

impl QueueObserver for PanickingObserver {
    fn on_event(&self, _id: JobId, event: &JobEvent<'_>) {
        if matches!(event, JobEvent::Downloaded | JobEvent::Finished(_)) {
            panic!("observer blew up");
        }
    }
}

#[test]
fn a_panicking_observer_does_not_stop_the_queue() {
    let stages = FakeStages::new(u64::MAX);
    let queue = InstallQueue::start(stages.clone(), Arc::new(PanickingObserver)).unwrap();

    let results = queue.install_batch(vec![meta("A"), meta("B")]);

    assert!(results.iter().all(Result::is_ok), "{results:?}");
    queue.wait_idle();
}

#[test]
fn free_space_is_read_once_per_busy_period() {
    // Each package's parts fill the whole temp disk, so every download after
    // the first is admitted only once the install ahead of it has finished.
    let stages = FakeStages::split(50, 10_000);
    stages
        .size("A", 50)
        .size("B", 50)
        .size("C", 50)
        .size("D", 50);
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("B"), meta("C")]);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(stages.0.free_reads.load(Ordering::SeqCst), 1);

    assert!(queue.install_batch(vec![meta("D")])[0].is_ok());
    assert_eq!(
        stages.0.free_reads.load(Ordering::SeqCst),
        2,
        "read again after idle"
    );
}

#[test]
fn cancelling_a_queued_job_finishes_it_without_preparing_it() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_download, "A");
    let (queue, events) = start(&stages);
    queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();

    assert_eq!(queue.cancel(b), CancelResult::Cancelled);

    assert_eq!(events.of(b), ["queued B", "finished cancelled"]);
    held.open();
    queue.wait_idle();
    assert!(!stages.0.logged("prepared B"));
}

#[test]
fn cancelling_a_download_waiting_for_space_releases_it() {
    let stages = FakeStages::split(99, 10_000);
    stages.size("A", 50).size("B", 50).size("C", 10);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("B waiting", || {
        events.of(b).contains(&"waiting".to_string())
    });

    assert_eq!(queue.cancel(b), CancelResult::Cancelled);

    eventually("B finished", || {
        events.of(b).last().map(String::as_str) == Some("finished cancelled")
    });
    queue.submit(Job::Install(meta("C"))).unwrap(); // 50 + 10 = 60 fits once B is gone
    eventually("C downloaded", || stages.0.logged("downloaded C"));
    held.open();
}

#[test]
fn cancelling_a_running_download_stops_it() {
    let stages = FakeStages::new(u64::MAX);
    let _held = Fake::hold(&stages.0.hold_download, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A preparing", || {
        events.of(a).contains(&"preparing".to_string())
    });

    assert_eq!(queue.cancel(a), CancelResult::Cancelled);

    assert!(matches!(queue.wait_for(&[a])[0], JobOutcome::Cancelled));
}

#[test]
fn cancelling_a_downloaded_job_skips_its_install() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("B downloaded", || {
        events.of(b).contains(&"downloaded".to_string())
    });

    assert_eq!(queue.cancel(b), CancelResult::Cancelled);

    held.open();
    queue.wait_idle();
    assert!(!stages.0.logged("installed B"));
}

#[test]
fn cancelling_an_install_before_its_swap_leaves_nothing_installed() {
    let stages = FakeStages::new(u64::MAX);
    let _held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A extracting", || {
        events.of(a).contains(&"installing Extracting".to_string())
    });

    assert_eq!(queue.cancel(a), CancelResult::Cancelled);

    assert!(matches!(queue.wait_for(&[a])[0], JobOutcome::Cancelled));
    assert!(!stages.0.logged("installed A"));
}

#[test]
fn a_swap_cannot_be_cancelled_and_completes() {
    let stages = FakeStages::new(u64::MAX);
    let swap = Fake::hold(&stages.0.hold_swap, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A swapping", || {
        events.of(a).contains(&"installing Installing".to_string())
    });

    assert_eq!(queue.cancel(a), CancelResult::TooLate);

    swap.open();
    assert!(matches!(queue.wait_for(&[a])[0], JobOutcome::Installed(_)));
}

#[test]
fn an_unknown_job_cannot_be_cancelled() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _) = start(&stages);
    assert_eq!(queue.cancel(JobId(999)), CancelResult::Unknown);
}

#[test]
fn cancel_all_cancels_every_unfinished_job() {
    let stages = FakeStages::new(u64::MAX);
    let _held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    let ids: Vec<JobId> = ["A", "B", "C"]
        .iter()
        .map(|t| queue.submit(Job::Install(meta(t))).unwrap())
        .collect();
    eventually("A extracting", || {
        events
            .of(ids[0])
            .contains(&"installing Extracting".to_string())
    });

    queue.canceller().cancel_all();

    assert!(queue
        .wait_for(&ids)
        .iter()
        .all(|o| matches!(o, JobOutcome::Cancelled)));
}

#[test]
fn abandoning_returns_without_waiting_for_a_slow_install() {
    let stages = FakeStages::new(u64::MAX);
    // An install that ignores its token, like a long temp-directory delete.
    let stuck = Fake::hold(&stages.0.hold_install, "A");
    stages.0.ignore_cancel.lock().unwrap().insert("A".into());
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A extracting", || {
        events.of(a).contains(&"installing Extracting".to_string())
    });

    queue.canceller().abandon();
    let begun = Instant::now();
    let outcomes = queue.wait_for(&[a]);
    drop(queue); // abandoned: does not join

    assert!(begun.elapsed() < Duration::from_secs(1));
    assert!(matches!(outcomes[0], JobOutcome::Cancelled));
    stuck.open();
}

#[test]
fn an_abandoned_queue_keeps_its_lock_until_its_detached_threads_finish() {
    let stages = FakeStages::new(u64::MAX);
    let stuck = Fake::hold(&stages.0.hold_install, "A");
    stages.0.ignore_cancel.lock().unwrap().insert("A".into());
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A extracting", || {
        events.of(a).contains(&"installing Extracting".to_string())
    });
    queue.canceller().abandon();
    queue.wait_for(&[a]);
    drop(queue); // abandoned: does not join, the install is still running

    let second = InstallQueue::start(stages.clone(), Arc::new(Events::default()));
    assert!(matches!(second, Err(ManagerError::QueueBusy { .. })));

    stuck.open();
    eventually("the detached threads release the lock", || {
        InstallQueue::start(stages.clone(), Arc::new(Events::default())).is_ok()
    });
}

#[test]
fn abandoning_still_waits_for_a_swap_in_progress() {
    let stages = FakeStages::new(u64::MAX);
    let swap = Fake::hold(&stages.0.hold_swap, "A");
    // Held so the job is still unfinished when the swap ends: abandoning
    // waits for the renames, not for the cleanup after them.
    let cleanup = Fake::hold(&stages.0.hold_after_swap, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A swapping", || {
        events.of(a).contains(&"installing Installing".to_string())
    });
    let opener = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        swap.open();
    });

    queue.canceller().abandon();
    let outcomes = queue.wait_for(&[a]);

    assert!(
        stages.0.logged("swapped A"),
        "returned before the swap ended"
    );
    assert!(
        matches!(outcomes[0], JobOutcome::Cancelled),
        "{:?}",
        outcomes[0]
    );
    opener.join().unwrap();
    cleanup.open();
}

#[test]
fn a_submit_after_abandoning_is_refused() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _) = start(&stages);

    queue.canceller().abandon();

    let refused = queue.submit(Job::Install(meta("A")));
    assert!(
        matches!(refused, Err(ManagerError::QueueClosed)),
        "{refused:?}"
    );
}

#[test]
fn a_batch_refuses_a_duplicate_per_package_and_installs_the_rest() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _) = start(&stages);

    let results = queue.install_batch(vec![meta("A"), meta("A"), meta("B")]);

    assert_eq!(results[0].as_ref().unwrap().region, "A");
    assert!(
        matches!(results[1], Err(ManagerError::AlreadyQueued { .. })),
        "{:?}",
        results[1]
    );
    assert_eq!(results[2].as_ref().unwrap().region, "B");
}

#[test]
fn abandoning_does_not_wait_for_the_cleanup_after_a_swap() {
    let stages = FakeStages::new(u64::MAX);
    let cleanup = Fake::hold(&stages.0.hold_after_swap, "A");
    let (queue, _) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A swapped", || stages.0.logged("swapped A"));

    assert_eq!(queue.cancel(a), CancelResult::TooLate, "still swapping");
    queue.canceller().abandon();
    let begun = Instant::now();
    let outcomes = queue.wait_for(&[a]);

    assert!(begun.elapsed() < Duration::from_secs(1));
    assert!(matches!(outcomes[0], JobOutcome::Cancelled));
    cleanup.open();
}

#[test]
fn an_install_that_never_ends_its_swap_clears_it_on_return() {
    let stages = FakeStages::new(u64::MAX);
    stages.0.skip_end_swap.lock().unwrap().insert("A".into());
    let (queue, _) = start(&stages);

    assert!(queue.install_batch(vec![meta("A")])[0].is_ok());

    assert!(!queue.shared.lock().swapping);
}

/// Sets up A installing (held) and B downloaded behind it, with dropping
/// B's payload held. Returns the queue, both ids and B's drop gate.
fn a_downloaded_payload_held_in_its_drop(
    stages: &FakeStages,
) -> (Arc<InstallQueue<FakeStages>>, [JobId; 2], OpenOnDrop) {
    Fake::hold(&stages.0.hold_install, "A");
    let gate = OpenOnDrop(Fake::hold(&stages.0.hold_drop, "B"));
    let (queue, events) = start(stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("B downloaded", || {
        events.of(b).contains(&"downloaded".to_string())
    });
    (Arc::new(queue), [a, b], gate)
}

/// Runs `wait_for` on its own thread. The receiver yields whether B's
/// payload had been dropped by the time it returned.
fn wait_on_a_thread(
    queue: &Arc<InstallQueue<FakeStages>>,
    stages: &FakeStages,
    ids: [JobId; 2],
) -> std::sync::mpsc::Receiver<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    let queue = Arc::clone(queue);
    let stages = stages.clone();
    std::thread::spawn(move || {
        let outcomes = queue.wait_for(&ids);
        assert!(outcomes.iter().all(|o| matches!(o, JobOutcome::Cancelled)));
        let _ = tx.send(stages.0.logged("dropped B"));
    });
    rx
}

/// The first Ctrl+C must clean temp: a cancelled job whose download was
/// waiting to install finishes only once its temp tree is deleted.
#[test]
fn cancelling_waits_for_a_downloaded_package_to_be_deleted() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, ids, gate) = a_downloaded_payload_held_in_its_drop(&stages);
    let canceller = queue.canceller();
    let cancelling = std::thread::spawn(move || canceller.cancel_all());
    eventually("B's payload is being dropped", || {
        stages.0.logged("dropping B")
    });

    let waited = wait_on_a_thread(&queue, &stages, ids);

    assert!(
        waited.recv_timeout(Duration::from_millis(200)).is_err(),
        "wait_for returned before the temp tree was deleted"
    );
    drop(gate);
    assert!(
        waited.recv_timeout(Duration::from_secs(5)).unwrap(),
        "wait_for returned before the payload was dropped"
    );
    cancelling.join().unwrap();
}

/// A second Ctrl+C quits at once: abandoning does not wait for the delete.
#[test]
fn abandoning_does_not_wait_for_a_downloaded_package_to_be_deleted() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, ids, gate) = a_downloaded_payload_held_in_its_drop(&stages);
    let canceller = queue.canceller();
    let abandoning = std::thread::spawn(move || canceller.abandon());
    eventually("B's payload is being dropped", || {
        stages.0.logged("dropping B")
    });

    let waited = wait_on_a_thread(&queue, &stages, ids);

    assert!(
        !waited.recv_timeout(Duration::from_secs(5)).unwrap(),
        "an abandoned wait_for returns while the delete still runs"
    );
    drop(gate);
    abandoning.join().unwrap();
}

/// Abandoning returns to its caller at once: the delete runs elsewhere, so
/// a UI that abandons keeps drawing and can leave.
#[test]
fn abandon_returns_while_a_downloaded_package_is_still_being_deleted() {
    let stages = FakeStages::new(u64::MAX);
    let (queue, _ids, gate) = a_downloaded_payload_held_in_its_drop(&stages);
    let canceller = queue.canceller();
    let (tx, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        canceller.abandon();
        let _ = tx.send(());
    });

    assert!(
        returned.recv_timeout(Duration::from_secs(5)).is_ok(),
        "abandon waited for the delete"
    );
    eventually("B's payload is being dropped", || {
        stages.0.logged("dropping B")
    });
    assert!(!stages.0.logged("dropped B"), "the delete is still held");
    drop(gate);
    eventually("B's payload dropped", || stages.0.logged("dropped B"));
}

/// The same holds for a job cancelled while it waits for space: its
/// prepared package owns a temp directory too.
#[test]
fn cancelling_a_download_waiting_for_space_waits_for_its_temp_directory() {
    let stages = FakeStages::split(99, 10_000);
    stages.size("A", 50).size("B", 50);
    Fake::hold(&stages.0.hold_install, "A");
    let held_drop = Fake::hold(&stages.0.hold_drop, "B");
    let (queue, events) = start(&stages);
    let queue = Arc::new(queue);
    // Declared after the queue, so on a failure it opens before the queue's
    // drop joins the downloader held at it.
    let gate = OpenOnDrop(held_drop);
    queue.submit(Job::Install(meta("A"))).unwrap();
    let b = queue.submit(Job::Install(meta("B"))).unwrap();
    eventually("B waiting", || {
        events.of(b).contains(&"waiting".to_string())
    });

    assert_eq!(queue.cancel(b), CancelResult::Cancelled);
    eventually("B's payload is being dropped", || {
        stages.0.logged("dropping B")
    });
    let (tx, waited) = std::sync::mpsc::channel();
    let waiter = Arc::clone(&queue);
    let logged = stages.clone();
    std::thread::spawn(move || {
        waiter.wait_for(&[b]);
        let _ = tx.send(logged.0.logged("dropped B"));
    });

    assert!(
        waited.recv_timeout(Duration::from_millis(200)).is_err(),
        "wait_for returned before the temp directory was deleted"
    );
    drop(gate);
    assert!(waited.recv_timeout(Duration::from_secs(5)).unwrap());
}

/// The store keys packages by folder name, which ignores case, so the
/// duplicate guard must too.
#[test]
fn a_job_for_the_same_package_in_another_case_is_refused() {
    let stages = FakeStages::new(u64::MAX);
    let _held = OpenOnDrop(Fake::hold(&stages.0.hold_download, "eu"));
    let (queue, _) = start(&stages);
    queue.submit(Job::Install(meta("eu"))).unwrap();

    let remove = queue.submit(Job::Remove {
        region: "EU".into(),
        package_type: PackageType::Ortho,
    });

    assert!(
        matches!(remove, Err(ManagerError::AlreadyQueued { .. })),
        "{remove:?}"
    );
}

/// Reading the free space can block on a slow filesystem, so it happens
/// outside the state lock: the queue still takes work meanwhile.
#[test]
fn reading_the_free_space_does_not_hold_the_queue() {
    let stages = FakeStages::new(u64::MAX);
    let read = Arc::new(Gate::default());
    *stages.0.hold_free_read.lock().unwrap() = Some(Arc::clone(&read));
    let (queue, _) = start(&stages);
    let queue = Arc::new(queue);
    // Declared after the queue, so on a failure it opens first.
    let read = OpenOnDrop(read);
    queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("the free space is being read", || {
        stages.0.free_reads.load(Ordering::SeqCst) == 1
    });

    let (tx, submitted) = std::sync::mpsc::channel();
    let submitter = Arc::clone(&queue);
    std::thread::spawn(move || {
        let _ = tx.send(submitter.submit(Job::Install(meta("B"))).is_ok());
    });

    assert!(
        submitted.recv_timeout(Duration::from_secs(5)).unwrap(),
        "submit waited for the free-space read"
    );
    drop(read);
    queue.wait_idle();
    assert!(stages.0.logged("installed B"));
    assert_eq!(stages.0.free_reads.load(Ordering::SeqCst), 1);
}

fn remove_eu() -> Job {
    Job::Remove {
        region: "EU".into(),
        package_type: PackageType::Ortho,
    }
}

#[test]
fn cancelling_a_queued_remove_removes_nothing() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_install, "A");
    let (queue, events) = start(&stages);
    let a = queue.submit(Job::Install(meta("A"))).unwrap();
    eventually("A extracting", || {
        events.of(a).contains(&"installing Extracting".to_string())
    });
    let removal = queue.submit(remove_eu()).unwrap();

    assert_eq!(queue.cancel(removal), CancelResult::Cancelled);

    held.open();
    queue.wait_idle();
    assert!(!stages.0.logged("removed EU"));
    assert_eq!(events.of(removal), ["queued EU", "finished cancelled"]);
}

#[test]
fn a_started_remove_cannot_be_cancelled_and_completes() {
    let stages = FakeStages::new(u64::MAX);
    let held = Fake::hold(&stages.0.hold_remove, "EU");
    let (queue, events) = start(&stages);
    // Declared after the queue, so on a failure it opens before the
    // queue's drop joins the installer held at it.
    let held = OpenOnDrop(held);
    let removal = queue.submit(remove_eu()).unwrap();
    eventually("EU removing", || {
        events.of(removal).contains(&"removing".to_string())
    });

    assert_eq!(queue.cancel(removal), CancelResult::TooLate);

    drop(held);
    assert!(matches!(queue.wait_for(&[removal])[0], JobOutcome::Removed));
    assert!(stages.0.logged("removed EU"));
}
