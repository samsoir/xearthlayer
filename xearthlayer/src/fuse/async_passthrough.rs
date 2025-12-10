//! Async passthrough FUSE filesystem with pipeline integration.
//!
//! This filesystem overlays an existing scenery pack directory and:
//! - Passes through all real files (DSF, .ter, .png, etc.)
//! - Generates DDS textures on-demand via the async pipeline
//!
//! Unlike `PassthroughFS`, this implementation submits jobs to a Tokio-based
//! async pipeline for tile generation, allowing true concurrent I/O.

use crate::coord::TileCoord;
use crate::fuse::{generate_default_placeholder, parse_dds_filename, DdsFilename};
use crate::pipeline::{JobId, JobResult};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::{ENOENT, ENOTDIR};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

/// Time-to-live for attribute caching.
const TTL: Duration = Duration::from_secs(1);

/// Base inode for generated DDS files (virtual files).
const VIRTUAL_INODE_BASE: u64 = 0x1000_0000_0000_0000;

/// Request for DDS generation sent to the async pipeline.
#[derive(Debug)]
pub struct DdsRequest {
    /// Unique request ID for tracing
    pub job_id: JobId,
    /// Tile coordinates (tile-level, not chunk-level)
    pub tile: TileCoord,
    /// Channel to send result back
    pub result_tx: oneshot::Sender<DdsResponse>,
}

/// Response from the async pipeline.
#[derive(Debug)]
pub struct DdsResponse {
    /// The generated DDS data
    pub data: Vec<u8>,
    /// Whether this was a cache hit
    pub cache_hit: bool,
    /// Generation duration
    pub duration: Duration,
}

impl From<JobResult> for DdsResponse {
    fn from(result: JobResult) -> Self {
        Self {
            data: result.dds_data,
            cache_hit: result.cache_hit,
            duration: result.duration,
        }
    }
}

/// Handler function type for processing DDS requests.
///
/// This is called by the filesystem when a virtual DDS file is read.
/// The handler should process the request asynchronously and send the
/// result back via the oneshot channel in the request.
pub type DdsHandler = Arc<dyn Fn(DdsRequest) + Send + Sync>;

/// Async passthrough FUSE filesystem with pipeline integration.
///
/// This filesystem overlays an existing scenery pack:
/// - Real files are passed through directly from the source directory
/// - DDS textures that don't exist are generated via the async pipeline
///
/// # Architecture
///
/// ```text
/// FUSE Handler Thread          Tokio Runtime
/// ┌─────────────────┐          ┌─────────────────┐
/// │  read() called  │          │                 │
/// │       │         │          │  Pipeline       │
/// │       ▼         │          │  Processor      │
/// │ Create oneshot  │──req───►│       │         │
/// │       │         │          │       ▼         │
/// │   Block on rx   │◄──res───│  DDS Data       │
/// │       │         │          │                 │
/// │       ▼         │          └─────────────────┘
/// │  reply.data()   │
/// └─────────────────┘
/// ```
pub struct AsyncPassthroughFS {
    /// Source directory containing the scenery pack
    source_dir: PathBuf,
    /// Handler for DDS generation requests
    dds_handler: DdsHandler,
    /// Handle to the Tokio runtime for blocking on channels
    runtime_handle: Handle,
    /// Expected DDS file size
    expected_dds_size: usize,
    /// Inode to path mapping for real files
    inode_to_path: Arc<Mutex<HashMap<u64, PathBuf>>>,
    /// Path to inode mapping for real files
    path_to_inode: Arc<Mutex<HashMap<PathBuf, u64>>>,
    /// Virtual inode to DDS filename mapping
    virtual_inode_to_dds: Arc<Mutex<HashMap<u64, DdsFilename>>>,
    /// Next available inode for real files
    next_inode: Arc<Mutex<u64>>,
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
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();

        // Reserve inode 1 for root
        inode_to_path.insert(1, source_dir.clone());
        path_to_inode.insert(source_dir.clone(), 1);

