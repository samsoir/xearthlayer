//! Fuse3 passthrough filesystem implementation.
//!
//! This is the async multi-threaded FUSE implementation using fuse3.
//! All operations are async and run concurrently on the Tokio runtime.

use super::inode::InodeManager;
use super::types::{Fuse3Error, Fuse3Result, MountHandle};
use crate::coord::TileCoord;
use crate::fuse::async_passthrough::{DdsHandler, DdsRequest};
use crate::fuse::{
    get_default_placeholder, parse_dds_filename, validate_dds_or_placeholder, DdsFilename,
};
use crate::pipeline::JobId;
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, FileAttr, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyInit, ReplyStatFs,
};
use fuse3::raw::Filesystem;
use fuse3::{Errno, MountOptions, Result as Fuse3InternalResult};
use futures::stream::{self, BoxStream, StreamExt};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

/// Time-to-live for attribute caching.
const TTL: Duration = Duration::from_secs(1);

/// Configuration for virtual DDS file attributes.
struct VirtualDdsConfig {
    /// Expected size of generated DDS files
    size: u64,
    /// Block size for filesystem reporting
    blksize: u32,
}

impl VirtualDdsConfig {
    fn new(size: u64) -> Self {
        Self {
            size,
            blksize: 4096,
        }
    }

    fn blocks(&self) -> u64 {
        self.size.div_ceil(self.blksize as u64)
    }
}

/// Async multi-threaded FUSE filesystem using fuse3.
///
/// This filesystem overlays an existing scenery pack:
/// - Real files are passed through directly from the source directory
/// - DDS textures that don't exist are generated via the async pipeline
///
/// Unlike the `fuser`-based implementation, all operations are async and
/// run concurrently on the Tokio runtime. This enables parallel processing
/// of X-Plane's DDS texture requests.
pub struct Fuse3PassthroughFS {
    /// Source directory containing the scenery pack
    source_dir: PathBuf,
    /// Handler for DDS generation requests
    dds_handler: DdsHandler,
    /// Inode manager for path/coordinate mappings
    inode_manager: InodeManager,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
}

impl Fuse3PassthroughFS {
    /// Create a new fuse3 passthrough filesystem.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory
    /// * `dds_handler` - Handler function for DDS generation requests
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(source_dir: PathBuf, dds_handler: DdsHandler, expected_dds_size: usize) -> Self {
        Self {
            inode_manager: InodeManager::new(source_dir.clone()),
            source_dir,
            dds_handler,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
        }
    }

    /// Set the timeout for DDS generation.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Mount the filesystem at the given path.
    ///
    /// Returns a handle that can be used to unmount the filesystem.
    /// The filesystem is automatically unmounted when the handle is dropped.
    ///
    /// **Note**: The returned `MountHandle` must be awaited to keep the
    /// filesystem running. For background mounting, use `mount_spawned`.
    pub async fn mount(self, mountpoint: &str) -> Fuse3Result<MountHandle> {
        let mut mount_options = MountOptions::default();
        mount_options.read_only(true);
        mount_options.force_readdir_plus(false);

        let mount_path = PathBuf::from(mountpoint);

        // Use unprivileged mount on Linux
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

        Ok(MountHandle::new(handle))
    }

    /// Mount the filesystem at the given path as a spawned background task.
    ///
    /// This spawns the fuse3 mount as a Tokio task, allowing it to run in the
    /// background without blocking. The returned `SpawnedMountHandle` can be
    /// safely stored and dropped outside of an async context.
    ///
    /// This is the recommended method for use in `MountManager` where mounts
    /// need to persist while the main thread does other work.
    pub async fn mount_spawned(
        self,
        mountpoint: &str,
    ) -> Fuse3Result<super::types::SpawnedMountHandle> {
        use tokio::sync::oneshot;

        let mut mount_options = MountOptions::default();
        mount_options.read_only(true);
        mount_options.force_readdir_plus(false);

        let mount_path = PathBuf::from(mountpoint);
        let mount_path_for_handle = mount_path.clone();

        // Create channel for unmount signaling
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();

        // Mount the filesystem
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

        // Spawn task that keeps the mount alive and handles unmount signal
        let task = tokio::spawn(async move {
            tokio::select! {
                // Wait for the mount to complete (external unmount)
                result = handle => {
                    result
                }
                // Wait for unmount signal
                _ = unmount_rx => {
                    // Signal received, let the mount handle drop naturally
                    // which will trigger unmount
                    Ok(())
                }
            }
        });

        Ok(super::types::SpawnedMountHandle::new(
            task,
            unmount_tx,
            mount_path_for_handle,
        ))
    }

    /// Request DDS generation from the async pipeline.
    ///
    /// This is fully async - no blocking calls. When the FUSE timeout expires,
    /// the cancellation token is triggered to abort the pipeline processing,
    /// releasing resources (HTTP connections, semaphore permits, etc.).
    async fn request_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        let job_id = JobId::new();

        // Convert chunk coordinates to tile coordinates
        let tile = Self::chunk_to_tile_coords(coords);

