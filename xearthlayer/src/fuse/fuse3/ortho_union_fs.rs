//! Consolidated ortho union FUSE filesystem.
//!
//! This module provides a FUSE filesystem that presents all ortho sources
//! (patches AND regional packages) as a single unified view. It uses
//! [`OrthoUnionIndex`] to merge files with priority-based collision resolution.
//!
//! # Architecture
//!
//! ```text
//! ~/.xearthlayer/
//! ├── patches/                  ─┐
//! │   ├── A_KDEN_Mesh/           │
//! │   └── B_KLAX_Mesh/           │
//! └── packages/                  │   OrthoUnionIndex
//!     ├── na_ortho/              ├────────────────────► Fuse3OrthoUnionFS
//!     ├── eu_ortho/              │                              │
//!     └── sa_ortho/             ─┘                              ▼
//!                                                     FUSE Mount Point
//!                                                Custom Scenery/zzXEL_ortho/
//! ```
//!
//! # Priority Resolution
//!
//! Sources are sorted alphabetically by `sort_key`:
//! 1. Patches: `_patches/{folder_name}` (underscore sorts first)
//! 2. Packages: `{region}` (e.g., "eu", "na", "sa")
//!
//! First source wins on collision.
//!
//! # DDS Texture Generation
//!
//! When X-Plane requests a DDS texture:
//! 1. Check if the texture exists in any source → passthrough read
//! 2. If not, parse the filename for coordinates → generate via DdsHandler
//!
//! This ensures sources can include pre-built textures, but XEL generates
//! missing textures dynamically using its configured imagery provider.

use super::inode::InodeManager;
use super::shared::{DdsRequestor, FileAttrBuilder, VirtualDdsConfig, TTL};
use super::types::{Fuse3Error, Fuse3Result};
use crate::fuse::async_passthrough::DdsHandler;
use crate::fuse::{get_default_placeholder, parse_dds_filename};
use crate::ortho_union::OrthoUnionIndex;
use crate::pipeline::StorageConcurrencyLimiter;
use crate::prefetch::TileRequestCallback;
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyInit, ReplyStatFs,
};
use fuse3::raw::Filesystem;
use fuse3::{Errno, MountOptions, Result as Fuse3InternalResult};
use futures::stream::{self, BoxStream, StreamExt};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::oneshot;
use tracing::{debug, trace};

/// Consolidated ortho union FUSE filesystem.
///
/// This filesystem merges all ortho sources (patches + regional packages) into
/// a single virtual view:
///
/// - Files from sources are passed through from their real locations
/// - DDS textures that don't exist are generated via the async pipeline
/// - Priority is determined by alphabetical `sort_key` ordering
///   - Patches (`_patches/*`) always sort before packages
///   - Packages sort alphabetically by region (eu < na < sa)
///
/// # Example
///
/// ```ignore
/// use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
/// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
///
/// let index = OrthoUnionIndexBuilder::new()
///     .with_patches_dir("/home/user/.xearthlayer/patches")
///     .add_packages(installed_packages)
///     .build()?;
///
/// let fs = Fuse3OrthoUnionFS::new(index, dds_handler, expected_dds_size);
/// fs.mount_spawned("/path/to/mountpoint").await?;
/// ```
pub struct Fuse3OrthoUnionFS {
    /// Union index mapping virtual paths to real file locations
    index: Arc<OrthoUnionIndex>,
    /// Handler for DDS generation requests
    dds_handler: DdsHandler,
    /// Inode manager for path mappings
    inode_manager: InodeManager,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
    /// Limiter for concurrent disk I/O operations
    disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    /// Optional callback for tile request tracking
    tile_request_callback: Option<TileRequestCallback>,
}

impl Fuse3OrthoUnionFS {
    /// Create a new consolidated ortho union filesystem.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_handler` - Handler for DDS generation requests
    /// * `expected_dds_size` - Expected size of generated DDS files
    ///
    /// # Example
    ///
    /// ```ignore
    /// let fs = Fuse3OrthoUnionFS::new(index, dds_handler, 11_174_016);
    /// ```
    pub fn new(index: OrthoUnionIndex, dds_handler: DdsHandler, expected_dds_size: usize) -> Self {
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::with_defaults(
            "ortho_union_disk_io",
        ));
        debug!(
            max_concurrent = disk_io_limiter.max_concurrent(),
            sources = index.source_count(),
            files = index.file_count(),
            "Consolidated ortho union FUSE filesystem initialized"
        );

