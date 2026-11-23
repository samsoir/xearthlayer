//! Scenery packages: what `xearthlayer packages` and the package manager do.
//!
//! Installs, updates and removes run through the core [`InstallQueue`] on a
//! worker thread. A [`GuiObserver`] records every job event into a table the
//! frontend polls with [`package_jobs`]. One operation runs at a time, as the
//! queue's lock on the temp directory requires anyway.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::State;
use xearthlayer::manager::{
    create_consolidated_overlay, create_consolidated_overlay_except, remove_overlay_symlink,
    Canceller, HttpLibraryClient, InstallQueue, Job, JobEvent, JobId, JobKind, JobOutcome,
    LibraryClient, LocalPackageStore, PackageInstaller, PackageStatus, QueueObserver,
    UpdateChecker,
};
use xearthlayer::xworld_pro::{detect, overlay_plan, OverlayCoverage, OverlayPlan};
use xearthlayer_package::{LibraryEntry, PackageLibrary, PackageMetadata, PackageType};

use crate::environment::Environment;
use crate::views::blocking;
use crate::AppState;

// ============================================================================
// Views
// ============================================================================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledView {
    pub region: String,
    pub package_type: String,
    pub version: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledList {
    pub install_location: String,
    pub packages: Vec<InstalledView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LibraryStatus {
    NotInstalled,
    UpToDate,
    Update,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryRowView {
    pub region: String,
    pub package_type: String,
    pub version: String,
    pub status: LibraryStatus,
    pub installed_version: Option<String>,
    pub download_bytes: Option<u64>,
}

/// A package the frontend names: its region code and type.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRef {
    pub region: String,
    pub package_type: String,
}

impl PackageRef {
    fn kind(&self) -> Result<PackageType, String> {
        match self.package_type.to_lowercase().as_str() {
            "ortho" => Ok(PackageType::Ortho),
            "overlay" => Ok(PackageType::Overlay),
            other => Err(format!("Unknown package type '{other}'")),
        }
    }
}

/// One row of the Progress table.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: u64,
    pub kind: String,
    pub region: String,
    pub package_type: String,
    pub version: Option<String>,
    pub stage: String,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub finished: bool,
    pub succeeded: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobsView {
    pub busy: bool,
    pub jobs: Vec<JobView>,
    /// Notes from the last operation, such as the overlay rebuild.
    pub notes: Vec<String>,
}

// ============================================================================
// State
// ============================================================================

/// Package operations in this process.
#[derive(Default)]
pub struct PackageOps {
    busy: AtomicBool,
    jobs: Arc<Mutex<Vec<JobView>>>,
    notes: Mutex<Vec<String>>,
    canceller: Mutex<Option<Canceller>>,
}

impl PackageOps {
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn cancel_all(&self) {
        if let Some(canceller) = self
            .canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            canceller.cancel_all();
        }
    }

    fn view(&self) -> JobsView {
        JobsView {
            busy: self.is_busy(),
            jobs: self.jobs.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            notes: self.notes.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    fn set_notes(&self, notes: Vec<String>) {
        *self.notes.lock().unwrap_or_else(|e| e.into_inner()) = notes;
    }
}

/// Records job events into the shared table. Runs under the queue's lock,
/// so it only takes the table's lock for an update.
struct GuiObserver {
    jobs: Arc<Mutex<Vec<JobView>>>,
}

impl QueueObserver for GuiObserver {
    fn on_event(&self, id: JobId, event: &JobEvent<'_>) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let JobEvent::Queued(summary) = event {
            jobs.push(JobView {
                id: id.0,
                kind: match summary.kind {
                    JobKind::Install => "install".into(),
                    JobKind::Remove => "remove".into(),
                },
                region: summary.region.to_uppercase(),
                package_type: summary.package_type.to_string(),
                version: summary.version.as_ref().map(|v| v.to_string()),
                stage: "Queued".into(),
                ..JobView::default()
            });
            return;
        }
        let Some(job) = jobs.iter_mut().find(|j| j.id == id.0) else {
            return;
        };
        match event {
            JobEvent::Queued(_) => {}
            JobEvent::Preparing => job.stage = "Preparing".into(),
            JobEvent::WaitingForSpace { needed, available } => {
                job.stage = "Waiting for disk space".into();
                job.message = Some(format!(
                    "needs {}, {} free",
                    format_bytes(*needed),
                    format_bytes(*available)
                ));
            }
            JobEvent::Downloading(progress) => {
                job.stage = "Downloading".into();
                job.message = None;
                job.bytes_done = progress.total_bytes_downloaded;
                job.bytes_total = progress.total_bytes;
            }
            JobEvent::Downloaded => job.stage = "Downloaded, waiting to install".into(),
            JobEvent::Installing(stage) => job.stage = stage.name().into(),
            JobEvent::Removing => job.stage = "Removing".into(),
            JobEvent::Finished(outcome) => {
                job.finished = true;
                match outcome {
                    JobOutcome::Installed(_) => {
                        job.stage = "Installed".into();
                        job.succeeded = true;
                    }
                    JobOutcome::Removed => {
                        job.stage = "Removed".into();
                        job.succeeded = true;
                    }
                    JobOutcome::Cancelled => job.stage = "Cancelled".into(),
                    JobOutcome::Failed(e) => {
                        job.stage = "Failed".into();
                        job.message = Some(e.to_string());
                    }
                }
            }
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    xearthlayer::config::format_size(bytes as usize)
}

// ============================================================================
// Reading
// ============================================================================

#[tauri::command]
pub async fn packages_installed() -> Result<InstalledList, String> {
    blocking(|| {
        let env = Environment::load()?;
        let store = LocalPackageStore::new(&env.install_location);
        let packages = if env.install_location.exists() {
            store.list().map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        Ok(InstalledList {
            install_location: env.install_location.display().to_string(),
            packages: packages
                .iter()
                .map(|p| InstalledView {
                    region: p.region().to_uppercase(),
                    package_type: p.package_type().to_string(),
                    version: p.version().to_string(),
                    path: p.path.display().to_string(),
                    size_bytes: p.size_bytes(),
                })
                .collect(),
        })
    })
    .await
}

fn fetch_library(env: &Environment) -> Result<(HttpLibraryClient, PackageLibrary), String> {
    let client = HttpLibraryClient::new();
    let url = env.library_url()?;
    let library = client
        .fetch_library(&url)
        .map_err(|e| format!("Failed to fetch the package library: {e}"))?;
    Ok((client, library))
}

fn library_rows(
    env: &Environment,
    client: &HttpLibraryClient,
    library: &PackageLibrary,
) -> Result<Vec<LibraryRowView>, String> {
    let store = LocalPackageStore::new(&env.install_location);
    let statuses = UpdateChecker::new(&store, client)
        .list_all(library)
        .map_err(|e| e.to_string())?;
    Ok(library
        .entries
        .iter()
        .map(|entry| {
            let info = statuses.iter().find(|info| {
                info.region.eq_ignore_ascii_case(&entry.title)
                    && info.package_type == entry.package_type
            });
            let (status, installed_version) = match info.map(|i| &i.status) {
                Some(PackageStatus::UpdateAvailable { installed, .. }) => {
                    (LibraryStatus::Update, Some(installed.to_string()))
                }
                Some(PackageStatus::UpToDate | PackageStatus::Orphaned { .. }) => {
                    (LibraryStatus::UpToDate, Some(entry.version.to_string()))
                }
                _ => (LibraryStatus::NotInstalled, None),
            };
            LibraryRowView {
                region: entry.title.to_uppercase(),
                package_type: entry.package_type.to_string(),
                version: entry.version.to_string(),
                status,
                installed_version,
                download_bytes: None,
            }
        })
        .collect())
}

#[tauri::command]
pub async fn packages_library() -> Result<Vec<LibraryRowView>, String> {
    blocking(|| {
        let env = Environment::load()?;
        let (client, library) = fetch_library(&env)?;
        library_rows(&env, &client, &library)
    })
    .await
}

#[tauri::command]
pub fn package_jobs(state: State<'_, AppState>) -> JobsView {
    state.packages.view()
}

#[tauri::command]
pub fn package_jobs_clear(state: State<'_, AppState>) {
    state
        .packages
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|j| !j.finished);
    state.packages.set_notes(Vec::new());
}

#[tauri::command]
pub fn package_cancel(state: State<'_, AppState>, id: u64) -> String {
    let canceller = state
        .packages
        .canceller
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match canceller.map(|c| c.cancel(JobId(id))) {
        Some(xearthlayer::manager::CancelResult::Cancelled) => "Cancelling".into(),
        Some(xearthlayer::manager::CancelResult::TooLate) => {
            "Too late to cancel: the package is being swapped or removed".into()
        }
        _ => "That job is no longer running".into(),
    }
}

// ============================================================================
// Changing
// ============================================================================

/// Why a package may not be replaced or removed right now: `run` (this app's
/// or another process's) serves every installed package through its mount.
fn refuse_while_running(state: &AppState) -> Result<(), String> {
    if state.service.is_active() {
        return Err(
            "Stop XEarthLayer on the Dashboard before updating or removing packages.".into(),
        );
    }
    let lock = xearthlayer::paths::lock_file();
    if let Some(pid) = xearthlayer::system::process::running_instance(&lock) {
        return Err(format!(
            "XEarthLayer is running (pid {pid}): stop it before updating or removing packages, \
             or delete {} if XEarthLayer is not running",
            lock.display()
        ));
    }
    Ok(())
}

/// The selected entries, each followed by its region's overlay when
/// `packages.auto_install_overlays` is on, as `packages install` plans it.
fn plan_install<'a>(
    entries: &[&'a LibraryEntry],
    library: &'a PackageLibrary,
    auto_install_overlays: bool,
) -> Vec<&'a LibraryEntry> {
    let mut planned: Vec<&LibraryEntry> = Vec::new();
    for &entry in entries {
        let overlay = (auto_install_overlays && entry.package_type == PackageType::Ortho)
            .then(|| library.find(&entry.title, PackageType::Overlay))
            .flatten();
        for e in std::iter::once(entry).chain(overlay) {
            if !planned
                .iter()
                .any(|p| p.title == e.title && p.package_type == e.package_type)
            {
                planned.push(e);
            }
        }
    }
    planned
}

#[tauri::command]
pub async fn packages_install(
    state: State<'_, AppState>,
    packages: Vec<PackageRef>,
) -> Result<(), String> {
    let refs = packages;
    let env = blocking(Environment::load).await?;
    let (env, client, metadata) = blocking(move || {
        let (client, library) = fetch_library(&env)?;
        let mut selected = Vec::new();
        for r in &refs {
            let kind = r.kind()?;
            let entry = library
                .find(&r.region, kind)
                .ok_or_else(|| format!("{} {} is not in the library", r.region, kind))?;
            selected.push(entry);
        }
        let planned = plan_install(
            &selected,
            &library,
            env.config.packages.auto_install_overlays,
        );
        let metadata = fetch_metadata(&client, &planned)?;
        Ok((env, client, metadata))
    })
    .await?;
    drop(client);

    let store = LocalPackageStore::new(&env.install_location);
    let installed = store.list_keys().unwrap_or_default();
    let replacing = metadata.iter().any(|m| {
        installed
            .iter()
            .any(|(r, t)| r.eq_ignore_ascii_case(&m.title) && *t == m.package_type)
    });
    if replacing {
        refuse_while_running(&state)?;
    }
    let jobs = metadata.into_iter().map(Job::Install).collect();
    start_operation(&state, env, jobs)
}

#[tauri::command]
pub async fn packages_update_all(state: State<'_, AppState>) -> Result<usize, String> {
    refuse_while_running(&state)?;
    let (env, metadata) = blocking(|| {
        let env = Environment::load()?;
        let (client, library) = fetch_library(&env)?;
        let rows = library_rows(&env, &client, &library)?;
        let updates: Vec<&LibraryEntry> = library
            .entries
            .iter()
            .filter(|entry| {
                rows.iter().any(|row| {
                    row.status == LibraryStatus::Update
                        && row.region.eq_ignore_ascii_case(&entry.title)
                        && row.package_type == entry.package_type.to_string()
                })
            })
            .collect();
        let metadata = fetch_metadata(&client, &updates)?;
        Ok((env, metadata))
    })
    .await?;
    let count = metadata.len();
    if count > 0 {
        start_operation(
            &state,
            env,
            metadata.into_iter().map(Job::Install).collect(),
        )?;
    }
    Ok(count)
}

#[tauri::command]
pub async fn packages_remove(
    state: State<'_, AppState>,
    packages: Vec<PackageRef>,
) -> Result<(), String> {
    refuse_while_running(&state)?;
    let env = blocking(Environment::load).await?;
    let jobs = packages
        .iter()
        .map(|r| {
            Ok(Job::Remove {
                region: r.region.to_lowercase(),
                package_type: r.kind()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    start_operation(&state, env, jobs)
}

#[tauri::command]
pub async fn packages_rebuild_overlays() -> Result<Vec<String>, String> {
    blocking(|| {
        let env = Environment::load()?;
        Ok(rebuild_overlays(&env))
    })
    .await
}

fn fetch_metadata(
    client: &HttpLibraryClient,
    entries: &[&LibraryEntry],
) -> Result<Vec<PackageMetadata>, String> {
    entries
        .iter()
        .map(|entry| {
            client.fetch_metadata(&entry.metadata_url).map_err(|e| {
                format!(
                    "Failed to fetch {} {} metadata: {e}",
                    entry.title, entry.package_type
                )
            })
        })
        .collect()
}

/// Run `jobs` through one queue on a worker thread, then rebuild the
/// consolidated overlay when an overlay changed.
fn start_operation(state: &AppState, env: Environment, jobs: Vec<Job>) -> Result<(), String> {
    if state.service.is_active() && jobs.iter().any(|j| matches!(j, Job::Remove { .. })) {
        return Err("Stop XEarthLayer on the Dashboard before removing packages.".into());
    }
    if state
        .packages
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("Another package operation is running. Wait for it to finish.".into());
    }
    state.packages.set_notes(Vec::new());
    let ops = Arc::clone(&state.packages);
    let spawned = std::thread::Builder::new()
        .name("xel-packages".into())
        .spawn(move || {
            let notes = run_operation(&ops, &env, jobs);
            ops.set_notes(notes);
            ops.busy.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        state.packages.busy.store(false, Ordering::SeqCst);
        return Err(format!("Failed to start the package worker: {e}"));
    }
    Ok(())
}

fn run_operation(ops: &PackageOps, env: &Environment, jobs: Vec<Job>) -> Vec<String> {
    let overlay_changed = jobs
        .iter()
        .any(|job| job.summary().package_type == PackageType::Overlay);
    let removed_overlays: Vec<String> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Remove {
                region,
                package_type: PackageType::Overlay,
            } => Some(region.clone()),
            _ => None,
        })
        .collect();

    let installer =
        PackageInstaller::new(LocalPackageStore::new(&env.install_location), &env.temp_dir)
            .with_parallel_downloads(env.config.package_downloads());
    let observer = Arc::new(GuiObserver {
        jobs: Arc::clone(&ops.jobs),
    });
    let queue = match InstallQueue::start(installer, observer) {
        Ok(queue) => queue,
        Err(e) => return vec![format!("Could not start: {e}")],
    };
    *ops.canceller.lock().unwrap_or_else(|e| e.into_inner()) = Some(queue.canceller());

    let mut notes = Vec::new();
    let mut ids = Vec::new();
    for job in jobs {
        let summary = job.summary();
        match queue.submit(job) {
            Ok(id) => ids.push(id),
            Err(e) => notes.push(format!(
                "{} {}: {e}",
                summary.region.to_uppercase(),
                summary.package_type
            )),
        }
    }
    let outcomes = queue.wait_for(&ids);
    // A live canceller keeps the queue's lock held, so it goes first.
    *ops.canceller.lock().unwrap_or_else(|e| e.into_inner()) = None;
    drop(queue);

    let any_succeeded = outcomes
        .iter()
        .any(|o| matches!(o, JobOutcome::Installed(_) | JobOutcome::Removed));
    if let Some(custom_scenery) = env.custom_scenery.as_deref() {
        for region in &removed_overlays {
            if let Err(e) = remove_overlay_symlink(region, custom_scenery) {
                tracing::warn!(region = %region, error = %e, "Failed to remove overlay symlink");
            }
        }
    }
    if overlay_changed && any_succeeded {
        notes.extend(rebuild_overlays(env));
    }
    if any_succeeded
        && xearthlayer::system::process::running_instance(&xearthlayer::paths::lock_file())
            .is_some()
    {
        notes.push("Restart XEarthLayer to use the new packages.".into());
    }
    notes
}

/// Rebuild the consolidated overlay folder, the same choice `xearthlayer run`
/// and `packages install` make. Returns what to tell the user.
fn rebuild_overlays(env: &Environment) -> Vec<String> {
    if env.config.packages.disable_overlays {
        return vec![
            "packages.disable_overlays is on: the consolidated overlay was not rebuilt.".into(),
        ];
    }
    let Some(scenery_path) = env.custom_scenery.as_deref() else {
        return vec!["No Custom Scenery folder is configured: overlays were not linked.".into()];
    };
    rebuild_overlays_in(env, scenery_path)
}

fn rebuild_overlays_in(env: &Environment, scenery_path: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    let store = LocalPackageStore::new(&env.install_location);
    let coverage = env
        .config
        .packages
        .xworld_pro
        .then(|| OverlayCoverage::from_detection(&detect(scenery_path)))
        .filter(|c| !c.is_empty());
    let result = match overlay_plan(false, coverage.as_ref()) {
        OverlayPlan::Except(coverage) => {
            let result = create_consolidated_overlay_except(&store, scenery_path, &|r: &str| {
                coverage.suppresses(r)
            });
            notes.push(format!(
                "X-World Pro active ({}): left out {} overlay regions",
                coverage.describe(),
                result.excluded_regions.len()
            ));
            result
        }
        _ => create_consolidated_overlay(&store, scenery_path),
    };
    if result.success {
        notes.push(format!(
            "Consolidated overlay updated: {} regions, {} files",
            result.package_count, result.file_count
        ));
    } else if let Some(error) = result.error {
        notes.push(format!(
            "Failed to update the consolidated overlay: {error}"
        ));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_refs_parse_their_type_ignoring_case() {
        let r = PackageRef {
            region: "EU".into(),
            package_type: "Ortho".into(),
        };
        assert_eq!(r.kind().unwrap(), PackageType::Ortho);
        let bad = PackageRef {
            region: "EU".into(),
            package_type: "mesh".into(),
        };
        assert!(bad.kind().is_err());
    }

    #[test]
    fn the_observer_tracks_a_job_from_queued_to_finished() {
        let jobs = Arc::new(Mutex::new(Vec::new()));
        let observer = GuiObserver {
            jobs: Arc::clone(&jobs),
        };
        let summary = Job::Remove {
            region: "eu".into(),
            package_type: PackageType::Ortho,
        }
        .summary();
        observer.on_event(JobId(7), &JobEvent::Queued(&summary));
        observer.on_event(JobId(7), &JobEvent::Removing);
        assert_eq!(jobs.lock().unwrap()[0].stage, "Removing");

        observer.on_event(JobId(7), &JobEvent::Finished(&JobOutcome::Removed));
        let job = jobs.lock().unwrap()[0].clone();
        assert_eq!((job.region.as_str(), job.kind.as_str()), ("EU", "remove"));
        assert!(job.finished && job.succeeded);
    }

    #[test]
    fn events_for_unknown_jobs_are_ignored() {
        let jobs = Arc::new(Mutex::new(Vec::new()));
        let observer = GuiObserver {
            jobs: Arc::clone(&jobs),
        };
        observer.on_event(JobId(1), &JobEvent::Preparing);
        assert!(jobs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_new_operation_table_is_idle_and_empty() {
        let ops = PackageOps::default();
        let view = ops.view();
        assert!(!view.busy && view.jobs.is_empty() && view.notes.is_empty());
    }
}
