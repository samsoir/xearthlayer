//! Multi-mount manager for tracking and managing FUSE mounts.
//!
//! This module provides the `MountManager` which coordinates mounting
//! multiple ortho packages simultaneously. Each ortho package gets its
//! own FUSE mount where DDS files are generated on-demand.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::defaults::{DEFAULT_FUSE_CONGESTION_THRESHOLD, DEFAULT_FUSE_MAX_BACKGROUND};
use crate::config::DiskIoProfile;
use crate::executor::StorageConcurrencyLimiter;
use crate::fuse::fuse3::Fuse3OrthoUnionFS;
use crate::fuse::SpawnedMountHandle;
use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};
use crate::metrics::TelemetrySnapshot;
use crate::ortho_union::{
    default_cache_path, IndexBuildProgressCallback, OrthoUnionIndex, OrthoUnionIndexBuilder,
};
use crate::package::{
    InstalledPackage as PackageInstalledPackage, Package as PackageCore, PackageType,
};
use crate::panic as panic_handler;
use crate::patches::{extract_dsf_regions, PatchDiscovery};
use crate::prefetch::tile_based::DdsAccessEvent;
use crate::prefetch::TileRequestCallback;
use crate::scene_tracker::{DefaultSceneTracker, FuseAccessEvent};
use crate::service::{ServiceConfig, ServiceError, XEarthLayerService};

use super::local::LocalPackageStore;
use super::{ManagerError, ManagerResult};

/// Information about an active mount.
#[derive(Debug)]
pub struct ActiveMount {
    /// The region code.
    pub region: String,
    /// The package type.
    pub package_type: PackageType,
    /// Path where the package is mounted.
    pub mountpoint: PathBuf,
}

/// Result of mounting the consolidated ortho union filesystem.
#[derive(Debug)]
pub struct ConsolidatedOrthoMountResult {
    /// Path where the consolidated ortho is mounted.
    pub mountpoint: PathBuf,
    /// Total number of sources (patches + packages).
    pub source_count: usize,
    /// Total files in the union index.
    pub file_count: usize,
    /// Names of patches included (if any).
    pub patch_names: Vec<String>,
    /// Regions of packages included.
    pub package_regions: Vec<String>,
    /// Whether the mount succeeded.
    pub success: bool,
    /// Error message if mount failed.
    pub error: Option<String>,
}

impl ConsolidatedOrthoMountResult {
    /// Create a successful consolidated mount result.
    pub fn success(
        mountpoint: PathBuf,
        source_count: usize,
        file_count: usize,
        patch_names: Vec<String>,
        package_regions: Vec<String>,
    ) -> Self {
        Self {
            mountpoint,
            source_count,
            file_count,
            patch_names,
            package_regions,
            success: true,
            error: None,
        }
    }

    /// Create a failed consolidated mount result.
    pub fn failure(mountpoint: PathBuf, error: String) -> Self {
        Self {
            mountpoint,
            source_count: 0,
            file_count: 0,
            patch_names: Vec::new(),
            package_regions: Vec::new(),
            success: false,
            error: Some(error),
        }
    }

    /// Create a result indicating no sources were found (not an error).
    pub fn no_sources() -> Self {
        Self {
            mountpoint: PathBuf::new(),
            source_count: 0,
            file_count: 0,
            patch_names: Vec::new(),
            package_regions: Vec::new(),
            success: true,
            error: None,
        }
    }
}

