//! AsyncPassthroughFS implementation.
//!
//! This module contains the FUSE filesystem implementation that coordinates
//! between real file passthrough and virtual DDS generation.

use super::attributes::{
    file_type_from_metadata, metadata_to_attr, virtual_dds_attr, VirtualDdsConfig,
};
use super::inode::InodeManager;
use super::types::{DdsHandler, DdsRequest, DdsResponse};
use crate::coord::TileCoord;
use crate::executor::JobId;
use crate::fuse::{get_default_placeholder, parse_dds_filename, DdsFilename};
use fuser::{FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request};
use libc::{ENOENT, ENOTDIR};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

/// Time-to-live for attribute caching.
const TTL: Duration = Duration::from_secs(1);

/// Async passthrough FUSE filesystem with pipeline integration.
///
/// This filesystem overlays an existing scenery pack:
/// - Real files are passed through directly from the source directory
/// - DDS textures that don't exist are generated via the async pipeline
///
/// # Responsibilities
///
/// This struct acts as a coordinator, delegating to:
/// - [`InodeManager`] for inode allocation and mapping
/// - [`DdsHandler`] for DDS generation requests
/// - Attribute helpers for file metadata conversion
///
/// # Example
///
/// ```ignore
/// use xearthlayer::fuse::async_passthrough::{AsyncPassthroughFS, DdsHandler};
/// use std::sync::Arc;
/// use tokio::runtime::Handle;
///
/// let handler: DdsHandler = Arc::new(|req| {
///     // Process DDS request...
/// });
///
/// let fs = AsyncPassthroughFS::new(
///     PathBuf::from("/scenery/pack"),
///     handler,
///     Handle::current(),
///     11_174_016, // Expected BC1 DDS size
/// );
/// ```
pub struct AsyncPassthroughFS {
    /// Source directory containing the scenery pack
    source_dir: PathBuf,
    /// Handler for DDS generation requests
    dds_handler: DdsHandler,
    /// Handle to the Tokio runtime for blocking on channels
    runtime_handle: Handle,
    /// Inode manager for path/coordinate mappings
    inode_manager: InodeManager,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
}

impl AsyncPassthroughFS {
    /// Create a new async passthrough filesystem.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory
    /// * `dds_handler` - Handler function for DDS generation requests
    /// * `runtime_handle` - Handle to the Tokio runtime
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(
        source_dir: PathBuf,
        dds_handler: DdsHandler,
        runtime_handle: Handle,
        expected_dds_size: usize,
    ) -> Self {
        Self {
            inode_manager: InodeManager::new(source_dir.clone()),
            source_dir,
            dds_handler,
            runtime_handle,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
        }
    }

    /// Set the timeout for DDS generation.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Request DDS generation from the async pipeline.
    ///
    /// This method blocks the calling thread until the DDS is generated
    /// or the timeout is reached.
    fn request_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        let job_id = JobId::auto();

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
            "Requesting DDS generation"
        );

        // Create oneshot channel for response
        let (tx, rx) = oneshot::channel();

        // Create cancellation token (note: fuser-based implementation is synchronous
        // and doesn't benefit from cancellation as much as fuse3, but we include it
        // for API consistency)
        let cancellation_token = CancellationToken::new();

        let request = DdsRequest {
            job_id: job_id.clone(),
            tile,
            result_tx: tx,
            cancellation_token: cancellation_token.clone(),
            is_prefetch: false,
        };

        // Submit request to the handler
        (self.dds_handler)(request);

        // Block waiting for response with timeout
        // If timeout occurs, cancel the token to abort any in-flight work
        let result = self.wait_for_response(job_id, rx);

        // Cancel if we timed out or got an error (the wait_for_response already
        // returned a placeholder in those cases, so this just ensures cleanup)
        if result.len() == get_default_placeholder().len() && result == get_default_placeholder() {
            cancellation_token.cancel();
        }

        result
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

    /// Wait for DDS response with timeout handling.
    fn wait_for_response(&self, job_id: JobId, rx: oneshot::Receiver<DdsResponse>) -> Vec<u8> {
        let result = self
            .runtime_handle
            .block_on(async { tokio::time::timeout(self.generation_timeout, rx).await });

        match result {
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
                get_default_placeholder()
            }
            Err(_) => {
                // Timeout
                warn!(
                    job_id = %job_id,
                    timeout_secs = self.generation_timeout.as_secs(),
                    "DDS generation timed out"
                );
                get_default_placeholder()
            }
        }
    }

    /// Read data from a real file on disk.
    fn read_real_file(&self, path: &PathBuf, offset: i64, size: u32, reply: ReplyData) {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                error!(path = ?path, error = %e, "Failed to open file");
                reply.error(libc::EIO);
                return;
            }
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset as u64)) {
            error!(path = ?path, error = %e, "Failed to seek");
            reply.error(libc::EIO);
            return;
        }

        let mut buffer = vec![0u8; size as usize];
        match file.read(&mut buffer) {
            Ok(bytes_read) => {
                buffer.truncate(bytes_read);
                reply.data(&buffer);
            }
            Err(e) => {
                error!(path = ?path, error = %e, "Failed to read");
                reply.error(libc::EIO);
            }
        }
    }
}

