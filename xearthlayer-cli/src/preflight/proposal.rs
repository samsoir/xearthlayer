//! What the migration proposes to do with each resource.
//!
//! Pure. Deciding what to propose and asking the user about it are separate:
//! the decision table below has no terminal, no filesystem writes and no
//! prompts, so the governing invariant can be asserted directly rather than
//! inferred from what a prompt happened to render.

use std::path::{Path, PathBuf};
use xearthlayer::config::ConfigFile;
use xearthlayer::paths::{self, BaseDirectories};

/// A resource the migration has an opinion about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Config,
    Cache,
    Packages,
    Patches,
    TempDir,
    Log,
    SceneryIndex,
    OrthoUnionIndex,
    VersionCheck,
}

impl Resource {
    /// The configuration key that overrides this resource, if any.
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Resource::Cache => Some("cache.directory"),
            Resource::Packages => Some("packages.install_location"),
            Resource::Patches => Some("patches.directory"),
            Resource::TempDir => Some("packages.temp_dir"),
            Resource::Log => Some("logging.file"),
            Resource::Config
            | Resource::SceneryIndex
            | Resource::OrthoUnionIndex
            | Resource::VersionCheck => None,
        }
    }
}

/// What to do with a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Use the new platform-native location.
    Adopt,
    /// Write the current absolute path into the migrated configuration, so
    /// nothing moves and nothing breaks.
    Keep,
}

/// One row of the readback.
#[derive(Debug, Clone)]
pub struct ResourceProposal {
    pub resource: Resource,
    /// Where it is now, when that differs from the new location.
    pub pinned_to: Option<PathBuf>,
    /// Where it would go under the new layout.
    pub proposed: PathBuf,
    pub disposition: Disposition,
    /// Whether the user may change this. Resources with no configuration key
    /// have nothing to ask about.
    pub negotiable: bool,
}

impl ResourceProposal {
    /// Where the resource resolves if this proposal is accepted.
    pub fn effective_path(&self) -> Option<PathBuf> {
        match self.disposition {
            Disposition::Keep => self.pinned_to.clone(),
            Disposition::Adopt => Some(self.proposed.clone()),
        }
    }
}

/// Decide what to propose for every resource.
///
/// `exists` answers whether a path holds anything, injected so the table can be
/// asserted without a filesystem.
///
/// # The rule
///
/// **Accepting the proposal must never break a working installation.** Which
/// settles every case without further judgement:
///
/// | Situation | Proposal |
/// |---|---|
/// | Not configurable, regenerable | Adopt, silently |
/// | Absent, or worthless (log, staging) | Adopt |
/// | Real data at a legacy default | **Keep**, and offer the change |
/// | A custom location | **Keep**, and offer the change |
///
/// A user is stranded only by explicitly choosing to relocate, at which point
/// the guidance has already said what that costs. Pressing Enter is always
/// safe.
pub fn propose(
    config: &ConfigFile,
    legacy_dir: &Path,
    target: &dyn BaseDirectories,
    exists: impl Fn(&Path) -> bool,
) -> Vec<ResourceProposal> {
    let adopt = |resource, proposed| ResourceProposal {
        resource,
        pinned_to: None,
        proposed,
        disposition: Disposition::Adopt,
        negotiable: false,
    };

    // An explicit setting and an inferred legacy default are different claims
    // and get different treatment.
    //
    // A configured location is the user's answer and is kept whether or not it
    // currently exists: a drive that happens to be unmounted is not consent to
    // relocate their cache. A legacy default is only an assumption, so it is
    // kept only when something is actually there to strand.
    let negotiable = |resource: Resource,
                      configured: Option<PathBuf>,
                      legacy_default: Option<PathBuf>,
                      proposed: PathBuf| {
        let keep = configured
            .filter(|c| c != &proposed)
            .or_else(|| legacy_default.filter(|d| d != &proposed && exists(d)));
        ResourceProposal {
            resource,
            negotiable: keep.is_some(),
            disposition: if keep.is_some() {
                Disposition::Keep
            } else {
                Disposition::Adopt
            },
            pinned_to: keep,
            proposed,
        }
    };

    vec![
        adopt(Resource::Config, paths::config_file_in(target)),
        adopt(
            Resource::SceneryIndex,
            paths::scenery_index_cache_in(target),
        ),
        adopt(
            Resource::OrthoUnionIndex,
            paths::ortho_union_index_cache_in(target),
        ),
        adopt(Resource::VersionCheck, paths::version_check_file_in(target)),
        negotiable(
            Resource::Cache,
            Some(config.cache.directory.clone()),
            None,
            target.cache_dir(),
        ),
        negotiable(
            Resource::Packages,
            config.packages.install_location.clone(),
            Some(legacy_dir.join("packages")),
            paths::packages_dir_in(target),
        ),
        negotiable(
            Resource::Patches,
            config.patches.directory.clone(),
            Some(legacy_dir.join("patches")),
            paths::patches_dir_in(target),
        ),
        negotiable(
            Resource::TempDir,
            config.packages.temp_dir.clone(),
            Some(legacy_dir.join("tmp")),
            paths::temp_dir_in(target),
        ),
        // A log is worthless: there is never anything to strand, so it adopts
        // even when it sits at a legacy default.
        adopt(Resource::Log, paths::log_file_in(target)),
    ]
}

