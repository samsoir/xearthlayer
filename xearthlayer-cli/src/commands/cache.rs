//! Cache management CLI commands.
//!
//! `clear` and `stats` touch only the disk tiers
//! ([`xearthlayer::cache::find_tiers`]), never the index caches or the
//! package staging directory that share `cache.directory` (#399). `clear`
//! refuses while something is using the cache.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use xearthlayer::cache::{clear_disk_cache, disk_cache_stats, DiskCacheUsage};
use xearthlayer::config::format_size;
use xearthlayer::manager::{queue_in_use, queue_lock_file};

use crate::commands::packages::actions::{check_removal, RunActive};
use crate::commands::packages::traits::PackageManagerService;
use crate::commands::packages::{default_temp_dir, DefaultPackageManagerService};
use crate::error::CliError;

/// Cache action subcommands.
#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Clear the disk cache: cached tiles, imagery chunks and night lighting
    Clear,
    /// Show disk cache statistics per tier
    Stats,
}

/// Run a cache subcommand.
pub fn run(action: CacheAction) -> Result<(), CliError> {
    let config = super::common::load_config()?;
    let cache_dir = &config.cache.directory;

    match action {
        CacheAction::Clear => {
            println!("Clearing disk cache at: {}", cache_dir.display());
            let removed = clear(
                cache_dir,
                &default_temp_dir(&config),
                &DefaultPackageManagerService::new(),
            )?;
            for line in usage_lines(&removed) {
                println!("{line}");
            }
            println!(
                "Deleted {} files, freed {}",
                removed.files(),
                format_size(removed.bytes() as usize)
            );
            Ok(())
        }
        CacheAction::Stats => {
            println!("Disk cache: {}", cache_dir.display());
            let stats =
                disk_cache_stats(cache_dir).map_err(|e| CliError::CacheStats(e.to_string()))?;
            for line in usage_lines(&stats) {
                println!("{line}");
            }
            Ok(())
        }
    }
}

/// What is using the cache, so it cannot be cleared now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheInUse {
    /// `xearthlayer run` is serving tiles from it.
    Run(RunActive),
    /// A package install, update or removal holds the queue lock in the
    /// package staging directory, which is inside the cache by default.
    PackageOperation { lock: PathBuf },
}

impl CacheInUse {
    /// What is running and what to do about it.
    pub fn message(&self) -> String {
        match self {
            Self::Run(run) => run.refusal("clearing the cache"),
            Self::PackageOperation { lock } => format!(
                "a package install, update or removal is running (it holds {}): \
                 let it finish, or stop it, before clearing the cache",
                lock.display()
            ),
        }
    }
}

/// Whether the cache is in use: `run` is active (the check the package
/// commands make) or a package operation holds the queue lock in
/// `temp_dir`.
fn cache_in_use(
    manager: &dyn PackageManagerService,
    temp_dir: &Path,
) -> Result<Option<CacheInUse>, CliError> {
    if let Err(run) = check_removal(manager) {
        return Ok(Some(CacheInUse::Run(run)));
    }
    let busy = queue_in_use(temp_dir).map_err(|e| CliError::CacheClear(e.to_string()))?;
    Ok(busy.then(|| CacheInUse::PackageOperation {
        lock: queue_lock_file(temp_dir),
    }))
}

/// Clear the disk tiers of `cache_dir`, unless the cache is in use.
fn clear(
    cache_dir: &Path,
    temp_dir: &Path,
    manager: &dyn PackageManagerService,
) -> Result<DiskCacheUsage, CliError> {
    if let Some(in_use) = cache_in_use(manager, temp_dir)? {
        return Err(CliError::CacheInUse(in_use.message()));
    }
    clear_disk_cache(cache_dir).map_err(|e| CliError::CacheClear(e.to_string()))
}