impl Filesystem for AsyncPassthroughFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!(parent = parent, name = ?name, "lookup");

        // Get parent path
        let parent_path = match self.inode_manager.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let child_path = parent_path.join(name);
        let name_str = name.to_string_lossy();

        // Check if the file exists on disk
        if child_path.exists() {
            match fs::metadata(&child_path) {
                Ok(metadata) => {
                    let inode = self.inode_manager.get_or_create_inode(&child_path);
                    let attr = metadata_to_attr(inode, &metadata);
                    reply.entry(&TTL, &attr, 0);
                    return;
                }
                Err(e) => {
                    debug!(
                        path = ?child_path,
                        error = %e,
                        "Failed to get metadata"
                    );
                }
            }
        }

        // File doesn't exist - check if it's a DDS file we can generate
        if name_str.ends_with(".dds") {
            if let Ok(coords) = parse_dds_filename(&name_str) {
                let inode = self.inode_manager.create_virtual_inode(coords);
                let attr = virtual_dds_attr(inode, &self.virtual_dds_config);
                reply.entry(&TTL, &attr, 0);
                return;
            }
        }

        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        debug!(ino = ino, "getattr");

        // Check if it's a virtual inode
        if InodeManager::is_virtual_inode(ino) {
            if self.inode_manager.get_virtual_dds(ino).is_some() {
                let attr = virtual_dds_attr(ino, &self.virtual_dds_config);
                reply.attr(&TTL, &attr);
                return;
            }
            reply.error(ENOENT);
            return;
        }

        // Real file
        let path = match self.inode_manager.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match fs::metadata(&path) {
            Ok(metadata) => {
                let attr = metadata_to_attr(ino, &metadata);
                reply.attr(&TTL, &attr);
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        debug!(ino = ino, offset = offset, size = size, "read");

        // Check if it's a virtual DDS file
        if InodeManager::is_virtual_inode(ino) {
            let coords = match self.inode_manager.get_virtual_dds(ino) {
                Some(c) => c,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };

            // Request DDS from the async pipeline
            let data = self.request_dds(&coords);
            let offset = offset as usize;
            let size = size as usize;

            if offset >= data.len() {
                reply.data(&[]);
            } else {
                let end = std::cmp::min(offset + size, data.len());
                reply.data(&data[offset..end]);
            }
            return;
        }

        // Real file - read from disk
        let path = match self.inode_manager.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        self.read_real_file(&path, offset, size, reply);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!(ino = ino, offset = offset, "readdir");

        let path = match self.inode_manager.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if !path.is_dir() {
            reply.error(ENOTDIR);
            return;
        }

        let mut entries: Vec<(u64, FileType, OsString)> = Vec::new();

        // Add . and ..
        entries.push((ino, FileType::Directory, OsString::from(".")));

        // Get parent inode
        let parent_inode = if path == self.source_dir {
            ino // Root's parent is itself
        } else if let Some(parent) = path.parent() {
            self.inode_manager.get_inode(parent).unwrap_or(1)
        } else {
            1
        };
        entries.push((parent_inode, FileType::Directory, OsString::from("..")));

        // Read directory contents
        match fs::read_dir(&path) {
            Ok(dir_entries) => {
                for entry in dir_entries.flatten() {
                    let entry_path = entry.path();
                    let entry_name = entry.file_name();

                    if let Ok(metadata) = entry.metadata() {
                        let entry_inode = self.inode_manager.get_or_create_inode(&entry_path);
                        let file_type = file_type_from_metadata(&metadata);
                        entries.push((entry_inode, file_type, entry_name));
                    }
                }
            }
            Err(e) => {
                error!(path = ?path, error = %e, "Failed to read directory");
                reply.error(libc::EIO);
                return;
            }
        }

        // Return entries starting from offset
        for (i, (inode, file_type, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // reply.add returns true if buffer is full
            if reply.add(inode, (i + 1) as i64, file_type, &name) {
                break;
            }
        }

        reply.ok();
    }

    fn flush(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        // Read-only filesystem - nothing to flush
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // Read-only filesystem - nothing to sync
        reply.ok();
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: i32, reply: fuser::ReplyEmpty) {
        // Allow all access for simplicity
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn create_test_handler() -> (DdsHandler, Arc<AtomicUsize>) {
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        let handler: DdsHandler = Arc::new(move |req: DdsRequest| {
            count_clone.fetch_add(1, Ordering::SeqCst);

            // Simulate async processing
            let response = DdsResponse {
                data: vec![0xDD, 0x53, 0x20, 0x00], // DDS magic bytes
                cache_hit: false,
                duration: Duration::from_millis(100),
                job_succeeded: true,
            };
            let _ = req.result_tx.send(response);
        });

        (handler, call_count)
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let tile = AsyncPassthroughFS::chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 10000); // 160000 / 16
        assert_eq!(tile.col, 5250); // 84000 / 16
        assert_eq!(tile.zoom, 16); // 20 - 4
    }

    #[test]
    fn test_chunk_to_tile_coords_low_zoom() {
        let coords = DdsFilename {
            row: 16,
            col: 32,
            zoom: 4,
            map_type: "BI".to_string(),
        };

        let tile = AsyncPassthroughFS::chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 1);
        assert_eq!(tile.col, 2);
        assert_eq!(tile.zoom, 0); // saturating_sub prevents underflow
    }

    #[test]
    fn test_handler_called() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (handler, call_count) = create_test_handler();

        let temp_dir = tempfile::tempdir().unwrap();
        let fs = AsyncPassthroughFS::new(
            temp_dir.path().to_path_buf(),
            handler,
            runtime.handle().clone(),
            1024,
        );

        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let data = fs.request_dds(&coords);

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(data, vec![0xDD, 0x53, 0x20, 0x00]);
    }

    #[test]
    fn test_timeout_returns_placeholder() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        // Handler that never responds
        let handler: DdsHandler = Arc::new(|_req: DdsRequest| {
            // Intentionally don't send response - let it timeout
        });

        let temp_dir = tempfile::tempdir().unwrap();

        let fs = AsyncPassthroughFS::new(
            temp_dir.path().to_path_buf(),
            handler,
            runtime.handle().clone(),
            1024,
        )
        .with_timeout(Duration::from_millis(100));

        let coords = DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
        };

        let data = fs.request_dds(&coords);

        // Should return placeholder (non-empty data)
        assert!(!data.is_empty());
    }

    #[test]
    fn test_channel_closed_returns_placeholder() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        // Handler that drops the sender immediately
        let handler: DdsHandler = Arc::new(|req: DdsRequest| {
            drop(req.result_tx);
        });

        let temp_dir = tempfile::tempdir().unwrap();
        let fs = AsyncPassthroughFS::new(
            temp_dir.path().to_path_buf(),
            handler,
            runtime.handle().clone(),
            1024,
        );

        let coords = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };

        let data = fs.request_dds(&coords);

        // Should return placeholder
        assert!(!data.is_empty());
    }
}
