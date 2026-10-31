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
use xearthlayer::config::{ConfigFile, ConfigKey};
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
pub type PackageKey = (String, PackageType);

// ============================================================================
// Data
// ============================================================================

/// The three tabs, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Local,
    Library,
    Progress,
    Settings,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Local, Tab::Library, Tab::Progress, Tab::Settings];

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

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// Typed text: a URL, a path, a number.
    Text,
    /// true or false, flipped with Space or the left and right arrows.
    Flag,
}

/// One `[packages]` setting on the Settings tab (#336).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingRow {
    pub key: ConfigKey,
    /// As `xearthlayer config get` shows it: empty when unset.
    pub value: String,
    /// What an empty value means, shown dimmed in its place.
    pub default: Option<String>,
    pub kind: SettingKind,
    /// The same help `config.ini` carries for it, as paragraphs.
    pub help: Vec<String>,
}

/// The Settings tab's rows, from the configuration file.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsState {
    Loading,
    Loaded(Vec<SettingRow>),
    Failed(String),
}

/// The value being typed into a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub buffer: String,
    /// The insertion point, in characters.
    pub cursor: usize,
}

/// A key typed while a setting is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
}

/// The rows of the Settings tab for `config`: every `[packages]` setting in
/// `config.ini`'s order, with its value, default and help.
pub fn setting_rows(config: &ConfigFile) -> Vec<SettingRow> {
    xearthlayer::config::help::PACKAGES_SETTINGS
        .iter()
        .map(|&key| {
            let kind = match key {
                ConfigKey::PackagesAutoInstallOverlays
                | ConfigKey::PackagesDisableOverlays
                | ConfigKey::PackagesXworldPro => SettingKind::Flag,
                _ => SettingKind::Text,
            };
            let default = match key {
                ConfigKey::PackagesInstallLocation => {
                    Some(xearthlayer::paths::packages_dir().display().to_string())
                }
                ConfigKey::PackagesTempDir => {
                    Some(xearthlayer::paths::temp_dir().display().to_string())
                }
                ConfigKey::PackagesCustomSceneryPath => {
                    Some("found from xplane.scenery_dir or X-Plane's own files".to_string())
                }
                _ => None,
            };
            SettingRow {
                key,
                value: key.get(config),
                default,
                kind,
                help: xearthlayer::config::help::help(key),
            }
        })
        .collect()
}

/// One installed package.
#[derive(Debug, Clone)]
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
        /// The entries already installed, at an older version.
        replacing: Vec<PackageKey>,
        /// What was left out and why, kept for the request once confirmed.
        notes: Vec<String>,
    },
    ConfirmCancelAll,
}

/// What the runner tells the state.
#[derive(Debug)]
pub enum Msg {
    Installed(Result<Vec<LocalRow>, String>),
    Library(Result<Vec<LibraryRow>, String>),
    LibraryNotConfigured,
    /// A running `xearthlayer run`, if any.
    RunActive(Option<RunActive>),
    /// One result per entry of an [`Effect::Install`]. Each error names its
    /// package: "NA-CANADA ortho: connection refused".
    MetadataReady(Vec<Result<PackageMetadata, String>>),
    /// The queue accepted this job. Sent from the queue's own `Queued`
    /// event, so it arrives before any other message for the job.
    Queued(JobId, JobSummary),
    /// `submit_batch` returned: the request was accepted, and these of its
    /// packages were refused, each naming its package. The runner sends it
    /// after every `Queued` of that batch, and after a [`Effect::Submit`] or
    /// [`Effect::Remove`] it always sends this or `QueueRefused`: until then
    /// the queue is not released.
    Submitted {
        refused: Vec<String>,
    },
    /// The queue would not start, and why.
    QueueRefused(String),
    Job(JobId, JobMsg),
    Status(String),
    /// The overlay rebuild finished, with the line to show if it has one.
    /// Every [`Effect::RebuildOverlays`] is answered by exactly one.
    OverlaysRebuilt(Option<String>),
    /// The Settings tab's rows, read from the configuration file.
    Settings(Result<Vec<SettingRow>, String>),
    /// A [`Effect::SaveSetting`] was carried out: the rows as saved, or why
    /// nothing was. Every one is answered by exactly one of these.
    SettingSaved {
        key: ConfigKey,
        result: Result<Vec<SettingRow>, String>,
    },
    /// Time passed. Sent about once a second so an expired quit prompt
    /// clears without a key press.
    Tick,
}

/// A job's progress, as an owned copy of the queue's event.
#[derive(Debug, Clone)]
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
    /// Start editing the selected setting.
    Edit,
    /// A key typed into the setting being edited.
    EditKey(EditKey),
    Refresh,
    Help,
    Quit,
    Confirm,
    Dismiss,
}

/// Everything `update` reacts to.
#[derive(Debug)]
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
    /// `replacing` names those already installed: a `run` found when the
    /// effect is carried out refuses them and lets the rest install.
    Submit {
        packages: Vec<PackageMetadata>,
        replacing: Vec<PackageKey>,
    },
    /// Start the queue if needed and submit these removals as one batch,
    /// or refuse all of them if a `run` is found when it is carried out.
    Remove(Vec<PackageKey>),
    CancelJob(JobId),
    CancelAll,
    /// Leave the running jobs to the queue's own shutdown.
    Abandon,
    RebuildOverlays,
    /// Read the Settings tab's rows, answered by [`Msg::Settings`].
    LoadSettings,
    /// Save one setting, as `xearthlayer config set` does, unless a `run`
    /// is found when it is carried out. Answered by [`Msg::SettingSaved`].
    SaveSetting {
        key: ConfigKey,
        value: String,
    },
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
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    installed: usize,
    removed: usize,
    failed: usize,
    cancelled: usize,
}

impl Tally {
    /// What ended since `earlier`, a tally taken before this one.
    fn since(&self, earlier: &Tally) -> Tally {
        Tally {
            installed: self.installed - earlier.installed,
            removed: self.removed - earlier.removed,
            failed: self.failed - earlier.failed,
            cancelled: self.cancelled - earlier.cancelled,
        }
    }