        // Use a virtual root path for the inode manager
        let virtual_root = PathBuf::from("/");

        Self {
            index: Arc::new(index),
            dds_handler,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
        }
    }

    /// Create with custom disk I/O limiter.
    ///
    /// Use this when you need to share a disk I/O limiter across multiple
    /// filesystems or customize the concurrency limits.
    pub fn with_disk_io_limiter(
        index: OrthoUnionIndex,
        dds_handler: DdsHandler,
        expected_dds_size: usize,
        disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    ) -> Self {
        let virtual_root = PathBuf::from("/");

        Self {
            index: Arc::new(index),
            dds_handler,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
        }
    }

    /// Set the timeout for DDS generation.
    ///
    /// After this timeout, a placeholder texture is returned to prevent
    /// X-Plane from blocking indefinitely.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Set the callback for tile request tracking.
    ///
    /// This callback is invoked whenever a tile is requested, enabling
    /// the prefetch system to infer aircraft position from FUSE requests.
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.tile_request_callback = Some(callback);
        self
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        &self.disk_io_limiter
    }

    /// Get the ortho union index.
    pub fn index(&self) -> &OrthoUnionIndex {
        &self.index
    }

    /// Mount the filesystem at the given path.
    ///
    /// This is a blocking operation that runs until the filesystem is unmounted.
    /// For non-blocking usage, see [`mount_spawned`](Self::mount_spawned).
    pub async fn mount(self, mountpoint: &str) -> Fuse3Result<super::types::MountHandle> {
        let mut mount_options = MountOptions::default();
        mount_options.read_only(true);
        mount_options.force_readdir_plus(false);

        let mount_path = PathBuf::from(mountpoint);

        #[cfg(target_os = "linux")]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount_with_unprivileged(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        #[cfg(not(target_os = "linux"))]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        Ok(super::types::MountHandle::new(handle))
    }

    /// Mount the filesystem as a spawned background task.
    ///
    /// Returns a handle that can be used to unmount the filesystem later.
    /// The filesystem runs in the background until unmounted.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = fs.mount_spawned("/path/to/mountpoint").await?;
    /// // ... filesystem is running ...
    /// handle.unmount().await?;
    /// ```
    pub async fn mount_spawned(
        self,
        mountpoint: &str,
    ) -> Fuse3Result<super::types::SpawnedMountHandle> {
        let mut mount_options = MountOptions::default();
        mount_options.read_only(true);
        mount_options.force_readdir_plus(false);

        let mount_path = PathBuf::from(mountpoint);
        let mount_path_for_handle = mount_path.clone();

        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();

        #[cfg(target_os = "linux")]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount_with_unprivileged(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        #[cfg(not(target_os = "linux"))]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        let task = tokio::spawn(async move {
            tokio::select! {
                result = handle => result,
                _ = unmount_rx => Ok(()),
            }
        });

        Ok(super::types::SpawnedMountHandle::new(
            task,
            unmount_tx,
            mount_path_for_handle,
        ))
    }

    /// Request DDS generation by filename string.
    ///
    /// Wrapper around the trait method that parses the filename first.
    async fn request_dds(&self, name_str: &str) -> Option<Vec<u8>> {
        let coords = parse_dds_filename(name_str).ok()?;
        Some(self.request_dds_impl(&coords).await)
    }
}

// =============================================================================
// Trait Implementations for Shared FUSE Functionality
// =============================================================================

impl FileAttrBuilder for Fuse3OrthoUnionFS {
    fn virtual_dds_config(&self) -> &VirtualDdsConfig {
        &self.virtual_dds_config
    }
}

impl DdsRequestor for Fuse3OrthoUnionFS {
    fn dds_handler(&self) -> &DdsHandler {
        &self.dds_handler
    }

    fn generation_timeout(&self) -> Duration {
        self.generation_timeout
    }