/// Manager for multiple FUSE mounts.
///
/// Coordinates mounting and unmounting of ortho packages. Each ortho
/// package gets its own FUSE mount where DDS textures are generated
/// on-demand as X-Plane requests them.
///
/// Overlay packages are not mounted via FUSE since they contain only
/// static DSF files that don't need on-demand generation.
pub struct MountManager {
    /// Active mount sessions, keyed by region.
    /// Uses SpawnedMountHandle which can be safely dropped from any context.
    sessions: HashMap<String, SpawnedMountHandle>,
    /// Services that own the Tokio runtimes - MUST be kept alive while sessions are active.
    /// The service contains the runtime that processes DDS generation requests.
    services: HashMap<String, XEarthLayerService>,
    /// Mount information for display purposes.
    mounts: HashMap<String, ActiveMount>,
    /// Target scenery directory for mounts (e.g., X-Plane Custom Scenery).
    /// If None, packages are mounted in-place.
    scenery_path: Option<PathBuf>,
    /// Scene Tracker for empirical X-Plane request tracking.
    ///
    /// This serves as the single source of truth for:
    /// - Tile tracking (which DDS tiles X-Plane has requested)
    /// - Burst detection (identifying loading patterns)
    scene_tracker: Arc<DefaultSceneTracker>,
    /// Active patches union filesystem mount (if any).
    patches_session: Option<SpawnedMountHandle>,
    /// Patches mount info for display.
    patches_mount: Option<ActiveMount>,
    /// Service for patches (owns the runtime for DDS generation).
    patches_service: Option<XEarthLayerService>,
    /// Consolidated ortho union filesystem mount (if any).
    consolidated_session: Option<SpawnedMountHandle>,
    /// Consolidated ortho mount info for display.
    consolidated_mount: Option<ActiveMount>,
    /// Service for consolidated ortho (owns the runtime for DDS generation).
    consolidated_service: Option<XEarthLayerService>,
    /// DDS access event receiver for tile-based prefetching.
    /// This is populated when mounting consolidated ortho and can be retrieved
    /// by the prefetcher via `take_dds_access_receiver()`.
    dds_access_rx: Option<mpsc::UnboundedReceiver<DdsAccessEvent>>,
    /// Scene Tracker event receiver for empirical scenery tracking.
    /// This is populated when mounting consolidated ortho and can be retrieved
    /// by the Scene Tracker via `take_scene_tracker_receiver()`.
    scene_tracker_rx: Option<mpsc::UnboundedReceiver<FuseAccessEvent>>,
    /// OrthoUnionIndex for DSF tile enumeration (tile-based prefetch).
    /// This is populated when mounting consolidated ortho.
    ortho_union_index: Option<Arc<OrthoUnionIndex>>,
    /// Geospatial reference index for region-level ownership queries.
    /// Populated with PatchCoverage data when mounting consolidated ortho.
    geo_index: Option<Arc<GeoIndex>>,
    /// Maximum pending background FUSE requests (kernel limit).
    fuse_max_background: u16,
    /// Congestion threshold for background FUSE requests (kernel limit).
    fuse_congestion_threshold: u16,
}