    /// "2 installed", "1 failed": one part per outcome that happened.
    fn parts(&self) -> Vec<String> {
        [
            (self.installed, "installed"),
            (self.removed, "removed"),
            (self.failed, "failed"),
            (self.cancelled, "cancelled"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect()
    }
}

/// The status while the overlay folder is rebuilt.
const REBUILDING: &str = "Rebuilding the overlay folder…";

/// The end of the queued note, which the idle queue replaces.
const SEE_PROGRESS: &str = "queued: see Progress";

/// The status says how many jobs were queued, and nothing has replaced it.
fn is_queued_note(status: &str) -> bool {
    status.contains(SEE_PROGRESS)
}

// ============================================================================
// State
// ============================================================================

/// The package manager's whole state.
#[derive(Debug)]
pub struct AppState {
    tab: Tab,
    local: Vec<LocalRow>,
    /// An answer to the first read of the installed list has come.
    local_loaded: bool,
    /// The reads given and not yet answered. The worker shares a load with
    /// one of its kind still waiting, and answers that pair once, so these
    /// are flags rather than counts: any answer clears its flag.
    reading_local: bool,
    library: LibraryState,
    reading_library: bool,
    filter: TypeFilter,
    jobs: Vec<JobRow>,
    /// The selected row of each tab, an index into the rows it shows.
    selection: [usize; 4],
    settings: SettingsState,
    /// The setting being edited, on the selected Settings row.
    editing: Option<Editor>,
    /// A setting is being saved: no other edit starts until it is answered.
    saving: bool,
    local_marks: BTreeSet<PackageKey>,
    library_marks: BTreeSet<PackageKey>,
    /// Marks cleared by a confirmed action, kept until the queue answers
    /// so a refusal can put them back.
    /// Only one package request is in flight at a time: while this is set,
    /// new ones are refused.
    held_marks: Option<(Tab, BTreeSet<PackageKey>)>,
    /// Jobs queued since the request in flight was made.
    queued_count: usize,
    /// One of them installs a package.
    queued_install: bool,
    /// The packages of the request in flight already installed, for its
    /// `Submit`.
    replacing: Vec<PackageKey>,
    /// A `Submit` or `Remove` was given and the queue has not answered, so
    /// the queue must not be released yet.
    submitting: bool,
    /// Why some of the request in flight will not happen: a package left
    /// out, a failed metadata fetch. Later lines about the request keep them.
    request_notes: Vec<String>,
    /// The notes of the requests the queue accepted since it was last idle,
    /// for its "Done" line: none of their packages has a Progress row.
    done_notes: Vec<String>,
    /// The idle queue's "Done" line, kept beside the overlay rebuild's.
    idle_note: Option<String>,
    /// Overlay rebuilds asked for and not answered. The queue is held until
    /// they are, so the rebuild runs under its lock.
    rebuilds: usize,
    modal: Option<Modal>,
    status: Option<String>,
    run_active: Option<RunActive>,
    quitting: Quitting,
    /// An overlay was installed or removed since the queue was last idle.
    overlay_changed: bool,
    /// The jobs that install or remove an overlay.
    overlay_jobs: HashSet<JobId>,
    auto_install_overlays: bool,
    tally: Tally,
    /// The tally when the queue was last idle.
    tally_at_idle: Tally,
    started: Instant,
}

impl AppState {
    /// A state with nothing loaded, and the effects that load it.
    pub fn new(auto_install_overlays: bool) -> (Self, Vec<Effect>) {
        let mut state = Self {
            tab: Tab::Local,
            local: Vec::new(),
            local_loaded: false,
            reading_local: false,
            library: LibraryState::Loading,
            reading_library: false,
            filter: TypeFilter::All,
            jobs: Vec::new(),
            selection: [0; 4],
            settings: SettingsState::Loading,
            editing: None,
            saving: false,
            local_marks: BTreeSet::new(),
            library_marks: BTreeSet::new(),
            held_marks: None,
            queued_count: 0,
            queued_install: false,
            replacing: Vec::new(),
            submitting: false,
            request_notes: Vec::new(),
            done_notes: Vec::new(),
            idle_note: None,
            rebuilds: 0,
            modal: None,
            status: None,
            run_active: None,
            quitting: Quitting::No,
            overlay_changed: false,
            overlay_jobs: HashSet::new(),
            auto_install_overlays,
            tally: Tally::default(),
            tally_at_idle: Tally::default(),
            started: Instant::now(),
        };
        let mut effects = reload();
        effects.push(Effect::LoadSettings);
        state.note_reads(&effects);
        (state, effects)
    }

    /// Apply one input, returning the work it calls for.
    pub fn update(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        if let Some(Modal::ConfirmQuit { since, .. }) = self.modal {
            if now.saturating_duration_since(since) > QUIT_CONFIRM_TIMEOUT {
                self.modal = None;
                // Dismissing the expired prompt only clears it.
                if matches!(input, Input::Action(Action::Dismiss)) {
                    return Vec::new();
                }
            }
        }
        let effects = match input {
            Input::Action(action) => self.on_action(action, now),
            Input::Msg(msg) => self.on_msg(msg),
        };
        self.note_reads(&effects);
        effects
    }

    /// Note the reads among `effects` as in flight, until their answers.
    fn note_reads(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::LoadInstalled => self.reading_local = true,
                Effect::LoadLibrary => self.reading_library = true,
                // While cancelling, the status keeps saying how to quit now.
                Effect::RebuildOverlays if self.quitting == Quitting::No => {
                    self.status = self.rebuilding_line();
                }
                _ => {}
            }
        }
    }

    /// Jobs queued and not yet finished.
    pub fn unfinished(&self) -> usize {
        self.jobs.iter().filter(|job| !job.finished).count()
    }

