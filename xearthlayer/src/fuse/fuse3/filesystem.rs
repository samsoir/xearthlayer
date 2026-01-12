//! Fuse3 passthrough filesystem implementation.
//!
//! This is the async multi-threaded FUSE implementation using fuse3.
//! All operations are async and run concurrently on the Tokio runtime.

use super::inode::InodeManager;
use super::shared::{DdsRequestor, FileAttrBuilder, VirtualDdsConfig, TTL};
use super::types::{Fuse3Error, Fuse3Result, MountHandle};
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::parse_dds_filename;
use crate::prefetch::TileRequestCallback;
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyInit, ReplyOpen, ReplyStatFs,
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
use tracing::{debug, trace};

/// Async multi-threaded FUSE filesystem using fuse3.
///
/// This filesystem overlays an existing scenery pack:
/// - Real files are passed through directly from the source directory
/// - DDS textures that don't exist are generated via the async pipeline
///
/// Unlike the `fuser`-based implementation, all operations are async and
/// run concurrently on the Tokio runtime. This enables parallel processing
/// of X-Plane's DDS texture requests.
///
/// # Disk I/O Concurrency
///
/// To prevent file descriptor exhaustion under heavy load, disk I/O operations
/// are protected by a semaphore-based concurrency limiter. This prevents the
/// system from opening too many file handles simultaneously when X-Plane
/// requests many files during scene loading.
pub struct Fuse3PassthroughFS {
    /// Source directory containing the scenery pack
    source_dir: PathBuf,
    /// Client for DDS generation requests (daemon architecture)
    dds_client: Arc<dyn DdsClient>,
    /// Inode manager for path/coordinate mappings
    inode_manager: InodeManager,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
    /// Limiter for concurrent disk I/O operations
    disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    /// Optional callback for tile request tracking (for FUSE inference).
    tile_request_callback: Option<TileRequestCallback>,
}

impl Fuse3PassthroughFS {
    /// Create a new fuse3 passthrough filesystem.
    ///
    /// Uses default disk I/O concurrency limits: `min(num_cpus * 16, 256)`.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(
        source_dir: PathBuf,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::with_defaults("fuse_disk_io"));
        debug!(
            max_concurrent = disk_io_limiter.max_concurrent(),
            "FUSE disk I/O concurrency limiter initialized"
        );
        Self {
            inode_manager: InodeManager::new(source_dir.clone()),
            source_dir,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
        }
    }

    /// Create a new fuse3 passthrough filesystem with custom disk I/O limits.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    /// * `disk_io_limiter` - Custom concurrency limiter for disk I/O operations
    pub fn with_disk_io_limiter(
        source_dir: PathBuf,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
        disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    ) -> Self {
        Self {
            inode_manager: InodeManager::new(source_dir.clone()),
            source_dir,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
        }
    }

    /// Set the timeout for DDS generation.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Set the callback for tile request tracking.
    ///
    /// This callback is invoked for every tile request from X-Plane, enabling
    /// FUSE-based position inference when UDP telemetry is unavailable.
    ///
    /// # Arguments
    ///
    /// * `callback` - Function to call with tile coordinates when a DDS request is made
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.tile_request_callback = Some(callback);
        self
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        &self.disk_io_limiter
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
        // Tell kernel we don't implement opendir - it should call readdir directly
        mount_options.no_open_dir_support(true);

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
        // Tell kernel we don't implement opendir - it should call readdir directly
        mount_options.no_open_dir_support(true);

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
}

// =============================================================================
// Trait Implementations for Shared FUSE Functionality
// =============================================================================

impl FileAttrBuilder for Fuse3PassthroughFS {
    fn virtual_dds_config(&self) -> &VirtualDdsConfig {
        &self.virtual_dds_config
    }
}

impl DdsRequestor for Fuse3PassthroughFS {
    fn dds_client(&self) -> &Arc<dyn DdsClient> {
        &self.dds_client
    }

