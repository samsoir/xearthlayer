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
use crate::coord::TileCoord;
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::coalesce::RequestCoalescer;
use crate::fuse::{get_default_placeholder, parse_dds_filename};
use crate::geo_index::GeoIndex;
use crate::ortho_union::OrthoUnionIndex;
use crate::prefetch::{DdsAccessEvent, DsfTileCoord, PrefetchStateObserver, TileRequestCallback};
use crate::scene_tracker::{DdsTileCoord, FuseAccessEvent};
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEntry, ReplyInit, ReplyOpen, ReplyStatFs,
};
use fuse3::raw::Filesystem;
use fuse3::{Errno, MountOptions, Result as Fuse3InternalResult};
use futures::stream::{self, Stream, StreamExt};
use std::ffi::{OsStr, OsString};
use std::io::SeekFrom;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tracing::{debug, trace, Instrument};

/// FUSE open flag: bypass kernel page cache for this file.
///
/// From `linux/fuse.h`: `#define FOPEN_DIRECT_IO (1 << 0)`
///
/// When set in `ReplyOpen::flags`, the kernel sends every `read()` through
/// the FUSE handler instead of serving from its page cache. Used for virtual
/// DDS files so that `FuseLoadMonitor`, `SceneTracker`, and `DdsAccessEvent`
/// see every X-Plane read.
const FOPEN_DIRECT_IO: u32 = 1;

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
    /// Geospatial reference database for region-level ownership queries.
    ///
    /// Used to determine if a scenery file falls within a patch-owned region.
    /// When set, FUSE filters lazy resolution to only serve patch sources in
    /// those regions, hiding package files that would cause X-Plane conflicts.
    geo_index: Option<Arc<GeoIndex>>,
    /// Observer for prefetch state divergence (#176).
    state_observer: Option<Arc<PrefetchStateObserver>>,
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
    /// Optional metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics.
    metrics_client: Option<crate::metrics::MetricsClient>,
    /// Maximum pending background FUSE requests (kernel limit).
    /// When set, overrides the kernel default (12) in the FUSE init handshake.
    fuse_max_background: Option<u16>,
    /// Congestion threshold for background FUSE requests (kernel limit).
    /// When set, overrides the kernel default (9) in the FUSE init handshake.
    fuse_congestion_threshold: Option<u16>,
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
            geo_index: None,
            state_observer: None,
            dds_client,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            dds_access_tx: None,
            scene_tracker_tx: None,
            metrics_client: None,
            fuse_max_background: None,
            fuse_congestion_threshold: None,
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
            geo_index: None,
            state_observer: None,
            dds_client,
            inode_manager: InodeManager::new(virtual_root),
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            dds_access_tx: None,
            scene_tracker_tx: None,
            metrics_client: None,
            fuse_max_background: None,
            fuse_congestion_threshold: None,
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

    /// Set the metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics
    /// to the metrics system.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
        self
    }

    /// Set the geospatial reference index for region-level ownership queries.
    ///
    /// When set, FUSE uses the GeoIndex to determine if a scenery file falls
    /// within a patch-owned DSF region. Files in patched regions are resolved
    /// only from patch sources, hiding package files that could conflict.
    pub fn with_geo_index(mut self, geo_index: Arc<GeoIndex>) -> Self {
        self.geo_index = Some(geo_index);
        self
    }

    /// Set the prefetch state observer.
    ///
    /// When set, an on-demand generation in a region prefetch marked
    /// `Prefetched` or `NoCoverage` demotes that region.
    pub fn with_state_observer(mut self, observer: Arc<PrefetchStateObserver>) -> Self {
        self.state_observer = Some(observer);
        self
    }

    /// Set the FUSE kernel background request limits.
    ///
    /// These values are sent to the kernel during the FUSE init handshake to
    /// control how many concurrent background requests (readahead, async reads)
    /// are allowed before throttling. The kernel defaults (12/9) are too low
    /// for X-Plane's concurrent scenery reads.
    pub fn with_fuse_limits(mut self, max_background: u16, congestion_threshold: u16) -> Self {
        self.fuse_max_background = Some(max_background);
        self.fuse_congestion_threshold = Some(congestion_threshold);
        self
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        &self.disk_io_limiter
    }

    /// Check if a scenery filename falls in a geo-filtered (patch-owned) region.
    ///
    /// Returns true when the file's DSF region has [`PatchCoverage`] in the
    /// GeoIndex, meaning only patch sources should serve files for this region.
    fn is_geo_filtered(&self, filename: &str) -> bool {
        let geo_index = match self.geo_index {
            Some(ref gi) => gi,
            None => return false,
        };

        use crate::geo_index::{DsfRegion, PatchCoverage};
        use crate::prefetch::tile_based::DsfTileCoord;

        DsfTileCoord::from_scenery_filename(filename)
            .map(|dsf| {
                let region = DsfRegion::new(dsf.lat, dsf.lon);
                geo_index.contains::<PatchCoverage>(&region)
            })
            .unwrap_or(false)
    }

    /// Resolve a lazy path with geospatial awareness.
    ///
    /// If the file is in a patch-owned region (per GeoIndex), patch sources are
    /// searched first. If the file isn't found in patches, all sources are tried
    /// as fallback — this handles cross-region DSF references where a non-patched
    /// DSF references terrain (e.g., `_sea.ter`) in a patched region.
    ///
    /// DDS generation blocking is handled separately by the passthrough gate in
    /// `lookup()`, not here.
    ///
    /// This is the composition point: GeoIndex (geography) + OrthoUnionIndex (files).
    /// Record one FUSE `read()` call for the amplification metric.
    ///
    /// `returned` is what this call hands back to the kernel; `materialised`
    /// is what the handler had to obtain to produce it. The kernel caps each read
    /// at 1 MiB, so a handler that builds the whole object per call inflates
    /// the second against the first. See #233 and #234.
    fn record_read(&self, returned: u64, materialised: u64, virtual_dds: bool) {
        if let Some(metrics) = &self.metrics_client {
            metrics.fuse_read(returned, materialised, virtual_dds);
        }
    }

    fn resolve_lazy_geo(&self, virtual_path: &std::path::Path, filename: &str) -> Option<PathBuf> {
        if self.is_geo_filtered(filename) {
            // Patched region: patches first, packages fill gaps
            self.index
                .resolve_lazy_filtered(virtual_path, |s| s.is_patch())
                .or_else(|| self.index.resolve_lazy(virtual_path))
        } else {
            // Normal: all sources
            self.index.resolve_lazy(virtual_path)
        }
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

        #[cfg(target_os = "linux")]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount_with_unprivileged(self, mount_path.clone())
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        #[cfg(not(target_os = "linux"))]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount(self, mount_path.clone())
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        Ok(super::types::SpawnedMountHandle::spawn_from_handle(
            handle, mount_path,
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

    fn on_dds_response(&self, tile: TileCoord, cache_hit: bool) {
        if let Some(ref observer) = self.state_observer {
            observer.observe(tile, cache_hit);
        }
    }
}

impl Filesystem for Fuse3OrthoUnionFS {
    async fn init(&self, _req: Request) -> Fuse3InternalResult<ReplyInit> {
        debug!(
            sources = self.index.source_count(),
            files = self.index.file_count(),
            fuse_max_background = ?self.fuse_max_background,
            fuse_congestion_threshold = ?self.fuse_congestion_threshold,
            "fuse3 ortho union: init"
        );
        let mut reply = ReplyInit::new(NonZeroU32::new(1024 * 1024).unwrap());
        reply.max_background = self.fuse_max_background;
        reply.congestion_threshold = self.fuse_congestion_threshold;
        Ok(reply)
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
        //
        // Geospatial filtering: In patch-owned regions, only patch sources are
        // searched — package files are invisible. This prevents X-Plane from
        // discovering package terrain that references DDS files we won't generate.
        if let Some(real_path) = self.resolve_lazy_geo(&child_path, &name_str) {
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

        // Check if it's a DDS file we can generate.
        //
        // In patched regions, patch DDS files are served via resolve_lazy_geo() above
        // (passthrough from the patch source). If we reach here, no source has this DDS
        // on disk — generate it. This handles boundary cases where package `.ter` files
        // reference DDS textures the patch doesn't provide (different naming convention).
        if name_str.ends_with(".dds") {
            // Water mask guard (#68): X-Plane requests BORDER_TEX as .dds first,
            // but the actual water mask is a .png on disk. If we generate a virtual
            // DDS, X-Plane gets satellite imagery instead of the alpha mask. Return
            // ENOENT so X-Plane falls back to the .png via lazy resolution.
            let png_name = name_str.replace(".dds", ".png");
            let png_path = parent_path.join(&*png_name);
            if self.resolve_lazy_geo(&png_path, &png_name).is_some() {
                trace!(file = %name_str, "Water mask PNG exists, skipping DDS generation");
                return Err(Errno::from(libc::ENOENT));
            }

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

        // Try lazy resolution for terrain/textures directories (geospatial-aware)
        let filename = virtual_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        if let Some(real_path) = self.resolve_lazy_geo(&virtual_path, filename) {
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

            let fuse_read_span = tracing::debug_span!(target: "profiling", "fuse_read", ino = ino, offset = offset, size = size,);
            let data = self
                .request_dds(&filename)
                .instrument(fuse_read_span)
                .await
                .unwrap_or_else(get_default_placeholder);

            let offset = offset as usize;
            let size = size as usize;

            let end = std::cmp::min(offset.saturating_add(size), data.len());
            let slice = data.get(offset..end).unwrap_or(&[]);
            self.record_read(slice.len() as u64, data.len() as u64, true);

            return Ok(ReplyData {
                data: Bytes::copy_from_slice(slice),
            });
        }

        // Real file from union index
        let virtual_path = self
            .inode_manager
            .get_path(ino)
            .ok_or(Errno::from(libc::ENOENT))?;

        // Try to resolve the real path - first from index, then lazy (geospatial-aware)
        let filename = virtual_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        let real_path = if let Some(source) = self.index.resolve(&virtual_path) {
            source.real_path.clone()
        } else if let Some(lazy_path) = self.resolve_lazy_geo(&virtual_path, filename) {
            lazy_path
        } else {
            return Err(Errno::from(libc::ENOENT));
        };

        // Acquire disk I/O permit
        let _permit = self.disk_io_limiter.acquire().await;

        // Read only the requested window. The kernel caps each FUSE read at
        // max_pages * PAGE_SIZE (1 MiB on Linux), so reading the whole file
        // here would move it once per call -- 238x for the largest installed
        // ortho DSF. See #233.
        let mut file = fs::File::open(&real_path)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        // `take` bounds the read at `size`; `read_to_end` stops early at EOF,
        // which is how a range spanning the end becomes a short read.
        let mut buf = Vec::with_capacity(size as usize);
        (&mut file)
            .take(size as u64)
            .read_to_end(&mut buf)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        // Seeking past EOF is legal and yields nothing, so this also covers
        // the at-or-past-EOF case without a separate branch.
        self.record_read(buf.len() as u64, buf.len() as u64, false);

        Ok(ReplyData {
            data: Bytes::from(buf),
        })
    }

    async fn readdir(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: i64,
    ) -> Fuse3InternalResult<
        ReplyDirectory<impl Stream<Item = Fuse3InternalResult<DirectoryEntry>> + Send + '_>,
    > {
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
        for (entry_offset, dir_entry) in (3i64..).zip(self.index.list_directory(&virtual_path)) {
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
    ) -> Fuse3InternalResult<
        ReplyDirectoryPlus<impl Stream<Item = Fuse3InternalResult<DirectoryEntryPlus>> + Send + '_>,
    > {
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
        for (entry_offset, dir_entry) in (3i64..).zip(self.index.list_directory(&virtual_path)) {
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
        }

        // Skip entries based on offset
        let entries: Vec<_> = entries.into_iter().skip(offset as usize).map(Ok).collect();

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries).boxed(),
        })
    }

    async fn open(&self, _req: Request, inode: u64, _flags: u32) -> Fuse3InternalResult<ReplyOpen> {
        if InodeManager::is_virtual_inode(inode) {
            // Virtual DDS files: bypass kernel page cache so every read()
            // goes through our FUSE handler. This ensures FuseLoadMonitor,
            // SceneTracker, and DdsAccessEvent see all X-Plane reads.
            Ok(ReplyOpen {
                fh: 0,
                flags: FOPEN_DIRECT_IO,
            })
        } else {
            // Real passthrough files: use default kernel caching
            Ok(ReplyOpen { fh: 0, flags: 0 })
        }
    }

    async fn release(
        &self,
        _req: Request,
        _inode: u64,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> Fuse3InternalResult<()> {
        // Stateless I/O — no file handle state to clean up.
        // Required now that open() is implemented (FOPEN_DIRECT_IO):
        // the kernel tracks file handles through our handler and sends
        // release() when files are closed, including during unmount.
        Ok(())
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

    // ========================================================================
    // Passthrough read amplification (#233)
    // ========================================================================

    /// Build a one-package index containing a single file of `size` bytes and
    /// return the mounted FS, its inode, and the metrics receiver.
    fn passthrough_fixture(
        temp: &TempDir,
        size: usize,
    ) -> (
        Fuse3OrthoUnionFS,
        u64,
        tokio::sync::mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) {
        let pkg_dir = temp.path().join("test_ortho");
        let dsf_dir = pkg_dir.join("Earth nav data/+40-080");
        std::fs::create_dir_all(&dsf_dir).unwrap();
        // Byte i = i as u8, so a slice's content proves which range was served.
        let content: Vec<u8> = (0..size).map(|i| i as u8).collect();
        std::fs::write(dsf_dir.join("+40-074.dsf"), &content).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let (metrics_tx, metrics_rx) = tokio::sync::mpsc::unbounded_channel();
        let fs = Fuse3OrthoUnionFS::new(index, create_test_client(), 1024)
            .with_metrics(crate::metrics::MetricsClient::new(metrics_tx));
        let inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("Earth nav data/+40-080/+40-074.dsf"));

        (fs, inode, metrics_rx)
    }

    fn test_request() -> fuse3::raw::Request {
        fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        }
    }

    /// Drain the single `FuseRead` event a read is expected to emit.
    fn drain_one_read(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) -> (u64, u64, bool) {
        match rx
            .try_recv()
            .expect("read() must emit exactly one FuseRead")
        {
            crate::metrics::MetricEvent::FuseRead {
                returned,
                materialised,
                virtual_dds,
            } => (returned, materialised, virtual_dds),
            other => panic!("expected FuseRead, got {other:?}"),
        }
    }

    /// A ranged read must not materialise more than it returns.
    ///
    /// The kernel caps every FUSE read at 1 MiB, so X-Plane asking for a whole
    /// 4 MiB file still arrives as several ranged calls. Answering each by
    /// reading the entire file makes the handler move N times what it delivers
    /// -- 12-23x at DDS size, up to 238x on the largest installed ortho DSF.
    ///
    /// This test fails against the pre-fix handler, which reports
    /// materialised == 4 MiB for a 64 KiB request.
    #[tokio::test]
    async fn test_passthrough_read_materialises_only_the_requested_range() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 4 * 1024 * 1024;
        const READ_SIZE: u32 = 64 * 1024;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, 0, READ_SIZE)
            .await
            .unwrap();
        assert_eq!(reply.data.len(), READ_SIZE as usize);

        let (returned, materialised, virtual_dds) = drain_one_read(&mut metrics_rx);
        assert!(
            !virtual_dds,
            "a real file on disk is not a virtual DDS tile"
        );
        assert_eq!(returned, READ_SIZE as u64);
        assert_eq!(
            materialised, returned,
            "serving {READ_SIZE} bytes must not read {materialised} bytes from a {FILE_SIZE}-byte file"
        );
    }

    /// The same guarantee mid-file: seeking must not re-read the prefix.
    #[tokio::test]
    async fn test_passthrough_read_mid_file_does_not_read_the_prefix() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 1024 * 1024;
        const OFFSET: u64 = 768 * 1024;
        const READ_SIZE: u32 = 128 * 1024;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, OFFSET, READ_SIZE)
            .await
            .unwrap();

        // Content proves the right window was served, not just the right length.
        assert_eq!(reply.data.len(), READ_SIZE as usize);
        assert_eq!(reply.data[0], (OFFSET as usize % 256) as u8);

        let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
        assert_eq!(returned, READ_SIZE as u64);
        assert_eq!(materialised, returned);
    }

    /// A range that runs off the end returns a short read, not an error.
    #[tokio::test]
    async fn test_passthrough_read_spanning_eof_is_clamped() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 100_000;
        const OFFSET: u64 = 90_000;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, OFFSET, 65536)
            .await
            .unwrap();

        let expected = FILE_SIZE as u64 - OFFSET;
        assert_eq!(reply.data.len() as u64, expected);

        let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
        assert_eq!(returned, expected);
        assert_eq!(materialised, returned);
    }

    /// Reading at or past EOF returns empty rather than erroring.
    #[tokio::test]
    async fn test_passthrough_read_at_and_past_eof_returns_empty() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 4096;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        for offset in [FILE_SIZE as u64, FILE_SIZE as u64 * 4] {
            let reply = fs
                .read(test_request(), inode, 0, offset, 4096)
                .await
                .unwrap();
            assert!(reply.data.is_empty(), "offset {offset} is at or past EOF");

            let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
            assert_eq!(returned, 0);
            assert_eq!(materialised, 0, "an empty reply must allocate nothing");
        }
    }

    // ========================================================================
    // Patched region passthrough gate tests (Issue #51)
    // ========================================================================

    /// Test that lookup returns ENOENT for DDS files in patched regions.
    ///
    /// When a patch owns a region (per GeoIndex), FUSE should never generate
    /// DDS textures for that region — it should return ENOENT for missing files.
    #[tokio::test]
    /// Test that DDS from a patch source is served via passthrough in patched regions.
    ///
    /// When a patch has the DDS file on disk, `resolve_lazy_geo()` serves it directly
    /// (passthrough). DDS generation is never needed.
    async fn test_lookup_patched_region_serves_patch_dds_via_passthrough() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch with DSF in region (33, -119) and a real DDS file
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        let patch_textures = patch_dir.join("textures");
        std::fs::create_dir_all(&patch_textures).unwrap();

        // Place a DDS file in the patch using GO2 convention at zoom 18.
        // Patches commonly use GO218 (Google GO2 at ZL18) naming.
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 18).unwrap();
        let dds_name = format!("{}_{}_GO218.dds", tc.row, tc.col);
        std::fs::write(patch_textures.join(&dds_name), b"fake dds data").unwrap();

        // Create a package so we have a textures/ directory
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        // Get inode for "textures" directory
        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        // Lookup should succeed — the patch has this DDS on disk, served via passthrough
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS from patch should succeed via passthrough, got: {:?}",
            result
        );
    }

    /// Test that package terrain files in patched regions are served as fallback.
    ///
    /// When a patch owns a region but doesn't include a specific file (e.g., _sea.ter),
    /// FUSE should fall through to the package source instead of returning ENOENT.
    /// This handles cross-region DSF references where a non-patched DSF references
    /// terrain in a patched region.
    #[tokio::test]
    async fn test_lookup_patched_region_falls_through_to_package_for_terrain() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch owning region (33, -119)
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        // Create a package with a _sea.ter file whose chunk coords fall in region (33, -119).
        // This simulates X-Plane's cross-region DSF references where a non-patched DSF
        // references sea terrain in a patched region.
        let na_pkg = create_test_package(&temp, "na");

        // Place a terrain file in the package's terrain dir with coords in the patched region
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 16).unwrap();
        let sea_ter_name = format!("{}_{}_BI16_sea.ter", tc.row, tc.col);
        let pkg_terrain_dir = temp.path().join("na_ortho/terrain");
        std::fs::write(pkg_terrain_dir.join(&sea_ter_name), b"sea terrain data").unwrap();

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&sea_ter_name).unwrap();
        assert_eq!(dsf.lat, 33, "sea.ter filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "sea.ter filename should map to lon -119");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        let terrain_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("terrain"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Lookup should succeed: the file exists in the package, and even though
        // the region is patched, FUSE should fall through to the package source
        // when the patch doesn't have the file.
        let result = fs
            .lookup(req, terrain_inode, std::ffi::OsStr::new(&sea_ter_name))
            .await;

        assert!(
            result.is_ok(),
            "Lookup for package terrain in patched region should fall through, got: {:?}",
            result
        );
    }

    /// Test that DDS generation is allowed in patched regions when the patch doesn't
    /// provide the DDS file.
    ///
    /// This handles boundary cases: a non-patched DSF references package terrain in a
    /// patched region. That terrain's `.ter` references a DDS (e.g., `BI16.dds`) that
    /// doesn't exist on disk. Since the patch uses different filenames (e.g., `GO218.dds`),
    /// the patch can't provide this DDS. XEL should generate it rather than returning ENOENT.
    #[tokio::test]
    async fn test_lookup_patched_region_allows_dds_generation_for_package_textures() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch owning region (33, -119)
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        // Create a package so we have a textures/ directory
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        // Get inode for "textures" directory
        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // DDS filename in the patched region, using package naming convention (BI16).
        // The patch uses GO218 convention, so it doesn't have this DDS.
        // XEL should allow generation rather than blocking with ENOENT.
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 16).unwrap();
        let dds_name = format!("{}_{}_BI16.dds", tc.row, tc.col);

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        // Lookup should SUCCEED — the DDS gets a virtual inode for generation,
        // even though the region is patched. The patch doesn't provide this DDS.
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "DDS in patched region should allow generation when patch doesn't provide it, got: {:?}",
            result
        );
    }

    /// Test DDS generation in patched regions at a different zoom level (ZL14).
    ///
    /// Patches can be at any zoom level. This verifies the DDS gate removal works
    /// correctly for higher zoom levels where coordinate scale differs.
    #[tokio::test]
    async fn test_lookup_patched_region_allows_dds_generation_at_different_zoom() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // DDS at zoom 14 (different from the standard ZL16 package convention)
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 14).unwrap();
        let dds_name = format!("{}_{}_BI14.dds", tc.row, tc.col);

        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "DDS at ZL14 in patched region should allow generation, got: {:?}",
            result
        );
    }

    // ========================================================================
    // Water mask guard tests (Issue #68)
    // ========================================================================

    /// Test that lookup returns ENOENT for DDS when a PNG water mask exists.
    ///
    /// X-Plane requests BORDER_TEX water masks as `.dds` first. If a `.png`
    /// with the same stem exists on disk, we must return ENOENT so X-Plane
    /// falls back to the real PNG water mask instead of getting satellite imagery.
    #[tokio::test]
    async fn test_lookup_returns_enoent_for_dds_when_png_water_mask_exists() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with a PNG water mask in textures/
        let pkg_dir = temp.path().join("eu_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();

        // Place a PNG water mask (the kind referenced by _sea_overlay.ter BORDER_TEX)
        let png_name = "24496_33152_ZL16.png";
        std::fs::write(
            pkg_dir.join("textures").join(png_name),
            b"fake png water mask",
        )
        .unwrap();

        let pkg = InstalledPackage::new(
            Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Request the DDS version — should fail because the PNG water mask exists
        let dds_name = "24496_33152_ZL16.dds";
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(dds_name))
            .await;

        assert!(
            result.is_err(),
            "Lookup for DDS should return ENOENT when PNG water mask exists, got: {:?}",
            result
        );
    }

    /// Test that DDS generation still works when no PNG water mask exists.
    ///
    /// This verifies the water mask guard doesn't block normal DDS generation
    /// for ortho tile textures (which have no corresponding PNG on disk).
    #[tokio::test]
    async fn test_lookup_allows_dds_generation_when_no_png_exists() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with textures/ but no PNG for this tile
        let pkg_dir = temp.path().join("eu_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Request a DDS with no corresponding PNG — should succeed (virtual inode)
        let result = fs
            .lookup(
                req,
                textures_inode,
                std::ffi::OsStr::new("10000_5000_BI16.dds"),
            )
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS without PNG water mask should succeed for generation, got: {:?}",
            result
        );
    }

    /// Test that lookup in a non-patched region still creates a virtual inode for DDS generation.
    #[tokio::test]
    async fn test_lookup_non_patched_region_creates_virtual_inode() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package (no patches = no patched regions)
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Any DDS filename should succeed (virtual inode for generation)
        let result = fs
            .lookup(
                req,
                textures_inode,
                std::ffi::OsStr::new("10000_5000_BI16.dds"),
            )
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS in non-patched region should succeed for generation"
        );
    }

    // ========================================================================
    // FOPEN_DIRECT_IO tests (Issue #65)
    // ========================================================================

    #[tokio::test]
    async fn test_open_virtual_dds_returns_direct_io() {
        use super::FOPEN_DIRECT_IO;
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Virtual DDS inode (above VIRTUAL_INODE_BASE)
        use crate::fuse::support::inode::VIRTUAL_INODE_BASE;
        let virtual_inode = VIRTUAL_INODE_BASE + 42;
        let result: Fuse3InternalResult<ReplyOpen> =
            fs.open(req, virtual_inode, libc::O_RDONLY as u32).await;

        let reply = result.expect("open on virtual DDS inode should succeed");
        assert_eq!(reply.fh, 0, "file handle should be stateless");
        assert_eq!(
            reply.flags, FOPEN_DIRECT_IO,
            "virtual DDS files should have FOPEN_DIRECT_IO flag"
        );
    }

    #[tokio::test]
    async fn test_open_real_inode_returns_default_flags() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Real inode (below VIRTUAL_INODE_BASE)
        let real_inode = 42u64;
        let result: Fuse3InternalResult<ReplyOpen> =
            fs.open(req, real_inode, libc::O_RDONLY as u32).await;

        let reply = result.expect("open on real inode should succeed");
        assert_eq!(reply.fh, 0, "file handle should be stateless");
        assert_eq!(
            reply.flags, 0,
            "real passthrough files should have default flags (no DIRECT_IO)"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // PrefetchStateObserver wiring (#176)
    //
    // The observer's own policy is unit-tested in `prefetch::state_observer`.
    // What those tests cannot see is whether FUSE ever *calls* it: every one
    // of them invokes `observe()` directly. These two exercise the real
    // chain — `DdsRequestor::request_dds_impl` → `do_request` →
    // `on_dds_response` → `observer.observe` → `GeoIndex` — over a real
    // `Fuse3OrthoUnionFS` and a real `GeoIndex`, so cutting any link in it
    // fails a test instead of silently disabling the feature.
    //
    // The final link (`manager::mounts` attaching the observer at mount
    // construction) is not covered: reaching it needs a whole
    // `XEarthLayerService` and a live FUSE mount.
    // ─────────────────────────────────────────────────────────────────────────

    /// Drive one full FUSE DDS request against a `Prefetched` region and
    /// return whether that region still carries prefetch state afterwards.
    ///
    /// `cache_hit` is what the stand-in executor reports back, so the caller
    /// controls the single input the observer's policy turns on.
    async fn run_dds_request_over_prefetched_region(cache_hit: bool) -> bool {
        use crate::geo_index::{DsfRegion, PrefetchedRegion};

        let temp = TempDir::new().unwrap();
        let index = OrthoUnionIndexBuilder::new()
            .add_package(create_test_package(&temp, "na"))
            .build()
            .unwrap();

        // A region prefetch claims is fully cached.
        let tile = crate::coord::to_tile_coords(33.5, -118.5, 12).unwrap();
        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());

        let (client, mut rx) = MockDdsClient::new();
        let fs = Fuse3OrthoUnionFS::new(index, client as Arc<dyn DdsClient>, 1024)
            .with_state_observer(Arc::new(PrefetchStateObserver::new(Arc::clone(&geo_index))));

        // Stand in for the executor daemon: answer the one request FUSE makes.
        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("FUSE must submit a DDS request");
            request
                .response_tx
                .expect("FUSE requests carry a response channel")
                .send(DdsResponse::new(
                    vec![0u8; 16],
                    cache_hit,
                    Duration::from_millis(1),
                    true,
                ))
                .ok();
        });

        let coords = crate::fuse::DdsFilename {
            row: tile.row * 16,
            col: tile.col * 16,
            zoom: tile.zoom + 4,
            map_type: "BI".to_string(),
        };
        let _ = fs.request_dds_impl(&coords).await;
        responder.await.unwrap();

        geo_index.get::<PrefetchedRegion>(&region).is_some()
    }

    #[tokio::test]
    async fn test_fuse_on_demand_generation_demotes_prefetched_region() {
        assert!(
            !run_dds_request_over_prefetched_region(false).await,
            "a FUSE on-demand generation inside a Prefetched region must reach \
             the observer and demote it — if this passes only because the \
             observer was never called, the whole feature is inert"
        );
    }

    #[tokio::test]
    async fn test_fuse_cache_hit_leaves_prefetched_region_alone() {
        // Guards the `cache_hit` argument specifically: a hook that called
        // the observer but hardcoded `false` would pass the test above.
        assert!(
            run_dds_request_over_prefetched_region(true).await,
            "serving a prefetched tile from cache is the system working — \
             the region must keep its state"
        );
    }
}
