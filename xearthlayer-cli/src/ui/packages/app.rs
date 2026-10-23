//! Package manager state and its pure update.
//!
//! [`AppState::update`] takes a key's [`Action`] or a runner's [`Msg`] and
//! returns the [`Effect`]s for the runner to carry out. Nothing here touches
//! the terminal, the network or the disk, so every rule is tested directly.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use semver::Version;
use xearthlayer::manager::{JobId, JobKind, JobSummary};
use xearthlayer_package::{LibraryEntry, PackageLibrary, PackageMetadata, PackageType};

use crate::commands::packages::actions::{
    plan_install, LibraryRow, LibraryStatus, RunActive, RESTART_NOTE,
};
use crate::commands::packages::progress::label;
use crate::ui::dashboard::state::QUIT_CONFIRM_TIMEOUT;

/// Rows moved by `PageUp` and `PageDown`.
const PAGE_ROWS: usize = 10;

/// A package as the Local and Library tabs identify it: region and type.
type PackageKey = (String, PackageType);

// ============================================================================
// Data
// ============================================================================

/// The three tabs, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Local,
    Library,
    Progress,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Local, Tab::Library, Tab::Progress];

    fn index(self) -> usize {
        self as usize
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One installed package.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired by the runner in Task 5
pub struct LocalRow {
    pub region: String,
    pub package_type: PackageType,
    pub version: Version,
    pub size_bytes: u64,
    pub path: PathBuf,
    pub published_at: DateTime<Utc>,
    /// The library's newer version, if it has one.
    pub update: Option<Version>,
}

impl LocalRow {
    fn key(&self) -> PackageKey {
        (self.region.clone(), self.package_type)
    }
}

/// What the Library tab has to show.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired by the runner in Task 5
pub enum LibraryState {
    Loading,
    Loaded(Vec<LibraryRow>),
    Failed(String),
    /// No `packages.library_url` is configured.
    NotConfigured,
}

/// Which package types the Library tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFilter {
    All,
    Ortho,
    Overlay,
}

impl TypeFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Ortho,
            Self::Ortho => Self::Overlay,
            Self::Overlay => Self::All,
        }
    }

    fn shows(self, package_type: PackageType) -> bool {
        match self {
            Self::All => true,
            Self::Ortho => package_type == PackageType::Ortho,
            Self::Overlay => package_type == PackageType::Overlay,
        }
    }
}

/// How a job's row looks on the Progress tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobLook {
    Queued,
    Preparing,
    Waiting {
        needed: u64,
        available: u64,
    },
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    Downloaded,
    /// Extracting, installing or removing.
    Working(&'static str),
    Done,
    Failed(String),
    Cancelled,
}

/// One job on the Progress tab.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired by the runner in Task 5
pub struct JobRow {
    pub id: JobId,
    /// "NA-CANADA ortho v2.1.0", as the console shows it.
    pub label: String,
    pub kind: JobKind,
    pub look: JobLook,
    pub finished: bool,
}

/// A dialog over the tabs. While one is open, only its own keys apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    Help,
    ConfirmQuit {
        since: Instant,
        /// Jobs that quitting would cancel.
        running: usize,
    },
    ConfirmDelete {
        targets: Vec<PackageKey>,
        total_bytes: u64,
    },
    ConfirmInstall {
        /// Everything that will install, overlays included.
        entries: Vec<LibraryEntry>,
        /// Selected packages left out because they are already current.
        skipped: Vec<String>,
    },
    ConfirmCancelAll,
}

/// What the runner tells the state.
#[derive(Debug)]
#[allow(dead_code)] // wired by the runner in Task 5
pub enum Msg {
    Installed(Result<Vec<LocalRow>, String>),
    Library(Result<Vec<LibraryRow>, String>),
    LibraryNotConfigured,
    /// The pid of a running `xearthlayer run`, if any.
    RunActive(Option<u32>),
    /// One result per entry of an [`Effect::Install`]. Each error names its
    /// package: "NA-CANADA ortho: connection refused".
    MetadataReady(Vec<Result<PackageMetadata, String>>),
    /// The queue accepted this job. Sent from the queue's own `Queued`
    /// event, so it arrives before any other message for the job.
    Queued(JobId, JobSummary),
    /// `submit_batch` returned: the request was accepted, and these of its
    /// packages were refused, each naming its package.
    Submitted {
        refused: Vec<String>,
    },
    /// The queue would not start, and why.
    QueueRefused(String),
    Job(JobId, JobMsg),
    Status(String),
}

/// A job's progress, as an owned copy of the queue's event.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired by the runner in Task 5
pub enum JobMsg {
    Preparing,
    Waiting {
        needed: u64,
        available: u64,
    },
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    Downloaded,
    /// An install stage's name.
    Stage(&'static str),
    Removing,
    Installed,
    Removed,
    Cancelled,
    Failed(String),
}

/// What a key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NextTab,
    PrevTab,
    GoTab(Tab),
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Mark,
    MarkAll,
    CycleFilter,
    Update,
    Delete,
    Install,
    Cancel,
    CancelAll,
    ClearFinished,
    Refresh,
    Help,
    Quit,
    Confirm,
    Dismiss,
}

/// Everything `update` reacts to.
#[derive(Debug)]
#[allow(dead_code)] // wired by the runner in Task 5
pub enum Input {
    Action(Action),
    Msg(Msg),
}

/// Work for the runner.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    LoadInstalled,
    LoadLibrary,
    CheckRun,
    /// Fetch these entries' metadata, answered by [`Msg::MetadataReady`].
    Install(Vec<LibraryEntry>),
    /// Start the queue if needed and submit these installs as one batch.
    Submit(Vec<PackageMetadata>),
    /// Start the queue if needed and submit these removals as one batch.
    Remove(Vec<PackageKey>),
    CancelJob(JobId),
    CancelAll,
    /// Leave the running jobs to the queue's own shutdown.
    Abandon,
    RebuildOverlays,
    /// The queue is idle: drop it and its lock.
    ReleaseQueue,
    Exit,
}

/// How far quitting has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quitting {
    No,
    /// Jobs are being cancelled; exit once they finish.
    Cancelling,
    /// The user would not wait for the cancellations.
    Abandoned,
}

/// How the session's jobs ended, for the summary printed after exit.
#[derive(Debug, Default)]
struct Tally {
    installed: usize,
    removed: usize,
    failed: usize,
    cancelled: usize,
}

// ============================================================================
// State
// ============================================================================

/// The package manager's whole state.
#[derive(Debug)]
pub struct AppState {
    tab: Tab,
    local: Vec<LocalRow>,
    library: LibraryState,
    filter: TypeFilter,
    jobs: Vec<JobRow>,
    /// The selected row of each tab, an index into the rows it shows.
    selection: [usize; 3],
    local_marks: BTreeSet<PackageKey>,
    library_marks: BTreeSet<PackageKey>,
    /// Marks cleared by a confirmed action, kept until the queue answers
    /// so a refusal can put them back.
    /// Only one package request is in flight at a time: while this is set,
    /// new ones are refused.
    held_marks: Option<(Tab, BTreeSet<PackageKey>)>,
    /// Jobs queued since the request in flight was made.
    queued_count: usize,
    modal: Option<Modal>,
    status: Option<String>,
    run_active: Option<u32>,
    quitting: Quitting,
    /// An overlay was installed or removed since the queue was last idle.
    overlay_changed: bool,
    /// The jobs that install or remove an overlay.
    overlay_jobs: HashSet<JobId>,
    auto_install_overlays: bool,
    tally: Tally,
}

impl AppState {
    /// A state with nothing loaded, and the effects that load it.
    #[allow(dead_code)] // wired by the runner in Task 5
    pub fn new(auto_install_overlays: bool) -> (Self, Vec<Effect>) {
        let state = Self {
            tab: Tab::Local,
            local: Vec::new(),
            library: LibraryState::Loading,
            filter: TypeFilter::All,
            jobs: Vec::new(),
            selection: [0; 3],
            local_marks: BTreeSet::new(),
            library_marks: BTreeSet::new(),
            held_marks: None,
            queued_count: 0,
            modal: None,
            status: None,
            run_active: None,
            quitting: Quitting::No,
            overlay_changed: false,
            overlay_jobs: HashSet::new(),
            auto_install_overlays,
            tally: Tally::default(),
        };
        (state, reload())
    }

    /// Apply one input, returning the work it calls for.
    #[allow(dead_code)] // wired by the runner in Task 5
    pub fn update(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        if let Some(Modal::ConfirmQuit { since, .. }) = self.modal {
            if now.saturating_duration_since(since) > QUIT_CONFIRM_TIMEOUT {
                self.modal = None;
            }
        }
        match input {
            Input::Action(action) => self.on_action(action, now),
            Input::Msg(msg) => self.on_msg(msg),
        }
    }

    /// Jobs queued and not yet finished.
    pub fn unfinished(&self) -> usize {
        self.jobs.iter().filter(|job| !job.finished).count()
    }

