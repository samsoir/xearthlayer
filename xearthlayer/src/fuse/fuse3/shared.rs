//! Shared traits and types for fuse3 filesystem implementations.
//!
//! This module provides common abstractions used across all fuse3 filesystems:
//! - [`Fuse3PassthroughFS`](super::Fuse3PassthroughFS) - Single directory overlay
//! - [`Fuse3UnionFS`](super::Fuse3UnionFS) - Patch union filesystem
//! - [`Fuse3OrthoUnionFS`](super::Fuse3OrthoUnionFS) - Consolidated ortho mount
//!
//! # Design Principles
//!
//! These traits follow SOLID principles:
//! - **Single Responsibility**: Each trait handles one concern
//! - **Interface Segregation**: Small, focused interfaces
//! - **Dependency Inversion**: Filesystems depend on abstractions

use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse3::raw::reply::FileAttr;
use fuse3::FileType;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use std::sync::Arc;

use crate::coord::TileCoord;
use crate::executor::DdsClient;
use crate::fuse::coalesce::{CoalesceResult, CoalescedResult, RequestCoalescer};
use crate::fuse::{get_default_placeholder, validate_dds_or_placeholder, DdsFilename};

/// Time-to-live for FUSE attribute caching.
///
/// This value is shared across all filesystem implementations.
pub const TTL: Duration = Duration::from_secs(1);

// =============================================================================
// VirtualDdsConfig - Shared DDS file configuration
// =============================================================================

/// Configuration for virtual DDS file attributes.
///
/// Virtual DDS files are generated on-demand by XEarthLayer. This struct
/// holds the configuration for reporting their attributes to FUSE.
#[derive(Debug, Clone)]
pub struct VirtualDdsConfig {
    /// Expected size of generated DDS files (bytes).
    size: u64,
    /// Block size for filesystem reporting.
    blksize: u32,
}

impl VirtualDdsConfig {
    /// Create a new virtual DDS configuration.
    ///
    /// # Arguments
    ///
    /// * `size` - Expected size of generated DDS files in bytes
    pub fn new(size: u64) -> Self {
        Self {
            size,
            blksize: 4096,
        }
    }

    /// Get the expected file size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Get the block size.
    pub fn blksize(&self) -> u32 {
        self.blksize
    }

    /// Calculate the number of blocks for this file size.
    pub fn blocks(&self) -> u64 {
        self.size.div_ceil(self.blksize as u64)
    }
}

impl Default for VirtualDdsConfig {
    fn default() -> Self {
        // Default DDS size: 4096x4096 BC1 with mipmaps ≈ 11MB
        Self::new(11_184_952)
    }
}

// =============================================================================
// FileAttrBuilder - Trait for building FUSE file attributes
// =============================================================================

/// Trait for building FUSE file attributes.
///
/// This trait provides methods to convert various sources into [`FileAttr`]
/// structs suitable for FUSE responses. Implementations should use this
/// trait to ensure consistent attribute generation across filesystems.
///
/// # Example
///
/// ```ignore
/// impl FileAttrBuilder for MyFilesystem {
///     fn virtual_dds_config(&self) -> &VirtualDdsConfig {
///         &self.dds_config
///     }
/// }
///
/// // Now use the default implementations:
/// let attr = fs.virtual_dds_attr(inode);
/// let dir_attr = fs.root_dir_attr();
/// ```
pub trait FileAttrBuilder {
    /// Get the virtual DDS configuration.
    fn virtual_dds_config(&self) -> &VirtualDdsConfig;

    /// Convert filesystem metadata to FUSE file attributes.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    /// * `metadata` - Standard library filesystem metadata
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
            ctime: UNIX_EPOCH.into(),
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
    ///
    /// Virtual DDS files are generated on-demand and don't exist on disk.
    /// This method creates appropriate attributes for FUSE responses.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    fn virtual_dds_attr(&self, ino: u64) -> FileAttr {
        let config = self.virtual_dds_config();
        let now = SystemTime::now().into();

        FileAttr {
            ino,
            size: config.size(),
            blocks: config.blocks(),
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::RegularFile,
            perm: 0o444, // Read-only
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: config.blksize(),
        }
    }

    /// Create attributes for the root directory.
    fn root_dir_attr(&self) -> FileAttr {
        let now = SystemTime::now().into();

        FileAttr {
            ino: 1,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }

    /// Create attributes for a virtual directory.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    fn virtual_dir_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now().into();

        FileAttr {
            ino,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }
}

// =============================================================================
// Tile Coordinate Conversion
// =============================================================================