/// One line per tier, then the total; or a line saying there is nothing.
fn usage_lines(usage: &DiskCacheUsage) -> Vec<String> {
    if usage.tiers.is_empty() {
        return vec!["  No cached tiles".to_string()];
    }
    let rows: Vec<(String, usize, u64)> = usage
        .tiers
        .iter()
        .map(|t| (t.tier.to_string(), t.files, t.bytes))
        .chain(std::iter::once((
            "Total".to_string(),
            usage.files(),
            usage.bytes(),
        )))
        .collect();
    let label = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let files = rows
        .iter()
        .map(|r| r.1.to_string().len())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(name, n, bytes)| {
            format!(
                "  {name:<label$}  {n:>files$} files  {}",
                format_size(*bytes as usize)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::packages::test_support::{run_active, FakeManager};
    use std::fs;
    use tempfile::TempDir;
    use xearthlayer::cache::layout;
    use xearthlayer::cache::{CacheTier, TierUsage};

    /// A cache directory with one tier and the package staging directory
    /// where the package commands put it by default.
    struct Cache {
        _temp: TempDir,
        root: PathBuf,
        tmp: PathBuf,
        tile: PathBuf,
    }

    fn cache() -> Cache {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let tile = layout::dds_dir(&root, "Bing Maps").join("+37-123/tile.cache");
        fs::create_dir_all(tile.parent().unwrap()).unwrap();
        fs::write(&tile, [0u8; 64]).unwrap();
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("part.001"), [1u8; 8]).unwrap();
        Cache {
            _temp: temp,
            root,
            tmp,
            tile,
        }
    }

    fn refusal(result: Result<DiskCacheUsage, CliError>) -> String {
        match result {
            Err(CliError::CacheInUse(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn clear_refuses_while_run_is_active() {
        let cache = cache();
        let manager = FakeManager::default().with_running_instance(4242);

        let why = refusal(clear(&cache.root, &cache.tmp, &manager));

        assert_eq!(why, run_active(4242).refusal("clearing the cache"));
        assert!(cache.tile.is_file());
    }

    #[test]
    fn clear_refuses_while_a_package_operation_holds_the_queue_lock() {
        let cache = cache();
        let lock = fs::File::create(queue_lock_file(&cache.tmp)).unwrap();
        lock.try_lock().unwrap();

        let why = refusal(clear(&cache.root, &cache.tmp, &FakeManager::default()));

        assert_eq!(
            why,
            CacheInUse::PackageOperation {
                lock: queue_lock_file(&cache.tmp)
            }
            .message()
        );
        assert!(cache.tile.is_file());
        assert!(queue_lock_file(&cache.tmp).is_file());
    }

    #[test]
    fn clear_removes_the_tiers_once_nothing_uses_the_cache() {
        let cache = cache();
        // A lock file nobody holds: a package operation ran and finished.
        fs::File::create(queue_lock_file(&cache.tmp)).unwrap();

        let removed = clear(&cache.root, &cache.tmp, &FakeManager::default()).unwrap();

        assert_eq!(
            removed.tiers,
            vec![TierUsage {
                tier: CacheTier::Dds {
                    provider: "Bing Maps".into()
                },
                files: 1,
                bytes: 64,
            }]
        );
        assert!(!cache.tile.exists());
        assert!(cache.tmp.join("part.001").is_file());
        assert!(queue_lock_file(&cache.tmp).is_file());
    }

    /// `run` is named first: it is the one a user is likelier to have
    /// forgotten, and stopping it is what they need to do either way.
    #[test]
    fn run_is_named_when_both_are_using_the_cache() {
        let cache = cache();
        let lock = fs::File::create(queue_lock_file(&cache.tmp)).unwrap();
        lock.try_lock().unwrap();
        let manager = FakeManager::default().with_running_instance(4242);

        assert_eq!(
            cache_in_use(&manager, &cache.tmp).unwrap(),
            Some(CacheInUse::Run(run_active(4242)))
        );
    }

    #[test]
    fn nothing_uses_a_cache_whose_staging_directory_does_not_exist() {
        let temp = TempDir::new().unwrap();
        assert_eq!(
            cache_in_use(&FakeManager::default(), &temp.path().join("tmp")).unwrap(),
            None
        );
    }

    #[test]
    fn the_refusals_say_what_is_running_and_what_to_do() {
        assert_eq!(
            CacheInUse::Run(run_active(4242)).message(),
            run_active(4242).refusal("clearing the cache")
        );
        assert_eq!(
            CacheInUse::PackageOperation {
                lock: PathBuf::from("/cache/tmp/.queue.lock")
            }
            .message(),
            "a package install, update or removal is running (it holds \
             /cache/tmp/.queue.lock): let it finish, or stop it, before clearing the cache"
        );
    }

    #[test]
    fn usage_is_one_aligned_line_per_tier_and_a_total() {
        let usage = DiskCacheUsage {
            tiers: vec![
                TierUsage {
                    tier: CacheTier::Dds {
                        provider: "Bing Maps".into(),
                    },
                    files: 1200,
                    bytes: 3 * 1024 * 1024 * 1024,
                },
                TierUsage {
                    tier: CacheTier::NightLighting,
                    files: 7,
                    bytes: 2048,
                },
            ],
        };

        assert_eq!(
            usage_lines(&usage),
            vec![
                "  DDS tiles (Bing Maps)  1200 files  3 GB",
                "  Night lighting            7 files  2 KB",
                "  Total                  1207 files  3.0 GB",
            ]
        );
    }

    #[test]
    fn an_empty_cache_says_so() {
        assert_eq!(
            usage_lines(&DiskCacheUsage::default()),
            vec!["  No cached tiles"]
        );
    }
}
