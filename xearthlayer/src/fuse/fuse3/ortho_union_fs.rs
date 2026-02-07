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
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::coalesce::RequestCoalescer;
use crate::fuse::{get_default_placeholder, parse_dds_filename};
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::{DdsAccessEvent, DsfTileCoord, FuseLoadMonitor, TileRequestCallback};
use crate::scene_tracker::{DdsTileCoord, FuseAccessEvent};
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEntry, ReplyInit, ReplyOpen, ReplyStatFs,
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
use tokio::sync::mpsc;
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
    /// Client for DDS generation requests (new daemon architecture)
    dds_client: Arc<dyn DdsClient>,
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
    /// Request coalescer for deduplicating concurrent requests
    request_coalescer: Arc<RequestCoalescer>,
    /// Optional channel for notifying prefetcher of DDS accesses.
    ///
    /// When set, the filesystem sends a [`DdsAccessEvent`] for each DDS
    /// texture request, enabling the tile-based prefetcher to track
    /// which DSF tiles X-Plane is loading.
    dds_access_tx: Option<mpsc::UnboundedSender<DdsAccessEvent>>,
    /// Optional channel for notifying Scene Tracker of DDS accesses.
    ///
    /// When set, the filesystem sends a [`FuseAccessEvent`] for each DDS
    /// texture request, enabling the Scene Tracker to build an empirical
    /// model of what X-Plane has requested.
    ///
    /// Unlike `dds_access_tx` which sends derived DSF regions, this channel
    /// sends raw DDS tile coordinates (row, col, zoom) for Scene Tracker
    /// to store as empirical data.
    scene_tracker_tx: Option<mpsc::UnboundedSender<FuseAccessEvent>>,
    /// Optional load monitor for circuit breaker integration.
    ///
    /// When set, records each DDS request so the circuit breaker can
    /// detect when X-Plane is heavily loading scenery and pause prefetching.
    load_monitor: Option<Arc<dyn FuseLoadMonitor>>,
    /// Optional metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics.
    metrics_client: Option<crate::metrics::MetricsClient>,
}

