//! FUSE filesystem implementation for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture file reads
//! and generates satellite imagery DDS files on-demand.

use crate::coord::TileCoord;
use crate::dds::{DdsEncoder, DdsFormat};
use crate::fuse::{generate_default_placeholder, parse_dds_filename};
use crate::orchestrator::TileOrchestrator;
use crate::provider::Provider;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::{ENOENT, ENOSYS};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info, warn};

/// Inode numbers for filesystem entries.
const ROOT_INODE: u64 = 1;
const TEXTURES_DIR_INODE: u64 = 2;
const TILE_FILE_INODE_BASE: u64 = 1000;

/// Time-to-live for attribute caching.
const TTL: Duration = Duration::from_secs(1);

/// XEarthLayer FUSE filesystem.
///
/// Provides a virtual filesystem with structure:
/// ```text
/// /
/// └── textures/
///     ├── +37-123_BI16.dds (generated on-demand)
///     ├── +37-122_BI16.dds (generated on-demand)
///     └── ...
/// ```
#[allow(dead_code)] // Will be used when implementing full FUSE server
pub struct XEarthLayerFS<P: Provider> {
    /// Tile orchestrator for downloading imagery
    orchestrator: Arc<TileOrchestrator<P>>,
    /// DDS compression format
    dds_format: DdsFormat,
    /// Number of mipmap levels
    mipmap_count: usize,
}

impl<P: Provider + 'static> XEarthLayerFS<P> {
    /// Create a new XEarthLayer filesystem.
    ///
    /// # Arguments
    ///
    /// * `orchestrator` - Tile orchestrator for downloading imagery
    /// * `dds_format` - DDS compression format (BC1 or BC3)
    /// * `mipmap_count` - Number of mipmap levels (typically 5)
    pub fn new(
        orchestrator: TileOrchestrator<P>,
        dds_format: DdsFormat,
        mipmap_count: usize,
    ) -> Self {
        Self {
            orchestrator: Arc::new(orchestrator),
            dds_format,
            mipmap_count,
        }
    }

    /// Get file attributes for a path.
    fn get_attrs(&self, ino: u64) -> Option<FileAttr> {
        let now = SystemTime::now();

        match ino {
            ROOT_INODE => Some(FileAttr {
                ino: ROOT_INODE,
                size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 1000,
                gid: 1000,
                rdev: 0,
                blksize: 512,
                flags: 0,
            }),
            TEXTURES_DIR_INODE => Some(FileAttr {
                ino: TEXTURES_DIR_INODE,
                size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 1000,
                gid: 1000,
                rdev: 0,
                blksize: 512,
                flags: 0,
            }),
            _ if ino >= TILE_FILE_INODE_BASE => {
                // This is a tile file - report as a regular file
                // Size is approximate (actual size varies by content)
                let estimated_size = match self.dds_format {
                    DdsFormat::BC1 => 11_174_016, // 4096×4096 BC1 with 5 mipmaps
                    DdsFormat::BC3 => 22_347_904, // 4096×4096 BC3 with 5 mipmaps
                };

                Some(FileAttr {
                    ino,
                    size: estimated_size,
                    blocks: estimated_size.div_ceil(512),
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
                })
            }
            _ => None,
        }
    }

    /// Generate inode number from filename.
    fn filename_to_inode(&self, name: &OsStr) -> Option<u64> {
        let name_str = name.to_str()?;

        // Try to parse as DDS filename
        if let Ok(coords) = parse_dds_filename(name_str) {
            // Create a unique inode from coordinates
            // Use a hash-like function to map coords to inode space
            let inode = TILE_FILE_INODE_BASE
                + ((coords.row.unsigned_abs() as u64) * 1000000)
                + ((coords.col.unsigned_abs() as u64) * 1000)
                + (coords.zoom as u64);
            Some(inode)
        } else {
            None
        }
    }

    /// Generate DDS file on-demand for the given coordinates.
    #[allow(dead_code)] // Will be used when read() is fully implemented
    fn generate_tile_dds(&self, coords: &crate::fuse::DdsFilename) -> Vec<u8> {
        info!(
            "Generating DDS for tile: row={}, col={}, zoom={}",
            coords.row, coords.col, coords.zoom
        );

        // Convert DDS filename coords to TileCoord
        // Note: DDS filename uses row/col, we need to use our TileCoord system
        let tile = TileCoord {
            row: coords.row as u32,
            col: coords.col as u32,
            zoom: coords.zoom,
        };

        // Download tile imagery
        let image = match self.orchestrator.download_tile(&tile) {
            Ok(img) => {
                info!(
                    "Downloaded tile successfully: {}×{} pixels",
                    img.width(),
                    img.height()
                );
                img
            }
            Err(e) => {
                error!("Failed to download tile: {}", e);
                warn!("Returning magenta placeholder for failed tile");

                // Return placeholder instead of failing
                return match generate_default_placeholder() {
                    Ok(placeholder) => placeholder,
                    Err(placeholder_err) => {
                        error!("Failed to generate placeholder: {}", placeholder_err);
                        // Return empty vec as last resort
                        Vec::new()
                    }
                };
            }
        };

        // Encode to DDS
        let encoder = DdsEncoder::new(self.dds_format).with_mipmap_count(self.mipmap_count);

        match encoder.encode(&image) {
            Ok(dds_data) => {
                info!("DDS encoding completed: {} bytes", dds_data.len());
                dds_data
            }
            Err(e) => {
                error!("Failed to encode DDS: {}", e);
                warn!("Returning magenta placeholder for failed encoding");

                // Return placeholder instead of failing
                match generate_default_placeholder() {
                    Ok(placeholder) => placeholder,
                    Err(placeholder_err) => {
                        error!("Failed to generate placeholder: {}", placeholder_err);
                        Vec::new()
                    }
                }
            }
        }
    }
}

