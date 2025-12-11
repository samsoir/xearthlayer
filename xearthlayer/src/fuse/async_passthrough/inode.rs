//! Inode management for the async passthrough filesystem.
//!
//! This module provides inode allocation and mapping between:
//! - Real file paths and their inodes
//! - Virtual DDS files and their inodes
//!
//! Virtual inodes use a high base value to distinguish them from real file inodes.

use crate::fuse::DdsFilename;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Base inode for generated DDS files (virtual files).
///
/// Virtual inodes start at this value to avoid collision with real file inodes.
pub const VIRTUAL_INODE_BASE: u64 = 0x1000_0000_0000_0000;

/// Manages inode allocation and path/coordinate mappings.
///
/// This struct is thread-safe and can be shared across FUSE handler threads.
///
/// # Responsibilities
///
/// - Allocate unique inodes for real files
/// - Map paths to inodes and vice versa
/// - Create deterministic virtual inodes for DDS coordinates
/// - Track virtual inode to DDS coordinate mappings
///
/// # Example
///
/// ```ignore
/// use xearthlayer::fuse::async_passthrough::InodeManager;
/// use std::path::PathBuf;
///
/// let manager = InodeManager::new(PathBuf::from("/scenery"));
///
/// // Get inode for a real file
/// let inode = manager.get_or_create_inode(&PathBuf::from("/scenery/tile.dsf"));
///
/// // Create virtual inode for DDS
/// let coords = DdsFilename { row: 100, col: 200, zoom: 16, map_type: "BI".to_string() };
/// let virtual_inode = manager.create_virtual_inode(coords);
/// ```
pub struct InodeManager {
    /// Inode to path mapping for real files
    inode_to_path: Mutex<HashMap<u64, PathBuf>>,
    /// Path to inode mapping for real files
    path_to_inode: Mutex<HashMap<PathBuf, u64>>,
    /// Virtual inode to DDS filename mapping
    virtual_inode_to_dds: Mutex<HashMap<u64, DdsFilename>>,
    /// Next available inode for real files
    next_inode: Mutex<u64>,
}

impl InodeManager {
    /// Create a new inode manager with the given root directory.
    ///
    /// The root directory is assigned inode 1 (the FUSE root inode).
    pub fn new(root_dir: PathBuf) -> Self {
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();

        // Reserve inode 1 for root
        inode_to_path.insert(1, root_dir.clone());
        path_to_inode.insert(root_dir, 1);

        Self {
            inode_to_path: Mutex::new(inode_to_path),
            path_to_inode: Mutex::new(path_to_inode),
            virtual_inode_to_dds: Mutex::new(HashMap::new()),
            next_inode: Mutex::new(2),
        }
    }

    /// Get or create an inode for a real file path.
    ///
    /// If the path already has an inode, returns it.
    /// Otherwise, allocates a new inode and stores the mapping.
    pub fn get_or_create_inode(&self, path: &Path) -> u64 {
        let mut path_to_inode = self.path_to_inode.lock().unwrap();

        if let Some(&inode) = path_to_inode.get(path) {
            return inode;
        }

        let mut next_inode = self.next_inode.lock().unwrap();
        let inode = *next_inode;
        *next_inode += 1;

        path_to_inode.insert(path.to_path_buf(), inode);
        drop(path_to_inode);

        let mut inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.insert(inode, path.to_path_buf());

        inode
    }

    /// Get the path for a real file inode.
    ///
    /// Returns `None` if the inode is not mapped to a path.
    pub fn get_path(&self, inode: u64) -> Option<PathBuf> {
        let inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.get(&inode).cloned()
    }

    /// Get the inode for a path if it exists.
    ///
    /// Unlike `get_or_create_inode`, this does not allocate a new inode.
    pub fn get_inode(&self, path: &Path) -> Option<u64> {
        let path_to_inode = self.path_to_inode.lock().unwrap();
        path_to_inode.get(path).copied()
    }

    /// Create a virtual inode for a DDS file.
    ///
    /// The inode is deterministically computed from the coordinates,
    /// ensuring the same coordinates always produce the same inode.
    pub fn create_virtual_inode(&self, coords: DdsFilename) -> u64 {
        let inode = Self::compute_virtual_inode(&coords);

        let mut virtual_map = self.virtual_inode_to_dds.lock().unwrap();
        virtual_map.insert(inode, coords);

        inode
    }

    /// Compute the virtual inode for given coordinates.
    ///
    /// This is a pure function that deterministically maps coordinates to inodes.
    fn compute_virtual_inode(coords: &DdsFilename) -> u64 {
        VIRTUAL_INODE_BASE
            + ((coords.row as u64) << 32)
            + ((coords.col as u64) << 8)
            + (coords.zoom as u64)
    }

