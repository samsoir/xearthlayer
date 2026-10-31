//! Package decisions shared by the CLI handlers and the package manager TUI.
//!
//! Nothing here prints or prompts: every function takes what it needs and
//! returns what the caller should show or act on, so the same rules apply
//! whether the user typed `xearthlayer packages install` or pressed a key.

use std::path::{Path, PathBuf};

use semver::Version;
use xearthlayer::manager::{
    create_consolidated_overlay, create_consolidated_overlay_except, remove_overlay_symlink,
    LocalPackageStore, ManagerResult, PackageStatus,
};
use xearthlayer::xworld_pro::{detect, overlay_plan, OverlayCoverage, OverlayPlan};
use xearthlayer_package::{LibraryEntry, PackageLibrary, PackageType};

use super::traits::PackageManagerService;

// ============================================================================
// Library status
// ============================================================================

/// Where a library package stands against what is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryStatus {
    NotInstalled,
    UpToDate,
    /// Installed at an older version than the library offers.
    Update {
        installed: Version,
    },
}

/// One library package and its status.
#[derive(Debug, Clone)]
pub struct LibraryRow {
    pub entry: LibraryEntry,
    pub status: LibraryStatus,
}

/// Every package the library offers, in library order, with its status.
/// Fails when the update check does, rather than showing every package as
/// not installed.
pub fn library_status(
    manager: &dyn PackageManagerService,
    store: &LocalPackageStore,
    library: &PackageLibrary,
) -> ManagerResult<Vec<LibraryRow>> {
    let statuses = manager.try_check_updates(store, library)?;
    Ok(library
        .entries
        .iter()
        .map(|entry| {
            let status = statuses
                .iter()
                .find(|(info, _)| {
                    info.region.eq_ignore_ascii_case(&entry.title)
                        && info.package_type == entry.package_type
                })
                .map(|(_, status)| status);
            LibraryRow {
                entry: entry.clone(),
                status: match status {
                    Some(PackageStatus::UpdateAvailable { installed, .. }) => {
                        LibraryStatus::Update {
                            installed: installed.clone(),
                        }
                    }
                    Some(PackageStatus::UpToDate | PackageStatus::Orphaned { .. }) => {
                        LibraryStatus::UpToDate
                    }
                    Some(PackageStatus::NotInstalled { .. }) | None => LibraryStatus::NotInstalled,
                },
            }
        })
        .collect())
}

// ============================================================================
// Install planning
// ============================================================================

/// The packages to install: each selected entry, followed by its region's
/// overlay when auto-install is on and the library has one.
pub fn plan_install<'a>(
    entries: &[&'a LibraryEntry],
    library: &'a PackageLibrary,
    auto_install_overlays: bool,
) -> Vec<&'a LibraryEntry> {
    entries
        .iter()
        .flat_map(|&entry| {
            let overlay = (auto_install_overlays && entry.package_type == PackageType::Ortho)
                .then(|| library.find(&entry.title, PackageType::Overlay))
                .flatten();
            std::iter::once(entry).chain(overlay)
        })
        .collect()
}

// ============================================================================
// Refusals while `xearthlayer run` is serving packages
// ============================================================================

/// Another `xearthlayer run` (`pid`) is serving the installed packages, as
/// its instance lock (`lock`) says.
///
/// `run` keeps every installed package's files open through one union
/// filesystem, so a package cannot change underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunActive {
    pub pid: u32,
    pub lock: PathBuf,
}

impl RunActive {
    /// Why the job was refused, and what to do about it. The lock outlives
    /// a `run` that exits and its pid may be reused, so a stale lock is
    /// named for the user to delete.
    pub fn message(&self) -> String {
        format!(
            "XEarthLayer is running (pid {}): stop it before updating or removing packages, \
             or delete {} if XEarthLayer is not running",
            self.pid,
            self.lock.display()
        )
    }

    /// The refusal for a status line, which has no room for the lock path.
    /// The details pane carries the full [`message`](Self::message).
    pub fn short_message(&self) -> String {
        format!("XEarthLayer is running (pid {}): see Details", self.pid)
    }
}