        debug!(
            job_id = %job_id,
            chunk_row = coords.row,
            chunk_col = coords.col,
            chunk_zoom = coords.zoom,
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            "Requesting DDS generation (fuse3 async)"
        );

        // Create oneshot channel for response
        let (tx, rx) = oneshot::channel();

        // Create cancellation token to abort pipeline on timeout
        let cancellation_token = CancellationToken::new();

        let request = DdsRequest {
            job_id,
            tile,
            result_tx: tx,
            cancellation_token: cancellation_token.clone(),
        };

        // Submit request to the handler
        (self.dds_handler)(request);

        // Await response with timeout (fully async - no blocking!)
        let data = match tokio::time::timeout(self.generation_timeout, rx).await {
            Ok(Ok(response)) => {
                debug!(
                    job_id = %job_id,
                    cache_hit = response.cache_hit,
                    duration_ms = response.duration.as_millis(),
                    size_bytes = response.data.len(),
                    "DDS generation complete"
                );
                response.data
            }
            Ok(Err(_)) => {
                // Channel closed - sender dropped
                error!(job_id = %job_id, "DDS generation channel closed unexpectedly");
                // Cancel any in-flight work
                cancellation_token.cancel();
                get_default_placeholder()
            }
            Err(_) => {
                // Timeout - cancel the pipeline to release resources
                warn!(
                    job_id = %job_id,
                    timeout_secs = self.generation_timeout.as_secs(),
                    "DDS generation timed out - cancelling pipeline"
                );
                cancellation_token.cancel();
                get_default_placeholder()
            }
        };

        // Critical: Validate DDS before returning to X-Plane
        // Invalid DDS data causes X-Plane to crash
        let context = format!("tile({},{},{})", tile.row, tile.col, tile.zoom);
        validate_dds_or_placeholder(data, &context)
    }

    /// Convert chunk-level coordinates to tile-level coordinates.
    fn chunk_to_tile_coords(coords: &DdsFilename) -> TileCoord {
        let tile_zoom = coords.zoom.saturating_sub(4);
        TileCoord {
            row: coords.row / 16,
            col: coords.col / 16,
            zoom: tile_zoom,
        }
    }

    /// Convert std metadata to fuse3 FileAttr.
    fn metadata_to_attr(&self, ino: u64, metadata: &std::fs::Metadata) -> FileAttr {
        let kind = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        FileAttr {
            ino,
            size: metadata.len(),
            blocks: metadata.blocks(),
            atime: metadata.accessed().unwrap_or(UNIX_EPOCH).into(),
            mtime: metadata.modified().unwrap_or(UNIX_EPOCH).into(),
            ctime: UNIX_EPOCH.into(), // Creation time not available on all platforms
            kind,
            perm: (metadata.mode() & 0o7777) as u16,
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: 4096,
        }
    }

    /// Create attributes for a virtual DDS file.
    fn virtual_dds_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now().into();

        FileAttr {
            ino,
            size: self.virtual_dds_config.size,
            blocks: self.virtual_dds_config.blocks(),
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::RegularFile,
            perm: 0o444, // Read-only
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: self.virtual_dds_config.blksize,
        }
    }
}

impl Filesystem for Fuse3PassthroughFS {
    // Required: Associated types for directory streams
    type DirEntryStream<'a>
        = BoxStream<'a, Fuse3InternalResult<DirectoryEntry>>
    where
        Self: 'a;
    type DirEntryPlusStream<'a>
        = BoxStream<'a, Fuse3InternalResult<DirectoryEntryPlus>>
    where
        Self: 'a;