    /// One line on what the session did, printed after exit.
    pub fn summary(&self) -> String {
        let mut parts = self.tally.parts();
        let unfinished = self.unfinished();
        if unfinished > 0 {
            parts.push(format!("{unfinished} unfinished"));
        }
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
    // Read-only views for rendering
    // ------------------------------------------------------------------------

    /// The tab being shown.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// The installed packages.
    pub fn local_rows(&self) -> &[LocalRow] {
        &self.local
    }

    /// Whether this installed package is marked.
    pub fn is_local_marked(&self, row: &LocalRow) -> bool {
        self.local_marks.contains(&row.key())
    }

    /// What the Library tab has to show.
    pub fn library(&self) -> &LibraryState {
        &self.library
    }

    /// The type filter of the Library tab.
    pub fn filter(&self) -> TypeFilter {
        self.filter
    }

    /// The library rows the filter shows, in order. Empty unless loaded.
    pub fn library_rows(&self) -> Vec<&LibraryRow> {
        self.shown_library()
    }

    /// Whether this library package is marked.
    pub fn is_library_marked(&self, row: &LibraryRow) -> bool {
        self.library_marks
            .contains(&(row.entry.title.clone(), row.entry.package_type))
    }

    /// Whether installing this library package also installs its overlay:
    /// an ortho, with overlays following and the library offering one.
    pub fn overlay_follows(&self, row: &LibraryRow) -> bool {
        let LibraryState::Loaded(rows) = &self.library else {
            return false;
        };
        self.auto_install_overlays
            && row.entry.package_type == PackageType::Ortho
            && find_row(rows, &row.entry.title, PackageType::Overlay).is_some()
    }

    /// Every job, in the order they were queued.
    pub fn jobs(&self) -> &[JobRow] {
        &self.jobs
    }

    /// The open dialog, if any.
    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// The status line.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// The pid of the `xearthlayer run` serving packages, if any.
    pub fn run_active(&self) -> Option<u32> {
        self.run_active.as_ref().map(|run| run.pid)
    }

    /// The refusal the details panes end with while `run` is serving.
    pub fn run_refusal(&self) -> Option<&RunActive> {
        self.run_active.as_ref()
    }

    /// The installed list has been read at least once, well or not.
    pub fn local_loaded(&self) -> bool {
        self.local_loaded
    }

    /// A read of the installed list is in flight.
    pub fn reading_local(&self) -> bool {
        self.reading_local
    }

    /// A read of the library is in flight.
    pub fn reading_library(&self) -> bool {
        self.reading_library
    }

    /// When the state was made: the spinner counts its frames from here.
    pub fn started(&self) -> Instant {
        self.started
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
        if self.editing.is_some() {
            return self.on_edit_action(action);
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
            (Action::Edit, Tab::Settings) => self.begin_edit(),
            (Action::Refresh, _) => {
                self.library = LibraryState::Loading;
                let mut effects = reload();
                effects.push(Effect::LoadSettings);
                return effects;
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

    // ------------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------------

    /// Why settings cannot be edited now, if they cannot: `run` reads them,
    /// and a queued job was started with them (#336).
    pub fn settings_refusal(&self) -> Option<String> {
        if let Some(run) = &self.run_active {
            return Some(format!(
                "XEarthLayer is running (pid {}): settings are read-only until it stops",
                run.pid
            ));
        }
        let busy = self.unfinished() > 0
            || self.held_marks.is_some()
            || self.submitting
            || self.rebuilds > 0;
        busy.then(|| {
            "Packages are being installed or removed: finish or cancel the jobs first".to_string()
        })
    }

    fn selected_setting(&self) -> Option<&SettingRow> {
        match &self.settings {
            SettingsState::Loaded(rows) => rows.get(self.selection[Tab::Settings.index()]),
            _ => None,
        }
    }

    fn begin_edit(&mut self) {
        let Some(row) = self.selected_setting() else {
            return;
        };
        if self.saving {
            self.status = Some(format!("saving {}", row.key.name()));
            return;
        }
        if let Some(reason) = self.settings_refusal() {
            self.status = Some(reason);
            return;
        }
        let buffer = match row.kind {
            // Unset reads as false.
            SettingKind::Flag if row.value != "true" => "false".to_string(),
            _ => row.value.clone(),
        };
        let hint = match row.kind {
            SettingKind::Text => "Enter saves   Esc cancels",
            SettingKind::Flag => "Space or arrows flip   Enter saves   Esc cancels",
        };
        self.status = Some(format!("Editing {}: {hint}", row.key.name()));
        self.editing = Some(Editor {
            cursor: buffer.chars().count(),
            buffer,
        });
    }

    /// Keys while a setting is edited: only the editor's apply.
    fn on_edit_action(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Confirm => return self.commit_edit(),
            Action::Dismiss => {
                self.editing = None;
                self.status = Some("Edit cancelled".to_string());
            }
            Action::EditKey(key) => self.edit_key(key),
            _ => {}
        }
        Vec::new()
    }

    fn edit_key(&mut self, key: EditKey) {
        let kind = self.selected_setting().map(|row| row.kind);
        let Some(editor) = self.editing.as_mut() else {
            return;
        };
        if kind == Some(SettingKind::Flag) {
            if matches!(key, EditKey::Char(' ') | EditKey::Left | EditKey::Right) {
                editor.buffer = if editor.buffer == "true" {
                    "false"
                } else {
                    "true"
                }
                .to_string();
                editor.cursor = editor.buffer.chars().count();
            }
            return;
        }
        let len = editor.buffer.chars().count();
        let byte = |s: &str, at: usize| s.char_indices().nth(at).map_or(s.len(), |(i, _)| i);
        match key {
            EditKey::Char(c) => {
                let at = byte(&editor.buffer, editor.cursor);
                editor.buffer.insert(at, c);
                editor.cursor += 1;
            }
            EditKey::Backspace if editor.cursor > 0 => {
                let at = byte(&editor.buffer, editor.cursor - 1);
                editor.buffer.remove(at);
                editor.cursor -= 1;
            }
            EditKey::Delete if editor.cursor < len => {
                let at = byte(&editor.buffer, editor.cursor);
                editor.buffer.remove(at);
            }
            EditKey::Left => editor.cursor = editor.cursor.saturating_sub(1),
            EditKey::Right => editor.cursor = (editor.cursor + 1).min(len),
            EditKey::Home => editor.cursor = 0,
            EditKey::End => editor.cursor = len,
            _ => {}
        }
    }

    /// Validate the edited value as `config set` would, and save it.
    fn commit_edit(&mut self) -> Vec<Effect> {
        let Some(row) = self.selected_setting().cloned() else {
            self.editing = None;
            return Vec::new();
        };
        let value = self
            .editing
            .as_ref()
            .map(|e| e.buffer.trim().to_string())
            .unwrap_or_default();
        if value == row.value {
            self.editing = None;
            self.status = Some(format!("{} unchanged", row.key.name()));
            return Vec::new();
        }
        if let Err(e) = row.key.validate(&value) {
            // Stay in the editor so the value can be corrected. The error
            // names the setting.
            self.status = Some(e.to_string());
            return Vec::new();
        }
        self.editing = None;
        self.saving = true;
        self.status = Some(format!("Saving {}", row.key.name()));
        vec![Effect::SaveSetting {
            key: row.key,
            value,
        }]
    }

    fn on_setting_saved(
        &mut self,
        key: ConfigKey,
        result: Result<Vec<SettingRow>, String>,
    ) -> Vec<Effect> {
        self.saving = false;
        match result {
            Ok(rows) => {
                self.settings = SettingsState::Loaded(rows);
                self.clamp(Tab::Settings);
                self.status = Some(format!("Saved {}", key.name()));
                // What the new value changes is read again.
                match key {
                    ConfigKey::PackagesLibraryUrl => {
                        self.library = LibraryState::Loading;
                        vec![Effect::LoadLibrary]
                    }
                    ConfigKey::PackagesInstallLocation => {
                        self.library = LibraryState::Loading;
                        reload()
                    }
                    _ => Vec::new(),
                }
            }
            Err(reason) => {
                self.status = Some(reason);
                Vec::new()
            }
        }
    }

    /// The Settings tab's rows.
    pub fn settings(&self) -> &SettingsState {
        &self.settings
    }

    /// The setting being edited, if one is.
    pub fn editor(&self) -> Option<&Editor> {
        self.editing.as_ref()
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
                self.submitting = true;
                vec![Effect::Remove(targets)]
            }
            Some(Modal::ConfirmInstall {
                entries,
                replacing,
                notes,
                ..
            }) => {
                self.hold_marks(Tab::Library);
                self.replacing = replacing;
                self.request_notes = notes;
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

    /// Quitting was confirmed: exit, or cancel the running jobs first. A
    /// submission in flight counts as running: its jobs are cancelled as they
    /// are queued, and the exit comes once the queue has answered.
    fn quit_now(&mut self) -> Vec<Effect> {
        if self.unfinished() == 0 && !self.submitting {
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
            Tab::Progress | Tab::Settings => Vec::new(),
        }
    }

    fn shown_len(&self, tab: Tab) -> usize {
        match tab {
            Tab::Local => self.local.len(),
            Tab::Library => self.shown_library().len(),
            Tab::Progress => self.jobs.len(),
            Tab::Settings => match &self.settings {
                SettingsState::Loaded(rows) => rows.len(),
                _ => 0,
            },
        }
    }

    fn marks_mut(&mut self, tab: Tab) -> Option<&mut BTreeSet<PackageKey>> {
        match tab {
            Tab::Local => Some(&mut self.local_marks),
            Tab::Library => Some(&mut self.library_marks),
            Tab::Progress | Tab::Settings => None,
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
            Tab::Progress | Tab::Settings => return Vec::new(),
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

    /// Clear a tab's marks, keeping a copy until the queue answers. This
    /// starts a request: it has no notes yet.
    fn hold_marks(&mut self, tab: Tab) {
        if let Some(marks) = self.marks_mut(tab) {
            let marks = std::mem::take(marks);
            self.held_marks = Some((tab, marks));
            self.queued_count = 0;
            self.queued_install = false;
            self.request_notes.clear();
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
        match &self.run_active {
            Some(run) => {
                self.status = Some(run.short_message());
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
                // An update replaces what is installed.
                self.replacing = entries
                    .iter()
                    .map(|entry| (entry.title.clone(), entry.package_type))
                    .collect();
                effects.push(Effect::Install(entries));
            }
        }
        if !effects.is_empty() {
            self.hold_marks(Tab::Local);
            self.request_notes = notes.clone();
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
        let mut replacing = Vec::new();
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
                Some(LibraryStatus::Update { .. }) => {
                    replacing.push((entry.title.clone(), entry.package_type));
                    entries.push(entry.clone());
                }
                _ => entries.push(entry.clone()),
            }
        }

        let mut left_out = Vec::new();
        if let (Some(run), false) = (&self.run_active, refused.is_empty()) {
            left_out.push(format!(
                "{} left out. {}",
                refused.join(", "),
                run.short_message()
            ));
        }
        let mut notes = left_out.clone();
        if entries.is_empty() {
            if !skipped.is_empty() {
                notes.push(format!("{}: already up to date", skipped.join(", ")));
            }
        } else {
            if self.run_active.is_some() {
                notes.push(format!("New packages: {RESTART_NOTE}"));
            }
            self.modal = Some(Modal::ConfirmInstall {
                entries,
                skipped,
                replacing,
                notes: left_out,
            });
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
            Msg::Installed(result) => self.on_installed(result),
            Msg::Library(Ok(rows)) => self.set_library(LibraryState::Loaded(rows)),
            Msg::Library(Err(e)) => self.set_library(LibraryState::Failed(e)),
            Msg::LibraryNotConfigured => self.set_library(LibraryState::NotConfigured),
            Msg::RunActive(run) => self.run_active = run,
            Msg::MetadataReady(results) => return self.on_metadata(results),
            Msg::Queued(id, summary) => return self.on_queued(id, summary),
            Msg::Submitted { refused } => return self.on_submitted(refused),
            Msg::QueueRefused(reason) => {
                let notes = std::mem::take(&mut self.request_notes);
                self.status = joined(notes.into_iter().chain([reason]));
                self.restore_marks();
                return self.queue_answered();
            }
            Msg::Job(id, job_msg) => return self.on_job(id, job_msg),
            Msg::Status(text) => self.status = Some(text),
            Msg::OverlaysRebuilt(line) => return self.on_rebuilt(line),
            Msg::Settings(Ok(rows)) => {
                self.settings = SettingsState::Loaded(rows);
                self.clamp(Tab::Settings);
            }
            Msg::Settings(Err(e)) => self.settings = SettingsState::Failed(e),
            Msg::SettingSaved { key, result } => return self.on_setting_saved(key, result),
            // `update` already expired the quit prompt.
            Msg::Tick => {}
        }
        Vec::new()
    }

    fn on_installed(&mut self, result: Result<Vec<LocalRow>, String>) {
        self.local_loaded = true;
        self.reading_local = false;
        match result {
            Ok(rows) => {
                // A hint, so it never replaces what the status line says.
                if rows.is_empty() && self.status.is_none() {
                    self.status =
                        Some("Nothing installed yet: press 2 for the Library".to_string());
                }
                self.local = rows;
                self.clamp(Tab::Local);
            }
            Err(e) => {
                self.status = Some(format!("could not read the installed packages: {e}"));
            }
        }
    }

    fn set_library(&mut self, library: LibraryState) {
        self.reading_library = false;
        self.library = library;
        self.clamp(Tab::Library);
    }

    fn on_metadata(&mut self, results: Vec<Result<PackageMetadata, String>>) -> Vec<Effect> {
        let replacing = std::mem::take(&mut self.replacing);
        if self.quitting != Quitting::No {
            self.status = Some("cancelled".to_string());
            self.request_notes.clear();
            self.restore_marks();
            return Vec::new();
        }
        let (fetched, failed): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        let fetched: Vec<PackageMetadata> = fetched.into_iter().filter_map(Result::ok).collect();
        let failed: Vec<String> = failed.into_iter().filter_map(Result::err).collect();
        if !failed.is_empty() {
            // A package that failed here never gets a Progress row.
            let failed = failed
                .iter()
                .map(|e| format!("could not fetch metadata for {e}"));
            self.request_notes.extend(failed);
            self.status = joined(self.request_notes.iter().cloned());
        }
        if fetched.is_empty() {
            // The request ends here, and so do its notes.
            self.request_notes.clear();
            self.restore_marks();
            return Vec::new();
        }
        self.submitting = true;
        vec![Effect::Submit {
            packages: fetched,
            replacing,
        }]
    }

    fn on_queued(&mut self, id: JobId, summary: JobSummary) -> Vec<Effect> {
        self.queued_count += 1;
        self.queued_install |= summary.kind == JobKind::Install;
        if self.quitting == Quitting::No {
            let notes = self.done_notes.iter().chain(&self.request_notes).cloned();
            self.status = joined(std::iter::once(self.queued_note()).chain(notes));
        }
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
    /// what was queued and what was refused. Its notes and refusals are kept
    /// for the idle queue's "Done" line.
    fn on_submitted(&mut self, refused: Vec<String>) -> Vec<Effect> {
        self.held_marks = None;
        let request_notes = std::mem::take(&mut self.request_notes);
        self.done_notes
            .extend(request_notes.into_iter().chain(refused));
        let mut notes = Vec::new();
        if self.queued_count > 0 {
            notes.push(self.queued_note());
        }
        notes.extend(self.done_notes.iter().cloned());
        if self.queued_install && self.run_active.is_some() {
            notes.push(format!("New packages: {RESTART_NOTE}"));
        }
        // While cancelling, the status keeps saying how to quit now.
        if !notes.is_empty() && self.quitting == Quitting::No {
            self.status = Some(notes.join("; "));
        }
        self.queued_count = 0;
        self.queue_answered()
    }

    /// The queue answered the submission in flight: if that left it idle,
    /// take the idle step the finished jobs deferred.
    fn queue_answered(&mut self) -> Vec<Effect> {
        if std::mem::take(&mut self.submitting) && self.unfinished() == 0 {
            return self.idle();
        }
        Vec::new()
    }

    /// The queue went idle: say what it did in place of the queued note,
    /// rebuild the overlays once if any changed, and release the queue (and
    /// exit if quitting was waiting for this) once no rebuild is pending.
    /// Taken exactly once per transition to idle.
    fn idle(&mut self) -> Vec<Effect> {
        let done = self.tally.since(&self.tally_at_idle);
        self.tally_at_idle = self.tally;
        let notes = std::mem::take(&mut self.done_notes);
        self.idle_note = None;
        let queued = self.status.as_deref().is_some_and(is_queued_note);
        if self.quitting == Quitting::No && (queued || !notes.is_empty()) {
            self.status = self.done_note(&done, notes);
            self.idle_note = self.status.clone();
        }
        let mut effects = Vec::new();
        if std::mem::take(&mut self.overlay_changed) {
            effects.push(Effect::RebuildOverlays);
            self.rebuilds += 1;
        }
        if self.rebuilds == 0 {
            effects.extend(self.release());
        }
        effects
    }

    /// Drop the idle queue, and exit if quitting was waiting for it.
    fn release(&self) -> Vec<Effect> {
        let mut effects = vec![Effect::ReleaseQueue];
        if self.quitting == Quitting::Cancelling {
            effects.push(Effect::Exit);
        }
        effects
    }

    /// The overlay rebuild answered: show its line beside the idle queue's,
    /// and release the queue it held unless new work came meanwhile (that
    /// work's idle step releases it).
    fn on_rebuilt(&mut self, line: Option<String>) -> Vec<Effect> {
        self.rebuilds = self.rebuilds.saturating_sub(1);
        let rebuilding = self.rebuilding_line();
        let idle_note = self.idle_note.take();
        match line {
            Some(line) => self.status = joined(idle_note.into_iter().chain([line])),
            // Nothing to say: only the rebuilding note goes.
            None if self.status == rebuilding => self.status = idle_note,
            None => {}
        }
        if self.rebuilds == 0 && self.unfinished() == 0 && !self.submitting {
            return self.release();
        }
        Vec::new()
    }

    /// The status while the overlay folder is rebuilt, after the idle line.
    fn rebuilding_line(&self) -> Option<String> {
        joined(
            self.idle_note
                .iter()
                .cloned()
                .chain([REBUILDING.to_string()]),
        )
    }

    /// "N jobs queued: see Progress"
    fn queued_note(&self) -> String {
        match self.queued_count {
            1 => format!("1 job {SEE_PROGRESS}"),
            n => format!("{n} jobs {SEE_PROGRESS}"),
        }
    }

    /// "Done: 1 installed, 1 failed", then `notes`, then the restart note if
    /// `run` is serving while a package was installed. `None` if nothing
    /// finished and there are no notes.
    fn done_note(&self, done: &Tally, notes: Vec<String>) -> Option<String> {
        let parts = done.parts();
        let done_part = (!parts.is_empty()).then(|| format!("Done: {}", parts.join(", ")));
        let restart = (done.installed > 0 && self.run_active.is_some())
            .then(|| format!("New packages: {RESTART_NOTE}"));
        joined(done_part.into_iter().chain(notes).chain(restart))
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
        // With a submission in flight the queue is about to get more work:
        // the queue's answer takes the idle step instead.
        if self.unfinished() == 0 && !self.submitting {
            effects.extend(self.idle());
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

/// The parts joined into one status line, or `None` if there are none.
fn joined(parts: impl IntoIterator<Item = String>) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().collect();
    (!parts.is_empty()).then(|| parts.join("; "))
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
    use crate::commands::packages::actions::{LibraryRow, LibraryStatus, RESTART_NOTE};
    use crate::commands::packages::test_support::run_active;
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

        assert_eq!(effects, [&RELOAD[..], &[Effect::LoadSettings]].concat());
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
    fn next_and_prev_tab_cycle_local_library_progress_settings() {
        let mut state = loaded();

        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Library);
        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Progress);
        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Settings);
        act(&mut state, Action::NextTab);
        assert_eq!(state.tab, Tab::Local);
        act(&mut state, Action::PrevTab);
        assert_eq!(state.tab, Tab::Settings);
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
        msg(&mut state, Msg::RunActive(Some(run_active(42))));

        let effects = act(&mut state, Action::Update);

        assert!(effects.is_empty());
        assert_eq!(status(&state), run_active(42).short_message());
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
        msg(&mut state, Msg::RunActive(Some(run_active(42))));

        let effects = act(&mut state, Action::Delete);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
        assert_eq!(status(&state), run_active(42).short_message());
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
            Some(Modal::ConfirmInstall {
                entries, skipped, ..
            }) => (
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
        msg(&mut state, Msg::RunActive(Some(run_active(42))));

        act(&mut state, Action::Install);

        assert_eq!(
            confirm_install(&state).0,
            ["AF-NORTH ortho", "AF-NORTH overlay"]
        );
        assert!(
            status(&state).contains(&run_active(42).short_message()),
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
        msg(&mut state, Msg::RunActive(Some(run_active(42))));

        act(&mut state, Action::Install);

        assert!(state.modal.is_none());
        assert!(
            status(&state).contains(&run_active(42).short_message()),
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

        assert_eq!(
            act(&mut state, Action::Refresh),
            [&RELOAD[..], &[Effect::LoadSettings]].concat()
        );
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

        assert_eq!(
            effects,
            [Effect::Submit {
                packages: vec![metadata("AF-NORTH", Ortho)],
                replacing: Vec::new(),
            }]
        );
        let failure = "could not fetch metadata for AF-NORTH overlay: timed out";
        assert_eq!(status(&state), failure);

        // The failed package gets no Progress row, so the note stays to the end.
        let ids = queue(&mut state, &[("AF-NORTH", Ortho, Some("1.0.0"))]);
        assert!(status(&state).contains(failure), "{}", status(&state));
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        assert!(status(&state).contains(failure), "{}", status(&state));
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        assert_eq!(status(&state), format!("Done: 1 installed; {failure}"));
    }

    /// Acting checks for `run` again, and refuses what would replace an
    /// installed package, so the submission says which those are.
    #[test]
    fn an_update_is_submitted_as_replacing_the_installed_package() {
        let mut state = loaded();
        let effects = act(&mut state, Action::Update);
        assert_eq!(
            effects,
            [Effect::Install(vec![entry("EU-WEST", Ortho, "1.1.0")])]
        );

        let effects = msg(
            &mut state,
            Msg::MetadataReady(vec![Ok(metadata("EU-WEST", Ortho))]),
        );

        assert_eq!(
            effects,
            [Effect::Submit {
                packages: vec![metadata("EU-WEST", Ortho)],
                replacing: vec![key("EU-WEST", Ortho)],
            }]
        );
    }

    #[test]
    fn a_library_install_submits_only_the_installed_packages_as_replacing() {
        // AF-NORTH ortho (new) and EU-WEST ortho (update).
        let mut state = library_tab(false, &[0, 2]);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);

        let effects = msg(
            &mut state,
            Msg::MetadataReady(vec![
                Ok(metadata("AF-NORTH", Ortho)),
                Ok(metadata("EU-WEST", Ortho)),
            ]),
        );

        assert_eq!(
            effects,
            [Effect::Submit {
                packages: vec![metadata("AF-NORTH", Ortho), metadata("EU-WEST", Ortho)],
                replacing: vec![key("EU-WEST", Ortho)],
            }]
        );
    }

    #[test]
    fn submitted_while_run_is_active_says_to_restart_it() {
        let mut state = library_tab(false, &[0, 2]);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);
        msg(
            &mut state,
            Msg::MetadataReady(vec![
                Ok(metadata("AF-NORTH", Ortho)),
                Ok(metadata("EU-WEST", Ortho)),
            ]),
        );
        // `run` started after the install was confirmed.
        msg(&mut state, Msg::RunActive(Some(run_active(42))));
        queue(&mut state, &[("AF-NORTH", Ortho, Some("1.0.0"))]);
        let left_out = format!("EU-WEST ortho left out. {}", run_active(42).short_message());

        msg(
            &mut state,
            Msg::Submitted {
                refused: vec![left_out.clone()],
            },
        );

        assert_eq!(
            status(&state),
            format!("1 job queued: see Progress; {left_out}; New packages: {RESTART_NOTE}")
        );
        assert_eq!(state.run_active(), Some(42));
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

    /// The queue, and with it the lock, is held until the rebuild answers.
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
        expected.push(Effect::RebuildOverlays);
        assert_eq!(effects, expected);
        assert!(!state.overlay_changed);
        assert_eq!(
            msg(&mut state, Msg::OverlaysRebuilt(None)),
            [Effect::ReleaseQueue]
        );
    }

    #[test]
    fn a_job_queued_during_the_rebuild_holds_the_queue_until_it_finishes() {
        let mut state = rebuilding();
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);

        assert_eq!(msg(&mut state, Msg::OverlaysRebuilt(None)), []);

        let mut expected = RELOAD.to_vec();
        expected.push(Effect::ReleaseQueue);
        assert_eq!(
            msg(&mut state, Msg::Job(ids[0], JobMsg::Installed)),
            expected
        );
    }

    #[test]
    fn an_idle_queue_during_the_rebuild_waits_for_it_to_release() {
        let mut state = rebuilding();
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);

        assert_eq!(msg(&mut state, Msg::Job(ids[0], JobMsg::Installed)), RELOAD);

        assert_eq!(
            msg(&mut state, Msg::OverlaysRebuilt(None)),
            [Effect::ReleaseQueue]
        );
    }

    /// Quitting waits for the rebuild's answer, and a second q still quits
    /// at once.
    #[test]
    fn quitting_while_the_overlays_rebuild_exits_once_they_are_done() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU", Overlay, Some("1.0.0"))]);
        act(&mut state, Action::Quit);
        assert_eq!(act(&mut state, Action::Confirm), [Effect::CancelAll]);

        // Too late to cancel: the swap finished.
        let mut expected = RELOAD.to_vec();
        expected.push(Effect::RebuildOverlays);
        assert_eq!(
            msg(&mut state, Msg::Job(ids[0], JobMsg::Installed)),
            expected
        );
        assert_eq!(status(&state), "Cancelling, press q again to quit now");

        assert_eq!(
            msg(&mut state, Msg::OverlaysRebuilt(None)),
            [Effect::ReleaseQueue, Effect::Exit]
        );
    }

    #[test]
    fn a_second_q_during_the_rebuild_quits_now() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU", Overlay, Some("1.0.0"))]);
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert_eq!(
            act(&mut state, Action::Quit),
            [Effect::Abandon, Effect::Exit]
        );
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
    fn the_queued_note_gives_way_to_what_was_done_once_the_queue_is_idle() {
        let mut state = loaded();
        let ids = queue(
            &mut state,
            &[("EU", Ortho, Some("1.0.0")), ("NA", Ortho, None)],
        );
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        assert_eq!(status(&state), "2 jobs queued: see Progress");

        msg(&mut state, Msg::Job(ids[1], JobMsg::Removed));

        assert_eq!(status(&state), "Done: 1 installed, 1 removed");
    }

    #[test]
    fn each_idle_queue_reports_only_its_own_jobs() {
        let mut state = loaded();
        let first = queue(&mut state, &[("EU", Ortho, Some("1.0.0"))]);
        msg(&mut state, Msg::Job(first[0], JobMsg::Installed));
        let second = queue(&mut state, &[("NA", Ortho, None)]);

        msg(&mut state, Msg::Job(second[0], JobMsg::Failed("x".into())));

        assert_eq!(status(&state), "Done: 1 failed");
    }

    #[test]
    fn an_idle_queue_while_run_is_active_says_to_restart_it() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(run_active(42))));
        let ids = queue(&mut state, &[("AF-NORTH", Ortho, Some("1.0.0"))]);

        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert_eq!(
            status(&state),
            format!("Done: 1 installed; New packages: {RESTART_NOTE}")
        );
    }

    #[test]
    fn an_idle_queue_leaves_a_later_status_alone() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU", Ortho, Some("1.0.0"))]);
        msg(
            &mut state,
            Msg::Status("could not rebuild the overlays".into()),
        );

        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert_eq!(status(&state), "could not rebuild the overlays");
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

        msg(&mut state, Msg::RunActive(Some(run_active(7))));
        assert_eq!(state.run_active(), Some(7));
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
    // Going idle with a submission in flight
    // ------------------------------------------------------------------------

    fn release_count(effects: &[Effect]) -> usize {
        effects
            .iter()
            .filter(|e| **e == Effect::ReleaseQueue)
            .count()
    }

    #[test]
    fn a_job_finishing_while_a_submit_is_pending_waits_for_submitted() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU", Overlay, Some("1.0.0"))]);
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);
        act(&mut state, Action::Install);
        act(&mut state, Action::Confirm);
        assert_eq!(
            msg(
                &mut state,
                Msg::MetadataReady(vec![Ok(metadata("AF-NORTH", Ortho))])
            ),
            [Effect::Submit {
                packages: vec![metadata("AF-NORTH", Ortho)],
                replacing: Vec::new(),
            }]
        );