/// Removing a package is refused while `run` serves it.
pub fn check_removal(manager: &dyn PackageManagerService) -> Result<(), RunActive> {
    match manager.running_instance() {
        Some(pid) => Err(RunActive {
            pid,
            lock: manager.instance_lock(),
        }),
        None => Ok(()),
    }
}

/// Replacing an installed package (an update, or an install over an installed
/// package) is refused while `run` serves it. New packages are not replacements.
pub fn check_replacement(manager: &dyn PackageManagerService) -> Result<(), RunActive> {
    check_removal(manager)
}

/// Said beside a package installed while `run` is serving: `run` picks it up
/// on its next start.
pub const RESTART_NOTE: &str = "restart xearthlayer run to use it";

// ============================================================================
// Overlays
// ============================================================================

/// Clean up after a package was removed: an overlay's stale per-region symlink
/// in Custom Scenery goes with it. Only logs, and only called once the removal
/// succeeded: a refused removal leaves the package installed, and it must keep
/// its symlink.
pub fn after_removal(region: &str, package_type: PackageType, custom_scenery_path: Option<&Path>) {
    let (PackageType::Overlay, Some(custom_scenery_path)) = (package_type, custom_scenery_path)
    else {
        return;
    };
    match remove_overlay_symlink(region, custom_scenery_path) {
        Ok(true) => {
            tracing::info!(region = %region, "Removed stale per-region overlay symlink");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(region = %region, error = %e, "Failed to remove per-region overlay symlink");
        }
    }
}

/// What rebuilding the consolidated overlay did, for the caller to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayReport {
    /// Notes and progress, in order.
    pub lines: Vec<String>,
    /// How the rebuild ended: the success summary, or on `failed` the error.
    /// `None` when nothing was rebuilt.
    pub outcome: Option<String>,
    /// The rebuild was attempted and failed.
    pub failed: bool,
}

impl OverlayReport {
    fn note(lines: &[&str]) -> Self {
        Self {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            ..Self::default()
        }
    }
}

