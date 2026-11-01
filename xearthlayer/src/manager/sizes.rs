//! Recorded package sizes (#324).
//!
//! Showing an installed package's size used to mean walking every file in it:
//! 5.3 million `stat` calls for a whole installed world, repeated on every
//! package manager refresh. The installer knows the size the moment it
//! extracts a package, so it records it here, and a listing reads one small
//! file instead. A package installed before this existed is measured once,
//! the first time its size is shown, and recorded then.
//!
//! The record lives in the packages folder, beside the packages and never
//! inside one: writing into a package folder would change its modification
//! time, which the ortho union index uses to decide its cache is stale.
//!
//! An entry is trusted only while the package's metadata file is the one it
//! was recorded against (same version, same modification time), so a package
//! replaced by any means is measured again rather than shown with an old size.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::installer::PACKAGE_METADATA_FILE;

/// The record's file name, in the packages folder.
pub const SIZE_RECORD_FILE: &str = ".xearthlayer-package-sizes";

const HEADER: &str = "XEARTHLAYER PACKAGE SIZES";
const VERSION: &str = "1";

/// A record larger than this is not ours or is damaged: ignored, not read.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// What a size was recorded against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    size: u64,
    version: String,
    metadata_mtime: u64,
}

/// The modification time of a package's metadata file, in seconds.
fn metadata_mtime(package_dir: &Path) -> Option<u64> {
    fs::metadata(package_dir.join(PACKAGE_METADATA_FILE))
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// The recorded sizes for the packages in one packages folder.
#[derive(Debug, Clone)]
pub struct SizeRecord {
    path: PathBuf,
    entries: BTreeMap<String, Entry>,
}

impl SizeRecord {
    /// The record of the packages folder `root`. A missing, unreadable or
    /// damaged record reads as empty: every size is then measured.
    pub fn load(root: &Path) -> Self {
        let path = root.join(SIZE_RECORD_FILE);
        let entries = read_entries(&path).unwrap_or_default();
        Self { path, entries }
    }

    /// The recorded size of the package in `package_dir` at `version`, if
    /// one was recorded against the metadata file it has now.
    pub fn size_of(&self, package_dir: &Path, version: &str) -> Option<u64> {
        let entry = self.entries.get(&folder_name(package_dir)?)?;
        (entry.version == version && Some(entry.metadata_mtime) == metadata_mtime(package_dir))
            .then_some(entry.size)
    }

    /// Record `size` for the package in `package_dir` at `version`, and drop
    /// entries whose package folder is gone. Best effort: a record that
    /// cannot be written only means measuring again next time.
    pub fn record(&mut self, package_dir: &Path, version: &str, size: u64) {
        let (Some(name), Some(mtime)) = (folder_name(package_dir), metadata_mtime(package_dir))
        else {
            return;
        };
        self.entries.insert(
            name,
            Entry {
                size,
                version: version.to_string(),
                metadata_mtime: mtime,
            },
        );
        if let Some(root) = self.path.parent() {
            self.entries.retain(|name, _| root.join(name).is_dir());
        }
        if let Err(e) = self.save() {
            tracing::debug!(path = %self.path.display(), error = %e, "Could not record package size");
        }
    }

    fn save(&self) -> io::Result<()> {
        crate::cache::integrity::write_atomic(&self.path, |w| {
            writeln!(w, "{HEADER}")?;
            writeln!(w, "{VERSION}")?;
            for (name, e) in &self.entries {
                writeln!(
                    w,
                    "{}\t{}\t{}\t{}",
                    e.size, e.metadata_mtime, e.version, name
                )?;
            }
            Ok(())
        })
    }
}

fn folder_name(package_dir: &Path) -> Option<String> {
    package_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
}

fn read_entries(path: &Path) -> Option<BTreeMap<String, Entry>> {
    if fs::metadata(path).ok()?.len() > MAX_RECORD_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != HEADER || lines.next()? != VERSION {
        return None;
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        // A damaged line is skipped: that package is measured again.
        let mut fields = line.splitn(4, '\t');
        let (Some(size), Some(mtime), Some(version), Some(name)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(size), Ok(metadata_mtime)) = (size.parse(), mtime.parse()) else {
            continue;
        };
        entries.insert(
            name.to_string(),
            Entry {
                size,
                version: version.to_string(),
                metadata_mtime,
            },
        );
    }
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn package(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(PACKAGE_METADATA_FILE), "metadata").unwrap();
        dir
    }

    #[test]
    fn a_recorded_size_is_read_back_by_a_later_load() {
        let root = TempDir::new().unwrap();
        let na = package(root.path(), "zzXEL_na_ortho");

        SizeRecord::load(root.path()).record(&na, "1.0.0", 4096);

        assert_eq!(
            SizeRecord::load(root.path()).size_of(&na, "1.0.0"),
            Some(4096)
        );
    }

    #[test]
    fn a_size_is_not_trusted_once_the_package_changes() {
        let root = TempDir::new().unwrap();
        let na = package(root.path(), "zzXEL_na_ortho");
        let mut record = SizeRecord::load(root.path());
        record.record(&na, "1.0.0", 4096);

        assert_eq!(record.size_of(&na, "1.1.0"), None, "another version");

        // The same version reinstalled by other means: a new metadata file.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(na.join(PACKAGE_METADATA_FILE))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(record.size_of(&na, "1.0.0"), None, "metadata file replaced");
    }

    #[test]
    fn entries_for_packages_that_are_gone_are_dropped_on_the_next_write() {
        let root = TempDir::new().unwrap();
        let na = package(root.path(), "zzXEL_na_ortho");
        let eu = package(root.path(), "zzXEL_eu_ortho");
        let mut record = SizeRecord::load(root.path());
        record.record(&na, "1.0.0", 1);
        record.record(&eu, "1.0.0", 2);

        fs::remove_dir_all(&na).unwrap();
        record.record(&eu, "1.0.0", 3);

        let text = fs::read_to_string(root.path().join(SIZE_RECORD_FILE)).unwrap();
        assert!(!text.contains("zzXEL_na_ortho"), "{text}");
        assert!(text.contains("3\t"), "{text}");
    }

    #[test]
    fn a_damaged_or_foreign_record_reads_as_empty() {
        let root = TempDir::new().unwrap();
        let na = package(root.path(), "zzXEL_na_ortho");
        let path = root.path().join(SIZE_RECORD_FILE);

        fs::write(&path, "something else entirely\n").unwrap();
        assert_eq!(SizeRecord::load(root.path()).size_of(&na, "1.0.0"), None);

        let mtime = metadata_mtime(&na).unwrap();
        fs::write(
            &path,
            format!("{HEADER}\n{VERSION}\nnot-a-number\t1\t1.0.0\tzzXEL_eu_ortho\n4096\t{mtime}\t1.0.0\tzzXEL_na_ortho\n"),
        )
        .unwrap();
        assert_eq!(
            SizeRecord::load(root.path()).size_of(&na, "1.0.0"),
            Some(4096),
            "a damaged line costs only its own package"
        );
    }

    #[test]
    fn the_record_lives_beside_the_packages_not_inside_one() {
        // Writing inside a package would change its folder's modification
        // time, which invalidates the ortho union index cache.
        let root = TempDir::new().unwrap();
        let na = package(root.path(), "zzXEL_na_ortho");
        let before = fs::metadata(&na).unwrap().modified().unwrap();

        SizeRecord::load(root.path()).record(&na, "1.0.0", 4096);

        assert!(root.path().join(SIZE_RECORD_FILE).is_file());
        assert_eq!(fs::metadata(&na).unwrap().modified().unwrap(), before);
    }
}