    /// Initialize filesystem.
    async fn init(&self, _req: Request) -> Fuse3InternalResult<ReplyInit> {
        debug!("fuse3: init");
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1024 * 1024).unwrap(), // 1MB max write
        })
    }

    /// Cleanup filesystem.
    async fn destroy(&self, _req: Request) {
        debug!("fuse3: destroy");
    }

    /// Look up a directory entry by name.
    async fn lookup(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
    ) -> Fuse3InternalResult<ReplyEntry> {
        trace!(parent = parent, name = ?name, "fuse3: lookup");

        // Get parent path
        let parent_path = self
            .inode_manager
            .get_path(parent)
            .ok_or(Errno::from(libc::ENOENT))?;

        let child_path = parent_path.join(name);
        let name_str = name.to_string_lossy();

        // Check if the file exists on disk
        if let Ok(metadata) = fs::metadata(&child_path).await {
            let inode = self.inode_manager.get_or_create_inode(&child_path);
            let attr = self.metadata_to_attr(inode, &metadata);
            return Ok(ReplyEntry {
                ttl: TTL,
                attr,
                generation: 0,
            });
        }

        // File doesn't exist - check if it's a DDS file we can generate
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

    /// Get file attributes.
    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyAttr> {
        trace!(ino = ino, "fuse3: getattr");

        // Check if it's a virtual inode
        if InodeManager::is_virtual_inode(ino) {
            if self.inode_manager.get_virtual_dds(ino).is_some() {
                let attr = self.virtual_dds_attr(ino);
                return Ok(ReplyAttr { ttl: TTL, attr });
            }
            return Err(Errno::from(libc::ENOENT));
        }

        // Real file
        let path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        let metadata = fs::metadata(&path)
            .await
            .map_err(|_| Errno::from(libc::ENOENT))?;

        let attr = self.metadata_to_attr(ino, &metadata);
        Ok(ReplyAttr { ttl: TTL, attr })
    }

    /// Read data from a file.
    async fn read(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Fuse3InternalResult<ReplyData> {
        trace!(ino = ino, offset = offset, size = size, "fuse3: read");

        // Check if it's a virtual DDS file
        if InodeManager::is_virtual_inode(ino) {
            let coords = self
                .inode_manager
                .get_virtual_dds(ino)
                .ok_or(Errno::from(libc::ENOENT))?;

            // Request DDS from the async pipeline (fully async!)
            let data = self.request_dds(&coords).await;
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

        // Real file - read from disk
        let path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        let data = fs::read(&path).await.map_err(|_| Errno::from(libc::EIO))?;

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

    /// Read directory contents.
    async fn readdir(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: i64,
    ) -> Fuse3InternalResult<ReplyDirectory<Self::DirEntryStream<'_>>> {
        trace!(ino = ino, offset = offset, "fuse3: readdir");

        let path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        if !path.is_dir() {
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

        // Get parent inode
        let parent_inode = if path == self.source_dir {
            ino // Root's parent is itself
        } else if let Some(parent) = path.parent() {
            self.inode_manager.get_inode(parent).unwrap_or(1)
        } else {
            1
        };

        entries.push(DirectoryEntry {
            inode: parent_inode,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
        });

        // Read directory contents
        let mut dir_entries = fs::read_dir(&path)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        let mut entry_offset = 3i64;
        while let Ok(Some(entry)) = dir_entries.next_entry().await {
            let entry_path = entry.path();
            let entry_name = entry.file_name();

            if let Ok(metadata) = entry.metadata().await {
                let entry_inode = self.inode_manager.get_or_create_inode(&entry_path);
                let kind = if metadata.is_dir() {
                    FileType::Directory
                } else if metadata.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };

                entries.push(DirectoryEntry {
                    inode: entry_inode,
                    kind,
                    name: entry_name,
                    offset: entry_offset,
                });
                entry_offset += 1;
            }
        }

        // Skip entries based on offset
        let entries: Vec<_> = entries.into_iter().skip(offset as usize).map(Ok).collect();

        Ok(ReplyDirectory {
            entries: stream::iter(entries).boxed(),
        })
    }

    /// Check file access permissions.
    async fn access(&self, _req: Request, _ino: u64, _mask: u32) -> Fuse3InternalResult<()> {
        // Allow all access for simplicity (read-only filesystem)
        Ok(())
    }

    /// Flush file.
    async fn flush(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
    ) -> Fuse3InternalResult<()> {
        // Read-only filesystem - nothing to flush
        Ok(())
    }

    /// Synchronize file contents.
    async fn fsync(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> Fuse3InternalResult<()> {
        // Read-only filesystem - nothing to sync
        Ok(())
    }

    /// Get filesystem statistics.
    async fn statfs(&self, _req: Request, _ino: u64) -> Fuse3InternalResult<ReplyStatFs> {
        // Return reasonable defaults for a read-only filesystem
        Ok(ReplyStatFs {
            blocks: 1000000,
            bfree: 0,
            bavail: 0,
            files: 100000,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::async_passthrough::DdsResponse;
    use std::sync::Arc;

    fn create_test_handler() -> DdsHandler {
        Arc::new(|req: DdsRequest| {
            let response = DdsResponse {
                data: vec![0xDD, 0x53, 0x20, 0x00], // Invalid DDS (wrong size/magic)
                cache_hit: false,
                duration: Duration::from_millis(100),
            };
            let _ = req.result_tx.send(response);
        })
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let tile = Fuse3PassthroughFS::chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 10000); // 160000 / 16
        assert_eq!(tile.col, 5250); // 84000 / 16
        assert_eq!(tile.zoom, 16); // 20 - 4
    }

    #[test]
    fn test_virtual_dds_config() {
        let config = VirtualDdsConfig::new(11_174_016);
        assert_eq!(config.size, 11_174_016);
        assert_eq!(config.blksize, 4096);
        // 11_174_016 / 4096 = 2728.125, ceiling = 2729 blocks
        assert_eq!(config.blocks(), 2729);
    }

    #[tokio::test]
    async fn test_request_dds_invalid_returns_placeholder() {
        // Test that invalid DDS data (wrong size/format) is replaced with placeholder
        let temp_dir = tempfile::tempdir().unwrap();
        let handler = create_test_handler(); // Returns only 4 bytes (invalid DDS)

        let fs = Fuse3PassthroughFS::new(temp_dir.path().to_path_buf(), handler, 1024);

        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let data = fs.request_dds(&coords).await;

        // Invalid DDS should be replaced with placeholder
        // Placeholder is 4096×4096 BC1 with 5 mipmaps = 11,174,016 bytes
        assert_eq!(data.len(), 11_174_016);
        assert_eq!(&data[0..4], b"DDS "); // Valid DDS magic
    }
}
