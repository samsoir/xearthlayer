//! A [`BaseDirectories`](super::BaseDirectories) double for tests.

use super::BaseDirectories;
use std::path::{Path, PathBuf};

/// Four distinct directories under a caller-owned root.
///
/// Public rather than `#[cfg(test)]` because the CLI crate's tests need it too,
/// and a `#[cfg(test)]` item is not visible across a crate boundary. Every test
/// that touches a path uses this, so no test reads a real home directory.
pub struct TestDirectories {
    root: PathBuf,
}

impl TestDirectories {
    /// Root the four directories at `root`, normally a `TempDir` path.
    pub fn rooted_at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl BaseDirectories for TestDirectories {
    fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }
    fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_roles_are_distinct_and_under_the_given_root() {
        let dir = tempfile::tempdir().unwrap();
        let d = TestDirectories::rooted_at(dir.path());
        let all = [d.config_dir(), d.cache_dir(), d.data_dir(), d.state_dir()];
        for path in &all {
            assert!(path.starts_with(dir.path()));
        }
        let mut unique = all.to_vec();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            4,
            "a test must be able to tell the roles apart"
        );
    }
}