    /// One line on what the session did, printed after exit.
    #[allow(dead_code)] // wired by the runner in Task 5
    pub fn summary(&self) -> String {
        let unfinished = self.unfinished();
        let parts: Vec<String> = [
            (self.tally.installed, "installed"),
            (self.tally.removed, "removed"),
            (self.tally.failed, "failed"),
            (self.tally.cancelled, "cancelled"),
            (unfinished, "unfinished"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
        if parts.is_empty() {
            "Package manager: no packages changed".to_string()
        } else {
            format!("Package manager: {}", parts.join(", "))
        }
    }

    /// The selected row of the current tab.
    pub fn selected(&self) -> usize {
        self.selection[self.tab.index()]
    }

    // ------------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------------

    fn on_action(&mut self, action: Action, now: Instant) -> Vec<Effect> {
        if action == Action::Quit {
            return self.on_quit(now);
        }
        if self.modal.is_some() {
            return self.on_modal_action(action);
        }
        let tab = self.tab;
        match (action, tab) {
            (Action::NextTab, _) => self.tab = tab.next(),
            (Action::PrevTab, _) => self.tab = tab.prev(),
            (Action::GoTab(to), _) => self.tab = to,
            (Action::Up, _) => self.move_by(-1),
            (Action::Down, _) => self.move_by(1),
            (Action::PageUp, _) => self.move_by(-(PAGE_ROWS as isize)),
            (Action::PageDown, _) => self.move_by(PAGE_ROWS as isize),
            (Action::Home, _) => self.move_by(isize::MIN),
            (Action::End, _) => self.move_by(isize::MAX),
            (Action::Mark, Tab::Local | Tab::Library) => self.toggle_mark(),
            (Action::MarkAll, Tab::Local | Tab::Library) => self.toggle_all_marks(),
            (Action::CycleFilter, Tab::Library) => {
                self.filter = self.filter.next();
                self.clamp(Tab::Library);
            }
            (Action::Update, Tab::Local) => return self.update_local(),
            (Action::Delete, Tab::Local) => self.delete_local(),
            (Action::Install, Tab::Library) => self.install_from_library(),
            (Action::Cancel, Tab::Progress) => {
                if let Some(job) = self.jobs.get(self.selected()) {
                    if !job.finished {
                        return vec![Effect::CancelJob(job.id)];
                    }
                }
            }
            (Action::CancelAll, Tab::Progress) => {
                if self.unfinished() > 0 {
                    self.modal = Some(Modal::ConfirmCancelAll);
                } else {
                    self.status = Some("No jobs to cancel".to_string());
                }
            }
            (Action::ClearFinished, Tab::Progress) => {
                self.jobs.retain(|job| !job.finished);
                let jobs = &self.jobs;
                self.overlay_jobs
                    .retain(|id| jobs.iter().any(|job| job.id == *id));
                self.clamp(Tab::Progress);
            }
            (Action::Refresh, _) => {
                self.library = LibraryState::Loading;
                return reload();
            }
            (Action::Help, _) => self.modal = Some(Modal::Help),
            // Esc with nothing to dismiss asks to quit, but never abandons.
            (Action::Dismiss, _) if self.quitting == Quitting::No => {
                return self.on_quit(now);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Keys under an open modal: only its own apply.
    fn on_modal_action(&mut self, action: Action) -> Vec<Effect> {
        match (action, &self.modal) {
            (Action::Dismiss, _) | (Action::Help, Some(Modal::Help)) => self.modal = None,
            (Action::Confirm, Some(Modal::Help)) => {}
            (Action::Confirm, _) => return self.confirm(),
            _ => {}
        }
        Vec::new()
    }

    fn confirm(&mut self) -> Vec<Effect> {
        match self.modal.take() {
            Some(Modal::ConfirmQuit { .. }) => self.quit_now(),
            Some(Modal::ConfirmDelete { .. } | Modal::ConfirmInstall { .. })
                if self.request_refused() =>
            {
                Vec::new()
            }
            Some(Modal::ConfirmDelete { targets, .. }) => {
                self.hold_marks(Tab::Local);
                vec![Effect::Remove(targets)]
            }
            Some(Modal::ConfirmInstall { entries, .. }) => {
                self.hold_marks(Tab::Library);
                vec![Effect::Install(entries)]
            }
            Some(Modal::ConfirmCancelAll) => vec![Effect::CancelAll],
            other => {
                self.modal = other;
                Vec::new()
            }
        }
    }

    fn on_quit(&mut self, now: Instant) -> Vec<Effect> {
        if self.quitting == Quitting::Cancelling {
            self.quitting = Quitting::Abandoned;
            return vec![Effect::Abandon, Effect::Exit];
        }
        if let Some(Modal::ConfirmQuit { .. }) = self.modal {
            self.modal = None;
            return self.quit_now();
        }
        self.modal = Some(Modal::ConfirmQuit {
            since: now,
            running: self.unfinished(),
        });
        Vec::new()
    }

    /// Quitting was confirmed: exit, or cancel the running jobs first.
    fn quit_now(&mut self) -> Vec<Effect> {
        if self.unfinished() == 0 {
            return vec![Effect::Exit];
        }
        self.quitting = Quitting::Cancelling;
        self.status = Some("Cancelling, press q again to quit now".to_string());
        vec![Effect::CancelAll]
    }

    // ------------------------------------------------------------------------
    // Selection and marks
    // ------------------------------------------------------------------------

    /// The library rows the filter shows.
    fn shown_library(&self) -> Vec<&LibraryRow> {
        match &self.library {
            LibraryState::Loaded(rows) => rows
                .iter()
                .filter(|row| self.filter.shows(row.entry.package_type))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The packages a tab shows, in order. Progress has none.
    fn shown_keys(&self, tab: Tab) -> Vec<PackageKey> {
        match tab {
            Tab::Local => self.local.iter().map(LocalRow::key).collect(),
            Tab::Library => self
                .shown_library()
                .iter()
                .map(|row| (row.entry.title.clone(), row.entry.package_type))
                .collect(),
            Tab::Progress => Vec::new(),
        }
    }

    fn shown_len(&self, tab: Tab) -> usize {
        match tab {
            Tab::Local => self.local.len(),
            Tab::Library => self.shown_library().len(),
            Tab::Progress => self.jobs.len(),
        }
    }

    fn marks_mut(&mut self, tab: Tab) -> Option<&mut BTreeSet<PackageKey>> {
        match tab {
            Tab::Local => Some(&mut self.local_marks),
            Tab::Library => Some(&mut self.library_marks),
            Tab::Progress => None,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.shown_len(self.tab).saturating_sub(1);
        let selected = &mut self.selection[self.tab.index()];
        *selected = selected.saturating_add_signed(delta).min(last);
    }

    fn toggle_mark(&mut self) {
        let tab = self.tab;
        let Some(key) = self.shown_keys(tab).into_iter().nth(self.selected()) else {
            return;
        };
        let marks = self.marks_mut(tab).expect("Local and Library have marks");
        if !marks.remove(&key) {
            marks.insert(key);
        }
    }

    /// Mark every shown row, or unmark them all if they all are.
    fn toggle_all_marks(&mut self) {
        let tab = self.tab;
        let keys = self.shown_keys(tab);
        let marks = self.marks_mut(tab).expect("Local and Library have marks");
        if keys.iter().all(|key| marks.contains(key)) {
            marks.clear();
        } else {
            marks.extend(keys);
        }
    }

    /// Keep a tab's selection and marks within the rows it shows.
    fn clamp(&mut self, tab: Tab) {
        // Marks wait for the rows to come back rather than going with them.
        if tab == Tab::Library && !matches!(self.library, LibraryState::Loaded(_)) {
            return;
        }
        let last = self.shown_len(tab).saturating_sub(1);
        let selected = &mut self.selection[tab.index()];
        *selected = (*selected).min(last);
        let keys: HashSet<PackageKey> = self.shown_keys(tab).into_iter().collect();
        if let Some(marks) = self.marks_mut(tab) {
            marks.retain(|key| keys.contains(key));
        }
    }

    /// The marked rows a tab shows, in order, else its selected row.
    fn targets(&self, tab: Tab) -> Vec<PackageKey> {
        let keys = self.shown_keys(tab);
        let marks = match tab {
            Tab::Local => &self.local_marks,
            Tab::Library => &self.library_marks,
            Tab::Progress => return Vec::new(),
        };
        let marked: Vec<PackageKey> = keys
            .iter()
            .filter(|key| marks.contains(*key))
            .cloned()
            .collect();
        if marked.is_empty() {
            keys.into_iter()
                .nth(self.selection[tab.index()])
                .into_iter()
                .collect()
        } else {
            marked
        }
    }

    /// Clear a tab's marks, keeping a copy until the queue answers.
    fn hold_marks(&mut self, tab: Tab) {
        if let Some(marks) = self.marks_mut(tab) {
            let marks = std::mem::take(marks);
            self.held_marks = Some((tab, marks));
            self.queued_count = 0;
        }
    }

    /// Put back the marks the last confirmed action cleared.
    fn restore_marks(&mut self) {
        if let Some((tab, held)) = self.held_marks.take() {
            if let Some(marks) = self.marks_mut(tab) {
                marks.extend(held);
            }
            self.clamp(tab);
        }
    }

    // ------------------------------------------------------------------------
    // Package actions
    // ------------------------------------------------------------------------

    /// A new package request is refused while quitting, or while the last
    /// one waits for the queue, saying why.
    fn request_refused(&mut self) -> bool {
        let reason = if self.quitting != Quitting::No {
            "quitting: no new package requests"
        } else if self.held_marks.is_some() {
            "waiting for the queue to accept the last request"
        } else {
            return false;
        };
        self.status = Some(reason.to_string());
        true
    }

    /// `RunActive` refuses the action, saying why.
    fn refused_while_running(&mut self) -> bool {
        match self.run_active {
            Some(pid) => {
                self.status = Some(RunActive(pid).message());
                true
            }
            None => false,
        }
    }

    fn update_local(&mut self) -> Vec<Effect> {
        if self.request_refused() || self.refused_while_running() {
            return Vec::new();
        }
        let targets = self.targets(Tab::Local);
        let rows: Vec<&LocalRow> = targets
            .iter()
            .filter_map(|key| self.local.iter().find(|row| row.key() == *key))
            .collect();
        let (outdated, current): (Vec<&LocalRow>, Vec<&LocalRow>) =
            rows.into_iter().partition(|row| row.update.is_some());

        let mut notes = Vec::new();
        if !current.is_empty() {
            notes.push(format!(
                "{}: already up to date",
                names(current.iter().map(|row| row.key()))
            ));
        }
        let mut effects = Vec::new();
        if !outdated.is_empty() {
            let LibraryState::Loaded(library) = &self.library else {
                self.status = Some("library not loaded yet, press r".to_string());
                return Vec::new();
            };
            let (found, missing): (Vec<_>, Vec<_>) = outdated
                .iter()
                .map(|row| (row.key(), find_row(library, &row.region, row.package_type)))
                .partition(|(_, found)| found.is_some());
            if !missing.is_empty() {
                notes.push(format!(
                    "{}: not in the library",
                    names(missing.into_iter().map(|(key, _)| key))
                ));
            }
            let entries: Vec<LibraryEntry> = found
                .into_iter()
                .filter_map(|(_, row)| row.map(|row| row.entry.clone()))
                .collect();
            if !entries.is_empty() {
                effects.push(Effect::Install(entries));
            }
        }
        if !effects.is_empty() {
            self.hold_marks(Tab::Local);
        }
        if !notes.is_empty() {
            self.status = Some(notes.join("; "));
        }
        effects
    }

    fn delete_local(&mut self) {
        if self.request_refused() || self.refused_while_running() {
            return;
        }
        let targets = self.targets(Tab::Local);
        if targets.is_empty() {
            return;
        }
        let total_bytes = self
            .local
            .iter()
            .filter(|row| targets.contains(&row.key()))
            .map(|row| row.size_bytes)
            .sum();
        self.modal = Some(Modal::ConfirmDelete {
            targets,
            total_bytes,
        });
    }

    fn install_from_library(&mut self) {
        if self.request_refused() {
            return;
        }
        let LibraryState::Loaded(rows) = &self.library else {
            self.status = Some("library not loaded yet, press r".to_string());
            return;
        };
        let selected: Vec<&LibraryRow> = self
            .targets(Tab::Library)
            .iter()
            .filter_map(|(title, package_type)| find_row(rows, title, *package_type))
            .collect();
        if selected.is_empty() {
            return;
        }
        let (current, wanted): (Vec<&LibraryRow>, Vec<&LibraryRow>) = selected
            .into_iter()
            .partition(|row| row.status == LibraryStatus::UpToDate);
        let skipped: Vec<String> = current.iter().map(|row| entry_name(&row.entry)).collect();

        // The overlay pairing is `plan_install`'s, over the whole library.
        let library = PackageLibrary {
            entries: rows.iter().map(|row| row.entry.clone()).collect(),
            ..PackageLibrary::new()
        };
        let wanted: Vec<&LibraryEntry> = wanted.iter().map(|row| &row.entry).collect();
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut refused = Vec::new();
        for entry in plan_install(&wanted, &library, self.auto_install_overlays) {
            if !seen.insert((entry.title.clone(), entry.package_type)) {
                continue;
            }
            match find_row(rows, &entry.title, entry.package_type).map(|row| &row.status) {
                // A following overlay already current is left out quietly.
                Some(LibraryStatus::UpToDate) => {}
                Some(LibraryStatus::Update { .. }) if self.run_active.is_some() => {
                    refused.push(entry_name(entry));
                }
                _ => entries.push(entry.clone()),
            }
        }

        let mut notes = Vec::new();
        if let (Some(pid), false) = (self.run_active, refused.is_empty()) {
            notes.push(format!(
                "{} left out. {}",
                refused.join(", "),
                RunActive(pid).message()
            ));
        }
        if entries.is_empty() {
            if !skipped.is_empty() {
                notes.push(format!("{}: already up to date", skipped.join(", ")));
            }
        } else {
            if self.run_active.is_some() {
                notes.push(format!("New packages: {RESTART_NOTE}"));
            }
            self.modal = Some(Modal::ConfirmInstall { entries, skipped });
        }
        if !notes.is_empty() {
            self.status = Some(notes.join("; "));
        }
    }

    // ------------------------------------------------------------------------
    // Messages
    // ------------------------------------------------------------------------

    fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Installed(Ok(rows)) => {
                if rows.is_empty() {
                    self.status =
                        Some("Nothing installed yet: press 2 for the Library".to_string());
                }
                self.local = rows;
                self.clamp(Tab::Local);
            }
            Msg::Installed(Err(e)) => {
                self.status = Some(format!("could not read the installed packages: {e}"));
            }
            Msg::Library(Ok(rows)) => self.set_library(LibraryState::Loaded(rows)),
            Msg::Library(Err(e)) => self.set_library(LibraryState::Failed(e)),
            Msg::LibraryNotConfigured => self.set_library(LibraryState::NotConfigured),
            Msg::RunActive(pid) => self.run_active = pid,
            Msg::MetadataReady(results) => return self.on_metadata(results),
            Msg::Queued(id, summary) => return self.on_queued(id, summary),
            Msg::Submitted { refused } => return self.on_submitted(refused),
            Msg::QueueRefused(reason) => {
                self.status = Some(reason);
                self.restore_marks();
                if self.unfinished() == 0 {
                    return vec![Effect::ReleaseQueue];
                }
            }
            Msg::Job(id, job_msg) => return self.on_job(id, job_msg),
            Msg::Status(text) => self.status = Some(text),
        }
        Vec::new()
    }

    fn set_library(&mut self, library: LibraryState) {
        self.library = library;
        self.clamp(Tab::Library);
    }

    fn on_metadata(&mut self, results: Vec<Result<PackageMetadata, String>>) -> Vec<Effect> {
        if self.quitting != Quitting::No {
            self.status = Some("cancelled".to_string());
            self.restore_marks();
            return Vec::new();
        }
        let (fetched, failed): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        let fetched: Vec<PackageMetadata> = fetched.into_iter().filter_map(Result::ok).collect();
        let failed: Vec<String> = failed.into_iter().filter_map(Result::err).collect();
        if !failed.is_empty() {
            self.status = Some(
                failed
                    .iter()
                    .map(|e| format!("could not fetch metadata for {e}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        if fetched.is_empty() {
            self.restore_marks();
            return Vec::new();
        }
        vec![Effect::Submit(fetched)]
    }

    fn on_queued(&mut self, id: JobId, summary: JobSummary) -> Vec<Effect> {
        self.queued_count += 1;
        self.status = Some(self.queued_note());
        if summary.package_type == PackageType::Overlay {
            self.overlay_jobs.insert(id);
        }
        self.jobs.push(JobRow {
            id,
            label: label(&summary),
            kind: summary.kind,
            look: JobLook::Queued,
            finished: false,
        });
        // Quitting already cancelled everything else.
        if self.quitting != Quitting::No {
            return vec![Effect::CancelJob(id)];
        }
        Vec::new()
    }

    /// The request in flight was accepted: release its marks and report
    /// what was queued and what was refused.
    fn on_submitted(&mut self, refused: Vec<String>) -> Vec<Effect> {
        self.held_marks = None;
        let mut notes = Vec::new();
        if self.queued_count > 0 {
            notes.push(self.queued_note());
        }
        notes.extend(refused);
        if !notes.is_empty() {
            self.status = Some(notes.join("; "));
        }
        self.queued_count = 0;
        if self.unfinished() == 0 {
            return vec![Effect::ReleaseQueue];
        }
        Vec::new()
    }

    /// "N jobs queued: see Progress"
    fn queued_note(&self) -> String {
        match self.queued_count {
            1 => "1 job queued: see Progress".to_string(),
            n => format!("{n} jobs queued: see Progress"),
        }
    }

    fn on_job(&mut self, id: JobId, msg: JobMsg) -> Vec<Effect> {
        let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
            tracing::debug!(job = id.0, "Package manager: message for an unknown job");
            return Vec::new();
        };
        if job.finished {
            return Vec::new();
        }
        let (look, finished) = match msg {
            JobMsg::Preparing => (JobLook::Preparing, false),
            JobMsg::Waiting { needed, available } => {
                (JobLook::Waiting { needed, available }, false)
            }
            JobMsg::Downloading { done, total } => (JobLook::Downloading { done, total }, false),
            JobMsg::Downloaded => (JobLook::Downloaded, false),
            JobMsg::Stage(stage) => (JobLook::Working(stage), false),
            JobMsg::Removing => (JobLook::Working("Removing"), false),
            JobMsg::Installed => {
                self.tally.installed += 1;
                (JobLook::Done, true)
            }
            JobMsg::Removed => {
                self.tally.removed += 1;
                (JobLook::Done, true)
            }
            JobMsg::Cancelled => {
                self.tally.cancelled += 1;
                (JobLook::Cancelled, true)
            }
            JobMsg::Failed(e) => {
                self.tally.failed += 1;
                (JobLook::Failed(e), true)
            }
        };
        let changed_files = look == JobLook::Done;
        job.look = look;
        if !finished {
            return Vec::new();
        }
        job.finished = true;
        if changed_files && self.overlay_jobs.contains(&id) {
            self.overlay_changed = true;
        }

        let mut effects = reload();
        if self.unfinished() == 0 {
            if std::mem::take(&mut self.overlay_changed) {
                effects.push(Effect::RebuildOverlays);
            }
            effects.push(Effect::ReleaseQueue);
            if self.quitting == Quitting::Cancelling {
                effects.push(Effect::Exit);
            }
        }
        effects
    }
}

/// Reload everything a finished job or a refresh may have changed.
fn reload() -> Vec<Effect> {
    vec![Effect::LoadInstalled, Effect::LoadLibrary, Effect::CheckRun]
}

/// The library row for a package, matched as the library matches regions.
fn find_row<'a>(
    rows: &'a [LibraryRow],
    region: &str,
    package_type: PackageType,
) -> Option<&'a LibraryRow> {
    rows.iter().find(|row| {
        row.entry.title.eq_ignore_ascii_case(region) && row.entry.package_type == package_type
    })
}

/// "NA-CANADA ortho"
fn entry_name(entry: &LibraryEntry) -> String {
    format!("{} {}", entry.title, entry.package_type)
}

/// "EU-WEST ortho, NA-EAST overlay"
fn names(keys: impl Iterator<Item = PackageKey>) -> String {
    keys.map(|(region, package_type)| format!("{region} {package_type}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use chrono::{DateTime, Utc};
    use semver::Version;
    use xearthlayer::manager::{JobId, JobKind, JobSummary};
    use xearthlayer_package::{LibraryEntry, PackageMetadata, PackageType};

    use super::*;
    use crate::commands::packages::actions::{LibraryRow, LibraryStatus, RunActive, RESTART_NOTE};
    use crate::ui::dashboard::state::QUIT_CONFIRM_TIMEOUT;

    use PackageType::{Ortho, Overlay};

    // ------------------------------------------------------------------------
    // Fake rows
    // ------------------------------------------------------------------------

    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).unwrap()
    }

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    fn local(region: &str, t: PackageType, size: u64, update: Option<&str>) -> LocalRow {
        LocalRow {
            region: region.to_string(),
            package_type: t,
            version: v("1.0.0"),
            size_bytes: size,
            path: PathBuf::from(format!("/packages/{region}")),
            published_at: epoch(),
            update: update.map(v),
        }
    }

    fn entry(title: &str, t: PackageType, version: &str) -> LibraryEntry {
        LibraryEntry::new(
            "sum",
            t,
            title,
            v(version),
            format!("https://example.com/{title}"),
        )
    }

    fn lib(title: &str, t: PackageType, version: &str, status: LibraryStatus) -> LibraryRow {
        LibraryRow {
            entry: entry(title, t, version),
            status,
        }
    }

    /// Local: EU-WEST ortho (update to 1.1.0), EU-WEST overlay, NA-EAST ortho.
    fn fake_local() -> Vec<LocalRow> {
        vec![
            local("EU-WEST", Ortho, 100, Some("1.1.0")),
            local("EU-WEST", Overlay, 20, None),
            local("NA-EAST", Ortho, 300, None),
        ]
    }

    /// Library: AF-NORTH ortho and overlay (new), EU-WEST ortho (update) and
    /// overlay (current), NA-EAST ortho (current), SA-SOUTH overlay (new).
    fn fake_library() -> Vec<LibraryRow> {
        vec![
            lib("AF-NORTH", Ortho, "1.0.0", LibraryStatus::NotInstalled),
            lib("AF-NORTH", Overlay, "1.0.0", LibraryStatus::NotInstalled),
            lib(
                "EU-WEST",
                Ortho,
                "1.1.0",
                LibraryStatus::Update {
                    installed: v("1.0.0"),
                },
            ),
            lib("EU-WEST", Overlay, "1.0.0", LibraryStatus::UpToDate),
            lib("NA-EAST", Ortho, "1.0.0", LibraryStatus::UpToDate),
            lib("SA-SOUTH", Overlay, "1.0.0", LibraryStatus::NotInstalled),
        ]
    }

    /// A state holding the fake rows, auto-install of overlays on.
    fn loaded() -> AppState {
        loaded_with(true)
    }

    fn loaded_with(auto_install_overlays: bool) -> AppState {
        let (mut state, _) = AppState::new(auto_install_overlays);
        let now = Instant::now();
        state.update(Input::Msg(Msg::Installed(Ok(fake_local()))), now);
        state.update(Input::Msg(Msg::Library(Ok(fake_library()))), now);
        state.update(Input::Msg(Msg::RunActive(None)), now);
        state
    }

    fn act(state: &mut AppState, action: Action) -> Vec<Effect> {
        state.update(Input::Action(action), Instant::now())
    }

    fn msg(state: &mut AppState, msg: Msg) -> Vec<Effect> {
        state.update(Input::Msg(msg), Instant::now())
    }

    fn key(region: &str, t: PackageType) -> (String, PackageType) {
        (region.to_string(), t)
    }

    fn status(state: &AppState) -> &str {
        state.status.as_deref().unwrap_or("")
    }

    fn summary(region: &str, t: PackageType, version: Option<&str>) -> JobSummary {
        JobSummary {
            kind: if version.is_some() {
                JobKind::Install
            } else {
                JobKind::Remove
            },
            region: region.to_string(),
            package_type: t,
            version: version.map(v),
        }
    }

    /// Queue jobs 1.. for these packages and return their ids.
    fn queue(state: &mut AppState, jobs: &[(&str, PackageType, Option<&str>)]) -> Vec<JobId> {
        let first = state.jobs.len() as u64 + 1;
        jobs.iter()
            .enumerate()
            .map(|(i, (region, t, version))| {
                let id = JobId(first + i as u64);
                msg(state, Msg::Queued(id, summary(region, *t, *version)));
                id
            })
            .collect()
    }

    fn metadata(title: &str, t: PackageType) -> PackageMetadata {
        PackageMetadata {
            spec_version: v("1.0.0"),
            title: title.to_string(),
            package_version: v("1.0.0"),
            published_at: epoch(),
            package_type: t,
            mountpoint: format!("zzXEL_{title}"),
            filename: format!("{title}.tar.gz"),
            parts: Vec::new(),
        }
    }

    const RELOAD: [Effect; 3] = [Effect::LoadInstalled, Effect::LoadLibrary, Effect::CheckRun];

    // ------------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------------

    #[test]
    fn new_loads_the_installed_list_the_library_and_the_run_check() {
        let (state, effects) = AppState::new(true);

        assert_eq!(effects, RELOAD);
        assert_eq!(state.tab, Tab::Local);
        assert!(matches!(state.library, LibraryState::Loading));
        assert_eq!(state.unfinished(), 0);
    }

    // ------------------------------------------------------------------------
    // Movement
    // ------------------------------------------------------------------------

    #[test]
    fn up_and_down_move_the_selection_clamped() {
        let mut state = loaded();

        for _ in 0..5 {
            act(&mut state, Action::Down);
        }
        assert_eq!(state.selected(), 2);
        act(&mut state, Action::Up);
        assert_eq!(state.selected(), 1);
        for _ in 0..5 {
            act(&mut state, Action::Up);
        }
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn page_keys_home_and_end_move_the_selection_clamped() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));

        act(&mut state, Action::End);
        assert_eq!(state.selected(), 5);
        act(&mut state, Action::Home);
        assert_eq!(state.selected(), 0);
        act(&mut state, Action::PageDown);
        assert_eq!(state.selected(), 5);
        act(&mut state, Action::PageUp);
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn each_tab_keeps_its_own_selection() {
        let mut state = loaded();
        act(&mut state, Action::End);
        act(&mut state, Action::GoTab(Tab::Library));

        assert_eq!(state.selected(), 0);
        act(&mut state, Action::GoTab(Tab::Local));
        assert_eq!(state.selected(), 2);
    }

    #[test]
    fn moving_in_an_empty_tab_stays_at_the_top() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Progress));

        act(&mut state, Action::Down);
        act(&mut state, Action::End);

        assert_eq!(state.selected(), 0);
    }

    // ------------------------------------------------------------------------
    // Tabs
    // ------------------------------------------------------------------------

    #[test]
    fn next_and_prev_tab_cycle_local_library_progress() {
        let mut state = loaded();

        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Library);
        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Progress);
        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Local);
        act(&mut state, Action::PrevTab);
        assert_eq!(state.tab, Tab::Progress);
        act(&mut state, Action::PrevTab);
        assert_eq!(state.tab, Tab::Library);
    }

    #[test]
    fn go_tab_jumps_to_that_tab() {
        let mut state = loaded();

        act(&mut state, Action::GoTab(Tab::Progress));
        assert_eq!(state.tab, Tab::Progress);
        act(&mut state, Action::GoTab(Tab::Local));
        assert_eq!(state.tab, Tab::Local);
    }

    // ------------------------------------------------------------------------
    // Marks
    // ------------------------------------------------------------------------

    #[test]
    fn mark_toggles_the_selected_row() {
        let mut state = loaded();
        act(&mut state, Action::Down);

        act(&mut state, Action::Mark);
        assert_eq!(
            state.local_marks.iter().collect::<Vec<_>>(),
            [&key("EU-WEST", Overlay)]
        );
        act(&mut state, Action::Mark);
        assert!(state.local_marks.is_empty());
    }

    #[test]
    fn mark_on_the_library_marks_the_library_row() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));

        act(&mut state, Action::Mark);

        assert!(state.local_marks.is_empty());
        assert!(state.library_marks.contains(&key("AF-NORTH", Ortho)));
    }

    #[test]
    fn mark_does_nothing_on_progress() {
        let mut state = loaded();
        queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);
        act(&mut state, Action::GoTab(Tab::Progress));

        act(&mut state, Action::Mark);
        act(&mut state, Action::MarkAll);

        assert!(state.local_marks.is_empty());
        assert!(state.library_marks.is_empty());
    }

    #[test]
    fn mark_all_marks_every_shown_row_then_unmarks_them() {
        let mut state = loaded();
        act(&mut state, Action::Mark);

        act(&mut state, Action::MarkAll);
        assert_eq!(state.local_marks.len(), 3);
        act(&mut state, Action::MarkAll);
        assert!(state.local_marks.is_empty());
    }

    // ------------------------------------------------------------------------
    // Type filter
    // ------------------------------------------------------------------------

    #[test]
    fn cycle_filter_goes_all_ortho_overlay_on_the_library_only() {
        let mut state = loaded();
        act(&mut state, Action::CycleFilter);
        assert_eq!(state.filter, TypeFilter::All);

        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::CycleFilter);
        assert_eq!(state.filter, TypeFilter::Ortho);
        act(&mut state, Action::CycleFilter);
        assert_eq!(state.filter, TypeFilter::Overlay);
        act(&mut state, Action::CycleFilter);
        assert_eq!(state.filter, TypeFilter::All);
    }

    #[test]
    fn selection_and_marks_apply_to_the_filtered_rows() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::End);
        act(&mut state, Action::MarkAll);

        act(&mut state, Action::CycleFilter);

        // The filter hides the overlays: their marks go and the selection
        // is clamped to the three ortho rows.
        assert_eq!(state.selected(), 2);
        assert_eq!(
            state.library_marks.iter().cloned().collect::<Vec<_>>(),
            [
                key("AF-NORTH", Ortho),
                key("EU-WEST", Ortho),
                key("NA-EAST", Ortho)
            ]
        );
        act(&mut state, Action::Home);
        act(&mut state, Action::Down);
        act(&mut state, Action::Mark);
        assert!(!state.library_marks.contains(&key("EU-WEST", Ortho)));
    }

    // ------------------------------------------------------------------------
    // Local: update
    // ------------------------------------------------------------------------

    #[test]
    fn local_update_is_refused_while_run_is_active() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(42)));

        let effects = act(&mut state, Action::Update);

        assert!(effects.is_empty());
        assert_eq!(status(&state), RunActive(42).message());
    }

    #[test]
    fn local_update_skips_a_package_already_up_to_date() {
        let mut state = loaded();
        act(&mut state, Action::End);

        let effects = act(&mut state, Action::Update);

        assert!(effects.is_empty());
        assert!(
            status(&state).contains("NA-EAST ortho: already up to date"),
            "{}",
            status(&state)
        );
    }

    #[test]
    fn local_update_installs_the_matching_library_entries() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::End);
        act(&mut state, Action::Mark);

        let effects = act(&mut state, Action::Update);

        assert_eq!(
            effects,
            [Effect::Install(vec![entry("EU-WEST", Ortho, "1.1.0")])]
        );
        assert!(state.local_marks.is_empty());
        assert!(
            status(&state).contains("NA-EAST ortho: already up to date"),
            "{}",
            status(&state)
        );
    }

    #[test]
    fn local_update_without_the_library_asks_for_a_refresh() {
        let (mut state, _) = AppState::new(true);
        msg(&mut state, Msg::Installed(Ok(fake_local())));

        let effects = act(&mut state, Action::Update);

        assert!(effects.is_empty());
        assert_eq!(status(&state), "library not loaded yet, press r");
    }

    #[test]
    fn queuing_says_where_to_look_without_switching_tabs() {
        let mut state = loaded();

        queue(
            &mut state,
            &[("EU-WEST", Ortho, Some("1.1.0")), ("NA", Ortho, None)],
        );

        assert_eq!(status(&state), "2 jobs queued: see Progress");
        assert_eq!(state.tab, Tab::Local);
    }

    // ------------------------------------------------------------------------
    // Local: delete
    // ------------------------------------------------------------------------

    #[test]
    fn local_delete_is_refused_while_run_is_active() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(42)));

        let effects = act(&mut state, Action::Delete);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
        assert_eq!(status(&state), RunActive(42).message());
    }

    #[test]
    fn local_delete_confirms_with_the_total_size_then_removes() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::Down);
        act(&mut state, Action::Mark);
        let targets = vec![key("EU-WEST", Ortho), key("EU-WEST", Overlay)];

        assert!(act(&mut state, Action::Delete).is_empty());
        assert_eq!(
            state.modal,
            Some(Modal::ConfirmDelete {
                targets: targets.clone(),
                total_bytes: 120
            })
        );

        let effects = act(&mut state, Action::Confirm);

        assert_eq!(effects, [Effect::Remove(targets)]);
        assert!(state.modal.is_none());
        assert!(state.local_marks.is_empty());
    }

    #[test]
    fn dismiss_closes_a_confirmation_without_acting() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::Delete);

        let effects = act(&mut state, Action::Dismiss);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
        assert_eq!(state.local_marks.len(), 1);
    }

    // ------------------------------------------------------------------------
    // Library: install
    // ------------------------------------------------------------------------

    fn library_tab(auto_install_overlays: bool, marks: &[usize]) -> AppState {
        let mut state = loaded_with(auto_install_overlays);
        act(&mut state, Action::GoTab(Tab::Library));
        for &row in marks {
            act(&mut state, Action::Home);
            for _ in 0..row {
                act(&mut state, Action::Down);
            }
            act(&mut state, Action::Mark);
        }
        state
    }

    fn confirm_install(state: &AppState) -> (Vec<String>, Vec<String>) {
        match &state.modal {
            Some(Modal::ConfirmInstall { entries, skipped }) => (
                entries
                    .iter()
                    .map(|e| format!("{} {}", e.title, e.package_type))
                    .collect(),
                skipped.clone(),
            ),
            other => panic!("expected ConfirmInstall, got {other:?}"),
        }
    }

    #[test]
    fn library_install_leaves_out_packages_already_current() {
        // AF-NORTH ortho (new) and NA-EAST ortho (current).
        let mut state = library_tab(true, &[0, 4]);

        assert!(act(&mut state, Action::Install).is_empty());

        let (entries, skipped) = confirm_install(&state);
        assert_eq!(entries, ["AF-NORTH ortho", "AF-NORTH overlay"]);
        assert_eq!(skipped, ["NA-EAST ortho"]);
    }

    #[test]
    fn library_install_follows_an_ortho_with_its_overlay_per_auto_install() {
        let mut on = library_tab(true, &[0]);
        let mut off = library_tab(false, &[0]);

        act(&mut on, Action::Install);
        act(&mut off, Action::Install);

        assert_eq!(
            confirm_install(&on).0,
            ["AF-NORTH ortho", "AF-NORTH overlay"]
        );
        assert_eq!(confirm_install(&off).0, ["AF-NORTH ortho"]);
    }

    #[test]
    fn library_install_quietly_drops_a_following_overlay_already_current() {
        // EU-WEST ortho (update); its overlay is current.
        let mut state = library_tab(true, &[2]);

        act(&mut state, Action::Install);

        let (entries, skipped) = confirm_install(&state);
        assert_eq!(entries, ["EU-WEST ortho"]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn library_install_lists_a_marked_overlay_once() {
        let mut state = library_tab(true, &[0, 1]);

        act(&mut state, Action::Install);

        assert_eq!(
            confirm_install(&state).0,
            ["AF-NORTH ortho", "AF-NORTH overlay"]
        );
    }

    #[test]
    fn library_install_drops_updates_while_run_is_active_and_keeps_new_installs() {
        // AF-NORTH ortho (new) and EU-WEST ortho (update).
        let mut state = library_tab(true, &[0, 2]);
        msg(&mut state, Msg::RunActive(Some(42)));

        act(&mut state, Action::Install);

        assert_eq!(
            confirm_install(&state).0,
            ["AF-NORTH ortho", "AF-NORTH overlay"]
        );
        assert!(
            status(&state).contains(&RunActive(42).message()),
            "{}",
            status(&state)
        );
        assert!(status(&state).contains(RESTART_NOTE), "{}", status(&state));
    }

    #[test]
    fn library_install_confirm_installs_and_clears_the_marks() {
        let mut state = library_tab(false, &[0, 5]);
        act(&mut state, Action::Install);

        let effects = act(&mut state, Action::Confirm);

        assert_eq!(
            effects,
            [Effect::Install(vec![
                entry("AF-NORTH", Ortho, "1.0.0"),
                entry("SA-SOUTH", Overlay, "1.0.0"),
            ])]
        );
        assert!(state.modal.is_none());
        assert!(state.library_marks.is_empty());
    }

    #[test]
    fn library_install_with_nothing_left_explains_and_opens_nothing() {
        let mut state = library_tab(true, &[4]);

        let effects = act(&mut state, Action::Install);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
        assert!(
            status(&state).contains("NA-EAST ortho: already up to date"),
            "{}",
            status(&state)
        );
    }

    #[test]
    fn library_install_with_only_updates_while_run_is_active_opens_nothing() {
        let mut state = library_tab(true, &[2]);
        msg(&mut state, Msg::RunActive(Some(42)));

        act(&mut state, Action::Install);

        assert!(state.modal.is_none());
        assert!(
            status(&state).contains(&RunActive(42).message()),
            "{}",
            status(&state)
        );
    }

    // ------------------------------------------------------------------------
    // Progress
    // ------------------------------------------------------------------------

    fn progress_with_two_jobs() -> (AppState, Vec<JobId>) {
        let mut state = loaded();
        let ids = queue(
            &mut state,
            &[("NA", Ortho, Some("1.0.0")), ("EU", Ortho, Some("1.0.0"))],
        );
        act(&mut state, Action::GoTab(Tab::Progress));
        (state, ids)
    }

    #[test]
    fn cancel_cancels_the_selected_unfinished_job() {
        let (mut state, ids) = progress_with_two_jobs();
        act(&mut state, Action::Down);

        assert_eq!(act(&mut state, Action::Cancel), [Effect::CancelJob(ids[1])]);
    }

    #[test]
    fn cancel_leaves_a_finished_job_alone() {
        let (mut state, ids) = progress_with_two_jobs();
        msg(&mut state, Msg::Job(ids[0], JobMsg::Failed("boom".into())));

        assert!(act(&mut state, Action::Cancel).is_empty());
    }

    #[test]
    fn cancel_all_asks_first() {
        let (mut state, _) = progress_with_two_jobs();

        assert!(act(&mut state, Action::CancelAll).is_empty());
        assert_eq!(state.modal, Some(Modal::ConfirmCancelAll));

        assert_eq!(act(&mut state, Action::Confirm), [Effect::CancelAll]);
        assert!(state.modal.is_none());
    }

    #[test]
    fn cancel_all_with_nothing_running_says_so() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Progress));

        act(&mut state, Action::CancelAll);

        assert!(state.modal.is_none());
        assert_eq!(status(&state), "No jobs to cancel");
    }

    #[test]
    fn clear_finished_removes_finished_rows() {
        let (mut state, ids) = progress_with_two_jobs();
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        act(&mut state, Action::End);

        act(&mut state, Action::ClearFinished);

        assert_eq!(
            state.jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
            [ids[1]]
        );
        assert_eq!(state.selected(), 0);
    }

    // ------------------------------------------------------------------------
    // Refresh and help
    // ------------------------------------------------------------------------

    #[test]
    fn refresh_reloads_everything_and_shows_the_library_loading() {
        let mut state = loaded();

        assert_eq!(act(&mut state, Action::Refresh), RELOAD);
        assert!(matches!(state.library, LibraryState::Loading));
    }

    #[test]
    fn help_opens_and_closes_with_help_or_dismiss() {
        let mut state = loaded();

        act(&mut state, Action::Help);
        assert_eq!(state.modal, Some(Modal::Help));
        act(&mut state, Action::Help);
        assert!(state.modal.is_none());

        act(&mut state, Action::Help);
        act(&mut state, Action::Dismiss);
        assert!(state.modal.is_none());
    }

    #[test]
    fn keys_other_than_the_modals_own_do_nothing_under_a_modal() {
        let mut state = loaded();
        act(&mut state, Action::Help);

        act(&mut state, Action::Down);
        act(&mut state, Action::NextTab);

        assert_eq!(state.selected(), 0);
        assert_eq!(state.tab, Tab::Local);
        assert_eq!(state.modal, Some(Modal::Help));
    }

    // ------------------------------------------------------------------------
    // Quit
    // ------------------------------------------------------------------------

    #[test]
    fn quit_asks_for_confirmation_with_the_running_count() {
        let (mut state, _) = progress_with_two_jobs();
        let now = Instant::now();

        let effects = state.update(Input::Action(Action::Quit), now);

        assert!(effects.is_empty());
        assert_eq!(
            state.modal,
            Some(Modal::ConfirmQuit {
                since: now,
                running: 2
            })
        );
    }

    #[test]
    fn confirming_quit_with_nothing_running_exits() {
        for confirm in [Action::Quit, Action::Confirm] {
            let mut state = loaded();
            act(&mut state, Action::Quit);

            assert_eq!(act(&mut state, confirm), [Effect::Exit]);
        }
    }

    #[test]
    fn confirming_quit_with_jobs_running_cancels_them_first() {
        for confirm in [Action::Quit, Action::Confirm] {
            let (mut state, _) = progress_with_two_jobs();
            act(&mut state, Action::Quit);

            assert_eq!(act(&mut state, confirm), [Effect::CancelAll]);
            assert_eq!(state.quitting, Quitting::Cancelling);
            assert!(state.modal.is_none());
            assert_eq!(status(&state), "Cancelling, press q again to quit now");
        }
    }

    #[test]
    fn dismiss_closes_the_quit_confirmation() {
        let mut state = loaded();
        act(&mut state, Action::Quit);

        assert!(act(&mut state, Action::Dismiss).is_empty());
        assert!(state.modal.is_none());
    }

    #[test]
    fn an_expired_quit_confirmation_closes_on_the_next_input() {
        let mut state = loaded();
        let start = Instant::now();
        state.update(Input::Action(Action::Quit), start);
        let later = start + QUIT_CONFIRM_TIMEOUT + Duration::from_millis(1);

        let effects = state.update(Input::Action(Action::Confirm), later);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
    }

    #[test]
    fn a_quit_confirmation_within_the_timeout_stays_open() {
        let mut state = loaded();
        let start = Instant::now();
        state.update(Input::Action(Action::Quit), start);

        state.update(
            Input::Msg(Msg::Status("tick".into())),
            start + QUIT_CONFIRM_TIMEOUT,
        );

        assert!(matches!(state.modal, Some(Modal::ConfirmQuit { .. })));
    }

    #[test]
    fn quit_while_cancelling_abandons_and_exits() {
        let (mut state, _) = progress_with_two_jobs();
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);

        assert_eq!(
            act(&mut state, Action::Quit),
            [Effect::Abandon, Effect::Exit]
        );
        assert_eq!(state.quitting, Quitting::Abandoned);
    }

    // ------------------------------------------------------------------------
    // Messages: metadata and queuing
    // ------------------------------------------------------------------------

    #[test]
    fn metadata_ready_submits_what_was_fetched_and_reports_failures() {
        let mut state = loaded();

        let effects = msg(
            &mut state,
            Msg::MetadataReady(vec![
                Ok(metadata("AF-NORTH", Ortho)),
                Err("AF-NORTH overlay: timed out".into()),
            ]),
        );

        assert_eq!(effects, [Effect::Submit(vec![metadata("AF-NORTH", Ortho)])]);
        assert_eq!(
            status(&state),
            "could not fetch metadata for AF-NORTH overlay: timed out"
        );
    }

    #[test]
    fn metadata_ready_with_every_fetch_failed_restores_the_marks() {
        let mut state = library_tab(false, &[0, 5]);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);
        assert!(state.library_marks.is_empty());

        let effects = msg(
            &mut state,
            Msg::MetadataReady(vec![
                Err("AF-NORTH ortho: timed out".into()),
                Err("SA-SOUTH overlay: 404".into()),
            ]),
        );

        assert!(effects.is_empty());
        assert_eq!(
            state.library_marks.iter().cloned().collect::<Vec<_>>(),
            [key("AF-NORTH", Ortho), key("SA-SOUTH", Overlay)]
        );
        assert!(status(&state).contains("AF-NORTH ortho: timed out"));
        assert!(status(&state).contains("SA-SOUTH overlay: 404"));
    }

    #[test]
    fn queued_adds_labelled_rows_and_counts_them_unfinished() {
        let mut state = loaded();

        let ids = queue(
            &mut state,
            &[
                ("NA-CANADA", Ortho, Some("2.1.0")),
                ("EU-WEST", Overlay, None),
            ],
        );

        let rows: Vec<_> = state
            .jobs
            .iter()
            .map(|j| (j.id, j.label.as_str(), j.kind, &j.look, j.finished))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    ids[0],
                    "NA-CANADA ortho v2.1.0",
                    JobKind::Install,
                    &JobLook::Queued,
                    false
                ),
                (
                    ids[1],
                    "EU-WEST overlay",
                    JobKind::Remove,
                    &JobLook::Queued,
                    false
                ),
            ]
        );
        assert_eq!(state.unfinished(), 2);
        assert!(state.overlay_jobs.contains(&ids[1]));
        assert!(!state.overlay_jobs.contains(&ids[0]));
    }

    #[test]
    fn submitted_releases_the_held_marks() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);

        queue(&mut state, &[("EU-WEST", Ortho, None)]);
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        msg(&mut state, Msg::QueueRefused("late".into()));

        assert!(state.local_marks.is_empty());
        assert!(state.held_marks.is_none());
    }

    #[test]
    fn submitted_shows_the_queued_count_and_each_refusal() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::Down);
        act(&mut state, Action::Mark);
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        queue(&mut state, &[("EU-WEST", Ortho, None)]);

        let effects = msg(
            &mut state,
            Msg::Submitted {
                refused: vec!["EU-WEST overlay: already queued".into()],
            },
        );

        assert!(effects.is_empty());
        assert_eq!(
            status(&state),
            "1 job queued: see Progress; EU-WEST overlay: already queued"
        );
    }

    #[test]
    fn a_submitted_batch_that_queued_nothing_releases_the_queue() {
        let mut state = loaded();
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);

        let effects = msg(
            &mut state,
            Msg::Submitted {
                refused: vec!["EU-WEST ortho: already queued".into()],
            },
        );

        assert_eq!(effects, [Effect::ReleaseQueue]);
        assert_eq!(status(&state), "EU-WEST ortho: already queued");
    }

    #[test]
    fn a_refused_queue_restores_the_marks() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        act(&mut state, Action::Down);
        act(&mut state, Action::Mark);
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        assert!(state.local_marks.is_empty());

        let effects = msg(
            &mut state,
            Msg::QueueRefused("another package operation is running".into()),
        );

        assert_eq!(status(&state), "another package operation is running");
        assert_eq!(state.local_marks.len(), 2);
        assert_eq!(effects, [Effect::ReleaseQueue]);
    }

    // ------------------------------------------------------------------------
    // Messages: job progress
    // ------------------------------------------------------------------------

    #[test]
    fn job_messages_update_the_rows_look() {
        let (mut state, ids) = progress_with_two_jobs();
        let look = |state: &AppState| state.jobs[0].look.clone();

        let steps = [
            (JobMsg::Preparing, JobLook::Preparing),
            (
                JobMsg::Waiting {
                    needed: 10,
                    available: 5,
                },
                JobLook::Waiting {
                    needed: 10,
                    available: 5,
                },
            ),
            (
                JobMsg::Downloading {
                    done: 3,
                    total: Some(9),
                },
                JobLook::Downloading {
                    done: 3,
                    total: Some(9),
                },
            ),
            (JobMsg::Downloaded, JobLook::Downloaded),
            (JobMsg::Stage("Extracting"), JobLook::Working("Extracting")),
            (JobMsg::Removing, JobLook::Working("Removing")),
        ];
        for (job_msg, expected) in steps {
            assert!(msg(&mut state, Msg::Job(ids[0], job_msg)).is_empty());
            assert_eq!(look(&state), expected);
            assert!(!state.jobs[0].finished);
        }
    }

    #[test]
    fn each_finish_marks_the_row_finished_and_reloads_the_lists() {
        let outcomes = [
            (JobMsg::Installed, JobLook::Done),
            (JobMsg::Removed, JobLook::Done),
            (JobMsg::Cancelled, JobLook::Cancelled),
            (
                JobMsg::Failed("disk full".into()),
                JobLook::Failed("disk full".into()),
            ),
        ];
        for (job_msg, expected) in outcomes {
            let (mut state, ids) = progress_with_two_jobs();

            let effects = msg(&mut state, Msg::Job(ids[0], job_msg));

            assert_eq!(effects, RELOAD);
            assert_eq!(state.jobs[0].look, expected);
            assert!(state.jobs[0].finished);
            assert_eq!(state.unfinished(), 1);
        }
    }

    #[test]
    fn a_message_for_a_finished_job_changes_nothing() {
        let (mut state, ids) = progress_with_two_jobs();
        msg(&mut state, Msg::Job(ids[0], JobMsg::Cancelled));

        assert!(msg(&mut state, Msg::Job(ids[0], JobMsg::Installed)).is_empty());
        assert_eq!(state.jobs[0].look, JobLook::Cancelled);
    }

    #[test]
    fn an_overlay_installed_or_removed_marks_the_overlay_changed() {
        for finish in [JobMsg::Installed, JobMsg::Removed] {
            let mut state = loaded();
            let ids = queue(
                &mut state,
                &[("EU", Overlay, Some("1.0.0")), ("NA", Ortho, Some("1.0.0"))],
            );

            msg(&mut state, Msg::Job(ids[0], finish));

            assert!(state.overlay_changed);
        }
    }

    #[test]
    fn an_overlay_that_failed_or_an_ortho_leaves_the_overlays_alone() {
        let mut state = loaded();
        let ids = queue(
            &mut state,
            &[
                ("EU", Overlay, Some("1.0.0")),
                ("NA", Ortho, Some("1.0.0")),
                ("SA", Ortho, Some("1.0.0")),
            ],
        );

        msg(&mut state, Msg::Job(ids[0], JobMsg::Failed("x".into())));
        msg(&mut state, Msg::Job(ids[1], JobMsg::Installed));

        assert!(!state.overlay_changed);
    }

    // ------------------------------------------------------------------------
    // Going idle
    // ------------------------------------------------------------------------

    #[test]
    fn the_last_finish_rebuilds_the_overlays_once_then_releases_the_queue() {
        let mut state = loaded();
        let ids = queue(
            &mut state,
            &[("EU", Overlay, Some("1.0.0")), ("NA", Ortho, Some("1.0.0"))],
        );
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        let effects = msg(&mut state, Msg::Job(ids[1], JobMsg::Installed));

        let mut expected = RELOAD.to_vec();
        expected.extend([Effect::RebuildOverlays, Effect::ReleaseQueue]);
        assert_eq!(effects, expected);
        assert!(!state.overlay_changed);
    }

    #[test]
    fn the_last_finish_without_an_overlay_change_only_releases_the_queue() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);

        let effects = msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        let mut expected = RELOAD.to_vec();
        expected.push(Effect::ReleaseQueue);
        assert_eq!(effects, expected);
    }

    #[test]
    fn the_last_finish_while_cancelling_exits() {
        let (mut state, ids) = progress_with_two_jobs();
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);
        msg(&mut state, Msg::Job(ids[0], JobMsg::Cancelled));

        let effects = msg(&mut state, Msg::Job(ids[1], JobMsg::Cancelled));

        let mut expected = RELOAD.to_vec();
        expected.extend([Effect::ReleaseQueue, Effect::Exit]);
        assert_eq!(effects, expected);
    }

    // ------------------------------------------------------------------------
    // Messages: data
    // ------------------------------------------------------------------------

    #[test]
    fn new_installed_data_clamps_the_selection_and_marks() {
        let mut state = loaded();
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::End);

        msg(
            &mut state,
            Msg::Installed(Ok(vec![local("EU-WEST", Overlay, 20, None)])),
        );

        assert_eq!(state.local.len(), 1);
        assert_eq!(state.selected(), 0);
        assert_eq!(
            state.local_marks.iter().cloned().collect::<Vec<_>>(),
            [key("EU-WEST", Overlay)]
        );
    }

    #[test]
    fn new_library_data_clamps_the_selection_and_marks() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::End);

        msg(
            &mut state,
            Msg::Library(Ok(vec![lib(
                "SA-SOUTH",
                Overlay,
                "1.0.0",
                LibraryStatus::NotInstalled,
            )])),
        );

        assert_eq!(state.selected(), 0);
        assert_eq!(
            state.library_marks.iter().cloned().collect::<Vec<_>>(),
            [key("SA-SOUTH", Overlay)]
        );
    }

    #[test]
    fn library_marks_survive_a_refresh() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);

        act(&mut state, Action::Refresh);
        msg(&mut state, Msg::Library(Ok(fake_library())));

        assert!(state.library_marks.contains(&key("AF-NORTH", Ortho)));
    }

    #[test]
    fn an_empty_installed_list_points_to_the_library() {
        let mut state = loaded();

        msg(&mut state, Msg::Installed(Ok(Vec::new())));

        assert_eq!(
            status(&state),
            "Nothing installed yet: press 2 for the Library"
        );
    }

    #[test]
    fn a_failed_installed_list_keeps_the_rows_and_says_why() {
        let mut state = loaded();

        msg(&mut state, Msg::Installed(Err("permission denied".into())));

        assert_eq!(state.local.len(), 3);
        assert!(status(&state).contains("permission denied"));
    }

    #[test]
    fn a_failed_or_missing_library_is_its_own_state() {
        let mut state = loaded();

        msg(&mut state, Msg::Library(Err("offline".into())));
        assert!(matches!(&state.library, LibraryState::Failed(e) if e == "offline"));

        msg(&mut state, Msg::LibraryNotConfigured);
        assert!(matches!(state.library, LibraryState::NotConfigured));
    }

    #[test]
    fn run_active_replaces_the_flag() {
        let mut state = loaded();

        msg(&mut state, Msg::RunActive(Some(7)));
        assert_eq!(state.run_active, Some(7));
        msg(&mut state, Msg::RunActive(None));
        assert_eq!(state.run_active, None);
    }

    #[test]
    fn a_status_message_shows_in_the_status_line() {
        let mut state = loaded();

        msg(
            &mut state,
            Msg::Status("Consolidated overlay updated".into()),
        );

        assert_eq!(status(&state), "Consolidated overlay updated");
    }

    // ------------------------------------------------------------------------
    // One package request at a time
    // ------------------------------------------------------------------------

    const WAITING: &str = "waiting for the queue to accept the last request";

    #[test]
    fn a_second_request_waits_for_the_first_to_be_accepted() {
        // Library: mark AF-NORTH ortho, install, confirm.
        let mut state = library_tab(false, &[0]);
        act(&mut state, Action::Install);
        assert_eq!(
            act(&mut state, Action::Confirm),
            [Effect::Install(vec![entry("AF-NORTH", Ortho, "1.0.0")])]
        );

        // While its metadata is fetched: Local, mark NA-EAST, delete.
        act(&mut state, Action::GoTab(Tab::Local));
        act(&mut state, Action::End);
        act(&mut state, Action::Mark);
        let effects = act(&mut state, Action::Delete);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
        assert_eq!(status(&state), WAITING);
        assert!(act(&mut state, Action::Update).is_empty());
        assert_eq!(status(&state), WAITING);

        // The first request fails: its own marks come back, on its own tab.
        msg(
            &mut state,
            Msg::MetadataReady(vec![Err("AF-NORTH ortho: timed out".into())]),
        );

        assert_eq!(
            state.library_marks.iter().cloned().collect::<Vec<_>>(),
            [key("AF-NORTH", Ortho)]
        );
        assert_eq!(
            state.local_marks.iter().cloned().collect::<Vec<_>>(),
            [key("NA-EAST", Ortho)]
        );
        assert!(state.held_marks.is_none());
    }

    #[test]
    fn install_waits_for_the_last_request_too() {
        let mut state = loaded();
        act(&mut state, Action::Update);
        act(&mut state, Action::GoTab(Tab::Library));

        act(&mut state, Action::Install);

        assert!(state.modal.is_none());
        assert_eq!(status(&state), WAITING);
    }

    #[test]
    fn a_confirmation_waits_for_the_last_request() {
        let mut state = loaded();
        state.held_marks = Some((Tab::Library, BTreeSet::new()));
        state.modal = Some(Modal::ConfirmDelete {
            targets: vec![key("NA-EAST", Ortho)],
            total_bytes: 300,
        });

        assert!(act(&mut state, Action::Confirm).is_empty());
        assert!(state.modal.is_none());
        assert_eq!(status(&state), WAITING);
    }

    #[test]
    fn a_new_request_is_taken_once_the_last_was_accepted() {
        let mut state = loaded();
        act(&mut state, Action::Update);
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );

        act(&mut state, Action::Delete);

        assert!(matches!(state.modal, Some(Modal::ConfirmDelete { .. })));
    }

    // ------------------------------------------------------------------------
    // Quitting takes no new work
    // ------------------------------------------------------------------------

    fn cancelling() -> (AppState, Vec<JobId>) {
        let (mut state, ids) = progress_with_two_jobs();
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);
        assert_eq!(state.quitting, Quitting::Cancelling);
        (state, ids)
    }

    #[test]
    fn package_actions_are_refused_while_quitting() {
        let (mut state, _) = cancelling();

        act(&mut state, Action::GoTab(Tab::Local));
        assert!(act(&mut state, Action::Update).is_empty());
        assert!(act(&mut state, Action::Delete).is_empty());
        act(&mut state, Action::GoTab(Tab::Library));
        assert!(act(&mut state, Action::Install).is_empty());

        assert!(state.modal.is_none());
        assert_eq!(status(&state), "quitting: no new package requests");
    }

    #[test]
    fn metadata_arriving_while_cancelling_submits_nothing() {
        let (mut state, _) = progress_with_two_jobs();
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);

        let effects = msg(
            &mut state,
            Msg::MetadataReady(vec![Ok(metadata("AF-NORTH", Ortho))]),
        );

        assert!(effects.is_empty());
        assert_eq!(status(&state), "cancelled");
        assert!(state.held_marks.is_none());
    }

    #[test]
    fn a_job_queued_while_cancelling_is_cancelled() {
        let (mut state, _) = cancelling();

        let effects = msg(
            &mut state,
            Msg::Queued(JobId(9), summary("AF-NORTH", Ortho, Some("1.0.0"))),
        );

        assert_eq!(effects, [Effect::CancelJob(JobId(9))]);
        assert_eq!(state.unfinished(), 3);
    }

    #[test]
    fn dismiss_with_no_modal_asks_to_quit() {
        let mut state = loaded();

        assert!(act(&mut state, Action::Dismiss).is_empty());

        assert!(matches!(state.modal, Some(Modal::ConfirmQuit { .. })));
    }

    #[test]
    fn dismiss_while_cancelling_does_not_abandon() {
        let (mut state, _) = cancelling();

        assert!(act(&mut state, Action::Dismiss).is_empty());

        assert_eq!(state.quitting, Quitting::Cancelling);
        assert!(state.modal.is_none());
    }

    #[test]
    fn a_job_message_for_an_unknown_id_is_ignored() {
        let (mut state, _) = progress_with_two_jobs();

        assert!(msg(&mut state, Msg::Job(JobId(99), JobMsg::Installed)).is_empty());
        assert_eq!(state.unfinished(), 2);
    }

    // ------------------------------------------------------------------------
    // Marks while the library reloads
    // ------------------------------------------------------------------------

    #[test]
    fn library_marks_survive_a_failed_reload() {
        let mut state = loaded();
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);

        act(&mut state, Action::Refresh);
        msg(&mut state, Msg::Library(Err("offline".into())));
        assert!(state.library_marks.contains(&key("AF-NORTH", Ortho)));

        msg(&mut state, Msg::Library(Ok(fake_library())));
        assert!(state.library_marks.contains(&key("AF-NORTH", Ortho)));
    }

    #[test]
    fn marks_restored_while_the_library_reloads_are_kept() {
        let mut state = library_tab(false, &[0]);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);
        act(&mut state, Action::Refresh);

        msg(
            &mut state,
            Msg::MetadataReady(vec![Err("AF-NORTH ortho: timed out".into())]),
        );

        assert!(state.library_marks.contains(&key("AF-NORTH", Ortho)));
    }

    // ------------------------------------------------------------------------
    // Summary
    // ------------------------------------------------------------------------

    #[test]
    fn summary_counts_the_outcomes() {
        let mut state = loaded();
        let ids = queue(
            &mut state,
            &[
                ("A", Ortho, Some("1.0.0")),
                ("B", Ortho, Some("1.0.0")),
                ("C", Ortho, None),
                ("D", Ortho, Some("1.0.0")),
                ("E", Ortho, Some("1.0.0")),
            ],
        );
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        msg(&mut state, Msg::Job(ids[1], JobMsg::Installed));
        msg(&mut state, Msg::Job(ids[2], JobMsg::Removed));
        msg(&mut state, Msg::Job(ids[3], JobMsg::Failed("x".into())));
        act(&mut state, Action::GoTab(Tab::Progress));
        act(&mut state, Action::ClearFinished);

        assert_eq!(
            state.summary(),
            "Package manager: 2 installed, 1 removed, 1 failed, 1 unfinished"
        );
    }

    #[test]
    fn summary_with_nothing_done_says_so() {
        let (state, _) = AppState::new(true);

        assert_eq!(state.summary(), "Package manager: no packages changed");
    }
}