/// Rebuild the consolidated overlay folder after an overlay install, remove or
/// update.
///
/// This creates a single `yzXEL_overlay/` folder in Custom Scenery containing
/// symlinks to all DSF files from installed overlay packages. Any stale
/// per-region symlinks (e.g. `yzXEL_na_overlay`) are automatically cleaned up.
pub fn rebuild_overlays(
    manager: &dyn PackageManagerService,
    install_dir: &Path,
    custom_scenery_path: Option<&Path>,
    disable_overlays: bool,
    xworld_pro: bool,
) -> OverlayReport {
    if disable_overlays {
        // Per `packages.disable_overlays` the consolidated overlay is suppressed
        // at runtime. We deliberately leave any existing folder alone here; the
        // next `xearthlayer run` removes it during startup. Issue #152.
        return OverlayReport::note(&[
            "Note: packages.disable_overlays = true; skipping consolidated overlay rebuild. \
             The folder will be removed on next 'xearthlayer run'.",
        ]);
    }
    let Some(scenery_path) = custom_scenery_path else {
        return OverlayReport::note(&[
            "Note: No Custom Scenery path configured. Overlay symlink not created.",
            "Set 'custom_scenery_path' in config to enable automatic overlay creation.",
        ]);
    };

    let mut report = OverlayReport::note(&["Rebuilding consolidated overlay..."]);
    let store = manager.create_store(install_dir);
    // The same choice `xearthlayer run` makes at startup.
    let coverage = xworld_pro
        .then(|| OverlayCoverage::from_detection(&detect(scenery_path)))
        .filter(|c| !c.is_empty());
    let result = match overlay_plan(disable_overlays, coverage.as_ref()) {
        OverlayPlan::Except(coverage) => {
            let result = create_consolidated_overlay_except(&store, scenery_path, &|r: &str| {
                coverage.suppresses(r)
            });
            report.lines.push(format!(
                "X-World Pro active ({}): left out {} overlay regions",
                coverage.describe(),
                result.excluded_regions.len()
            ));
            result
        }
        _ => create_consolidated_overlay(&store, scenery_path),
    };
    if result.success {
        report.outcome = Some(format!(
            "Consolidated overlay updated: {} regions, {} files",
            result.package_count, result.file_count
        ));
    } else if let Some(error) = result.error {
        report.failed = true;
        report.outcome = Some(format!(
            "Warning: Failed to create consolidated overlay: {}",
            error
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use semver::Version;
    use xearthlayer::manager::PackageStatus;
    use xearthlayer_package::PackageType;

    use super::super::test_support::{install_dir, run_active, FakeManager};
    use super::*;

    // ------------------------------------------------------------------------
    // library_status
    // ------------------------------------------------------------------------

    #[test]
    fn library_status_gives_each_entry_its_state() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["AF-NORTH", "EU-WEST", "NA-EAST"])
            .with_status(
                PackageType::Ortho,
                "AF-NORTH",
                PackageStatus::NotInstalled {
                    available: Version::new(1, 0, 0),
                },
            )
            .with_status(PackageType::Ortho, "EU-WEST", PackageStatus::UpToDate)
            .with_status(
                PackageType::Ortho,
                "NA-EAST",
                PackageStatus::UpdateAvailable {
                    installed: Version::new(0, 9, 0),
                    available: Version::new(1, 0, 0),
                },
            );
        let library = manager.fetch_library("").unwrap();
        let store = manager.create_store(&install_dir());

        let rows = library_status(&manager, &store, &library).unwrap();

        let states: Vec<(&str, &LibraryStatus)> = rows
            .iter()
            .map(|row| (row.entry.title.as_str(), &row.status))
            .collect();
        assert_eq!(
            states,
            [
                ("AF-NORTH", &LibraryStatus::NotInstalled),
                ("EU-WEST", &LibraryStatus::UpToDate),
                (
                    "NA-EAST",
                    &LibraryStatus::Update {
                        installed: Version::new(0, 9, 0)
                    }
                ),
            ]
        );
    }

    #[test]
    fn library_status_tells_ortho_and_overlay_of_one_region_apart() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST"])
            .with_library(PackageType::Overlay, &["EU-WEST"])
            .with_status(PackageType::Ortho, "EU-WEST", PackageStatus::UpToDate)
            .with_status(
                PackageType::Overlay,
                "EU-WEST",
                PackageStatus::NotInstalled {
                    available: Version::new(1, 0, 0),
                },
            );
        let library = manager.fetch_library("").unwrap();
        let store = manager.create_store(&install_dir());

        let rows = library_status(&manager, &store, &library).unwrap();

        assert_eq!(rows[0].status, LibraryStatus::UpToDate);
        assert_eq!(rows[1].status, LibraryStatus::NotInstalled);
    }

    #[test]
    fn library_status_fails_when_the_update_check_does() {
        // Shown as everything "not installed" otherwise, which is not true.
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST"])
            .with_failing_update_check();
        let library = manager.fetch_library("").unwrap();
        let store = manager.create_store(&install_dir());

        let result = library_status(&manager, &store, &library);

        assert!(result.is_err());
    }

    #[test]
    fn check_updates_still_gives_nothing_when_the_check_fails() {
        // The CLI's behaviour, kept on top of try_check_updates.
        let manager = FakeManager::default()
            .with_status(PackageType::Ortho, "EU-WEST", PackageStatus::UpToDate)
            .with_failing_update_check();
        let library = manager.fetch_library("").unwrap();
        let store = manager.create_store(&install_dir());

        assert!(manager.check_updates(&store, &library).is_empty());
        assert!(manager.try_check_updates(&store, &library).is_err());
    }

    // ------------------------------------------------------------------------
    // plan_install
    // ------------------------------------------------------------------------

    fn plan_titles(
        manager: &FakeManager,
        selected: &[(&str, PackageType)],
        auto_overlays: bool,
    ) -> Vec<String> {
        let library = manager.fetch_library("").unwrap();
        let entries: Vec<_> = selected
            .iter()
            .map(|(title, t)| library.find(title, *t).unwrap())
            .collect();
        plan_install(&entries, &library, auto_overlays)
            .iter()
            .map(|e| format!("{} ({})", e.title, e.package_type))
            .collect()
    }

    #[test]
    fn plan_install_follows_each_ortho_with_its_overlay_when_auto_is_on() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST", "AF-SOUTH"])
            .with_library(PackageType::Overlay, &["EU-WEST"]);

        let plan = plan_titles(
            &manager,
            &[
                ("EU-WEST", PackageType::Ortho),
                ("AF-SOUTH", PackageType::Ortho),
            ],
            true,
        );

        assert_eq!(
            plan,
            ["EU-WEST (ortho)", "EU-WEST (overlay)", "AF-SOUTH (ortho)"]
        );
    }

    #[test]
    fn plan_install_adds_nothing_when_auto_is_off() {
        let manager = FakeManager::default()
            .with_library(PackageType::Ortho, &["EU-WEST"])
            .with_library(PackageType::Overlay, &["EU-WEST"]);

        let plan = plan_titles(&manager, &[("EU-WEST", PackageType::Ortho)], false);

        assert_eq!(plan, ["EU-WEST (ortho)"]);
    }

    #[test]
    fn plan_install_gives_an_overlay_no_overlay_of_its_own() {
        let manager = FakeManager::default().with_library(PackageType::Overlay, &["EU-WEST"]);

        let plan = plan_titles(&manager, &[("EU-WEST", PackageType::Overlay)], true);

        assert_eq!(plan, ["EU-WEST (overlay)"]);
    }

    // ------------------------------------------------------------------------
    // run-active refusals
    // ------------------------------------------------------------------------

    #[test]
    fn removal_and_replacement_are_allowed_when_run_is_not_serving() {
        let manager = FakeManager::default();

        assert_eq!(check_removal(&manager), Ok(()));
        assert_eq!(check_replacement(&manager), Ok(()));
    }

    #[test]
    fn removal_and_replacement_are_refused_while_run_is_serving() {
        let manager = FakeManager::default().with_running_instance(4242);

        assert_eq!(check_removal(&manager), Err(run_active(4242)));
        assert_eq!(check_replacement(&manager), Err(run_active(4242)));
    }

    /// The lock outlives a `run` that exits, and its pid may be reused, so
    /// the refusal says how to clear a stale one.
    #[test]
    fn the_refusal_names_the_process_the_lock_and_what_to_do() {
        let run = RunActive {
            pid: 4242,
            lock: PathBuf::from("/state/xearthlayer.lock"),
        };
        assert_eq!(
            run.message(),
            "XEarthLayer is running (pid 4242): stop it before updating or removing packages, \
             or delete /state/xearthlayer.lock if XEarthLayer is not running"
        );
    }

    /// The status line has little room, and the lock path is the long part
    /// of the full refusal, so its short form leaves it to the details pane.
    #[test]
    fn the_short_refusal_names_the_process_and_where_the_rest_is() {
        let run = RunActive {
            pid: 4242,
            lock: PathBuf::from("/state/xearthlayer.lock"),
        };
        assert_eq!(
            run.short_message(),
            "XEarthLayer is running (pid 4242): see Details"
        );
    }

    // ------------------------------------------------------------------------
    // after_removal
    // ------------------------------------------------------------------------

    fn symlink_for(region: &str, scenery: &Path) -> std::path::PathBuf {
        let target = scenery.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = scenery.join(xearthlayer_package::package_mountpoint(
            region,
            PackageType::Overlay,
        ));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        link
    }

    #[test]
    fn after_removal_clears_the_per_region_overlay_symlink() {
        let scenery = tempfile::tempdir().unwrap();
        let link = symlink_for("NA", scenery.path());

        after_removal("NA", PackageType::Overlay, Some(scenery.path()));

        assert!(link.symlink_metadata().is_err());
    }

    #[test]
    fn after_removal_leaves_an_ortho_removal_alone() {
        let scenery = tempfile::tempdir().unwrap();
        let link = symlink_for("NA", scenery.path());

        after_removal("NA", PackageType::Ortho, Some(scenery.path()));

        assert!(link.symlink_metadata().is_ok());
    }

    #[test]
    fn after_removal_without_a_custom_scenery_path_does_nothing() {
        after_removal("NA", PackageType::Overlay, None);
    }

    // ------------------------------------------------------------------------
    // rebuild_overlays
    // ------------------------------------------------------------------------

    fn overlay_package(packages: &Path, region: &str) {
        let folder = xearthlayer_package::package_mountpoint(region, PackageType::Overlay);
        let dir = packages.join(&folder);
        let grid = dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&grid).unwrap();
        std::fs::write(
            grid.join(if region == "na" {
                "+33-119.dsf"
            } else {
                "+34-118.dsf"
            }),
            "dsf",
        )
        .unwrap();
        std::fs::write(
            dir.join("xearthlayer_scenery_package.txt"),
            format!(
                "REGIONAL SCENERY PACKAGE\n1.0.0\n{}  1.0.0\n2024-01-01T00:00:00Z\nY\n{folder}\n\
                 test.tar.gz\n1\n\nabc123  test.tar.gz  http://example.com/test.tar.gz\n",
                region.to_uppercase()
            ),
        )
        .unwrap();
    }

    /// Rebuild with an installed NA and EU overlay and X-World Pro Europe
    /// enabled in `scenery_packs.ini`.
    fn rebuild(disable_overlays: bool, xworld_pro: bool) -> OverlayReport {
        let packages = tempfile::tempdir().unwrap();
        overlay_package(packages.path(), "na");
        overlay_package(packages.path(), "eu");
        let scenery = tempfile::tempdir().unwrap();
        let cs = scenery.path().join("Custom Scenery");
        std::fs::create_dir_all(cs.join("simHeaven_X-WORLD-Pro_Europe-09-scenery")).unwrap();
        std::fs::write(
            cs.join("scenery_packs.ini"),
            "SCENERY_PACK Custom Scenery/simHeaven_X-WORLD-Pro_Europe-09-scenery/\n",
        )
        .unwrap();
        rebuild_overlays(
            &FakeManager::default(),
            packages.path(),
            Some(&cs),
            disable_overlays,
            xworld_pro,
        )
    }

    #[test]
    fn rebuild_reports_the_disable_overlays_note_and_builds_nothing() {
        let report = rebuild(true, true);

        let text = report.lines.join("\n");
        assert!(
            text.contains("skipping consolidated overlay rebuild"),
            "{text}"
        );
        assert!(!text.contains("X-World Pro active"), "{text}");
        assert_eq!(report.outcome, None);
        assert!(!report.failed);
    }

    #[test]
    fn rebuild_without_a_custom_scenery_path_says_so() {
        let report = rebuild_overlays(&FakeManager::default(), &install_dir(), None, false, false);

        let text = report.lines.join("\n");
        assert!(text.contains("No Custom Scenery path configured"), "{text}");
        assert!(
            text.contains("Set 'custom_scenery_path' in config"),
            "{text}"
        );
        assert_eq!(report.outcome, None);
        assert!(!report.failed);
    }

    #[test]
    fn rebuild_reports_the_x_world_pro_line_and_leaves_its_continent_out() {
        let report = rebuild(false, true);

        let text = report.lines.join("\n");
        assert!(text.contains("X-World Pro active (Europe)"), "{text}");
        assert!(text.contains("left out 1 overlay regions"), "{text}");
        assert_eq!(
            report.outcome.as_deref(),
            Some("Consolidated overlay updated: 1 regions, 1 files")
        );
        assert!(!report.failed);
    }

    #[test]
    fn rebuild_with_x_world_pro_off_builds_every_region() {
        let report = rebuild(false, false);

        assert_eq!(
            report.outcome.as_deref(),
            Some("Consolidated overlay updated: 2 regions, 2 files")
        );
        assert!(!report.failed);
    }
}