    fn context_label(&self) -> &'static str {
        "ortho_union"
    }

    fn tile_request_callback(&self) -> Option<&TileRequestCallback> {
        self.tile_request_callback.as_ref()
    }
}

impl Filesystem for Fuse3OrthoUnionFS {
    type DirEntryStream<'a>
        = BoxStream<'a, Fuse3InternalResult<DirectoryEntry>>
    where
        Self: 'a;
    type DirEntryPlusStream<'a>
        = BoxStream<'a, Fuse3InternalResult<DirectoryEntryPlus>>
    where
        Self: 'a;

    async fn init(&self, _req: Request) -> Fuse3InternalResult<ReplyInit> {
        debug!(
            sources = self.index.source_count(),
            files = self.index.file_count(),
            "fuse3 ortho union: init"
        );
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1024 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {
        debug!("fuse3 ortho union: destroy");
    }

    async fn lookup(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
    ) -> Fuse3InternalResult<ReplyEntry> {
        trace!(parent = parent, name = ?name, "fuse3 ortho union: lookup");

        // Get parent path (virtual path)
        let parent_path = if parent == 1 {
            PathBuf::new() // Root
        } else {
            self.inode_manager
                .get_path(parent)
                .ok_or(Errno::from(libc::ENOENT))?
        };

        let child_path = parent_path.join(name);
        let name_str = name.to_string_lossy();

        // Check if this path exists in the union index
        if let Some(source) = self.index.resolve(&child_path) {
            // Real file from a source
            if let Ok(metadata) = fs::metadata(&source.real_path).await {
                let inode = self.inode_manager.get_or_create_inode(&child_path);
                let attr = self.metadata_to_attr(inode, &metadata);
                return Ok(ReplyEntry {
                    ttl: TTL,
                    attr,
                    generation: 0,
                });
            }
        }

        // Check if it's a virtual directory in the union
        if self.index.is_directory(&child_path) {
            let inode = self.inode_manager.get_or_create_inode(&child_path);
            let attr = self.virtual_dir_attr(inode);
            return Ok(ReplyEntry {
                ttl: TTL,
                attr,
                generation: 0,
            });
        }

        // Try lazy resolution for terrain/textures directories
        // These directories are not fully scanned at startup for performance,
        // so we resolve files on-demand by checking the real filesystem.
        if let Some(real_path) = self.index.resolve_lazy(&child_path) {
            if let Ok(metadata) = fs::metadata(&real_path).await {
                let inode = self.inode_manager.get_or_create_inode(&child_path);
                let attr = self.metadata_to_attr(inode, &metadata);
                return Ok(ReplyEntry {
                    ttl: TTL,
                    attr,
                    generation: 0,
                });
            }
        }

        // Check if it's a DDS file we can generate
        if name_str.ends_with(".dds") {
            if let Ok(coords) = parse_dds_filename(&name_str) {
                let inode = self.inode_manager.create_virtual_inode(coords);
                let attr = self.virtual_dds_attr(inode);
                return Ok(ReplyEntry {
                    ttl: TTL,
                    attr,
                    generation: 0,
                });
            }
        }

        Err(Errno::from(libc::ENOENT))
    }

    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyAttr> {
        trace!(ino = ino, "fuse3 ortho union: getattr");

        // Root inode
        if ino == 1 {
            return Ok(ReplyAttr {
                ttl: TTL,
                attr: self.root_dir_attr(),
            });
        }

        // Virtual DDS inode
        if InodeManager::is_virtual_inode(ino) {
            if self.inode_manager.get_virtual_dds(ino).is_some() {
                let attr = self.virtual_dds_attr(ino);
                return Ok(ReplyAttr { ttl: TTL, attr });
            }
            return Err(Errno::from(libc::ENOENT));
        }

        // Real file or virtual directory
        let virtual_path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        // Check if it's a directory in the union
        if self.index.is_directory(&virtual_path) {
            let attr = self.virtual_dir_attr(ino);
            return Ok(ReplyAttr { ttl: TTL, attr });
        }

        // Must be a real file - try index lookup first
        if let Some(source) = self.index.resolve(&virtual_path) {
            let metadata = fs::metadata(&source.real_path)
                .await
                .map_err(|_| Errno::from(libc::ENOENT))?;
            let attr = self.metadata_to_attr(ino, &metadata);
            return Ok(ReplyAttr { ttl: TTL, attr });
        }

        // Try lazy resolution for terrain/textures directories
        if let Some(real_path) = self.index.resolve_lazy(&virtual_path) {
            let metadata = fs::metadata(&real_path)
                .await
                .map_err(|_| Errno::from(libc::ENOENT))?;
            let attr = self.metadata_to_attr(ino, &metadata);
            return Ok(ReplyAttr { ttl: TTL, attr });
        }

        Err(Errno::from(libc::ENOENT))
    }

    async fn read(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Fuse3InternalResult<ReplyData> {
        trace!(
            ino = ino,
            offset = offset,
            size = size,
            "fuse3 ortho union: read"
        );

        // Virtual DDS file - generate on demand
        if InodeManager::is_virtual_inode(ino) {
            let coords = self
                .inode_manager
                .get_virtual_dds(ino)
                .ok_or(Errno::from(libc::ENOENT))?;

            // Build filename for request_dds (use Display impl which includes correct zoom)
            let filename = format!("{}.dds", coords);

            let data = self
                .request_dds(&filename)
                .await
                .unwrap_or_else(get_default_placeholder);

            let offset = offset as usize;
            let size = size as usize;

            if offset >= data.len() {
                return Ok(ReplyData { data: Bytes::new() });
            }

            let end = std::cmp::min(offset + size, data.len());
            return Ok(ReplyData {
                data: Bytes::copy_from_slice(&data[offset..end]),
            });
        }

        // Real file from union index
        let virtual_path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        // Try to resolve the real path - first from index, then lazy
        let real_path = if let Some(source) = self.index.resolve(&virtual_path) {
            source.real_path.clone()
        } else if let Some(lazy_path) = self.index.resolve_lazy(&virtual_path) {
            lazy_path
        } else {
            return Err(Errno::from(libc::ENOENT));
        };

        // Acquire disk I/O permit
        let _permit = self.disk_io_limiter.acquire().await;
        let data = fs::read(&real_path)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        let offset = offset as usize;
        let size = size as usize;

        if offset >= data.len() {
            return Ok(ReplyData { data: Bytes::new() });
        }

        let end = std::cmp::min(offset + size, data.len());
        Ok(ReplyData {
            data: Bytes::copy_from_slice(&data[offset..end]),
        })
    }

    async fn readdir(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: i64,
    ) -> Fuse3InternalResult<ReplyDirectory<Self::DirEntryStream<'_>>> {
        trace!(ino = ino, offset = offset, "fuse3 ortho union: readdir");

        // Get virtual path for this directory
        let virtual_path = if ino == 1 {
            PathBuf::new()
        } else {
            self.inode_manager
                .get_path(ino)
                .ok_or(Errno::from(libc::ENOENT))?
        };

        // Verify it's a directory
        if ino != 1 && !self.index.is_directory(&virtual_path) {
            return Err(Errno::from(libc::ENOTDIR));
        }

        let mut entries: Vec<DirectoryEntry> = Vec::new();

        // Add . and ..
        entries.push(DirectoryEntry {
            inode: ino,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
        });

        // Parent inode
        let parent_inode = if ino == 1 {
            1 // Root's parent is itself
        } else if let Some(parent) = virtual_path.parent() {
            if parent.as_os_str().is_empty() {
                1 // Parent is root
            } else {
                self.inode_manager.get_inode(parent).unwrap_or(1)
            }
        } else {
            1
        };

        entries.push(DirectoryEntry {
            inode: parent_inode,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
        });

        // Get entries from union index
        let mut entry_offset = 3i64;
        for dir_entry in self.index.list_directory(&virtual_path) {
            let child_path = virtual_path.join(&dir_entry.name);
            let entry_inode = self.inode_manager.get_or_create_inode(&child_path);

            let kind = if dir_entry.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };

            entries.push(DirectoryEntry {
                inode: entry_inode,
                kind,
                name: dir_entry.name.clone(),
                offset: entry_offset,
            });
            entry_offset += 1;
        }

        // Skip entries based on offset
        let entries: Vec<_> = entries.into_iter().skip(offset as usize).map(Ok).collect();

        Ok(ReplyDirectory {
            entries: stream::iter(entries).boxed(),
        })
    }