/// Convert chunk-level DDS coordinates to tile-level coordinates.
///
/// DDS filenames use chunk coordinates (16x smaller grid than tiles).
/// This function converts chunk coordinates to tile coordinates for
/// the DDS generation pipeline.
///
/// # Coordinate Relationship
///
/// ```text
/// Chunk zoom = Tile zoom + 4
/// Chunk row  = Tile row * 16 + offset
/// Chunk col  = Tile col * 16 + offset
/// ```
///
/// # Arguments
///
/// * `coords` - Parsed DDS filename with chunk coordinates
///
/// # Returns
///
/// Tile coordinates suitable for the DDS handler
pub fn chunk_to_tile_coords(coords: &DdsFilename) -> TileCoord {
    let tile_zoom = coords.zoom.saturating_sub(4);
    TileCoord {
        row: coords.row / 16,
        col: coords.col / 16,
        zoom: tile_zoom,
    }
}

// =============================================================================
// DdsRequestor - Trait for DDS generation requests
// =============================================================================

/// Trait for requesting DDS texture generation.
///
/// This trait encapsulates the common pattern for requesting DDS textures
/// from the async pipeline, handling timeouts, cancellation, and validation.
///
/// # Implementation Notes
///
/// The default implementation handles:
/// 1. Converting chunk coordinates to tile coordinates
/// 2. Submitting requests to the DdsClient (new daemon architecture)
/// 3. Timeout handling with cancellation
/// 4. Response validation to prevent X-Plane crashes
///
/// # Migration
///
/// Implementations should provide `dds_client()`. The legacy `dds_handler()`
/// method exists for backward compatibility but should not be used in new code.
#[allow(async_fn_in_trait)] // Internal trait, Send bounds not needed
pub trait DdsRequestor: FileAttrBuilder {
    /// Get the DdsClient for submitting requests (new daemon architecture).
    ///
    /// This is the primary method for DDS generation. Implementations should
    /// return their DdsClient here. The client sends requests to the job
    /// executor daemon which handles tile generation asynchronously.
    fn dds_client(&self) -> &Arc<dyn DdsClient>;

    /// Get the timeout duration for DDS generation.
    fn generation_timeout(&self) -> Duration;

    /// Get the context label for logging.
    fn context_label(&self) -> &'static str;

    /// Get the optional tile request callback.
    fn tile_request_callback(&self) -> Option<&crate::prefetch::TileRequestCallback>;

    /// Get the optional request coalescer for deduplicating concurrent requests.
    ///
    /// When this returns `Some`, requests for the same tile are coalesced at the
    /// FUSE layer. Only one request is sent to the executor; all others wait for
    /// the same result. This prevents duplicate work during X-Plane's burst
    /// loading patterns.
    ///
    /// When `None` (default), no coalescing is performed at the FUSE layer.
    fn request_coalescer(&self) -> Option<&Arc<RequestCoalescer>> {
        None
    }

    /// Get the optional metrics client for reporting FUSE-level metrics.
    ///
    /// When `Some`, coalesced requests and other FUSE-specific metrics are
    /// reported to the metrics system.
    fn metrics_client(&self) -> Option<&crate::metrics::MetricsClient> {
        None
    }

    /// Request DDS generation from the async pipeline.
    ///
    /// This method handles the full request lifecycle:
    /// 1. Converts coordinates and notifies tile callback
    /// 2. **Coalescing check**: If a coalescer is configured, duplicate requests
    ///    for the same tile wait for the in-flight result instead of starting new work
    /// 3. Submits request via DdsClient (daemon architecture)
    /// 4. Awaits response with timeout
    /// 5. Validates DDS data before returning
    ///
    /// # Request Coalescing
    ///
    /// If `request_coalescer()` returns `Some`, requests are deduplicated at the
    /// FUSE layer. X-Plane commonly requests the same tile multiple times during
    /// scene loading - coalescing ensures only one actual generation runs.
    ///
    /// # Arguments
    ///
    /// * `coords` - Parsed DDS filename coordinates
    ///
    /// # Returns
    ///
    /// Generated DDS data, or placeholder on error/timeout
    async fn request_dds_impl(&self, coords: &DdsFilename) -> Vec<u8> {
        let tile = chunk_to_tile_coords(coords);
        let context_label = self.context_label();
        let timeout = self.generation_timeout();

        // Notify tile request callback for FUSE inference (fast, non-blocking)
        if let Some(callback) = self.tile_request_callback() {
            callback(tile);
        }

        let using_coalescer = self.request_coalescer().is_some();

        debug!(
            chunk_row = coords.row,
            chunk_col = coords.col,
            chunk_zoom = coords.zoom,
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            context = context_label,
            using_coalescer,
            "Requesting DDS generation"
        );

        // Request coalescing: if enabled, check for in-flight requests
        let data = if let Some(coalescer) = self.request_coalescer() {
            let coalesce_result = coalescer.register(tile);

            match coalesce_result {
                CoalesceResult::Coalesced(mut rx) => {
                    // Another request is in-flight - wait for its result
                    debug!(
                        tile = ?tile,
                        context = context_label,
                        "Request coalesced - waiting for in-flight result"
                    );

                    // Report coalesced request to metrics
                    if let Some(metrics) = self.metrics_client() {
                        metrics.job_coalesced();
                    }

                    match tokio::time::timeout(timeout, rx.recv()).await {
                        Ok(Ok(result)) => {
                            debug!(
                                tile = ?tile,
                                cache_hit = result.cache_hit,
                                duration_ms = result.duration.as_millis(),
                                context = context_label,
                                "Coalesced request completed"
                            );
                            result.into_data()
                        }
                        Ok(Err(_)) | Err(_) => {
                            // Broadcast channel error or timeout - return placeholder
                            warn!(
                                tile = ?tile,
                                context = context_label,
                                "Coalesced request failed - using placeholder"
                            );
                            get_default_placeholder()
                        }
                    }
                }
                CoalesceResult::NewRequest { tile, .. } => {
                    // This is the first request - do the actual work
                    let start = std::time::Instant::now();
                    let data = self.do_request(tile, timeout, context_label).await;
                    let duration = start.elapsed();

                    // Complete the coalescer so waiting requests receive the result
                    let result = CoalescedResult::new(data.clone(), false, duration);
                    coalescer.complete(tile, result);

                    data
                }
            }
        } else {
            // No coalescing - send request directly
            self.do_request(tile, timeout, context_label).await
        };

        // Critical: Validate DDS before returning to X-Plane
        // Invalid DDS data causes X-Plane to crash
        let validation_context =
            format!("{}({},{},{})", context_label, tile.row, tile.col, tile.zoom);
        validate_dds_or_placeholder(data, &validation_context)
    }

