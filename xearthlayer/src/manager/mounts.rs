//! Multi-mount manager for tracking and managing FUSE mounts.
//!
//! This module provides the `MountManager` which coordinates mounting
//! multiple ortho packages simultaneously. Each ortho package gets its
//! own FUSE mount where DDS files are generated on-demand.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::fuse::SpawnedMountHandle;
use crate::package::PackageType;
use crate::service::{ServiceConfig, ServiceError, XEarthLayerService};
use crate::telemetry::TelemetrySnapshot;

use super::local::{InstalledPackage, LocalPackageStore};
use super::{ManagerError, ManagerResult};

/// Result of mounting a single package.
#[derive(Debug)]
pub struct MountResult {
    /// The region code.
    pub region: String,
    /// The package type.
    pub package_type: PackageType,
    /// Path where the package is mounted.
    pub mountpoint: PathBuf,
    /// Whether the mount succeeded.
    pub success: bool,
    /// Error message if mount failed.
    pub error: Option<String>,
}

impl MountResult {
    /// Create a successful mount result.
    pub fn success(region: String, package_type: PackageType, mountpoint: PathBuf) -> Self {
        Self {
            region,
            package_type,
            mountpoint,
            success: true,
            error: None,
        }
    }

    /// Create a failed mount result.
    pub fn failure(
        region: String,
        package_type: PackageType,
        mountpoint: PathBuf,
        error: String,
    ) -> Self {
        Self {
            region,
            package_type,
            mountpoint,
            success: false,
            error: Some(error),
        }
    }
}

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
}