impl MountManager {
    /// Create a new mount manager that mounts packages in-place.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            services: HashMap::new(),
            mounts: HashMap::new(),
            scenery_path: None,
            scene_tracker: Arc::new(DefaultSceneTracker::with_defaults()),
            patches_session: None,
            patches_mount: None,
            patches_service: None,
            consolidated_session: None,
            consolidated_mount: None,
            consolidated_service: None,
            dds_access_rx: None,
            scene_tracker_rx: None,
            ortho_union_index: None,
            geo_index: None,
            fuse_max_background: DEFAULT_FUSE_MAX_BACKGROUND,
            fuse_congestion_threshold: DEFAULT_FUSE_CONGESTION_THRESHOLD,
        }
    }

    /// Create a new mount manager that mounts packages to a target scenery directory.
    ///
    /// When a scenery path is set, packages from `install_location` are mounted
    /// as directories in the scenery path (e.g., Custom Scenery).
    pub fn with_scenery_path(scenery_path: &std::path::Path) -> Self {
        let mut manager = Self::new();
        manager.scenery_path = Some(scenery_path.to_path_buf());
        manager
    }

    /// Set the FUSE kernel background request limits.
    ///
    /// These control how many concurrent background requests the Linux kernel
    /// allows before throttling FUSE operations. Higher values prevent X-Plane
    /// sim freezes at DSF boundaries.
    pub fn set_fuse_limits(&mut self, max_background: u16, congestion_threshold: u16) {
        self.fuse_max_background = max_background;
        self.fuse_congestion_threshold = congestion_threshold;
    }

    /// Get the number of active mounts.
    pub fn mount_count(&self) -> usize {
        self.sessions.len()
    }

    /// Check if any mounts are active.
    pub fn has_mounts(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// List active mounts.
    pub fn list_mounts(&self) -> Vec<&ActiveMount> {
        self.mounts.values().collect()
    }

    /// Check if a specific region is mounted.
    pub fn is_mounted(&self, region: &str) -> bool {
        self.sessions.contains_key(&region.to_lowercase())
    }

    /// Check if patches are mounted.
    pub fn has_patches(&self) -> bool {
        self.patches_session.is_some()
    }

    /// Get patches mount info (if mounted).
    pub fn patches_mount(&self) -> Option<&ActiveMount> {
        self.patches_mount.as_ref()
    }

    /// Mount all ortho sources as a single consolidated FUSE filesystem.
    ///
    /// This method creates a unified mount at `zzXEL_ortho` that includes:
    /// - All patches from `patches_dir` (highest priority via `_patches/` prefix)
    /// - All installed ortho packages from `store` (sorted alphabetically by region)
    ///
    /// This is the recommended mounting method as it provides:
    /// - Single mount point for X-Plane scenery management
    /// - Shared DDS generation resources
    /// - Unified file resolution with clear precedence rules
    ///
    /// # Arguments
    ///
    /// * `patches_dir` - Directory containing patch folders
    /// * `store` - Local package store for discovering installed packages
    /// * `service_builder` - Builder for creating the shared DDS service
    ///
    /// # Returns
    ///
    /// `ConsolidatedOrthoMountResult` indicating success/failure and source counts.
    pub fn mount_consolidated_ortho(
        &mut self,
        patches_dir: &std::path::Path,
        store: &LocalPackageStore,
        service_builder: &ServiceBuilder,
    ) -> ConsolidatedOrthoMountResult {
        self.mount_consolidated_ortho_with_progress(patches_dir, store, service_builder, None)
    }

    /// Mount consolidated ortho with progress callback.
    ///
    /// This variant allows tracking the index building progress for UI feedback.
    /// The progress callback is called during index building with updates about
    /// the current phase, sources being scanned, and files indexed.
    ///
    /// # Arguments
    ///
    /// * `patches_dir` - Path to patches directory
    /// * `store` - Local package store
    /// * `service_builder` - Service builder for FUSE mount
    /// * `progress` - Optional callback for progress updates
    pub fn mount_consolidated_ortho_with_progress(
        &mut self,
        patches_dir: &std::path::Path,
        store: &LocalPackageStore,
        service_builder: &ServiceBuilder,
        progress: Option<IndexBuildProgressCallback>,
    ) -> ConsolidatedOrthoMountResult {
        // Skip if already mounted
        if self.consolidated_session.is_some() {
            return ConsolidatedOrthoMountResult::failure(
                PathBuf::new(),
                "Consolidated ortho already mounted".to_string(),
            );
        }

        // Determine mountpoint
        let mountpoint = if let Some(ref scenery_path) = self.scenery_path {
            scenery_path.join("zzXEL_ortho")
        } else {
            return ConsolidatedOrthoMountResult::failure(
                PathBuf::new(),
                "No scenery path configured for consolidated mount".to_string(),
            );
        };

        // Build the union index with patches and packages
        let mut builder = OrthoUnionIndexBuilder::new();

        // Add patches (if patches directory exists)
        builder = builder.with_patches_dir(patches_dir);

        // Collect patch names for result (before building index)
        let patch_names: Vec<String> = {
            let discovery = PatchDiscovery::new(patches_dir);
            if discovery.exists() {
                discovery
                    .find_valid_patches()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect()
            } else {
                Vec::new()
            }
        };

        // Discover and add installed ortho packages
        let packages = match store.list() {
            Ok(p) => p,
            Err(e) => {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    format!("Failed to list packages: {}", e),
                );
            }
        };

        let mut package_regions = Vec::new();
        for pkg in packages {
            // Only include enabled ortho packages
            if pkg.package_type() != PackageType::Ortho {
                continue;
            }

            // Convert manager's InstalledPackage to package's InstalledPackage
            let core_pkg =
                PackageCore::new(pkg.region(), pkg.package_type(), pkg.version().clone());
            let pkg_installed = PackageInstalledPackage::new(core_pkg, &pkg.path);
            builder = builder.add_package(pkg_installed);
            package_regions.push(pkg.region().to_string());
        }

        // Check if there are any sources
        if patch_names.is_empty() && package_regions.is_empty() {
            tracing::info!("No patches or packages found, skipping consolidated mount");
            return ConsolidatedOrthoMountResult::no_sources();
        }

        // Build the index with optional progress reporting and caching
        let cache_path = default_cache_path();
        let index = match builder.build_with_progress(progress, cache_path.as_deref()) {
            Ok(i) => i,
            Err(e) => {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    format!("Failed to build ortho union index: {}", e),
                );
            }
        };

        let source_count = index.source_count();
        let file_count = index.file_count();

        // Build GeoIndex by extracting DSF regions from patch sources
        let geo_index = Arc::new(GeoIndex::new());
        let mut patched_entries = Vec::new();
        for source in index.sources() {
            if source.is_patch() {
                for (lat, lon) in extract_dsf_regions(&source.source_path) {
                    patched_entries.push((
                        DsfRegion::new(lat, lon),
                        PatchCoverage {
                            patch_name: source.display_name.clone(),
                        },
                    ));
                }
            }
        }
        if !patched_entries.is_empty() {
            geo_index.populate(patched_entries);
            tracing::info!(
                regions = geo_index.count::<PatchCoverage>(),
                "GeoIndex populated with patch coverage"
            );
        }

        tracing::info!(
            sources = source_count,
            files = file_count,
            patches = patch_names.len(),
            packages = package_regions.len(),
            "Built consolidated ortho union index"
        );

        // Create mountpoint directory
        if !mountpoint.exists() {
            if let Err(e) = std::fs::create_dir_all(&mountpoint) {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    format!("Failed to create mountpoint directory: {}", e),
                );
            }
        }

        // Create the service (owns the Tokio runtime for DDS generation)
        // Use XEarthLayerService::start() with integrated cache and metrics
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    format!("Failed to create runtime for service: {}", e),
                );
            }
        };

        let mut service = match runtime.block_on(service_builder.build_service_async()) {
            Ok(s) => s,
            Err(e) => {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    format!("Failed to create service: {}", e),
                );
            }
        };

        // Transfer runtime ownership to the service so it stays alive
        service.set_owned_runtime(runtime);

        // Get DdsClient and runtime from the service
        let dds_client = match service.dds_client() {
            Some(client) => client,
            None => {
                return ConsolidatedOrthoMountResult::failure(
                    mountpoint,
                    "DDS client not available (async provider required)".to_string(),
                );
            }
        };
        let expected_dds_size = service.expected_dds_size();
        let runtime_handle = service.runtime_handle().clone();

        // Create DDS access event channel for tile-based prefetching
        // The sender goes to FUSE, the receiver goes to the prefetcher
        let (dds_access_tx, dds_access_rx) = mpsc::unbounded_channel();

        // Create Scene Tracker event channel for empirical scenery tracking
        // The sender goes to FUSE, the receiver goes to the Scene Tracker
        let (scene_tracker_tx, scene_tracker_rx) = mpsc::unbounded_channel();

        // Store the index for tile-based prefetcher to use later
        let index_for_prefetch = Arc::new(index);

        // Create and mount the consolidated ortho union filesystem with DDS access channel
        // Wire Scene Tracker channel for empirical scenery tracking
        // Wire FUSE kernel limits for concurrent background request control
        let mut ortho_union_fs =
            Fuse3OrthoUnionFS::new((*index_for_prefetch).clone(), dds_client, expected_dds_size)
                .with_geo_index(Arc::clone(&geo_index))
                .with_dds_access_channel(dds_access_tx)
                .with_scene_tracker_channel(scene_tracker_tx)
                .with_fuse_limits(self.fuse_max_background, self.fuse_congestion_threshold);

        // Wire metrics client for coalesced request tracking
        if let Some(metrics) = service.metrics_client() {
            ortho_union_fs = ortho_union_fs.with_metrics(metrics);
        }
        let mountpoint_str = mountpoint.to_string_lossy();

        let mount_result = runtime_handle.block_on(ortho_union_fs.mount_spawned(&mountpoint_str));

        match mount_result {
            Ok(session) => {
                let mount_info = ActiveMount {
                    region: "consolidated".to_string(),
                    package_type: PackageType::Ortho,
                    mountpoint: mountpoint.clone(),
                };

                // Register mount point with panic handler for cleanup on crash
                panic_handler::register_mount(mountpoint.clone());

                // Store service to keep runtime alive, then store session and mount info
                self.consolidated_service = Some(service);
                self.consolidated_session = Some(session);
                self.consolidated_mount = Some(mount_info);

                // Store DDS access channel receiver, indexes for tile-based prefetcher
                self.dds_access_rx = Some(dds_access_rx);
                self.ortho_union_index = Some(index_for_prefetch);
                self.geo_index = Some(geo_index);

                // Store Scene Tracker receiver for empirical scenery tracking
                self.scene_tracker_rx = Some(scene_tracker_rx);

                tracing::info!(
                    mountpoint = %mountpoint.display(),
                    sources = source_count,
                    files = file_count,
                    "Consolidated ortho union filesystem mounted"
                );

                ConsolidatedOrthoMountResult::success(
                    mountpoint,
                    source_count,
                    file_count,
                    patch_names,
                    package_regions,
                )
            }
            Err(e) => {
                ConsolidatedOrthoMountResult::failure(mountpoint, format!("Failed to mount: {}", e))
            }
        }
    }

    /// Check if consolidated ortho is mounted.
    pub fn has_consolidated_ortho(&self) -> bool {
        self.consolidated_session.is_some()
    }

    /// Get consolidated ortho mount info (if mounted).
    pub fn consolidated_mount(&self) -> Option<&ActiveMount> {
        self.consolidated_mount.as_ref()
    }

    /// Take the DDS access event receiver for tile-based prefetching.
    ///
    /// This method takes ownership of the receiver, so it can only be called once.
    /// The receiver is created when mounting consolidated ortho and is used by
    /// the tile-based prefetcher to receive DDS access events from FUSE.
    ///
    /// # Returns
    ///
    /// `Some(receiver)` if consolidated ortho is mounted and receiver hasn't been taken,
    /// `None` otherwise.
    pub fn take_dds_access_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<DdsAccessEvent>> {
        self.dds_access_rx.take()
    }

    /// Take the Scene Tracker event receiver for empirical scenery tracking.
    ///
    /// This method takes ownership of the receiver, so it can only be called once.
    /// The receiver is created when mounting consolidated ortho and is used by
    /// the Scene Tracker to receive FUSE access events.
    ///
    /// # Returns
    ///
    /// `Some(receiver)` if consolidated ortho is mounted and receiver hasn't been taken,
    /// `None` otherwise.
    pub fn take_scene_tracker_receiver(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<FuseAccessEvent>> {
        self.scene_tracker_rx.take()
    }

    /// Get a reference to the OrthoUnionIndex for tile-based prefetching.
    ///
    /// The index is created when mounting consolidated ortho and can be used
    /// by the tile-based prefetcher to enumerate DDS files within DSF tiles.
    ///
    /// # Returns
    ///
    /// `Some(index)` if consolidated ortho is mounted, `None` otherwise.
    pub fn ortho_union_index(&self) -> Option<Arc<OrthoUnionIndex>> {
        self.ortho_union_index.clone()
    }

    /// Get the GeoIndex for geospatial ownership queries.
    ///
    /// The index is created when mounting consolidated ortho and populated
    /// with PatchCoverage data from patch sources.
    pub fn geo_index(&self) -> Option<Arc<GeoIndex>> {
        self.geo_index.clone()
    }

    /// Unmount a specific region.
    ///
    /// # Arguments
    ///
    /// * `region` - The region code to unmount
    pub fn unmount(&mut self, region: &str) -> ManagerResult<()> {
        let key = region.to_lowercase();

        if !self.sessions.contains_key(&key) {
            return Err(ManagerError::PackageNotFound {
                region: region.to_string(),
                package_type: "ortho".to_string(),
            });
        }

        // Unregister from panic handler before unmounting
        if let Some(mount) = self.mounts.get(&key) {
            panic_handler::unregister_mount(&mount.mountpoint);
        }

        // Remove session first - Drop will trigger unmount via fusermount
        self.sessions.remove(&key);
        // Then remove service - this shuts down the runtime
        self.services.remove(&key);
        self.mounts.remove(&key);

        Ok(())
    }

    /// Unmount all packages.
    ///
    /// Sessions are unmounted in reverse order of mounting.
    /// Patches are unmounted last (they were mounted first).
    pub fn unmount_all(&mut self) {
        // Collect keys to unmount (reverse order for clean shutdown)
        let keys: Vec<String> = self.sessions.keys().cloned().collect();

        for key in keys.into_iter().rev() {
            // Unregister from panic handler before unmounting
            if let Some(mount) = self.mounts.get(&key) {
                panic_handler::unregister_mount(&mount.mountpoint);
            }

            // Remove session first - Drop will trigger unmount via fusermount
            self.sessions.remove(&key);
            // Then remove service - this shuts down the runtime
            self.services.remove(&key);
            self.mounts.remove(&key);
        }

        // Unmount patches (if mounted separately)
        if let Some(ref mount) = self.patches_mount {
            panic_handler::unregister_mount(&mount.mountpoint);
        }
        self.patches_session = None;
        self.patches_mount = None;
        self.patches_service = None;

        // Unmount consolidated ortho (if mounted)
        if let Some(ref mount) = self.consolidated_mount {
            panic_handler::unregister_mount(&mount.mountpoint);
        }
        self.consolidated_session = None;
        self.consolidated_mount = None;
        self.consolidated_service = None;

        // Clear tile-based prefetch resources
        self.dds_access_rx = None;
        self.ortho_union_index = None;
        self.geo_index = None;

        // Clear Scene Tracker resources
        self.scene_tracker_rx = None;
    }

    /// Get a reference to a mounted service (if any).
    ///
    /// This returns a reference to the first available service, useful for
    /// accessing shared components like the DdsHandler for prefetch.
    pub fn get_service(&self) -> Option<&XEarthLayerService> {
        // Prefer consolidated service, then patches, then per-region services
        self.consolidated_service
            .as_ref()
            .or(self.patches_service.as_ref())
            .or_else(|| self.services.values().next())
    }

    /// Get the Scene Tracker for empirical X-Plane request tracking.
    ///
    /// The Scene Tracker maintains an empirical model of what X-Plane has
    /// requested, including:
    /// - Which DDS tiles have been accessed
    /// - Burst detection for loading patterns
    /// - Geographic region tracking
    ///
    pub fn scene_tracker(&self) -> Arc<DefaultSceneTracker> {
        Arc::clone(&self.scene_tracker)
    }

    /// Get aggregated telemetry from all mounted services.
    ///
    /// This combines metrics from all active service instances into a single
    /// snapshot for display purposes.
    pub fn aggregated_telemetry(&self) -> TelemetrySnapshot {
        let mut total = TelemetrySnapshot {
            uptime: Duration::ZERO,
            fuse_tiles_served: 0,
            fuse_requests_active: 0,
            fuse_requests_waiting: 0,
            jobs_submitted: 0,
            fuse_jobs_submitted: 0,
            jobs_completed: 0,
            jobs_failed: 0,
            jobs_timed_out: 0,
            jobs_active: 0,
            jobs_coalesced: 0,
            chunks_downloaded: 0,
            chunks_failed: 0,
            chunks_retried: 0,
            bytes_downloaded: 0,
            downloads_active: 0,
            memory_cache_hits: 0,
            memory_cache_misses: 0,
            memory_cache_hit_rate: 0.0,
            fuse_memory_cache_hit_rate: 0.0,
            memory_cache_size_bytes: 0,
            dds_disk_cache_hits: 0,
            dds_disk_cache_misses: 0,
            dds_disk_cache_hit_rate: 0.0,
            dds_disk_cache_size_bytes: 0,
            dds_disk_bytes_read: 0,
            chunk_disk_cache_hits: 0,
            chunk_disk_cache_misses: 0,
            chunk_disk_cache_hit_rate: 0.0,
            chunk_disk_cache_size_bytes: 0,
            chunk_disk_bytes_written: 0,
            chunk_disk_bytes_read: 0,
            encodes_completed: 0,
            encodes_active: 0,
            bytes_encoded: 0,
            jobs_per_second: 0.0,
            fuse_jobs_per_second: 0.0,
            chunks_per_second: 0.0,
            bytes_per_second: 0.0,
            peak_bytes_per_second: 0.0,
            total_download_time_ms: 0,
            total_assembly_time_ms: 0,
            total_encode_time_ms: 0,
        };

        // Collect all services: per-region + patches + consolidated
        let all_services: Vec<&XEarthLayerService> = self
            .services
            .values()
            .chain(self.patches_service.as_ref())
            .chain(self.consolidated_service.as_ref())
            .collect();

        for service in all_services {
            let snapshot = service.telemetry_snapshot();

            // Use the longest uptime (first service started)
            if snapshot.uptime > total.uptime {
                total.uptime = snapshot.uptime;
            }

            // Sum all counters
            total.fuse_tiles_served += snapshot.fuse_tiles_served;
            total.fuse_requests_active += snapshot.fuse_requests_active;
            total.fuse_requests_waiting += snapshot.fuse_requests_waiting;
            total.jobs_submitted += snapshot.jobs_submitted;
            total.fuse_jobs_submitted += snapshot.fuse_jobs_submitted;
            total.jobs_completed += snapshot.jobs_completed;
            total.jobs_failed += snapshot.jobs_failed;
            total.jobs_timed_out += snapshot.jobs_timed_out;
            total.jobs_active += snapshot.jobs_active;
            total.jobs_coalesced += snapshot.jobs_coalesced;
            total.chunks_downloaded += snapshot.chunks_downloaded;
            total.chunks_failed += snapshot.chunks_failed;
            total.chunks_retried += snapshot.chunks_retried;
            total.bytes_downloaded += snapshot.bytes_downloaded;
            total.downloads_active += snapshot.downloads_active;
            total.memory_cache_hits += snapshot.memory_cache_hits;
            total.memory_cache_misses += snapshot.memory_cache_misses;
            // Use max() for cache sizes since all services share the same cache
            // (summing would incorrectly report N times the actual size)
            total.memory_cache_size_bytes = total
                .memory_cache_size_bytes
                .max(snapshot.memory_cache_size_bytes);
            total.dds_disk_cache_hits += snapshot.dds_disk_cache_hits;
            total.dds_disk_cache_misses += snapshot.dds_disk_cache_misses;
            total.chunk_disk_cache_hits += snapshot.chunk_disk_cache_hits;
            total.chunk_disk_cache_misses += snapshot.chunk_disk_cache_misses;
            // Disk caches are also shared across services, so use max()
            total.dds_disk_cache_size_bytes = total
                .dds_disk_cache_size_bytes
                .max(snapshot.dds_disk_cache_size_bytes);
            total.chunk_disk_cache_size_bytes = total
                .chunk_disk_cache_size_bytes
                .max(snapshot.chunk_disk_cache_size_bytes);
            total.chunk_disk_bytes_written = total
                .chunk_disk_bytes_written
                .max(snapshot.chunk_disk_bytes_written);
            total.dds_disk_bytes_read = total.dds_disk_bytes_read.max(snapshot.dds_disk_bytes_read);
            total.chunk_disk_bytes_read = total
                .chunk_disk_bytes_read
                .max(snapshot.chunk_disk_bytes_read);
            total.encodes_completed += snapshot.encodes_completed;
            total.encodes_active += snapshot.encodes_active;
            total.bytes_encoded += snapshot.bytes_encoded;
            total.total_download_time_ms += snapshot.total_download_time_ms;
            total.total_assembly_time_ms += snapshot.total_assembly_time_ms;
            total.total_encode_time_ms += snapshot.total_encode_time_ms;

            // Use the highest peak from any service
            if snapshot.peak_bytes_per_second > total.peak_bytes_per_second {
                total.peak_bytes_per_second = snapshot.peak_bytes_per_second;
            }
        }

        // Recalculate rates based on aggregated data
        let uptime_secs = total.uptime.as_secs_f64().max(0.001);
        total.jobs_per_second = total.jobs_completed as f64 / uptime_secs;
        total.fuse_jobs_per_second = total.fuse_jobs_submitted as f64 / uptime_secs;
        total.chunks_per_second = total.chunks_downloaded as f64 / uptime_secs;
        total.bytes_per_second = total.bytes_downloaded as f64 / uptime_secs;

        // Recalculate cache hit rates
        let memory_total = total.memory_cache_hits + total.memory_cache_misses;
        total.memory_cache_hit_rate = if memory_total > 0 {
            total.memory_cache_hits as f64 / memory_total as f64
        } else {
            0.0
        };

        let dds_disk_total = total.dds_disk_cache_hits + total.dds_disk_cache_misses;
        total.dds_disk_cache_hit_rate = if dds_disk_total > 0 {
            total.dds_disk_cache_hits as f64 / dds_disk_total as f64
        } else {
            0.0
        };

        let chunk_disk_total = total.chunk_disk_cache_hits + total.chunk_disk_cache_misses;
        total.chunk_disk_cache_hit_rate = if chunk_disk_total > 0 {
            total.chunk_disk_cache_hits as f64 / chunk_disk_total as f64
        } else {
            0.0
        };

        total
    }
}

