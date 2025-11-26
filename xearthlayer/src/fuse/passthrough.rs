//! Passthrough FUSE filesystem with on-demand DDS texture generation.
//!
//! This filesystem overlays an existing scenery pack directory and:
//! - Passes through all real files (DSF, .ter, .png, etc.)
//! - Generates DDS textures on-demand when they don't exist on disk

use crate::cache::{Cache, CacheKey};
use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use crate::fuse::{generate_default_placeholder, parse_dds_filename, DdsFilename};
use crate::tile::{TileGenerator, TileRequest};
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
use tracing::{debug, error, info, warn};

/// Time-to-live for attribute caching.
const TTL: Duration = Duration::from_secs(1);

/// Base inode for generated DDS files (virtual files).
const VIRTUAL_INODE_BASE: u64 = 0x1000_0000_0000_0000;

/// Passthrough FUSE filesystem with on-demand DDS generation.
///
/// This filesystem overlays an existing scenery pack:
/// - Real files are passed through directly from the source directory
/// - DDS textures that don't exist are generated on-demand
pub struct PassthroughFS {
    /// Source directory containing the scenery pack
    source_dir: PathBuf,
    /// Tile generator for on-demand DDS creation
    generator: Arc<dyn TileGenerator>,
    /// Cache implementation
    cache: Arc<dyn Cache>,
    /// DDS compression format
    dds_format: DdsFormat,
    /// Inode to path mapping for real files
    inode_to_path: Arc<Mutex<HashMap<u64, PathBuf>>>,
    /// Path to inode mapping for real files
    path_to_inode: Arc<Mutex<HashMap<PathBuf, u64>>>,
    /// Virtual inode to DDS filename mapping
    virtual_inode_to_dds: Arc<Mutex<HashMap<u64, DdsFilename>>>,
    /// Next available inode for real files
    next_inode: Arc<Mutex<u64>>,
}

impl PassthroughFS {
    /// Create a new passthrough filesystem.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to the scenery pack directory
    /// * `generator` - Tile generator for on-demand DDS creation
    /// * `cache` - Cache implementation
    /// * `dds_format` - DDS compression format
    pub fn new(
        source_dir: PathBuf,
        generator: Arc<dyn TileGenerator>,
        cache: Arc<dyn Cache>,
        dds_format: DdsFormat,
    ) -> Self {
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();

        // Reserve inode 1 for root
        inode_to_path.insert(1, source_dir.clone());
        path_to_inode.insert(source_dir.clone(), 1);

        Self {
            source_dir,
            generator,
            cache,
            dds_format,
            inode_to_path: Arc::new(Mutex::new(inode_to_path)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode)),
            virtual_inode_to_dds: Arc::new(Mutex::new(HashMap::new())),
            next_inode: Arc::new(Mutex::new(2)),
        }
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
        let size = self.generator.expected_size() as u64;

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

    /// Generate DDS texture for the given coordinates.
    fn generate_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        let request = TileRequest::from(coords);

        // Check cache first
        let tile_zoom = coords.zoom.saturating_sub(4);
        let tile = TileCoord {
            row: coords.row / 16,
            col: coords.col / 16,
            zoom: tile_zoom,
        };
        let cache_key = CacheKey::new(self.cache.provider(), self.dds_format, tile);

        if let Some(data) = self.cache.get(&cache_key) {
            info!("Cache hit for DDS {:?}", coords);
            return data;
        }

        info!("Generating DDS for coords {:?}", coords);

        match self.generator.generate(&request) {
            Ok(data) => {
                // Cache the result
                if let Err(e) = self.cache.put(cache_key, data.clone()) {
                    warn!("Failed to cache DDS: {}", e);
                }
                data
            }
            Err(e) => {
                error!("Failed to generate DDS: {}", e);
                generate_default_placeholder().unwrap_or_default()
            }
        }
    }
}

impl Filesystem for PassthroughFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent={}, name={:?}", parent, name);

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
                    debug!("Failed to get metadata for {:?}: {}", child_path, e);
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
        debug!("getattr: ino={}", ino);

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
        debug!("read: ino={}, offset={}, size={}", ino, offset, size);

        // Check if it's a virtual DDS file
        if Self::is_virtual_inode(ino) {
            let coords = match self.get_virtual_dds(ino) {
                Some(c) => c,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };

            let data = self.generate_dds(&coords);
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
                error!("Failed to open file {:?}: {}", path, e);
                reply.error(libc::EIO);
                return;
            }
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset as u64)) {
            error!("Failed to seek in {:?}: {}", path, e);
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
                error!("Failed to read from {:?}: {}", path, e);
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
        debug!("readdir: ino={}, offset={}", ino, offset);

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
                error!("Failed to read directory {:?}: {}", path, e);
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
}