    async fn access(&self, _req: Request, _ino: u64, _mask: u32) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn flush(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
    ) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn fsync(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> Fuse3InternalResult<ReplyStatFs> {
        Ok(ReplyStatFs {
            blocks: 1000000,
            bfree: 0,
            bavail: 0,
            files: self.index.file_count() as u64,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::shared::chunk_to_tile_coords;
    use super::*;
    use crate::fuse::async_passthrough::{DdsRequest, DdsResponse};
    use crate::ortho_union::OrthoUnionIndexBuilder;
    use crate::package::{InstalledPackage, Package, PackageType};
    use semver::Version;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn create_test_handler() -> DdsHandler {
        Arc::new(|req: DdsRequest| {
            let response = DdsResponse {
                data: vec![0xDD, 0x53, 0x20, 0x00],
                cache_hit: false,
                duration: Duration::from_millis(100),
            };
            let _ = req.result_tx.send(response);
        })
    }

    fn create_test_patch(temp: &TempDir, name: &str) {
        let patch_dir = temp.path().join(name);
        std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
        std::fs::write(
            patch_dir.join("Earth nav data/+30-120/+33-119.dsf"),
            b"fake dsf",
        )
        .unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();
        std::fs::write(patch_dir.join("terrain/test.ter"), b"fake terrain").unwrap();
    }

    fn create_test_package(temp: &TempDir, region: &str) -> InstalledPackage {
        let pkg_dir = temp.path().join(format!("{}_ortho", region));
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();
        std::fs::write(pkg_dir.join("terrain/package.ter"), b"pkg terrain").unwrap();

        InstalledPackage::new(
            Package::new(region, PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        )
    }

    #[test]
    fn test_ortho_union_fs_creation_with_patches() {
        let temp = TempDir::new().unwrap();
        create_test_patch(&temp, "TestPatch");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(temp.path())
            .build()
            .unwrap();

        let handler = create_test_handler();
        let fs = Fuse3OrthoUnionFS::new(index, handler, 1024);

        assert_eq!(fs.index().source_count(), 1);
        assert!(fs.index().file_count() > 0);
    }

    #[test]
    fn test_ortho_union_fs_creation_with_packages() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let handler = create_test_handler();
        let fs = Fuse3OrthoUnionFS::new(index, handler, 1024);

        assert_eq!(fs.index().source_count(), 1);
        assert!(fs.index().file_count() > 0);
    }

    #[test]
    fn test_ortho_union_fs_creation_with_both() {
        let temp = TempDir::new().unwrap();

        // Create patches directory
        let patches_dir = temp.path().join("patches");
        std::fs::create_dir_all(&patches_dir).unwrap();
        create_test_patch(&TempDir::new_in(&patches_dir).unwrap(), "TestPatch");

        // Create package
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        let handler = create_test_handler();
        let fs = Fuse3OrthoUnionFS::new(index, handler, 1024);

        // At least 1 source (package is guaranteed)
        assert!(fs.index().source_count() >= 1);
    }

    #[test]
    fn test_virtual_dds_config() {
        let config = VirtualDdsConfig::new(11_174_016);
        assert_eq!(config.size(), 11_174_016);
        assert_eq!(config.blksize(), 4096);
        assert_eq!(config.blocks(), 2729);
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        let coords = crate::fuse::DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let tile = chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 10000);
        assert_eq!(tile.col, 5250);
        assert_eq!(tile.zoom, 16);
    }

    #[test]
    fn test_with_disk_io_limiter() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let handler = create_test_handler();
        let limiter = Arc::new(StorageConcurrencyLimiter::with_defaults("test"));

        let fs = Fuse3OrthoUnionFS::with_disk_io_limiter(index, handler, 1024, limiter.clone());

        assert!(Arc::ptr_eq(fs.disk_io_limiter(), &limiter));
    }

    #[test]
    fn test_with_timeout() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let handler = create_test_handler();
        let fs = Fuse3OrthoUnionFS::new(index, handler, 1024).with_timeout(Duration::from_secs(60));

        assert_eq!(fs.generation_timeout, Duration::from_secs(60));
    }
}