impl Default for MountManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MountManager {
    fn drop(&mut self) {
        // Ensure all mounts are cleaned up
        self.unmount_all();
    }
}

/// Builder for creating services for each package.
///
/// This helper creates properly configured service instances for mounting.
/// Uses [`XEarthLayerService::start()`] to create services with integrated
/// cache and metrics.
pub struct ServiceBuilder {
    service_config: ServiceConfig,
    provider_config: crate::provider::ProviderConfig,
    logger: Arc<dyn crate::log::Logger>,
    /// Shared disk I/O limiter across all service instances.
    /// Note: Currently unused as DiskCacheAdapter handles I/O internally.
    /// Kept for potential future use with shared I/O limiting.
    #[allow(dead_code)]
    disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    /// Shared tile request callback for FUSE-based position inference.
    /// When set, all services forward tile requests to this callback.
    tile_request_callback: Option<TileRequestCallback>,
}

impl ServiceBuilder {
    /// Create a new service builder.
    ///
    /// Creates a shared disk I/O concurrency limiter that will be used by
    /// all services built by this builder. This prevents disk I/O exhaustion
    /// when multiple packages are mounted simultaneously.
    ///
    /// The disk I/O limiter is tuned based on the configured or detected storage profile:
    /// - HDD: Conservative concurrency (1-4 ops)
    /// - SSD: Moderate concurrency (~32-64 ops)
    /// - NVMe: Aggressive concurrency (~128-256 ops)
    /// - Auto: Detects storage type from cache directory
    pub fn new(
        service_config: ServiceConfig,
        provider_config: crate::provider::ProviderConfig,
        logger: Arc<dyn crate::log::Logger>,
    ) -> Self {
        Self::with_disk_io_profile(
            service_config,
            provider_config,
            logger,
            DiskIoProfile::default(),
        )
    }

