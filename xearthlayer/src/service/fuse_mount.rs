//! FUSE filesystem mounting service.
//!
//! This module provides methods for mounting FUSE passthrough filesystems
//! that generate DDS textures on-demand. All methods use the fuse3 async
//! multi-threaded backend for optimal performance.
//!
//! # Mounting Modes
//!
//! Three mounting modes are available:
//!
//! - **Async** ([`mount_fuse3`]) - Returns a `MountHandle` for async contexts
//! - **Blocking** ([`mount_fuse3_blocking`]) - Blocks until unmounted (for CLI)
//! - **Spawned** ([`mount_fuse3_spawned`]) - Background task with safe sync drop
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::service::{FuseMountService, FuseMountConfig};
//!
//! let config = FuseMountConfig::new()
//!     .with_dds_handler(handler)
//!     .with_expected_size(expected_size)
//!     .with_timeout(Duration::from_secs(30));
//!
//! // Async mount
//! let handle = FuseMountService::mount_fuse3(&config, source, mountpoint).await?;
//!
//! // Blocking mount (CLI)
//! FuseMountService::mount_fuse3_blocking(&config, source, mountpoint, &runtime_handle)?;
//! ```

use std::path::PathBuf;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::fuse::{DdsHandler, Fuse3PassthroughFS, MountHandle, SpawnedMountHandle};
use crate::log::Logger;
use crate::log_info;

use super::error::ServiceError;

/// Configuration for FUSE filesystem mounting.
#[derive(Clone)]
pub struct FuseMountConfig {
    /// Handler for DDS generation requests
    dds_handler: DdsHandler,
    /// Expected size of generated DDS files
    expected_dds_size: usize,
    /// Generation timeout
    timeout: Duration,
    /// Optional logger for diagnostic messages
    logger: Option<std::sync::Arc<dyn Logger>>,
}

impl FuseMountConfig {
    /// Create a new mount configuration.
    ///
    /// # Arguments
    ///
    /// * `dds_handler` - Handler for DDS generation requests
    /// * `expected_dds_size` - Expected size of generated DDS files in bytes
    pub fn new(dds_handler: DdsHandler, expected_dds_size: usize) -> Self {
        Self {
            dds_handler,
            expected_dds_size,
            timeout: Duration::from_secs(30),
            logger: None,
        }
    }

    /// Set the generation timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the logger for diagnostic messages.
    pub fn with_logger(mut self, logger: std::sync::Arc<dyn Logger>) -> Self {
        self.logger = Some(logger);
        self
    }
}

/// Service for mounting FUSE passthrough filesystems.
///
/// This struct provides static methods for mounting filesystems in different
/// modes. It encapsulates path validation and filesystem creation.
pub struct FuseMountService;