    /// Check if an inode is virtual (generated DDS).
    pub fn is_virtual_inode(inode: u64) -> bool {
        inode >= VIRTUAL_INODE_BASE
    }

    /// Get DDS coordinates for a virtual inode.
    ///
    /// Returns `None` if the inode is not a registered virtual inode.
    pub fn get_virtual_dds(&self, inode: u64) -> Option<DdsFilename> {
        let virtual_map = self.virtual_inode_to_dds.lock().unwrap();
        virtual_map.get(&inode).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_new_reserves_root_inode() {
        let root = PathBuf::from("/test/scenery");
        let manager = InodeManager::new(root.clone());

        assert_eq!(manager.get_path(1), Some(root.clone()));
        assert_eq!(manager.get_inode(&root), Some(1));
    }

    #[test]
    fn test_get_or_create_inode_creates_new() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let path = temp.path().join("test.dsf");
        let inode = manager.get_or_create_inode(&path);

        assert!(inode >= 2); // Inode 1 is root
        assert_eq!(manager.get_path(inode), Some(path.clone()));
        assert_eq!(manager.get_inode(&path), Some(inode));
    }

    #[test]
    fn test_get_or_create_inode_returns_existing() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let path = temp.path().join("test.dsf");
        let inode1 = manager.get_or_create_inode(&path);
        let inode2 = manager.get_or_create_inode(&path);

        assert_eq!(inode1, inode2);
    }

    #[test]
    fn test_get_or_create_inode_unique_per_path() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let path1 = temp.path().join("test1.dsf");
        let path2 = temp.path().join("test2.dsf");

        let inode1 = manager.get_or_create_inode(&path1);
        let inode2 = manager.get_or_create_inode(&path2);

        assert_ne!(inode1, inode2);
    }

    #[test]
    fn test_get_path_returns_none_for_unknown() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        assert_eq!(manager.get_path(999), None);
    }

    #[test]
    fn test_get_inode_returns_none_for_unknown() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let unknown_path = temp.path().join("unknown.dsf");
        assert_eq!(manager.get_inode(&unknown_path), None);
    }

    #[test]
    fn test_is_virtual_inode() {
        assert!(!InodeManager::is_virtual_inode(1));
        assert!(!InodeManager::is_virtual_inode(1000));
        assert!(!InodeManager::is_virtual_inode(VIRTUAL_INODE_BASE - 1));
        assert!(InodeManager::is_virtual_inode(VIRTUAL_INODE_BASE));
        assert!(InodeManager::is_virtual_inode(VIRTUAL_INODE_BASE + 1));
        assert!(InodeManager::is_virtual_inode(u64::MAX));
    }

    #[test]
    fn test_create_virtual_inode() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let coords = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };

        let inode = manager.create_virtual_inode(coords.clone());

        assert!(InodeManager::is_virtual_inode(inode));
        assert_eq!(manager.get_virtual_dds(inode), Some(coords));
    }

    #[test]
    fn test_create_virtual_inode_deterministic() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let coords1 = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };
        let coords2 = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };

        let inode1 = manager.create_virtual_inode(coords1);
        let inode2 = manager.create_virtual_inode(coords2);

        assert_eq!(inode1, inode2);
    }

    #[test]
    fn test_create_virtual_inode_unique_for_different_coords() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        let coords1 = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };
        let coords2 = DdsFilename {
            row: 100,
            col: 201,
            zoom: 16,
            map_type: "BI".to_string(),
        };

        let inode1 = manager.create_virtual_inode(coords1);
        let inode2 = manager.create_virtual_inode(coords2);

        assert_ne!(inode1, inode2);
    }

    #[test]
    fn test_get_virtual_dds_returns_none_for_unknown() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        assert_eq!(manager.get_virtual_dds(VIRTUAL_INODE_BASE + 999), None);
    }

    #[test]
    fn test_virtual_and_real_inodes_dont_collide() {
        let temp = tempdir().unwrap();
        let manager = InodeManager::new(temp.path().to_path_buf());

        // Create many real inodes
        for i in 0..1000 {
            let path = temp.path().join(format!("file{}.dsf", i));
            let inode = manager.get_or_create_inode(&path);
            assert!(!InodeManager::is_virtual_inode(inode));
        }

        // Create virtual inode
        let coords = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };
        let virtual_inode = manager.create_virtual_inode(coords);
        assert!(InodeManager::is_virtual_inode(virtual_inode));
    }
}
