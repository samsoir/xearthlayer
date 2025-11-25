//! FUSE filesystem implementation for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture file reads
//! and generates satellite imagery DDS files on-demand.

use crate::cache::{Cache, CacheKey};
use crate::coord::to_tile_coords;
use crate::dds::{DdsEncoder, DdsFormat};
use crate::fuse::{generate_default_placeholder, parse_dds_filename, DdsFilename};
use crate::orchestrator::TileOrchestrator;
use crate::provider::Provider;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
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
pub struct XEarthLayerFS<P: Provider> {
    /// Tile orchestrator for downloading imagery
    orchestrator: Arc<TileOrchestrator<P>>,
    /// Cache implementation (can be CacheSystem, NoOpCache, or custom)
    cache: Arc<dyn Cache>,
    /// DDS compression format
    dds_format: DdsFormat,
    /// Number of mipmap levels
    mipmap_count: usize,
    /// Cache mapping inode numbers to DDS filenames for FUSE operations
    inode_cache: Arc<Mutex<HashMap<u64, DdsFilename>>>,
}

impl<P: Provider + 'static> XEarthLayerFS<P> {
    /// Create a new XEarthLayer filesystem.
    ///
    /// # Arguments
    ///
    /// * `orchestrator` - Tile orchestrator for downloading imagery
    /// * `cache` - Cache implementation (CacheSystem, NoOpCache, or custom)
    /// * `dds_format` - DDS compression format (BC1 or BC3)
    /// * `mipmap_count` - Number of mipmap levels (typically 5)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use xearthlayer::fuse::XEarthLayerFS;
    /// use xearthlayer::cache::{CacheSystem, CacheConfig, NoOpCache};
    /// use xearthlayer::orchestrator::TileOrchestrator;
    /// use xearthlayer::provider::BingMapsProvider;
    /// use xearthlayer::dds::DdsFormat;
    /// use std::sync::Arc;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// // With caching
    /// let cache_config = CacheConfig::new("bing");
    /// let cache = CacheSystem::new(cache_config)?;
    /// // let fs = XEarthLayerFS::new(orchestrator, Arc::new(cache), DdsFormat::BC1, 5);
    ///
    /// // Without caching (for testing)
    /// let no_cache = NoOpCache::new("bing");
    /// // let fs = XEarthLayerFS::new(orchestrator, Arc::new(no_cache), DdsFormat::BC1, 5);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(
        orchestrator: TileOrchestrator<P>,
        cache: Arc<dyn Cache>,
        dds_format: DdsFormat,
        mipmap_count: usize,
    ) -> Self {
        Self {
            orchestrator: Arc::new(orchestrator),
            cache,
            dds_format,
            mipmap_count,
            inode_cache: Arc::new(Mutex::new(HashMap::new())),
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

    /// Generate inode number from filename and cache the mapping.
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

            // Store the mapping in the cache
            if let Ok(mut cache) = self.inode_cache.lock() {
                cache.insert(inode, coords);
            }

            Some(inode)
        } else {
            None
        }
    }

    /// Generate DDS file on-demand for the given coordinates.
    fn generate_tile_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        info!(
            "Generating DDS for tile: lat={}, lon={}, zoom={}",
            coords.row, coords.col, coords.zoom
        );

        // Convert DDS filename coords (lat/lon in degrees) to TileCoord
        // The DDS filename format is: +LAT+LON_MAPTYPE_ZOOM.dds
        // For example: +37-123_BI16.dds means lat=37°, lon=-123°, zoom=16
        let lat = coords.row as f64;
        let lon = coords.col as f64;

        let tile = match to_tile_coords(lat, lon, coords.zoom) {
            Ok(t) => {
                info!(
                    "Converted lat={}, lon={}, zoom={} to tile row={}, col={}",
                    lat, lon, coords.zoom, t.row, t.col
                );
                t
            }
            Err(e) => {
                error!("Failed to convert coordinates: {}", e);
                warn!("Returning magenta placeholder for invalid coordinates");
                return match generate_default_placeholder() {
                    Ok(placeholder) => placeholder,
                    Err(placeholder_err) => {
                        error!("Failed to generate placeholder: {}", placeholder_err);
                        Vec::new()
                    }
                };
            }
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
            reply.error(ENOENT);
            return;
        }

        // Look up the filename from the inode cache
        let coords = {
            let cache = match self.inode_cache.lock() {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to lock inode cache: {}", e);
                    reply.error(libc::EIO);
                    return;
                }
            };

            match cache.get(&ino) {
                Some(coords) => coords.clone(),
                None => {
                    error!("Inode {} not found in cache", ino);
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Convert coordinates to tile coordinates
        let tile = match to_tile_coords(coords.row as f64, coords.col as f64, coords.zoom) {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to convert coordinates: {}", e);
                warn!("Returning magenta placeholder for invalid coordinates");
                let placeholder = match generate_default_placeholder() {
                    Ok(p) => p,
                    Err(placeholder_err) => {
                        error!("Failed to generate placeholder: {}", placeholder_err);
                        reply.error(libc::EIO);
                        return;
                    }
                };
                reply.data(&placeholder[offset as usize..]);
                return;
            }
        };

        // Create cache key
        let cache_key = CacheKey::new(self.cache.provider(), self.dds_format, tile);

        // Try to get from cache (memory → disk → none)
        let dds_data = if let Some(data) = self.cache.get(&cache_key) {
            info!("Cache hit for tile {:?}", tile);
            data
        } else {
            // Cache miss - generate tile
            info!("Cache miss for tile {:?}, generating...", tile);
            let data = self.generate_tile_dds(&coords);

            // Cache the generated tile
            if let Err(e) = self.cache.put(cache_key, data.clone()) {
                warn!("Failed to cache tile: {}", e);
                // Continue anyway - we have the data
            }

            data
        };

        // Return the requested portion of the data
        let offset = offset as usize;
        let size = size as usize;

        if offset >= dds_data.len() {
            // Offset is beyond the file
            reply.data(&[]);
            return;
        }

        let end = std::cmp::min(offset + size, dds_data.len());
        let data = &dds_data[offset..end];

        debug!(
            "Returning {} bytes (requested {}, available {})",
            data.len(),
            size,
            dds_data.len()
        );
        reply.data(data);
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