    /// Create a new service builder with a specific disk I/O profile.
    ///
    /// # Arguments
    ///
    /// * `disk_io_profile` - The disk I/O profile to use for concurrency limiting
    pub fn with_disk_io_profile(
        service_config: ServiceConfig,
        provider_config: crate::provider::ProviderConfig,
        logger: Arc<dyn crate::log::Logger>,
        disk_io_profile: DiskIoProfile,
    ) -> Self {
        // Resolve Auto profile based on cache directory (or current dir if not set)
        let resolved_profile = if let Some(cache_dir) = service_config.cache_directory() {
            disk_io_profile.resolve_for_path(cache_dir)
        } else {
            // If no cache directory is set, just use the profile as-is
            // (Auto will fall back to SSD in resolve_for_path)
            disk_io_profile.resolve_for_path(std::path::Path::new("."))
        };

        // Create limiter based on resolved profile
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::for_disk_io_profile(
            resolved_profile,
            "global_disk_io",
        ));

        tracing::info!(
            profile = %resolved_profile,
            max_concurrent = disk_io_limiter.max_concurrent(),
            "Created shared disk I/O limiter for multi-package mounting"
        );

        Self {
            service_config,
            provider_config,
            logger,
            disk_io_limiter,
            tile_request_callback: None,
        }
    }

    /// Set the tile request callback for FUSE-based position inference.
    ///
    /// When set, all services built by this builder will forward tile requests
    /// to this callback. This enables the `FuseRequestAnalyzer` to track tile
    /// loading patterns for position inference when telemetry is unavailable.
    ///
    /// # Arguments
    ///
    /// * `callback` - The callback to invoke for each tile request
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.tile_request_callback = Some(callback);
        self
    }

    /// Build a service using `XEarthLayerService::start()`.
    ///
    /// Creates a service with integrated metrics and cache, ensuring
    /// MetricsSystem, CacheLayer, and GC daemon are properly wired.
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to initialize.
    pub async fn build_service_async(&self) -> Result<XEarthLayerService, ServiceError> {
        let mut service = XEarthLayerService::start(
            self.service_config.clone(),
            self.provider_config.clone(),
            self.logger.clone(),
        )
        .await?;

        // Wire tile request callback for FUSE-based position inference
        if let Some(ref callback) = self.tile_request_callback {
            service.set_tile_request_callback(callback.clone());
        }

        tracing::info!(
            "Built service with integrated cache and metrics (XEarthLayerService::start)"
        );
        Ok(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_manager_new() {
        let manager = MountManager::new();
        assert_eq!(manager.mount_count(), 0);
        assert!(!manager.has_mounts());
    }

    #[test]
    fn test_mount_manager_default() {
        let manager = MountManager::default();
        assert_eq!(manager.mount_count(), 0);
    }

    #[test]
    fn test_list_mounts_empty() {
        let manager = MountManager::new();
        assert!(manager.list_mounts().is_empty());
    }

    #[test]
    fn test_is_mounted_empty() {
        let manager = MountManager::new();
        assert!(!manager.is_mounted("na"));
    }

    // Note: Integration tests requiring actual FUSE mounts are in integration tests
}