        Self {
            source_dir,
            dds_handler,
            runtime_handle,
            expected_dds_size,
            inode_to_path: Arc::new(Mutex::new(inode_to_path)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode)),
            virtual_inode_to_dds: Arc::new(Mutex::new(HashMap::new())),
            next_inode: Arc::new(Mutex::new(2)),
            generation_timeout: Duration::from_secs(30),
        }
    }

    /// Set the timeout for DDS generation.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Get or create an inode for a real path.
    fn get_or_create_inode(&self, path: &Path) -> u64 {
        let mut path_to_inode = self.path_to_inode.lock().unwrap();

        if let Some(&inode) = path_to_inode.get(path) {
            return inode;
        }

        let mut next_inode = self.next_inode.lock().unwrap();
        let inode = *next_inode;
        *next_inode += 1;

        path_to_inode.insert(path.to_path_buf(), inode);
        drop(path_to_inode);

        let mut inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.insert(inode, path.to_path_buf());

        inode
    }

    /// Get path for an inode.
    fn get_path(&self, inode: u64) -> Option<PathBuf> {
        let inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.get(&inode).cloned()
    }

    /// Create a virtual inode for a DDS file.
    fn create_virtual_inode(&self, coords: DdsFilename) -> u64 {
        // Create a unique virtual inode from coordinates
        let inode = VIRTUAL_INODE_BASE
            + ((coords.row as u64) << 32)
            + ((coords.col as u64) << 8)
            + (coords.zoom as u64);

        let mut virtual_map = self.virtual_inode_to_dds.lock().unwrap();
        virtual_map.insert(inode, coords);

        inode
    }

    /// Check if an inode is virtual (generated DDS).
    fn is_virtual_inode(inode: u64) -> bool {
        inode >= VIRTUAL_INODE_BASE
    }

    /// Get DDS coords for a virtual inode.
    fn get_virtual_dds(&self, inode: u64) -> Option<DdsFilename> {
        let virtual_map = self.virtual_inode_to_dds.lock().unwrap();
        virtual_map.get(&inode).cloned()
    }

    /// Convert filesystem metadata to FUSE FileAttr.
    fn metadata_to_attr(&self, inode: u64, metadata: &fs::Metadata) -> FileAttr {
        let kind = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let atime = metadata.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
        let ctime = UNIX_EPOCH + Duration::from_secs(metadata.ctime() as u64);

        FileAttr {
            ino: inode,
            size: metadata.size(),
            blocks: metadata.blocks(),
            atime,
            mtime,
            ctime,
            crtime: mtime,
            kind,
            perm: (metadata.mode() & 0o7777) as u16,
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: metadata.blksize() as u32,
            flags: 0,
        }
    }

    /// Create FileAttr for a virtual DDS file.
    fn virtual_dds_attr(&self, inode: u64) -> FileAttr {
        let now = SystemTime::now();
        let size = self.expected_dds_size as u64;

        FileAttr {
            ino: inode,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Request DDS generation from the async pipeline.
    ///
    /// This method blocks the calling thread until the DDS is generated
    /// or the timeout is reached.
    fn request_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        let job_id = JobId::new();

        // Convert chunk coordinates to tile coordinates
        let tile_zoom = coords.zoom.saturating_sub(4);
        let tile = TileCoord {
            row: coords.row / 16,
            col: coords.col / 16,
            zoom: tile_zoom,
        };

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

        let request = DdsRequest {
            job_id,
            tile,
            result_tx: tx,
        };

        // Submit request to the handler
        (self.dds_handler)(request);

        // Block waiting for response with timeout
        let result = self
            .runtime_handle
            .block_on(async { tokio::time::timeout(self.generation_timeout, rx).await });

        match result {
            Ok(Ok(response)) => {
                info!(
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
                generate_default_placeholder().unwrap_or_default()
            }
            Err(_) => {
                // Timeout
                warn!(
                    job_id = %job_id,
                    timeout_secs = self.generation_timeout.as_secs(),
                    "DDS generation timed out"
                );
                generate_default_placeholder().unwrap_or_default()
            }
        }
    }
}

impl Filesystem for AsyncPassthroughFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!(parent = parent, name = ?name, "lookup");

        // Get parent path
        let parent_path = match self.get_path(parent) {
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
                    let inode = self.get_or_create_inode(&child_path);
                    let attr = self.metadata_to_attr(inode, &metadata);
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
                let inode = self.create_virtual_inode(coords);
                let attr = self.virtual_dds_attr(inode);
                reply.entry(&TTL, &attr, 0);
                return;
            }
        }

        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        debug!(ino = ino, "getattr");

        // Check if it's a virtual inode
        if Self::is_virtual_inode(ino) {
            if self.get_virtual_dds(ino).is_some() {
                let attr = self.virtual_dds_attr(ino);
                reply.attr(&TTL, &attr);
                return;
            }
            reply.error(ENOENT);
            return;
        }

        // Real file
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match fs::metadata(&path) {
            Ok(metadata) => {
                let attr = self.metadata_to_attr(ino, &metadata);
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
        if Self::is_virtual_inode(ino) {
            let coords = match self.get_virtual_dds(ino) {
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
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let mut file = match File::open(&path) {
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

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!(ino = ino, offset = offset, "readdir");

        let path = match self.get_path(ino) {
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
            let parent_inodes = self.path_to_inode.lock().unwrap();
            *parent_inodes.get(parent).unwrap_or(&1)
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
                        let entry_inode = self.get_or_create_inode(&entry_path);
                        let file_type = if metadata.is_dir() {
                            FileType::Directory
                        } else if metadata.is_symlink() {
                            FileType::Symlink
                        } else {
                            FileType::RegularFile
                        };
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
            };
            let _ = req.result_tx.send(response);
        });

        (handler, call_count)
    }

    #[test]
    fn test_virtual_inode_detection() {
        assert!(!AsyncPassthroughFS::is_virtual_inode(1));
        assert!(!AsyncPassthroughFS::is_virtual_inode(1000));
        assert!(AsyncPassthroughFS::is_virtual_inode(VIRTUAL_INODE_BASE));
        assert!(AsyncPassthroughFS::is_virtual_inode(VIRTUAL_INODE_BASE + 1));
    }

    #[test]
    fn test_dds_response_from_job_result() {
        let result = JobResult {
            job_id: JobId::new(),
            dds_data: vec![1, 2, 3],
            duration: Duration::from_secs(1),
            failed_chunks: 0,
            cache_hit: true,
        };

        let response: DdsResponse = result.into();

        assert_eq!(response.data, vec![1, 2, 3]);
        assert!(response.cache_hit);
        assert_eq!(response.duration, Duration::from_secs(1));
    }

    #[test]
    fn test_handler_called() {
        // Create a new runtime - request_dds uses block_on internally,
        // which simulates FUSE calling from a sync context
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
        // Create a new runtime for this test
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
        .with_timeout(Duration::from_millis(100)); // Very short timeout for test

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
    fn test_create_virtual_inode_unique() {
        let (handler, _) = create_test_handler();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let fs = AsyncPassthroughFS::new(
            temp_dir.path().to_path_buf(),
            handler,
            runtime.handle().clone(),
            1024,
        );

        let coords1 = DdsFilename {
            row: 100,
            col: 200,
            zoom: 16,
            map_type: "BI".to_string(),
        };
        let coords2 = DdsFilename {
            row: 100,
            col: 201,
            zoom: 16,
            map_type: "BI".to_string(),
        };

        let inode1 = fs.create_virtual_inode(coords1);
        let inode2 = fs.create_virtual_inode(coords2);

        assert_ne!(inode1, inode2);
        assert!(AsyncPassthroughFS::is_virtual_inode(inode1));
        assert!(AsyncPassthroughFS::is_virtual_inode(inode2));
    }
}
