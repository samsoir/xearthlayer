//! File attribute conversion utilities for FUSE.
//!
//! This module provides functions to convert between filesystem metadata
//! and FUSE `FileAttr` structures.

use fuser::{FileAttr, FileType};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for virtual DDS file attributes.
#[derive(Debug, Clone)]
pub struct VirtualDdsConfig {
    /// Expected size of generated DDS files
    pub expected_size: u64,
    /// User ID for virtual files
    pub uid: u32,
    /// Group ID for virtual files
    pub gid: u32,
    /// Permission bits for virtual files
    pub perm: u16,
}

impl Default for VirtualDdsConfig {
    fn default() -> Self {
        Self {
            expected_size: 0,
            uid: 1000,
            gid: 1000,
            perm: 0o644,
        }
    }
}

impl VirtualDdsConfig {
    /// Create a new configuration with the specified expected DDS size.
    pub fn new(expected_size: u64) -> Self {
        Self {
            expected_size,
            ..Default::default()
        }
    }

    /// Set the user and group IDs.
    #[cfg(test)]
    pub fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }
}

/// Convert filesystem metadata to FUSE FileAttr.
///
/// # Arguments
///
/// * `inode` - The inode number to use
/// * `metadata` - The filesystem metadata to convert
///
/// # Returns
///
/// A `FileAttr` structure suitable for FUSE replies.
pub fn metadata_to_attr(inode: u64, metadata: &Metadata) -> FileAttr {
    let kind = if metadata.is_dir() {
        FileType::Directory
    } else if metadata.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let atime = metadata.accessed().unwrap_or(UNIX_EPOCH);
    let ctime = UNIX_EPOCH + Duration::from_secs(metadata.ctime() as u64);

    FileAttr {
        ino: inode,
        size: metadata.size(),
        blocks: metadata.blocks(),
        atime,
        mtime,
        ctime,
        crtime: mtime,
        kind,
        perm: (metadata.mode() & 0o7777) as u16,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    }
}

/// Create FileAttr for a virtual DDS file.
///
/// Virtual DDS files are generated on-demand and don't exist on disk.
/// This function creates appropriate attributes for such files.
///
/// # Arguments
///
/// * `inode` - The virtual inode number
/// * `config` - Configuration for the virtual file attributes
///
/// # Returns
///
/// A `FileAttr` structure for the virtual DDS file.
pub fn virtual_dds_attr(inode: u64, config: &VirtualDdsConfig) -> FileAttr {
    let now = SystemTime::now();
    let size = config.expected_size;

    FileAttr {
        ino: inode,
        size,
        blocks: size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::RegularFile,
        perm: config.perm,
        nlink: 1,
        uid: config.uid,
        gid: config.gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Determine the FUSE FileType from filesystem metadata.
pub fn file_type_from_metadata(metadata: &Metadata) -> FileType {
    if metadata.is_dir() {
        FileType::Directory
    } else if metadata.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use tempfile::tempdir;

    #[test]
    fn test_virtual_dds_config_default() {
        let config = VirtualDdsConfig::default();

        assert_eq!(config.expected_size, 0);
        assert_eq!(config.uid, 1000);
        assert_eq!(config.gid, 1000);
        assert_eq!(config.perm, 0o644);
    }

    #[test]
    fn test_virtual_dds_config_new() {
        let config = VirtualDdsConfig::new(11_174_016);

        assert_eq!(config.expected_size, 11_174_016);
        assert_eq!(config.uid, 1000);
        assert_eq!(config.gid, 1000);
    }

    #[test]
    fn test_virtual_dds_config_with_owner() {
        let config = VirtualDdsConfig::new(1024).with_owner(500, 500);

        assert_eq!(config.uid, 500);
        assert_eq!(config.gid, 500);
    }

    #[test]
    fn test_virtual_dds_attr() {
        let config = VirtualDdsConfig::new(11_174_016);
        let attr = virtual_dds_attr(12345, &config);

        assert_eq!(attr.ino, 12345);
        assert_eq!(attr.size, 11_174_016);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o644);
        assert_eq!(attr.nlink, 1);
    }

    #[test]
    fn test_virtual_dds_attr_blocks_calculated() {
        let config = VirtualDdsConfig::new(1024);
        let attr = virtual_dds_attr(1, &config);

        // 1024 bytes / 512 = 2 blocks
        assert_eq!(attr.blocks, 2);
    }

    #[test]
    fn test_virtual_dds_attr_blocks_rounds_up() {
        let config = VirtualDdsConfig::new(1025);
        let attr = virtual_dds_attr(1, &config);

        // 1025 bytes / 512 = 2.something, rounds up to 3
        assert_eq!(attr.blocks, 3);
    }

    #[test]
    fn test_metadata_to_attr_regular_file() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        File::create(&file_path).unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        let attr = metadata_to_attr(42, &metadata);

        assert_eq!(attr.ino, 42);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert!(attr.size == 0); // Empty file
    }

    #[test]
    fn test_metadata_to_attr_directory() {
        let temp = tempdir().unwrap();
        let dir_path = temp.path().join("subdir");
        fs::create_dir(&dir_path).unwrap();

        let metadata = fs::metadata(&dir_path).unwrap();
        let attr = metadata_to_attr(99, &metadata);

        assert_eq!(attr.ino, 99);
        assert_eq!(attr.kind, FileType::Directory);
    }

    #[test]
    fn test_metadata_to_attr_preserves_permissions() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        File::create(&file_path).unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        let attr = metadata_to_attr(1, &metadata);

        // Permissions should be preserved (masked to lower 12 bits)
        assert_eq!(attr.perm, (metadata.mode() & 0o7777) as u16);
    }

    #[test]
    fn test_file_type_from_metadata_regular() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        File::create(&file_path).unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        assert_eq!(file_type_from_metadata(&metadata), FileType::RegularFile);
    }

    #[test]
    fn test_file_type_from_metadata_directory() {
        let temp = tempdir().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        assert_eq!(file_type_from_metadata(&metadata), FileType::Directory);
    }

    #[cfg(unix)]
    #[test]
    fn test_file_type_from_metadata_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let file_path = temp.path().join("target.txt");
        let link_path = temp.path().join("link.txt");

        File::create(&file_path).unwrap();
        symlink(&file_path, &link_path).unwrap();

        let metadata = fs::symlink_metadata(&link_path).unwrap();
        assert_eq!(file_type_from_metadata(&metadata), FileType::Symlink);
    }
}