impl FuseMountService {
    /// Validate that source and mountpoint paths exist.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// Tuple of (source_path, mountpoint_path) on success.
    ///
    /// # Errors
    ///
    /// Returns `ServiceError::IoError` if either path doesn't exist.
    fn validate_paths(
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<(PathBuf, PathBuf), ServiceError> {
        let source_path = PathBuf::from(source_dir);
        if !source_path.exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Source directory does not exist: {}", source_dir),
            )));
        }

        let mount_path = PathBuf::from(mountpoint);
        if !mount_path.exists() {
            return Err(ServiceError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Mountpoint does not exist: {}", mountpoint),
            )));
        }

        Ok((source_path, mount_path))
    }

    /// Create a Fuse3PassthroughFS instance with the given configuration.
    fn create_filesystem(config: &FuseMountConfig, source_path: PathBuf) -> Fuse3PassthroughFS {
        Fuse3PassthroughFS::new(
            source_path,
            config.dds_handler.clone(),
            config.expected_dds_size,
        )
        .with_timeout(config.timeout)
    }

    /// Mount a FUSE passthrough filesystem (async).
    ///
    /// Uses the fuse3 library which runs all FUSE operations asynchronously
    /// on the Tokio runtime, enabling true parallel I/O processing.
    ///
    /// # Arguments
    ///
    /// * `config` - Mount configuration
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `MountHandle` that keeps the filesystem mounted. When awaited, it
    /// resolves when the filesystem is unmounted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source directory doesn't exist
    /// - Mountpoint directory doesn't exist
    /// - FUSE mount fails
    pub async fn mount_fuse3(
        config: &FuseMountConfig,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<MountHandle, ServiceError> {
        let (source_path, _) = Self::validate_paths(source_dir, mountpoint)?;

        if let Some(ref logger) = config.logger {
            log_info!(
                logger,
                "Mounting fuse3 passthrough: {} -> {}",
                source_dir,
                mountpoint
            );
        }

        let fs = Self::create_filesystem(config, source_path);
        fs.mount(mountpoint)
            .await
            .map_err(|e| ServiceError::FuseError(e.to_string()))
    }

    /// Mount a FUSE passthrough filesystem (blocking).
    ///
    /// This is a convenience wrapper that blocks until the filesystem is
    /// unmounted. Suitable for CLI applications.
    ///
    /// # Arguments
    ///
    /// * `config` - Mount configuration
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    /// * `runtime_handle` - Handle to the Tokio runtime
    ///
    /// # Note
    ///
    /// This method blocks until the filesystem is unmounted (e.g., via Ctrl+C
    /// or `fusermount -u`).
    pub fn mount_fuse3_blocking(
        config: &FuseMountConfig,
        source_dir: &str,
        mountpoint: &str,
        runtime_handle: &Handle,
    ) -> Result<(), ServiceError> {
        runtime_handle.block_on(async {
            let handle = Self::mount_fuse3(config, source_dir, mountpoint).await?;
            handle
                .await
                .map_err(|e| ServiceError::FuseError(e.to_string()))
        })
    }

    /// Mount a FUSE passthrough filesystem as a background task.
    ///
    /// Spawns the mount as a background Tokio task, returning a handle that
    /// can be safely stored and dropped outside of an async context.
    ///
    /// # Arguments
    ///
    /// * `config` - Mount configuration
    /// * `source_dir` - Path to the scenery pack directory to overlay
    /// * `mountpoint` - Path where the virtual filesystem will be mounted
    ///
    /// # Returns
    ///
    /// A `SpawnedMountHandle` that keeps the filesystem mounted. The handle
    /// can be dropped safely from any context - it will use `fusermount -u`
    /// as a fallback for cleanup if needed.
    pub async fn mount_fuse3_spawned(
        config: &FuseMountConfig,
        source_dir: &str,
        mountpoint: &str,
    ) -> Result<SpawnedMountHandle, ServiceError> {
        let (source_path, _) = Self::validate_paths(source_dir, mountpoint)?;

        if let Some(ref logger) = config.logger {
            log_info!(
                logger,
                "Mounting fuse3 passthrough (spawned): {} -> {}",
                source_dir,
                mountpoint
            );
        }

        let fs = Self::create_filesystem(config, source_path);
        fs.mount_spawned(mountpoint)
            .await
            .map_err(|e| ServiceError::FuseError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn create_test_handler() -> DdsHandler {
        Arc::new(|_req| {
            // Test handler does nothing
        })
    }

    #[test]
    fn test_mount_config_new() {
        let handler = create_test_handler();
        let config = FuseMountConfig::new(handler, 4096 * 4096);

        assert_eq!(config.expected_dds_size, 4096 * 4096);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(config.logger.is_none());
    }

    #[test]
    fn test_mount_config_with_timeout() {
        let handler = create_test_handler();
        let config = FuseMountConfig::new(handler, 1024).with_timeout(Duration::from_secs(60));

        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_validate_paths_source_not_found() {
        let result = FuseMountService::validate_paths("/nonexistent/source", "/tmp");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Source directory does not exist"));
    }

    #[test]
    fn test_validate_paths_mountpoint_not_found() {
        let temp = tempdir().unwrap();
        let source = temp.path().to_str().unwrap();

        let result = FuseMountService::validate_paths(source, "/nonexistent/mountpoint");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Mountpoint does not exist"));
    }

    #[test]
    fn test_validate_paths_success() {
        let source_temp = tempdir().unwrap();
        let mount_temp = tempdir().unwrap();
        let source = source_temp.path().to_str().unwrap();
        let mountpoint = mount_temp.path().to_str().unwrap();

        let result = FuseMountService::validate_paths(source, mountpoint);

        assert!(result.is_ok());
        let (source_path, mount_path) = result.unwrap();
        assert_eq!(source_path, source_temp.path());
        assert_eq!(mount_path, mount_temp.path());
    }
}