    /// Perform the actual DDS request (without coalescing).
    ///
    /// Sends the request to the job executor daemon via DdsClient.
    async fn do_request(
        &self,
        tile: TileCoord,
        timeout: Duration,
        context_label: &'static str,
    ) -> Vec<u8> {
        let client = self.dds_client();
        let cancellation_token = CancellationToken::new();

        // Submit request via DdsClient
        let rx = client.request_dds(tile, cancellation_token.clone());

        // Await response with timeout
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                debug!(
                    tile_row = tile.row,
                    tile_col = tile.col,
                    tile_zoom = tile.zoom,
                    cache_hit = response.cache_hit,
                    duration_ms = response.duration.as_millis(),
                    context = context_label,
                    "DDS request completed"
                );
                response.data
            }
            Ok(Err(_)) => {
                // Channel closed - daemon shutdown
                error!(
                    context = context_label,
                    "DDS daemon channel closed unexpectedly"
                );
                cancellation_token.cancel();
                get_default_placeholder()
            }
            Err(_) => {
                // Timeout - cancel the request
                // Enhanced logging for stall diagnosis
                error!(
                    tile_row = tile.row,
                    tile_col = tile.col,
                    tile_zoom = tile.zoom,
                    timeout_secs = timeout.as_secs(),
                    context = context_label,
                    "TIMEOUT: DDS generation exceeded {}s - possible executor stall. \
                     Tile: ({},{},{}) Context: {}. Returning placeholder.",
                    timeout.as_secs(),
                    tile.row,
                    tile.col,
                    tile.zoom,
                    context_label
                );
                cancellation_token.cancel();
                get_default_placeholder()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_dds_config_new() {
        let config = VirtualDdsConfig::new(1024);
        assert_eq!(config.size(), 1024);
        assert_eq!(config.blksize(), 4096);
    }

    #[test]
    fn test_virtual_dds_config_blocks() {
        // Exact block boundary
        let config = VirtualDdsConfig::new(4096);
        assert_eq!(config.blocks(), 1);

        // One byte over
        let config = VirtualDdsConfig::new(4097);
        assert_eq!(config.blocks(), 2);

        // Large file
        let config = VirtualDdsConfig::new(11_184_952);
        assert_eq!(config.blocks(), 2731);
    }

    #[test]
    fn test_virtual_dds_config_default() {
        let config = VirtualDdsConfig::default();
        assert_eq!(config.size(), 11_184_952);
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        // Standard zoom 18 chunk
        let coords = DdsFilename {
            row: 100000,
            col: 125184,
            zoom: 18,
            map_type: "BI".to_string(),
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.row, 100000 / 16);
        assert_eq!(tile.col, 125184 / 16);
        assert_eq!(tile.zoom, 14);
    }

    #[test]
    fn test_chunk_to_tile_coords_zoom_16() {
        let coords = DdsFilename {
            row: 25264,
            col: 10368,
            zoom: 16,
            map_type: "GO2".to_string(),
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.row, 25264 / 16);
        assert_eq!(tile.col, 10368 / 16);
        assert_eq!(tile.zoom, 12);
    }

    #[test]
    fn test_chunk_to_tile_coords_low_zoom() {
        // Edge case: zoom less than 4
        let coords = DdsFilename {
            row: 100,
            col: 200,
            zoom: 2,
            map_type: "BI".to_string(),
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.zoom, 0); // saturating_sub prevents underflow
    }
}