impl Fuse3OrthoUnionFS {
    /// Create a new consolidated ortho union filesystem.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    ///
    /// # Example
    ///
    /// ```ignore
    /// let fs = Fuse3OrthoUnionFS::new(index, dds_client, 11_174_016);
    /// ```
    pub fn new(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::with_defaults(
            "ortho_union_disk_io",
        ));
        let request_coalescer = Arc::new(RequestCoalescer::new());
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
            dds_client,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            dds_access_tx: None,
            scene_tracker_tx: None,
            load_monitor: None,
            metrics_client: None,
        }
    }

    /// Create with custom disk I/O limiter.
    ///
    /// Use this when you need to share a disk I/O limiter across multiple
    /// filesystems or customize the concurrency limits.
    pub fn with_disk_io_limiter(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
        disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    ) -> Self {
        let virtual_root = PathBuf::from("/");
        let request_coalescer = Arc::new(RequestCoalescer::new());

        Self {
            index: Arc::new(index),
            dds_client,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            dds_access_tx: None,
            scene_tracker_tx: None,
            load_monitor: None,
            metrics_client: None,
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

    /// Set the channel for DDS access events.
    ///
    /// When set, the filesystem sends a [`DdsAccessEvent`] for each DDS
    /// texture accessed. This enables the tile-based prefetcher to track
    /// which DSF tiles X-Plane is actively loading.
    ///
    /// The channel is fire-and-forget: sending is non-blocking and failures
    /// are silently ignored to avoid impacting FUSE performance.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (tx, rx) = mpsc::unbounded_channel();
    /// let fs = Fuse3OrthoUnionFS::new(index, client, size)
    ///     .with_dds_access_channel(tx);
    /// // rx is passed to AdaptivePrefetchCoordinator for DSF tile tracking
    /// ```
    pub fn with_dds_access_channel(mut self, tx: mpsc::UnboundedSender<DdsAccessEvent>) -> Self {
        self.dds_access_tx = Some(tx);
        self
    }

    /// Set the channel for Scene Tracker events.
    ///
    /// When set, the filesystem sends a [`FuseAccessEvent`] for each DDS
    /// texture accessed. This enables the Scene Tracker to build an empirical
    /// model of X-Plane's requests, which can be used for position inference
    /// and prefetch prediction.
    ///
    /// Unlike the prefetcher channel which sends derived DSF regions, this
    /// channel sends raw DDS tile coordinates (row, col, zoom) - the Scene
    /// Tracker stores empirical data and derives regions via calculation.
    ///
    /// The channel is fire-and-forget: sending is non-blocking and failures
    /// are silently ignored to avoid impacting FUSE performance.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (tx, rx) = mpsc::unbounded_channel();
    /// let fs = Fuse3OrthoUnionFS::new(index, client, size)
    ///     .with_scene_tracker_channel(tx);
    /// // rx is passed to DefaultSceneTracker::start()
    /// ```
    pub fn with_scene_tracker_channel(
        mut self,
        tx: mpsc::UnboundedSender<FuseAccessEvent>,
    ) -> Self {
        self.scene_tracker_tx = Some(tx);
        self
    }

    /// Set the load monitor for circuit breaker integration.
    ///
    /// When set, the filesystem calls [`FuseLoadMonitor::record_request()`]
    /// for each DDS texture read. This enables the circuit breaker to detect
    /// when X-Plane is heavily loading scenery (e.g., during scene load) and
    /// pause prefetching to avoid competing for resources.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let monitor = Arc::new(SharedFuseLoadMonitor::new());
    /// let fs = Fuse3OrthoUnionFS::new(index, client, size)
    ///     .with_load_monitor(monitor);
    /// ```
    pub fn with_load_monitor(mut self, monitor: Arc<dyn FuseLoadMonitor>) -> Self {
        self.load_monitor = Some(monitor);
        self
    }

    /// Set the metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics
    /// to the metrics system.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
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
        // Tell kernel we don't implement opendir - it should call readdir directly
        mount_options.no_open_dir_support(true);

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
        // Tell kernel we don't implement opendir - it should call readdir directly
        mount_options.no_open_dir_support(true);

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
    fn dds_client(&self) -> &Arc<dyn DdsClient> {
        &self.dds_client
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

    fn request_coalescer(&self) -> Option<&Arc<RequestCoalescer>> {
        Some(&self.request_coalescer)
    }

    fn metrics_client(&self) -> Option<&crate::metrics::MetricsClient> {
        self.metrics_client.as_ref()
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

            // Record request for circuit breaker (tracks X-Plane load rate)
            if let Some(ref monitor) = self.load_monitor {
                monitor.record_request();
            }

            // Send DDS access event to tile-based prefetcher (fire-and-forget)
            if let Some(ref tx) = self.dds_access_tx {
                // Convert DDS tile coordinates to DSF tile (1° × 1°)
                if let Some(dsf_tile) = DsfTileCoord::from_dds_filename(&format!("{}.dds", coords))
                {
                    let _ = tx.send(DdsAccessEvent::new(dsf_tile));
                }
            }

            // Send raw tile coordinates to Scene Tracker (fire-and-forget)
            // Scene Tracker stores empirical data; derives regions via calculation
            if let Some(ref tx) = self.scene_tracker_tx {
                let tile = DdsTileCoord::new(coords.row, coords.col, coords.zoom);
                let _ = tx.send(FuseAccessEvent::new(tile));
            }

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
        tracing::debug!(ino = ino, offset = offset, "FUSE readdir called");

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

    async fn readdirplus(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Fuse3InternalResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'_>>> {
        tracing::debug!(ino = ino, offset = offset, "FUSE readdirplus called");

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

        let mut entries: Vec<DirectoryEntryPlus> = Vec::new();

        // Add . entry
        entries.push(DirectoryEntryPlus {
            inode: ino,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
            attr: self.virtual_dir_attr(ino),
            entry_ttl: TTL,
            attr_ttl: TTL,
        });

        // Parent inode for ..
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

        entries.push(DirectoryEntryPlus {
            inode: parent_inode,
            generation: 0,
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
            attr: self.virtual_dir_attr(parent_inode),
            entry_ttl: TTL,
            attr_ttl: TTL,
        });

        // Get entries from union index
        let mut entry_offset = 3i64;
        for dir_entry in self.index.list_directory(&virtual_path) {
            let child_path = virtual_path.join(&dir_entry.name);
            let entry_inode = self.inode_manager.get_or_create_inode(&child_path);

            let (kind, attr) = if dir_entry.is_dir {
                (FileType::Directory, self.virtual_dir_attr(entry_inode))
            } else {
                // For real files, get actual metadata from the source path
                // This is critical for DSF and other passthrough files that need
                // accurate file sizes reported to X-Plane
                if let Some(source) = self.index.resolve(&child_path) {
                    if let Ok(metadata) = fs::metadata(&source.real_path).await {
                        (
                            FileType::RegularFile,
                            self.metadata_to_attr(entry_inode, &metadata),
                        )
                    } else {
                        // Fallback if metadata read fails
                        (FileType::RegularFile, self.virtual_dds_attr(entry_inode))
                    }
                } else {
                    // Entry not in index (shouldn't happen for list_directory entries)
                    (FileType::RegularFile, self.virtual_dds_attr(entry_inode))
                }
            };

            entries.push(DirectoryEntryPlus {
                inode: entry_inode,
                generation: 0,
                kind,
                name: dir_entry.name.clone(),
                offset: entry_offset,
                attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            });
            entry_offset += 1;
        }

        // Skip entries based on offset
        let entries: Vec<_> = entries.into_iter().skip(offset as usize).map(Ok).collect();

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries).boxed(),
        })
    }

    async fn opendir(
        &self,
        _req: Request,
        ino: u64,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyOpen> {
        tracing::debug!(ino = ino, "FUSE opendir called");
        // Return success with fh=0 for stateless directory I/O
        Ok(ReplyOpen { fh: 0, flags: 0 })
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
    use crate::coord::TileCoord;
    use crate::executor::{DdsClientError, Priority};
    use crate::ortho_union::OrthoUnionIndexBuilder;
    use crate::package::{InstalledPackage, Package, PackageType};
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use semver::Version;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    /// Mock DdsClient for testing
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

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

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

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

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

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

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

        let client = create_test_client();
        let limiter = Arc::new(StorageConcurrencyLimiter::with_defaults("test"));

        let fs = Fuse3OrthoUnionFS::with_disk_io_limiter(index, client, 1024, limiter.clone());

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

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_timeout(Duration::from_secs(60));

        assert_eq!(fs.generation_timeout, Duration::from_secs(60));
    }

    /// Test that readdirplus returns correct file sizes for passthrough files.
    ///
    /// This is a regression test for a bug where readdirplus was using virtual_dds_attr()
    /// for ALL non-directory files, causing passthrough files (like DSF) to report
    /// incorrect sizes (~11MB instead of actual size). X-Plane reads DSF files based
    /// on the reported size, so incorrect sizes caused dsf_ErrMissingAtom crashes.
    #[tokio::test]
    async fn test_readdirplus_returns_correct_file_sizes_for_passthrough_files() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with a DSF file of known size
        let pkg_dir = temp.path().join("test_ortho");
        let dsf_dir = pkg_dir.join("Earth nav data/+40-080");
        std::fs::create_dir_all(&dsf_dir).unwrap();

        // Create a DSF file with specific content (size = 27 bytes)
        let dsf_content = b"this is fake dsf content!!";
        let dsf_path = dsf_dir.join("+40-074.dsf");
        std::fs::write(&dsf_path, dsf_content).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        // Get the inode for the directory containing the DSF
        let dsf_dir_virtual = std::path::Path::new("Earth nav data/+40-080");
        let dir_inode = fs.inode_manager.get_or_create_inode(dsf_dir_virtual);

        // Create a fake request (uid/gid don't matter for this test)
        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Call readdirplus (req, ino, fh, offset, lock_owner)
        let result = fs.readdirplus(req, dir_inode, 0, 0, 0).await.unwrap();

        // Collect entries from the stream
        let entries: Vec<_> = result.entries.collect().await;

        // Find the DSF file entry
        let dsf_entry = entries
            .iter()
            .filter_map(|e| e.as_ref().ok())
            .find(|e| e.name.to_string_lossy().ends_with(".dsf"))
            .expect("DSF file should be in directory listing");

        // The critical assertion: file size should match actual file content size,
        // NOT the virtual DDS size (~11MB)
        let actual_size = dsf_content.len() as u64;
        let virtual_dds_size = 11_174_016u64; // VirtualDdsConfig::default().size()

        assert_eq!(
            dsf_entry.attr.size, actual_size,
            "DSF file size should be {} bytes (actual), not {} bytes (virtual DDS)",
            actual_size, dsf_entry.attr.size
        );
        assert_ne!(
            dsf_entry.attr.size, virtual_dds_size,
            "DSF file should NOT have virtual DDS size"
        );
    }
}
