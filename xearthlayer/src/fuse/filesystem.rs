//! FUSE filesystem implementation for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture file reads
//! and generates satellite imagery DDS files on-demand.

use crate::cache::{Cache, CacheKey};
use crate::coord::TileCoord;
use crate::dds::DdsFormat;
use crate::fuse::{generate_default_placeholder, parse_dds_filename, DdsFilename};
use crate::log::Logger;
use crate::tile::{TileGenerator, TileRequest};
use crate::{log_debug, log_error, log_info, log_warn};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
pub struct XEarthLayerFS {
    /// Tile generator (handles download + encoding)
    generator: Arc<dyn TileGenerator>,
    /// Cache implementation (can be CacheSystem, NoOpCache, or custom)
    cache: Arc<dyn Cache>,
    /// DDS compression format (used for cache key generation)
    dds_format: DdsFormat,
    /// Cache mapping inode numbers to DDS filenames for FUSE operations
    inode_cache: Arc<Mutex<HashMap<u64, DdsFilename>>>,
    /// Logger for diagnostic output
    logger: Arc<dyn Logger>,
}

impl XEarthLayerFS {
    /// Create a new XEarthLayer filesystem.
    ///
    /// # Arguments
    ///
    /// * `generator` - Tile generator (handles downloading and encoding)
    /// * `cache` - Cache implementation (CacheSystem, NoOpCache, or custom)
    /// * `dds_format` - DDS compression format for cache key generation
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::fuse::XEarthLayerFS;
    /// use xearthlayer::cache::{CacheSystem, CacheConfig, NoOpCache};
    /// use xearthlayer::tile::{DefaultTileGenerator, TileGenerator};
    /// use xearthlayer::orchestrator::TileOrchestrator;
    /// use xearthlayer::provider::{BingMapsProvider, ReqwestClient};
    /// use xearthlayer::texture::{TextureEncoder, DdsTextureEncoder};
    /// use xearthlayer::dds::DdsFormat;
    /// use std::sync::Arc;
    ///
    /// // Create tile generator (requires network access)
    /// let http_client = ReqwestClient::new()?;
    /// let provider = BingMapsProvider::new(http_client);
    /// let orchestrator = TileOrchestrator::new(provider, 30, 3, 32);
    /// let encoder: Arc<dyn TextureEncoder> = Arc::new(
    ///     DdsTextureEncoder::new(DdsFormat::BC1).with_mipmap_count(5)
    /// );
    /// let generator: Arc<dyn TileGenerator> = Arc::new(
    ///     DefaultTileGenerator::new(orchestrator, encoder)
    /// );
    ///
    /// // With caching
    /// let cache_config = CacheConfig::new("bing");
    /// let cache = CacheSystem::new(cache_config)?;
    /// let fs = XEarthLayerFS::new(generator.clone(), Arc::new(cache), DdsFormat::BC1);
    ///
    /// // Without caching (for testing)
    /// let no_cache = NoOpCache::new("bing");
    /// let fs = XEarthLayerFS::new(generator, Arc::new(no_cache), DdsFormat::BC1);
    /// ```
    pub fn new(
        generator: Arc<dyn TileGenerator>,
        cache: Arc<dyn Cache>,
        dds_format: DdsFormat,
        logger: Arc<dyn Logger>,
    ) -> Self {
        Self {
            generator,
            cache,
            dds_format,
            inode_cache: Arc::new(Mutex::new(HashMap::new())),
            logger,
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
                // Use generator to get expected size for 4096×4096 tile
                let estimated_size = self.generator.expected_size() as u64;

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
                + ((coords.row as u64) * 1000000)
                + ((coords.col as u64) * 1000)
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

    /// Generate tile texture on-demand for the given coordinates.
    fn generate_tile(&self, coords: &DdsFilename) -> Vec<u8> {
        let request = TileRequest::from(coords);

        match self.generator.generate(&request) {
            Ok(data) => data,
            Err(e) => {
                log_error!(self.logger, "Failed to generate tile: {}", e);
                log_warn!(self.logger, "Returning magenta placeholder");

                // Return placeholder instead of failing
                match generate_default_placeholder() {
                    Ok(placeholder) => placeholder,
                    Err(placeholder_err) => {
                        log_error!(
                            self.logger,
                            "Failed to generate placeholder: {}",
                            placeholder_err
                        );
                        Vec::new()
                    }
                }
            }
        }
    }
}

impl Filesystem for XEarthLayerFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        log_debug!(self.logger, "lookup: parent={}, name={:?}", parent, name);

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
        log_debug!(self.logger, "getattr: ino={}", ino);

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
        log_debug!(
            self.logger,
            "read: ino={}, offset={}, size={}",
            ino,
            offset,
            size
        );

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
                    log_error!(self.logger, "Failed to lock inode cache: {}", e);
                    reply.error(libc::EIO);
                    return;
                }
            };

            match cache.get(&ino) {
                Some(coords) => coords.clone(),
                None => {
                    log_error!(self.logger, "Inode {} not found in cache", ino);
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // DdsFilename already contains Web Mercator tile coordinates from the filename
        // e.g., "100000_125184_BI18.dds" -> row=100000, col=125184, zoom=18
        // These are chunk-level coordinates, so we need to convert to tile-level
        // Tile zoom = chunk zoom - 4 (because each tile = 16x16 chunks = 2^4)
        let tile_zoom = coords.zoom.saturating_sub(4);
        let tile = TileCoord {
            row: coords.row / 16,
            col: coords.col / 16,
            zoom: tile_zoom,
        };

        log_info!(
            self.logger,
            "Processing tile request: filename coords ({}, {}, zoom {}), tile coords ({}, {}, zoom {})",
            coords.row,
            coords.col,
            coords.zoom,
            tile.row,
            tile.col,
            tile_zoom
        );

        // Create cache key
        let cache_key = CacheKey::new(self.cache.provider(), self.dds_format, tile);

        // Try to get from cache (memory → disk → none)
        let dds_data = if let Some(data) = self.cache.get(&cache_key) {
            log_info!(self.logger, "Cache hit for tile {:?}", tile);
            data
        } else {
            // Cache miss - generate tile using injected generator
            log_info!(self.logger, "Cache miss for tile {:?}, generating...", tile);
            let data = self.generate_tile(&coords);

            // Cache the generated tile
            if let Err(e) = self.cache.put(cache_key, data.clone()) {
                log_warn!(self.logger, "Failed to cache tile: {}", e);
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

        log_debug!(
            self.logger,
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
        log_debug!(self.logger, "readdir: ino={}, offset={}", ino, offset);

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