impl MountManager {
    /// Create a new mount manager that mounts packages in-place.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            services: HashMap::new(),
            mounts: HashMap::new(),
            scenery_path: None,
        }
    }

    /// Create a new mount manager that mounts packages to a target scenery directory.
    ///
    /// When a scenery path is set, packages from `install_location` are mounted
    /// as directories in the scenery path (e.g., Custom Scenery).
    pub fn with_scenery_path(scenery_path: &std::path::Path) -> Self {
        Self {
            sessions: HashMap::new(),
            services: HashMap::new(),
            mounts: HashMap::new(),
            scenery_path: Some(scenery_path.to_path_buf()),
        }
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

    /// Mount all installed ortho packages.
    ///
    /// Discovers installed ortho packages from the store and mounts each one.
    /// Returns results for all packages, including failures.
    ///
    /// # Arguments
    ///
    /// * `store` - The local package store to discover packages from
    /// * `service_factory` - Factory function to create service instances
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut manager = MountManager::new();
    /// let store = LocalPackageStore::new("/path/to/scenery");
    /// let results = manager.mount_all(&store, |pkg| {
    ///     create_service_for_package(pkg, &service_config, &provider_config)
    /// })?;
    /// ```
    pub fn mount_all<F>(
        &mut self,
        store: &LocalPackageStore,
        service_factory: F,
    ) -> ManagerResult<Vec<MountResult>>
    where
        F: Fn(&InstalledPackage) -> Result<XEarthLayerService, ServiceError>,
    {
        let packages = store.list()?;
        let mut results = Vec::new();

        for package in packages {
            // Only mount ortho packages
            if package.package_type() != PackageType::Ortho {
                continue;
            }

            let result = self.mount_package(&package, &service_factory);
            results.push(result);
        }

        Ok(results)
    }

    /// Mount a single package.
    ///
    /// # Arguments
    ///
    /// * `package` - The installed package to mount
    /// * `service_factory` - Factory function to create a service instance
    pub fn mount_package<F>(
        &mut self,
        package: &InstalledPackage,
        service_factory: F,
    ) -> MountResult
    where
        F: Fn(&InstalledPackage) -> Result<XEarthLayerService, ServiceError>,
    {
        let region = package.region().to_lowercase();

        // Determine mountpoint: scenery_path/package_name or in-place
        let mountpoint = if let Some(ref scenery_path) = self.scenery_path {
            // Mount to Custom Scenery directory with package name
            let folder_name = package
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("zzXEL_{}_ortho", region));
            scenery_path.join(&folder_name)
        } else {
            // Mount in-place
            package.path.clone()
        };

        // Skip if already mounted
        if self.is_mounted(&region) {
            return MountResult::failure(
                region,
                package.package_type(),
                mountpoint,
                "Already mounted".to_string(),
            );
        }

        // Only mount ortho packages
        if package.package_type() != PackageType::Ortho {
            return MountResult::failure(
                region,
                package.package_type(),
                mountpoint,
                "Only ortho packages can be mounted".to_string(),
            );
        }

        // Create service for this package
        let service = match service_factory(package) {
            Ok(s) => s,
            Err(e) => {
                return MountResult::failure(
                    region,
                    package.package_type(),
                    mountpoint,
                    format!("Failed to create service: {}", e),
                );
            }
        };

        // Create mountpoint directory if it doesn't exist
        if !mountpoint.exists() {
            if let Err(e) = std::fs::create_dir_all(&mountpoint) {
                return MountResult::failure(
                    region,
                    package.package_type(),
                    mountpoint,
                    format!("Failed to create mountpoint directory: {}", e),
                );
            }
        }

        // Mount the filesystem using fuse3 async multi-threaded backend
        // Source = the installed package directory
        // Mountpoint = either in Custom Scenery (if scenery_path set) or in-place
        let source_str = package.path.to_string_lossy();
        let mountpoint_str = mountpoint.to_string_lossy();

        // Use fuse3 async multi-threaded backend with spawned task
        // This uses SpawnedMountHandle which can be safely dropped from any context
        let mount_result = service
            .runtime_handle()
            .block_on(service.serve_passthrough_fuse3_spawned(&source_str, &mountpoint_str));

        match mount_result {
            Ok(session) => {
                let mount_info = ActiveMount {
                    region: region.clone(),
                    package_type: package.package_type(),
                    mountpoint: mountpoint.clone(),
                };

                self.sessions.insert(region.clone(), session);
                // CRITICAL: Store the service to keep its Tokio runtime alive.
                // The DdsHandler closure holds a Handle to the runtime owned by the service.
                // If the service is dropped, the runtime shuts down and spawned tasks abort.
                self.services.insert(region.clone(), service);
                self.mounts.insert(region.clone(), mount_info);

                MountResult::success(region, package.package_type(), mountpoint)
            }
            Err(e) => MountResult::failure(
                region,
                package.package_type(),
                mountpoint,
                format!("Failed to mount: {}", e),
            ),
        }
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
    pub fn unmount_all(&mut self) {
        // Collect keys to unmount (reverse order for clean shutdown)
        let keys: Vec<String> = self.sessions.keys().cloned().collect();

        for key in keys.into_iter().rev() {
            // Remove session first - Drop will trigger unmount via fusermount
            self.sessions.remove(&key);
            // Then remove service - this shuts down the runtime
            self.services.remove(&key);
            self.mounts.remove(&key);
        }
    }

    /// Get aggregated telemetry from all mounted services.
    ///
    /// This combines metrics from all active service instances into a single
    /// snapshot for display purposes.
    pub fn aggregated_telemetry(&self) -> TelemetrySnapshot {
        let mut total = TelemetrySnapshot {
            uptime: Duration::ZERO,
            fuse_requests_active: 0,
            fuse_requests_waiting: 0,
            jobs_submitted: 0,
            jobs_completed: 0,
            jobs_failed: 0,
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
            memory_cache_size_bytes: 0,
            disk_cache_hits: 0,
            disk_cache_misses: 0,
            disk_cache_hit_rate: 0.0,
            disk_cache_size_bytes: 0,
            encodes_completed: 0,
            encodes_active: 0,
            bytes_encoded: 0,
            jobs_per_second: 0.0,
            chunks_per_second: 0.0,
            bytes_per_second: 0.0,
            peak_bytes_per_second: 0.0,
            total_download_time_ms: 0,
            total_assembly_time_ms: 0,
            total_encode_time_ms: 0,
        };

        for service in self.services.values() {
            let snapshot = service.telemetry_snapshot();

            // Use the longest uptime (first service started)
            if snapshot.uptime > total.uptime {
                total.uptime = snapshot.uptime;
            }

            // Sum all counters
            total.fuse_requests_active += snapshot.fuse_requests_active;
            total.fuse_requests_waiting += snapshot.fuse_requests_waiting;
            total.jobs_submitted += snapshot.jobs_submitted;
            total.jobs_completed += snapshot.jobs_completed;
            total.jobs_failed += snapshot.jobs_failed;
            total.jobs_active += snapshot.jobs_active;
            total.jobs_coalesced += snapshot.jobs_coalesced;
            total.chunks_downloaded += snapshot.chunks_downloaded;
            total.chunks_failed += snapshot.chunks_failed;
            total.chunks_retried += snapshot.chunks_retried;
            total.bytes_downloaded += snapshot.bytes_downloaded;
            total.downloads_active += snapshot.downloads_active;
            total.memory_cache_hits += snapshot.memory_cache_hits;
            total.memory_cache_misses += snapshot.memory_cache_misses;
            total.memory_cache_size_bytes += snapshot.memory_cache_size_bytes;
            total.disk_cache_hits += snapshot.disk_cache_hits;
            total.disk_cache_misses += snapshot.disk_cache_misses;
            total.disk_cache_size_bytes += snapshot.disk_cache_size_bytes;
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
        total.chunks_per_second = total.chunks_downloaded as f64 / uptime_secs;
        total.bytes_per_second = total.bytes_downloaded as f64 / uptime_secs;

        // Recalculate cache hit rates
        let memory_total = total.memory_cache_hits + total.memory_cache_misses;
        total.memory_cache_hit_rate = if memory_total > 0 {
            total.memory_cache_hits as f64 / memory_total as f64
        } else {
            0.0
        };

        let disk_total = total.disk_cache_hits + total.disk_cache_misses;
        total.disk_cache_hit_rate = if disk_total > 0 {
            total.disk_cache_hits as f64 / disk_total as f64
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
pub struct ServiceBuilder {
    service_config: ServiceConfig,
    provider_config: crate::provider::ProviderConfig,
    logger: Arc<dyn crate::log::Logger>,
}

impl ServiceBuilder {
    /// Create a new service builder.
    pub fn new(
        service_config: ServiceConfig,
        provider_config: crate::provider::ProviderConfig,
        logger: Arc<dyn crate::log::Logger>,
    ) -> Self {
        Self {
            service_config,
            provider_config,
            logger,
        }
    }

    /// Build a service for the given package.
    pub fn build(&self, _package: &InstalledPackage) -> Result<XEarthLayerService, ServiceError> {
        XEarthLayerService::new(
            self.service_config.clone(),
            self.provider_config.clone(),
            self.logger.clone(),
        )
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
    fn test_mount_result_success() {
        let result = MountResult::success(
            "na".to_string(),
            PackageType::Ortho,
            PathBuf::from("/path/to/mount"),
        );
        assert!(result.success);
        assert!(result.error.is_none());
        assert_eq!(result.region, "na");
    }

    #[test]
    fn test_mount_result_failure() {
        let result = MountResult::failure(
            "eu".to_string(),
            PackageType::Ortho,
            PathBuf::from("/path/to/mount"),
            "Test error".to_string(),
        );
        assert!(!result.success);
        assert_eq!(result.error, Some("Test error".to_string()));
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