/// Write a set of proposals into a configuration.
///
/// `Keep` writes the pinned absolute path, which is what stops a resource being
/// stranded. `Adopt` writes the new location, so the migrated configuration is
/// self-describing rather than relying on defaults that may move again.
///
/// Resources with no configuration key are not represented in the file at all;
/// they resolve through [`xearthlayer::paths`] and need nothing written.
pub fn apply(proposals: &[ResourceProposal], config: &mut ConfigFile) {
    for proposal in proposals {
        let Some(path) = proposal.effective_path() else {
            continue;
        };
        match proposal.resource {
            Resource::Cache => config.cache.directory = path,
            Resource::Packages => config.packages.install_location = Some(path),
            Resource::Patches => config.patches.directory = Some(path),
            Resource::TempDir => config.packages.temp_dir = Some(path),
            Resource::Log => config.logging.file = path,
            // Not configurable: resolved through the paths module.
            Resource::Config
            | Resource::SceneryIndex
            | Resource::OrthoUnionIndex
            | Resource::VersionCheck => {}
        }
    }
}

impl Resource {
    /// How the resource is named to the user.
    pub fn label(&self) -> &'static str {
        match self {
            Resource::Config => "Configuration",
            Resource::Cache => "Tile cache",
            Resource::Packages => "Scenery packages",
            Resource::Patches => "Scenery patches",
            Resource::TempDir => "Download staging",
            Resource::Log => "Log file",
            Resource::SceneryIndex => "Scenery index cache",
            Resource::OrthoUnionIndex => "Ortho index cache",
            Resource::VersionCheck => "Update check cache",
        }
    }
}