impl<P: Provider + 'static> Filesystem for XEarthLayerFS<P> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent={}, name={:?}", parent, name);

        match parent {
            ROOT_INODE => {
                // Looking up in root directory
                if name == "textures" {
                    if let Some(attr) = self.get_attrs(TEXTURES_DIR_INODE) {
                        reply.entry(&TTL, &attr, 0);
                        return;
                    }
                }
            }
            TEXTURES_DIR_INODE => {
                // Looking up a file in textures directory
                if let Some(inode) = self.filename_to_inode(name) {
                    if let Some(attr) = self.get_attrs(inode) {
                        reply.entry(&TTL, &attr, 0);
                        return;
                    }
                }
            }
            _ => {}
        }

        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);

        if let Some(attr) = self.get_attrs(ino) {
            reply.attr(&TTL, &attr);
        } else {
            reply.error(ENOENT);
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

        // Only handle tile files
        if ino < TILE_FILE_INODE_BASE {
            reply.error(ENOSYS);
            return;
        }

        // We need to get the filename from the inode
        // For now, this is a limitation - we can't easily reverse the inode mapping
        // In a real implementation, we'd cache the inode->filename mapping
        // For Phase 1, we'll generate a test tile response

        error!("read: inode-to-filename reverse mapping not yet implemented");
        reply.error(ENOSYS);
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

        match ino {
            ROOT_INODE => {
                if offset == 0 {
                    let _ = reply.add(ROOT_INODE, 0, FileType::Directory, ".");
                    let _ = reply.add(ROOT_INODE, 1, FileType::Directory, "..");
                    let _ = reply.add(TEXTURES_DIR_INODE, 2, FileType::Directory, "textures");
                }
                reply.ok();
            }
            TEXTURES_DIR_INODE => {
                if offset == 0 {
                    let _ = reply.add(TEXTURES_DIR_INODE, 0, FileType::Directory, ".");
                    let _ = reply.add(ROOT_INODE, 1, FileType::Directory, "..");

                    // For Phase 1, we don't list any files
                    // Files are generated on-demand when requested
                }
                reply.ok();
            }
            _ => {
                reply.error(ENOENT);
            }
        }
    }
}