    fn generation_timeout(&self) -> Duration {
        self.generation_timeout
    }

    fn context_label(&self) -> &'static str {
        "fuse3"
    }

    fn tile_request_callback(&self) -> Option<&TileRequestCallback> {
        self.tile_request_callback.as_ref()
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
            let data = self.request_dds_impl(&coords).await;
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

        // Real file - read from disk with concurrency limiting
        let path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        // Acquire disk I/O permit to prevent file descriptor exhaustion
        let _permit = self.disk_io_limiter.acquire().await;
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

        // Read directory contents with concurrency limiting
        let _permit = self.disk_io_limiter.acquire().await;
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

    /// Open a directory for reading.
    async fn opendir(
        &self,
        _req: Request,
        ino: u64,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyOpen> {
        trace!(ino = ino, "fuse3: opendir");
        // Return success with fh=0 for stateless directory I/O
        Ok(ReplyOpen { fh: 0, flags: 0 })
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
    use crate::coord::TileCoord;
    use crate::executor::{DdsClientError, Priority};
    use crate::fuse::DdsFilename;
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    /// Mock DdsClient for testing that returns invalid DDS data
    struct MockDdsClient {
        tx: mpsc::Sender<JobRequest>,
    }

    impl MockDdsClient {
        fn new() -> (Arc<Self>, mpsc::Receiver<JobRequest>) {
            let (tx, rx) = mpsc::channel(10);
            (Arc::new(Self { tx }), rx)
        }
    }

    impl DdsClient for MockDdsClient {
        fn submit(&self, request: JobRequest) -> Result<(), DdsClientError> {
            self.tx
                .try_send(request)
                .map_err(|_| DdsClientError::ChannelClosed)
        }

        fn request_dds(
            &self,
            tile: TileCoord,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (tx, rx) = oneshot::channel();
            let request = JobRequest {
                tile,
                priority: Priority::ON_DEMAND,
                cancellation,
                response_tx: Some(tx),
                origin: RequestOrigin::Fuse,
            };
            let _ = self.tx.try_send(request);
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            priority: Priority,
            origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (tx, rx) = oneshot::channel();
            let request = JobRequest {
                tile,
                priority,
                cancellation,
                response_tx: Some(tx),
                origin,
            };
            let _ = self.tx.try_send(request);
            rx
        }

        fn is_connected(&self) -> bool {
            !self.tx.is_closed()
        }
    }

    fn create_test_client() -> Arc<dyn DdsClient> {
        let (client, _rx) = MockDdsClient::new();
        client
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        use super::super::shared::chunk_to_tile_coords;

        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let tile = chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 10000); // 160000 / 16
        assert_eq!(tile.col, 5250); // 84000 / 16
        assert_eq!(tile.zoom, 16); // 20 - 4
    }

    #[test]
    fn test_virtual_dds_config() {
        let config = VirtualDdsConfig::new(11_174_016);
        assert_eq!(config.size(), 11_174_016);
        assert_eq!(config.blksize(), 4096);
        // 11_174_016 / 4096 = 2728.125, ceiling = 2729 blocks
        assert_eq!(config.blocks(), 2729);
    }

    #[tokio::test]
    async fn test_request_dds_timeout_returns_placeholder() {
        // Test that timeout returns placeholder (mock doesn't respond)
        let temp_dir = tempfile::tempdir().unwrap();
        let client = create_test_client();

        let fs = Fuse3PassthroughFS::new(temp_dir.path().to_path_buf(), client, 1024)
            .with_timeout(Duration::from_millis(100)); // Short timeout for test

        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let data = fs.request_dds_impl(&coords).await;

        // Timeout should return placeholder
        // Placeholder is 4096×4096 BC1 with 5 mipmaps = 11,174,016 bytes
        assert_eq!(data.len(), 11_174_016);
        assert_eq!(&data[0..4], b"DDS "); // Valid DDS magic
    }
}