        let effects = msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        assert_eq!(effects, RELOAD);

        let effects = msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        assert_eq!(effects, [Effect::RebuildOverlays]);
        assert_eq!(
            msg(&mut state, Msg::OverlaysRebuilt(None)),
            [Effect::ReleaseQueue]
        );
    }

    #[test]
    fn a_fast_failing_batch_releases_the_queue_once() {
        let mut state = loaded();
        let mut effects = act(&mut state, Action::Delete);
        effects.extend(act(&mut state, Action::Confirm));
        let ids = queue(&mut state, &[("EU-WEST", Ortho, None)]);
        effects.extend(msg(
            &mut state,
            Msg::Job(ids[0], JobMsg::Failed("x".into())),
        ));
        assert_eq!(release_count(&effects), 0);

        effects.extend(msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        ));

        assert_eq!(release_count(&effects), 1);
    }

    #[test]
    fn a_pending_remove_defers_idle_until_the_queue_answers() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);
        act(&mut state, Action::Delete);
        assert!(matches!(
            act(&mut state, Action::Confirm).as_slice(),
            [Effect::Remove(_)]
        ));

        assert_eq!(msg(&mut state, Msg::Job(ids[0], JobMsg::Installed)), RELOAD);
        assert_eq!(
            msg(&mut state, Msg::QueueRefused("busy".into())),
            [Effect::ReleaseQueue]
        );
    }

    #[test]
    fn cancelling_with_a_pending_submit_exits_once_after_submitted() {
        let (mut state, ids) = progress_with_two_jobs();
        act(&mut state, Action::GoTab(Tab::Local));
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        act(&mut state, Action::Quit);
        assert_eq!(act(&mut state, Action::Confirm), [Effect::CancelAll]);

        let mut effects = msg(&mut state, Msg::Job(ids[0], JobMsg::Cancelled));
        effects.extend(msg(&mut state, Msg::Job(ids[1], JobMsg::Cancelled)));
        effects.extend(msg(
            &mut state,
            Msg::Queued(JobId(3), summary("EU-WEST", Ortho, None)),
        ));
        effects.extend(msg(&mut state, Msg::Job(JobId(3), JobMsg::Cancelled)));
        assert!(!effects.contains(&Effect::Exit));
        assert_eq!(release_count(&effects), 0);

        let effects = msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );

        assert_eq!(effects, [Effect::ReleaseQueue, Effect::Exit]);
    }

    #[test]
    fn quitting_with_only_a_submit_pending_waits_for_it() {
        let mut state = loaded();
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        act(&mut state, Action::Quit);

        assert_eq!(act(&mut state, Action::Confirm), [Effect::CancelAll]);
        assert_eq!(
            msg(
                &mut state,
                Msg::Submitted {
                    refused: Vec::new()
                }
            ),
            [Effect::ReleaseQueue, Effect::Exit]
        );
    }

    #[test]
    fn the_cancelling_status_stays_while_jobs_queue_and_submit() {
        let (mut state, _) = progress_with_two_jobs();
        act(&mut state, Action::GoTab(Tab::Local));
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        act(&mut state, Action::Quit);
        act(&mut state, Action::Confirm);
        let cancelling = "Cancelling, press q again to quit now";

        msg(
            &mut state,
            Msg::Queued(JobId(3), summary("EU-WEST", Ortho, None)),
        );
        assert_eq!(status(&state), cancelling);
        msg(
            &mut state,
            Msg::Submitted {
                refused: vec!["EU-WEST overlay: already queued".into()],
            },
        );
        assert_eq!(status(&state), cancelling);
    }

    #[test]
    fn dismissing_an_expired_quit_prompt_just_clears_it() {
        let mut state = loaded();
        let start = Instant::now();
        state.update(Input::Action(Action::Quit), start);
        let later = start + QUIT_CONFIRM_TIMEOUT + Duration::from_millis(1);

        let effects = state.update(Input::Action(Action::Dismiss), later);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
    }

    #[test]
    fn a_tick_clears_an_expired_quit_prompt_and_nothing_else() {
        let mut state = loaded();
        let start = Instant::now();
        state.update(Input::Action(Action::Quit), start);

        let early = state.update(Input::Msg(Msg::Tick), start);
        assert!(early.is_empty());
        assert!(matches!(state.modal, Some(Modal::ConfirmQuit { .. })));

        let later = start + QUIT_CONFIRM_TIMEOUT + Duration::from_millis(1);
        let effects = state.update(Input::Msg(Msg::Tick), later);

        assert!(effects.is_empty());
        assert!(state.modal.is_none());
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

    // ------------------------------------------------------------------------
    // Reads in flight
    // ------------------------------------------------------------------------

    #[test]
    fn the_first_load_is_in_flight_until_the_installed_list_answers() {
        let (mut state, _) = AppState::new(true);
        assert!(!state.local_loaded());
        assert!(state.reading_local());

        msg(&mut state, Msg::Installed(Ok(Vec::new())));

        assert!(state.local_loaded());
        assert!(!state.reading_local());
    }

    #[test]
    fn a_failed_first_read_still_ends_the_first_load() {
        let (mut state, _) = AppState::new(true);

        msg(&mut state, Msg::Installed(Err("permission denied".into())));

        assert!(state.local_loaded());
        assert!(!state.reading_local());
    }

    #[test]
    fn a_reload_keeps_the_list_and_is_in_flight_until_it_answers() {
        let mut state = loaded();
        assert!(!state.reading_local());
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);

        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert!(state.reading_local());
        assert!(state.local_loaded());
        assert_eq!(state.local.len(), 3, "the list stays while it reloads");
        msg(&mut state, Msg::Installed(Ok(fake_local())));
        assert!(!state.reading_local());

        act(&mut state, Action::Refresh);
        assert!(state.reading_local());
    }

    #[test]
    fn a_library_reload_while_loaded_is_in_flight_until_it_answers() {
        let mut state = loaded();
        assert!(!state.reading_library());
        let ids = queue(&mut state, &[("NA", Ortho, Some("1.0.0"))]);

        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert!(state.reading_library());
        assert!(matches!(state.library, LibraryState::Loaded(_)));
        msg(&mut state, Msg::Library(Ok(fake_library())));
        assert!(!state.reading_library());

        for answer in [
            Msg::Library(Err("connection refused".into())),
            Msg::LibraryNotConfigured,
        ] {
            act(&mut state, Action::Refresh);
            assert!(state.reading_library());
            msg(&mut state, answer);
            assert!(!state.reading_library());
        }
    }

    /// A state whose last job installed an overlay, so the idle queue has
    /// just asked for the overlays to be rebuilt.
    fn rebuilding() -> AppState {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU", Overlay, Some("1.0.0"))]);
        let effects = msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        assert!(effects.contains(&Effect::RebuildOverlays), "{effects:?}");
        state
    }

    /// What the idle queue did stays beside the rebuild's note and line.
    #[test]
    fn the_overlay_rebuild_says_so_until_it_answers() {
        let mut state = rebuilding();
        assert_eq!(
            status(&state),
            "Done: 1 installed; Rebuilding the overlay folder…"
        );

        msg(
            &mut state,
            Msg::OverlaysRebuilt(Some(
                "Consolidated overlay updated: 2 regions, 9 files".into(),
            )),
        );

        assert_eq!(
            status(&state),
            "Done: 1 installed; Consolidated overlay updated: 2 regions, 9 files"
        );
    }

    #[test]
    fn a_rebuild_with_nothing_to_say_clears_its_note_only() {
        let mut state = rebuilding();
        msg(&mut state, Msg::OverlaysRebuilt(None));
        assert_eq!(status(&state), "Done: 1 installed");

        let mut state = rebuilding();
        msg(&mut state, Msg::Status("too late: finishing".into()));
        msg(&mut state, Msg::OverlaysRebuilt(None));
        assert_eq!(status(&state), "too late: finishing");
    }

    // ------------------------------------------------------------------------
    // Notes that outlive the status line they were first shown in
    // ------------------------------------------------------------------------

    #[test]
    fn a_package_left_out_while_run_is_active_is_still_named_when_done() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(run_active(42))));
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);
        for _ in 0..2 {
            act(&mut state, Action::Down);
        }
        act(&mut state, Action::Mark);
        act(&mut state, Action::Install);
        let left_out = format!("EU-WEST ortho left out. {}", run_active(42).short_message());
        assert!(status(&state).contains(&left_out), "{}", status(&state));

        act(&mut state, Action::Confirm);
        msg(
            &mut state,
            Msg::MetadataReady(vec![
                Ok(metadata("AF-NORTH", Ortho)),
                Ok(metadata("AF-NORTH", Overlay)),
            ]),
        );
        let ids = queue(
            &mut state,
            &[
                ("AF-NORTH", Ortho, Some("1.0.0")),
                ("AF-NORTH", Overlay, Some("1.0.0")),
            ],
        );
        assert!(status(&state).contains(&left_out), "{}", status(&state));
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        assert!(status(&state).contains(&left_out), "{}", status(&state));
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));
        msg(&mut state, Msg::Job(ids[1], JobMsg::Installed));

        assert_eq!(
            status(&state),
            format!("Done: 2 installed; {left_out}; New packages: {RESTART_NOTE}; Rebuilding the overlay folder…")
        );
    }

    #[test]
    fn a_dismissed_install_leaves_no_note_for_the_next_request() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(run_active(42))));
        act(&mut state, Action::GoTab(Tab::Library));
        act(&mut state, Action::Mark);
        for _ in 0..2 {
            act(&mut state, Action::Down);
        }
        act(&mut state, Action::Mark);
        act(&mut state, Action::Install);
        act(&mut state, Action::Dismiss);
        msg(&mut state, Msg::RunActive(None));
        act(&mut state, Action::GoTab(Tab::Local));
        act(&mut state, Action::Home);
        act(&mut state, Action::Down);
        act(&mut state, Action::Down);
        act(&mut state, Action::Delete);

        act(&mut state, Action::Confirm);
        let ids = queue(&mut state, &[("NA-EAST", Ortho, None)]);
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        msg(&mut state, Msg::Job(ids[0], JobMsg::Removed));

        assert_eq!(status(&state), "Done: 1 removed");
    }

    #[test]
    fn a_package_the_queue_refused_is_still_named_when_done() {
        let mut state = loaded();
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        let ids = queue(&mut state, &[("EU-WEST", Ortho, None)]);
        msg(
            &mut state,
            Msg::Submitted {
                refused: vec!["EU-WEST overlay: already queued".into()],
            },
        );

        msg(&mut state, Msg::Job(ids[0], JobMsg::Removed));

        assert_eq!(
            status(&state),
            "Done: 1 removed; EU-WEST overlay: already queued"
        );
    }

    #[test]
    fn an_update_that_skips_a_current_package_still_says_so_when_done() {
        let mut state = loaded();
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::Update);
        let current = "EU-WEST overlay, NA-EAST ortho: already up to date";
        assert_eq!(status(&state), current);

        msg(
            &mut state,
            Msg::MetadataReady(vec![Ok(metadata("EU-WEST", Ortho))]),
        );
        let ids = queue(&mut state, &[("EU-WEST", Ortho, Some("1.1.0"))]);
        msg(
            &mut state,
            Msg::Submitted {
                refused: Vec::new(),
            },
        );
        msg(&mut state, Msg::Job(ids[0], JobMsg::Installed));

        assert_eq!(status(&state), format!("Done: 1 installed; {current}"));
    }

    #[test]
    fn a_queue_that_will_not_start_keeps_the_metadata_failure() {
        let mut state = loaded();
        msg(
            &mut state,
            Msg::MetadataReady(vec![
                Ok(metadata("AF-NORTH", Ortho)),
                Err("AF-NORTH overlay: timed out".into()),
            ]),
        );

        msg(&mut state, Msg::QueueRefused("busy".into()));

        assert_eq!(
            status(&state),
            "could not fetch metadata for AF-NORTH overlay: timed out; busy"
        );
    }

    #[test]
    fn an_empty_installed_list_leaves_a_done_note_or_a_refusal_alone() {
        let mut state = loaded();
        let ids = queue(&mut state, &[("EU-WEST", Ortho, None)]);
        msg(&mut state, Msg::Job(ids[0], JobMsg::Removed));
        msg(&mut state, Msg::Installed(Ok(Vec::new())));
        assert_eq!(status(&state), "Done: 1 removed");

        let mut state = loaded();
        act(&mut state, Action::Delete);
        act(&mut state, Action::Confirm);
        msg(&mut state, Msg::QueueRefused("busy".into()));
        msg(&mut state, Msg::Installed(Ok(Vec::new())));
        assert_eq!(status(&state), "busy");
    }

    // ------------------------------------------------------------------------
    // Settings (#336)
    // ------------------------------------------------------------------------

    /// A loaded state on the Settings tab, its rows from a default config.
    fn on_settings() -> AppState {
        let mut state = loaded();
        msg(
            &mut state,
            Msg::Settings(Ok(setting_rows(&ConfigFile::default()))),
        );
        act(&mut state, Action::GoTab(Tab::Settings));
        state
    }

    fn select(state: &mut AppState, key: ConfigKey) {
        let SettingsState::Loaded(rows) = &state.settings else {
            panic!("settings not loaded");
        };
        state.selection[Tab::Settings.index()] = rows.iter().position(|r| r.key == key).unwrap();
    }

    fn type_text(state: &mut AppState, text: &str) {
        for c in text.chars() {
            act(state, Action::EditKey(EditKey::Char(c)));
        }
    }

    #[test]
    fn the_settings_tab_lists_every_packages_setting_in_config_order() {
        let state = on_settings();
        let SettingsState::Loaded(rows) = state.settings() else {
            panic!();
        };
        let keys: Vec<ConfigKey> = rows.iter().map(|r| r.key).collect();
        assert_eq!(keys, xearthlayer::config::help::PACKAGES_SETTINGS);
        assert!(rows.iter().all(|r| !r.help.is_empty()));
        assert_eq!(state.shown_len(Tab::Settings), 8);
    }

    #[test]
    fn enter_edits_and_enter_again_saves_a_valid_value() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesConcurrentDownloads);

        act(&mut state, Action::Edit);
        assert_eq!(state.editor().unwrap().buffer, "5");
        act(&mut state, Action::EditKey(EditKey::Backspace));
        type_text(&mut state, "8");
        let effects = act(&mut state, Action::Confirm);

        assert_eq!(
            effects,
            [Effect::SaveSetting {
                key: ConfigKey::PackagesConcurrentDownloads,
                value: "8".to_string()
            }]
        );
        assert!(state.editor().is_none());
        assert!(status(&state).contains("Saving packages.concurrent_downloads"));
    }

    #[test]
    fn an_invalid_value_is_refused_and_stays_open_to_correct() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesConcurrentDownloads);
        act(&mut state, Action::Edit);
        type_text(&mut state, "0");

        assert!(
            act(&mut state, Action::Confirm).is_empty(),
            "nothing is saved"
        );
        assert_eq!(state.editor().unwrap().buffer, "50");
        let said = status(&state);
        assert!(said.contains("packages.concurrent_downloads"), "{said}");
        assert_eq!(
            said.matches("packages.concurrent_downloads").count(),
            1,
            "named once: {said}"
        );
    }

    #[test]
    fn esc_cancels_and_an_unchanged_value_saves_nothing() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesTempDir);
        act(&mut state, Action::Edit);
        type_text(&mut state, "/tmp/x");
        act(&mut state, Action::Dismiss);
        assert!(state.editor().is_none());
        assert_eq!(status(&state), "Edit cancelled");

        act(&mut state, Action::Edit);
        assert!(act(&mut state, Action::Confirm).is_empty());
        assert!(status(&state).contains("unchanged"));
    }

    #[test]
    fn the_editor_inserts_and_deletes_at_the_cursor() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesTempDir);
        act(&mut state, Action::Edit);
        type_text(&mut state, "ac");
        act(&mut state, Action::EditKey(EditKey::Left));
        type_text(&mut state, "b");
        act(&mut state, Action::EditKey(EditKey::Home));
        act(&mut state, Action::EditKey(EditKey::Delete));
        act(&mut state, Action::EditKey(EditKey::End));
        type_text(&mut state, "é");
        act(&mut state, Action::EditKey(EditKey::Backspace));
        let editor = state.editor().unwrap();
        assert_eq!(editor.buffer, "bc");
        assert_eq!(editor.cursor, 2);
    }

    #[test]
    fn a_flag_flips_with_space_or_the_arrows_and_ignores_typing() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesAutoInstallOverlays);
        act(&mut state, Action::Edit);
        assert_eq!(state.editor().unwrap().buffer, "false");
        act(&mut state, Action::EditKey(EditKey::Char(' ')));
        type_text(&mut state, "xyz");
        assert_eq!(state.editor().unwrap().buffer, "true");
        act(&mut state, Action::EditKey(EditKey::Right));
        act(&mut state, Action::EditKey(EditKey::Left));
        assert_eq!(
            act(&mut state, Action::Confirm),
            [Effect::SaveSetting {
                key: ConfigKey::PackagesAutoInstallOverlays,
                value: "true".to_string()
            }]
        );
    }

    #[test]
    fn settings_are_read_only_while_run_is_active() {
        let mut state = on_settings();
        msg(
            &mut state,
            Msg::RunActive(Some(crate::commands::packages::test_support::run_active(
                4242,
            ))),
        );

        act(&mut state, Action::Edit);

        assert!(state.editor().is_none());
        assert!(status(&state).contains("pid 4242") && status(&state).contains("read-only"));
    }

    #[test]
    fn settings_are_read_only_while_jobs_are_queued_or_running() {
        let mut state = on_settings();
        queue(
            &mut state,
            &[("EU-WEST", PackageType::Ortho, Some("1.0.0"))],
        );

        act(&mut state, Action::Edit);

        assert!(state.editor().is_none());
        assert!(status(&state).contains("finish or cancel the jobs first"));
    }

    #[test]
    fn a_saved_setting_reloads_what_it_changes() {
        let rows = || Ok(setting_rows(&ConfigFile::default()));
        let mut state = on_settings();
        let saved = |state: &mut AppState, key| {
            msg(
                state,
                Msg::SettingSaved {
                    key,
                    result: rows(),
                },
            )
        };

        assert_eq!(
            saved(&mut state, ConfigKey::PackagesLibraryUrl),
            [Effect::LoadLibrary]
        );
        assert!(matches!(state.library, LibraryState::Loading));
        assert_eq!(
            saved(&mut state, ConfigKey::PackagesInstallLocation),
            RELOAD
        );
        assert!(saved(&mut state, ConfigKey::PackagesConcurrentDownloads).is_empty());
        assert_eq!(status(&state), "Saved packages.concurrent_downloads");

        msg(
            &mut state,
            Msg::SettingSaved {
                key: ConfigKey::PackagesTempDir,
                result: Err(
                    "XEarthLayer is running (pid 7): settings are read-only until it stops".into(),
                ),
            },
        );
        assert!(status(&state).contains("pid 7"));
        assert!(!state.saving);
    }

    #[test]
    fn a_second_edit_waits_for_the_save_in_flight() {
        let mut state = on_settings();
        select(&mut state, ConfigKey::PackagesTempDir);
        act(&mut state, Action::Edit);
        type_text(&mut state, "/tmp/x");
        act(&mut state, Action::Confirm);

        act(&mut state, Action::Edit);

        assert!(state.editor().is_none());
        assert!(status(&state).starts_with("saving"));
    }
}