/// Render the proposed layout for the user to read back.
///
/// Pure, so what the user is shown can be asserted. Every resource appears,
/// whether or not it is negotiable, because the question the screen answers is
/// "where will my files be", not "what are you asking me".
pub fn render(proposals: &[ResourceProposal]) -> String {
    let width = proposals
        .iter()
        .map(|p| p.resource.label().len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for p in proposals {
        let path = p.effective_path().unwrap_or_else(|| p.proposed.clone());
        let note = match p.disposition {
            Disposition::Keep => "  (kept where it is)".to_string(),
            // Naming the key tells the user what to edit if they disagree
            // later, which matters because we are not moving their data.
            Disposition::Adopt if p.negotiable => {
                format!(
                    "  (moves; set {} to keep it)",
                    p.resource.key().unwrap_or("")
                )
            }
            Disposition::Adopt => String::new(),
        };
        out.push_str(&format!(
            "  {:width$}  {}{}\n",
            p.resource.label(),
            path.display(),
            note,
            width = width
        ));
    }
    out
}

/// The resources the user may still decide about.
pub fn negotiable(proposals: &[ResourceProposal]) -> Vec<usize> {
    proposals
        .iter()
        .enumerate()
        .filter(|(_, p)| p.negotiable)
        .map(|(i, _)| i)
        .collect()
}

/// Flip a resource between keeping its current location and adopting the new
/// one.
pub fn toggle(proposal: &mut ResourceProposal) {
    proposal.disposition = match proposal.disposition {
        Disposition::Keep => Disposition::Adopt,
        Disposition::Adopt => Disposition::Keep,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFile;
    use xearthlayer::paths::testing::TestDirectories;

    fn legacy() -> PathBuf {
        PathBuf::from("/home/pilot/.xearthlayer")
    }

    fn target() -> TestDirectories {
        TestDirectories::rooted_at(Path::new("/home/pilot/native"))
    }

    /// A configuration whose defaults match `target`.
    ///
    /// `ConfigFile::default()` bakes in the *active* layout, which is the real
    /// machine's, so against a test target its values read as deliberate
    /// overrides. Aligning them is what "nothing is configured" means here.
    fn unconfigured_for(target: &TestDirectories) -> ConfigFile {
        let mut config = ConfigFile::default();
        config.cache.directory = target.cache_dir();
        config.patches.directory = Some(xearthlayer::paths::patches_dir_in(target));
        config.logging.file = xearthlayer::paths::log_file_in(target);
        config
    }

    fn find(proposals: &[ResourceProposal], r: Resource) -> &ResourceProposal {
        proposals
            .iter()
            .find(|p| p.resource == r)
            .unwrap_or_else(|| panic!("{r:?} must be in the proposal"))
    }

    #[test]
    fn the_readback_names_every_resource_and_where_it_will_be() {
        let legacy = legacy();
        let packages = legacy.join("packages");
        let exists = move |p: &Path| p == packages;
        let t = target();
        let rendered = render(&propose(&unconfigured_for(&t), &legacy, &t, exists));

        for label in [
            "Configuration",
            "Tile cache",
            "Scenery packages",
            "Log file",
            "Scenery index cache",
        ] {
            assert!(
                rendered.contains(label),
                "missing {label} from:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("kept where it is"),
            "a kept resource must say so:\n{rendered}"
        );
    }

    #[test]
    fn toggling_a_kept_resource_adopts_the_new_location() {
        let legacy = legacy();
        let packages = legacy.join("packages");
        let exists = move |p: &Path| p == packages;
        let t = target();
        let mut p = propose(&unconfigured_for(&t), &legacy, &t, exists);
        let indices = negotiable(&p);
        assert_eq!(indices.len(), 1, "only packages is negotiable here");

        let i = indices[0];
        assert_eq!(p[i].disposition, Disposition::Keep);
        toggle(&mut p[i]);
        assert_eq!(p[i].disposition, Disposition::Adopt);
        assert_eq!(
            p[i].effective_path().as_deref(),
            Some(p[i].proposed.as_path())
        );
    }

    #[test]
    fn the_configuration_always_moves_and_is_not_negotiable() {
        let t = target();
        let p = propose(&unconfigured_for(&t), &legacy(), &t, |_| false);
        let config = find(&p, Resource::Config);
        assert_eq!(config.disposition, Disposition::Adopt);
        assert!(
            !config.negotiable,
            "the config file is the one thing we relocate"
        );
    }

    #[test]
    fn regenerable_files_adopt_silently_and_are_not_negotiable() {
        let t = target();
        let p = propose(&unconfigured_for(&t), &legacy(), &t, |_| true);
        for r in [
            Resource::SceneryIndex,
            Resource::OrthoUnionIndex,
            Resource::VersionCheck,
        ] {
            let entry = find(&p, r);
            assert_eq!(entry.disposition, Disposition::Adopt, "{r:?}");
            assert!(
                !entry.negotiable,
                "{r:?} is not configurable, so there is nothing to ask"
            );
        }
    }

    #[test]
    fn a_populated_legacy_default_is_kept_so_accepting_cannot_break_anything() {
        // The governing invariant. Packages sit at the legacy default and hold
        // data, and we do not move data, so the only safe proposal is to keep.
        let legacy = legacy();
        let packages = legacy.join("packages");
        let exists = move |p: &Path| p == packages;
        let t = target();
        let p = propose(&unconfigured_for(&t), &legacy, &t, exists);

        let entry = find(&p, Resource::Packages);
        assert_eq!(entry.disposition, Disposition::Keep);
        assert!(entry.negotiable, "the user may still choose to relocate");
        assert_eq!(
            entry.pinned_to.as_deref(),
            Some(legacy.join("packages").as_path())
        );
    }

    #[test]
    fn an_empty_legacy_default_adopts_because_there_is_nothing_to_strand() {
        let t = target();
        let p = propose(&unconfigured_for(&t), &legacy(), &t, |_| false);
        assert_eq!(find(&p, Resource::Packages).disposition, Disposition::Adopt);
    }

    #[test]
    fn a_custom_location_is_kept_and_remains_negotiable() {
        let t = target();
        let mut config = unconfigured_for(&t);
        config.cache.directory = PathBuf::from("/media/Cache/xearthlayer-cache");
        let p = propose(&config, &legacy(), &t, |_| true);

        let entry = find(&p, Resource::Cache);
        assert_eq!(entry.disposition, Disposition::Keep);
        assert!(entry.negotiable);
        assert_eq!(
            entry.pinned_to.as_deref(),
            Some(Path::new("/media/Cache/xearthlayer-cache"))
        );
    }

    #[test]
    fn a_log_at_the_legacy_default_adopts_because_a_log_is_worthless() {
        let t = target();
        let mut config = unconfigured_for(&t);
        config.logging.file = legacy().join("xearthlayer.log");
        let p = propose(&config, &legacy(), &t, |_| true);
        assert_eq!(find(&p, Resource::Log).disposition, Disposition::Adopt);
    }

    #[test]
    fn accepting_every_proposal_never_strands_existing_data() {
        // The invariant, asserted directly: for a populated legacy install,
        // every resource that holds data must still resolve to it afterwards.
        let legacy = legacy();
        let populated = [legacy.join("packages"), legacy.join("patches")];
        let exists = move |p: &Path| populated.contains(&p.to_path_buf());

        // A configuration as a 0.4.x installation actually holds it. Not
        // ConfigFile::default(), which now reports the *new* layout: the
        // defaults moved when the resolver flipped, so a default-constructed
        // config no longer resembles a legacy one.
        let t = target();
        let mut config = unconfigured_for(&t);
        config.patches.directory = Some(legacy.join("patches"));
        let p = propose(&config, &legacy, &t, exists);

        for (resource, data) in [
            (Resource::Packages, legacy.join("packages")),
            (Resource::Patches, legacy.join("patches")),
        ] {
            let entry = find(&p, resource);
            assert_eq!(
                entry.effective_path().as_deref(),
                Some(data.as_path()),
                "{resource:?} would be stranded by accepting the proposal"
            );
        }
    }

    #[test]
    fn a_configured_location_is_kept_even_when_it_is_not_currently_present() {
        // A drive that happens to be unmounted is not consent to relocate the
        // user's cache. An explicit setting is their answer; only an inferred
        // legacy default needs to be checked for content.
        let t = target();
        let mut config = unconfigured_for(&t);
        config.cache.directory = PathBuf::from("/media/unmounted/cache");
        let p = propose(&config, &legacy(), &t, |_| false);
        let entry = find(&p, Resource::Cache);
        assert_eq!(entry.disposition, Disposition::Keep);
        assert_eq!(
            entry.pinned_to.as_deref(),
            Some(Path::new("/media/unmounted/cache"))
        );
    }

    #[test]
    fn nothing_is_negotiable_when_nothing_is_configured_or_populated() {
        // A pure default install has no questions to ask, so the migration can
        // complete without a terminal.
        let t = target();
        let p = propose(&unconfigured_for(&t), &legacy(), &t, |_| false);
        assert!(
            !p.iter().any(|e| e.negotiable),
            "a default install must not need a prompt: {:?}",
            p.iter()
                .filter(|e| e.negotiable)
                .map(|e| e.resource)
                .collect::<Vec<_>>()
        );
    }
}
