//! On-disk cache provider with LRU index tracking.
//!
//! This provider stores cache entries as files on disk and uses an in-memory
//! LRU index for efficient garbage collection without filesystem scanning.
//!
//! # Key Changes (v0.4)
//!
//! - **LRU Index**: Uses in-memory index for O(1) cache tracking
//! - **Reversible Filenames**: Keys encoded as `key.replace(':', '_')` instead of hashing
//! - **External GC**: GC is managed externally via [`CacheGcJob`], not an internal daemon
//!
//! # File Layout
//!
//! Files are stored in 1°×1° DSF region subdirectories with reversible names:
//! ```text
//! {cache_dir}/{region}/{key_with_underscores}.cache
//! ```
//!
//! The region is derived from the tile's geographic center (e.g., `+33-119`).
//! This enables parallel scanning on startup via rayon.
//!
//! Example: `tile:15:12754:5279` → `+33-119/tile_15_12754_5279.cache`
//!
//! # Migration Note
//!
//! Caches from versions prior to v0.3 (flat layout or hashed filenames) are
//! not automatically migrated. Run `xearthlayer cache migrate` to move flat
//! files into region subdirectories, or `xearthlayer cache clear` to start fresh.

use bytes::Bytes;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::config::DiskProviderConfig;
use crate::cache::integrity::{
    dds_tile_validator, discard, lit_payload_validator, raw_chunk_validator, CacheEntryValidator,
};
use crate::cache::lru_index::LruIndex;
use crate::cache::traits::{BoxFuture, Cache, GcResult, ServiceCacheError};
use crate::cache::DiskTier;
use crate::metrics::MetricsClient;

/// Choose the tier-appropriate validator.
///
/// This is the one place tier maps to a validator, keeping
/// `DiskCacheProvider` itself ignorant of what it stores — the same struct
/// backs all three tiers: DDS tiles, raw chunks (#253) and, with night
/// lighting on, lit payloads.
fn validator_for_tier(tier: DiskTier) -> Arc<dyn CacheEntryValidator> {
    match tier {
        DiskTier::Dds => Arc::new(dds_tile_validator()),
        DiskTier::Chunk => Arc::new(raw_chunk_validator()),
        DiskTier::Lit => Arc::new(lit_payload_validator()),
    }
}

/// On-disk cache provider with LRU index tracking.
///
/// This provider stores cache entries as files and maintains an in-memory
/// LRU index for efficient garbage collection. GC is handled externally
/// via `CacheGcJob` rather than an internal daemon.
///
/// # Lifecycle
///
/// 1. Create via `start()` or `start_with_index()`
/// 2. Index is populated from disk on startup
/// 3. External GC scheduler submits `CacheGcJob` when needed
/// 4. Call `shutdown()` for graceful cleanup
pub struct DiskCacheProvider {
    /// Cache directory path.
    directory: PathBuf,

    /// Maximum size in bytes.
    max_size_bytes: AtomicU64,

    /// In-memory LRU index for cache tracking.
    lru_index: Arc<LruIndex>,

    /// Cancellation token for graceful shutdown.
    shutdown: CancellationToken,

    /// Optional metrics client for reporting cache size updates.
    metrics_client: Option<MetricsClient>,

    /// Which disk cache tier this provider serves.
    /// Controls which metric event is emitted for size updates.
    tier: DiskTier,

    /// Validates entries read from disk before they are trusted.
    ///
    /// Chosen by `tier` at construction (see `validator_for_tier`). A cache
    /// is never a source of truth: `get()` treats a validator rejection the
    /// same as a missing file — discard the entry, report a miss (#253).
    validator: Arc<dyn CacheEntryValidator>,
}

impl DiskCacheProvider {
    /// Start a new disk cache provider.
    ///
    /// This creates the cache directory if needed and populates the LRU index
    /// from existing cache files on disk.
    ///
    /// # Arguments
    ///
    /// * `config` - Disk cache configuration
    ///
    /// # Returns
    ///
    /// A new `DiskCacheProvider` ready for use.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache directory cannot be created.
    pub async fn start(config: DiskProviderConfig) -> Result<Arc<Self>, ServiceCacheError> {
        // Create cache directory if it doesn't exist
        tokio::fs::create_dir_all(&config.directory)
            .await
            .map_err(ServiceCacheError::Io)?;

        let shutdown = CancellationToken::new();

        // Create LRU index
        let lru_index = Arc::new(LruIndex::new(config.directory.clone()));

        let provider = Arc::new(Self {
            directory: config.directory.clone(),
            max_size_bytes: AtomicU64::new(config.max_size_bytes),
            lru_index,
            shutdown,
            metrics_client: config.metrics_client,
            tier: config.tier,
            validator: validator_for_tier(config.tier),
        });

        // Populate LRU index from disk
        match provider.lru_index.populate_from_disk().await {
            Ok(stats) => {
                info!(
                    dir = %config.directory.display(),
                    tier = %config.tier,
                    files = stats.files_indexed,
                    skipped = stats.skipped_unparseable,
                    size_mb = stats.total_bytes / 1_000_000,
                    max_mb = config.max_size_bytes / 1_000_000,
                    "Disk cache provider started"
                );
            }
            Err(e) => {
                warn!(
                    dir = %config.directory.display(),
                    error = %e,
                    "Failed to populate LRU index from disk, starting empty"
                );
            }
        }

        Ok(provider)
    }

    /// Start a new disk cache provider with an existing LRU index.
    ///
    /// Use this when you want to share the LRU index with other components
    /// (e.g., the GC scheduler).
    ///
    /// # Arguments
    ///
    /// * `config` - Disk cache configuration
    /// * `lru_index` - Pre-existing LRU index
    pub async fn start_with_index(
        config: DiskProviderConfig,
        lru_index: Arc<LruIndex>,
    ) -> Result<Arc<Self>, ServiceCacheError> {
        // Create cache directory if it doesn't exist
        tokio::fs::create_dir_all(&config.directory)
            .await
            .map_err(ServiceCacheError::Io)?;

        let shutdown = CancellationToken::new();

        let provider = Arc::new(Self {
            directory: config.directory.clone(),
            max_size_bytes: AtomicU64::new(config.max_size_bytes),
            lru_index,
            shutdown,
            metrics_client: config.metrics_client,
            tier: config.tier,
            validator: validator_for_tier(config.tier),
        });

        info!(
            dir = %config.directory.display(),
            max_mb = config.max_size_bytes / 1_000_000,
            "Disk cache provider started with shared LRU index"
        );

        Ok(provider)
    }

    /// Returns a reference to the LRU index.
    ///
    /// This can be used by external GC schedulers to query cache state
    /// and create `CacheGcJob` instances.
    pub fn lru_index(&self) -> Arc<LruIndex> {
        Arc::clone(&self.lru_index)
    }

    /// Returns the cache directory path.
    pub fn directory(&self) -> &PathBuf {
        &self.directory
    }

    /// Returns the maximum cache size in bytes.
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    /// Shutdown the provider.
    ///
    /// This signals any pending operations to stop and cleans up resources.
    pub async fn shutdown(&self) {
        info!("Disk cache provider shutting down");
        self.shutdown.cancel();
        info!("Disk cache provider shutdown complete");
    }

    /// Returns the current cache size from the LRU index.
    ///
    /// The LRU index is populated from disk during `start()`, so this
    /// returns an accurate size without re-scanning. This avoids the
    /// double-counting bug that occurred when `populate_from_disk()` was
    /// called twice (once in `start()` and again here).
    ///
    /// Returns the total size in bytes.
    pub async fn scan_initial_size(&self) -> Result<u64, ServiceCacheError> {
        let size = self.lru_index.total_size();
        let count = self.lru_index.entry_count();

        // Seed the absolute disk cache size metric
        self.report_size_to_metrics();

        info!(
            files = count,
            size_mb = size / 1_000_000,
            "Disk cache initial size from LRU index"
        );

        Ok(size)
    }

    /// Check if garbage collection is needed.
    ///
    /// Returns `true` if the cache is over 95% of the maximum size.
    pub fn needs_gc(&self) -> bool {
        let current = self.lru_index.total_size();
        let max = self.max_size_bytes.load(Ordering::Relaxed);
        let threshold = (max as f64 * 0.95) as u64;
        current > threshold
    }

    /// Returns the target size for GC (80% of max).
    pub fn gc_target_size(&self) -> u64 {
        let max = self.max_size_bytes.load(Ordering::Relaxed);
        (max as f64 * 0.80) as u64
    }

    /// Report current cache size to metrics, using the appropriate event
    /// based on whether this is the DDS or chunk tier.
    fn report_size_to_metrics(&self) {
        let size = self.lru_index.total_size();
        if let Some(ref client) = self.metrics_client {
            match self.tier {
                DiskTier::Dds => client.dds_disk_cache_size(size),
                // Not on the dashboard yet.
                DiskTier::Lit => {}
                DiskTier::Chunk => {
                    client.disk_cache_size(size);
                    client.chunk_index_entries(self.lru_index.entry_count());
                }
            }
        }
    }

    /// Get the file path for a cache key.
    ///
    /// Delegates to `LruIndex::key_to_path()` as the single source of truth
    /// for path resolution, ensuring region subdirectories are used.
    fn key_path(&self, key: &str) -> PathBuf {
        self.lru_index.key_to_path(key)
    }
}

/// Numbers each write's temp file, so concurrent writes in this process
/// never share one.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A temp path of its own for one write of `path`, beside it:
/// `{file name}.{pid}.{n}.tmp`. Two writes of one key are possible at once
/// (a lit head read regenerating while a small-level load's write-back is
/// pending, #432); sharing one temp name, the second `create` truncated the
/// first's file, the first rename published it short, and the second
/// rename failed. The name still ends in `.tmp`, which `cache clear`
/// accepts in a region (`layout::holds_only_cache_entries`) and the index
/// scan skips (it reads `*.cache` only), as it did the old name.
fn temp_path_for(path: &std::path::Path) -> PathBuf {
    let n = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Writes `parts`, in order, as the file at `path`: to a temp file of its
/// own beside it ([`temp_path_for`]), renamed over `path` once every part
/// is written, so a reader never sees a torn file at the live path. The
/// temp file is removed on any failure. Creates the parent directory (the
/// region's, on its first write). Not fsynced before the rename, as this
/// tier's writes never were (see `docs/dev/cache-integrity-design.md`).
/// Blocking I/O.
fn write_parts(path: &std::path::Path, parts: &[Bytes]) -> std::io::Result<()> {
    let temp_path = temp_path_for(path);
    let written = write_temp(path, &temp_path, parts);
    match written.and_then(|()| std::fs::rename(&temp_path, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(e)
        }
    }
}

/// The first step of [`write_parts`]: `parts` written to `temp_path`, the
/// parent directory of `path` created first.
fn write_temp(
    path: &std::path::Path,
    temp_path: &std::path::Path,
    parts: &[Bytes],
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(temp_path)?;
    for part in parts {
        file.write_all(part)?;
    }
    Ok(())
}

impl Cache for DiskCacheProvider {
    fn set(&self, key: &str, value: Bytes) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        self.set_parts(key, vec![value])
    }

    /// The parts are written to the file one after another, never joined
    /// into one buffer, in one blocking task.
    fn set_parts(
        &self,
        key: &str,
        parts: Vec<Bytes>,
    ) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        let path = self.key_path(key);
        let size = parts.iter().map(|p| p.len() as u64).sum::<u64>();
        let key_owned = key.to_string();

        Box::pin(async move {
            debug!(
                key = %key_owned,
                size,
                path = %path.display(),
                tier = %self.tier,
                "Disk cache write starting"
            );

            let target = path.clone();
            tokio::task::spawn_blocking(move || write_parts(&target, &parts))
                .await
                .map_err(|e| ServiceCacheError::Io(std::io::Error::other(e)))?
                .map_err(ServiceCacheError::Io)?;

            // Update LRU index after successful write
            self.lru_index.record(&key_owned, size);

            // Report authoritative cache size to metrics
            self.report_size_to_metrics();

            match self.tier {
                DiskTier::Dds => {
                    debug!(key = %key_owned, size, path = %path.display(), "DDS disk cache write complete");
                }
                DiskTier::Chunk => {
                    debug!(key = %key_owned, size, "Cache set");
                }
                DiskTier::Lit => {
                    debug!(key = %key_owned, size, "Lit disk cache write complete");
                }
            }
            Ok(())
        })
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<Bytes>, ServiceCacheError>> {
        let path = self.key_path(key);
        let key_owned = key.to_string();
        let tier = self.tier;

        Box::pin(async move {
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    if let Err(reason) = self.validator.validate(&data) {
                        // A cache is never a source of truth. Delete the bad
                        // entry so the next request regenerates it — leaving
                        // it in place would reject it again on every read,
                        // which is how one corrupt tile became permanently
                        // magenta (#253).
                        discard(&path, &reason, self.validator.name());
                        self.lru_index.remove(&key_owned);
                        // The gauge otherwise self-corrects only on the next
                        // `set()` for this key, which never arrives for a
                        // tile that regenerates incomplete (#180) — leaving
                        // it stale-high until an unrelated write happens to
                        // touch it.
                        self.report_size_to_metrics();
                        return Ok(None);
                    }

                    // Update LRU index access time
                    self.lru_index.touch(&key_owned);
                    debug!(
                        key = %key_owned,
                        size = data.len(),
                        path = %path.display(),
                        tier = %tier,
                        "Disk cache hit"
                    );
                    Ok(Some(Bytes::from(data)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File not on disk - ensure index is clean
                    let was_in_index = self.lru_index.contains(&key_owned);
                    if was_in_index {
                        self.lru_index.remove(&key_owned);
                    }
                    debug!(
                        key = %key_owned,
                        path = %path.display(),
                        tier = %tier,
                        was_in_index,
                        "Disk cache miss — file not found"
                    );
                    Ok(None)
                }
                Err(e) => {
                    // A failing disk degrades to a miss rather than an error
                    // propagated to the caller — the tile regenerates from
                    // the next tier up instead of failing the request (#253).
                    warn!(
                        key = %key_owned,
                        path = %path.display(),
                        tier = %tier,
                        error = %e,
                        "Disk cache read error; treating as a miss"
                    );
                    Ok(None)
                }
            }
        })
    }

    fn delete(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        let path = self.key_path(key);
        let key_owned = key.to_string();

        Box::pin(async move {
            // Remove from index first
            self.lru_index.remove(&key_owned);

            // Then delete file
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    // Report authoritative cache size to metrics
                    self.report_size_to_metrics();
                    debug!(key = %key_owned, "Cache delete");
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(ServiceCacheError::Io(e)),
            }
        })
    }

    fn contains(&self, key: &str) -> BoxFuture<'_, Result<bool, ServiceCacheError>> {
        // The LRU index IS the canonical map for "what's in this cache"
        // — writes update it, deletes remove from it, GC removes from it.
        // XEL is the sole writer to the cache directory, so we trust the
        // index. No filesystem verify here (removed post-#172 — the
        // per-call `tokio::fs::metadata()` was producing thousands of
        // syscalls per filter cycle, which manifested as constant disk
        // activity in the TUI).
        //
        // Stale index entries (file missing but index says yes) are
        // handled reactively in `get()` — if the read fails with
        // NotFound, the entry is removed. Filter-side false positives
        // produce at most a skipped plan entry that X-Plane later
        // requests via FUSE and properly generates.
        let in_index = self.lru_index.contains(key);
        let tier = self.tier;
        debug!(
            key = %key,
            in_index,
            tier = %tier,
            "Disk cache contains (index-only)"
        );
        Box::pin(async move { Ok(in_index) })
    }

    fn contains_sync(&self, key: &str) -> bool {
        // Index-only, exactly like `contains()` — see the comment there for
        // why there is no filesystem verify.
        self.lru_index.contains(key)
    }

    fn size_bytes(&self) -> u64 {
        self.lru_index.total_size()
    }

    fn entry_count(&self) -> u64 {
        self.lru_index.entry_count()
    }

    fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes.load(Ordering::Relaxed)
    }

    fn set_max_size(&self, size_bytes: u64) -> BoxFuture<'_, Result<(), ServiceCacheError>> {
        Box::pin(async move {
            self.max_size_bytes.store(size_bytes, Ordering::Relaxed);
            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<GcResult, ServiceCacheError>> {
        // GC is now handled externally via CacheGcJob.
        // This method returns a no-op result.
        Box::pin(async move {
            Ok(GcResult {
                entries_removed: 0,
                bytes_freed: 0,
                duration_ms: 0,
            })
        })
    }
}

impl Drop for DiskCacheProvider {
    fn drop(&mut self) {
        // Signal shutdown if not already done
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::integrity::EXPECTED_DDS_SIZE;
    use std::time::Duration;
    use tempfile::TempDir;

    /// A DDS-tier provider built synchronously (no fs scan, no shared
    /// runtime dependency) so corruption tests can construct it inline.
    fn dds_provider(temp: &TempDir) -> Arc<DiskCacheProvider> {
        Arc::new(DiskCacheProvider {
            directory: temp.path().to_path_buf(),
            max_size_bytes: AtomicU64::new(1_000_000_000),
            lru_index: Arc::new(LruIndex::new(temp.path().to_path_buf())),
            shutdown: CancellationToken::new(),
            metrics_client: None,
            tier: DiskTier::Dds,
            validator: Arc::new(dds_tile_validator()),
        })
    }

    /// A chunk-tier provider, built the same way as `dds_provider` above.
    fn chunk_provider(temp: &TempDir) -> Arc<DiskCacheProvider> {
        Arc::new(DiskCacheProvider {
            directory: temp.path().to_path_buf(),
            max_size_bytes: AtomicU64::new(1_000_000_000),
            lru_index: Arc::new(LruIndex::new(temp.path().to_path_buf())),
            shutdown: CancellationToken::new(),
            metrics_client: None,
            tier: DiskTier::Chunk,
            validator: Arc::new(raw_chunk_validator()),
        })
    }

    /// A lit-tier provider, built the same way as `dds_provider` above.
    fn lit_provider(temp: &TempDir) -> Arc<DiskCacheProvider> {
        Arc::new(DiskCacheProvider {
            directory: temp.path().to_path_buf(),
            max_size_bytes: AtomicU64::new(1_000_000_000),
            lru_index: Arc::new(LruIndex::new(temp.path().to_path_buf())),
            shutdown: CancellationToken::new(),
            metrics_client: None,
            tier: DiskTier::Lit,
            validator: validator_for_tier(DiskTier::Lit),
        })
    }

    #[tokio::test]
    async fn lit_entry_without_the_magic_is_discarded_and_reported_as_a_miss() {
        let temp = TempDir::new().unwrap();
        let provider = lit_provider(&temp);
        let key = "lit:16:25312:10528";

        let path = provider.key_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a lit entry").unwrap();

        assert!(provider.get(key).await.unwrap().is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_lit_entry_with_the_magic_round_trips() {
        let temp = TempDir::new().unwrap();
        let provider = lit_provider(&temp);
        let key = "lit:16:25312:10528";

        let entry = Bytes::from([b"XELL".as_slice(), &[7u8; 64]].concat());
        provider.set(key, entry.clone()).await.unwrap();

        assert_eq!(provider.get(key).await.unwrap(), Some(entry));
    }

    /// The `*.tmp` files in the directory of `path`.
    fn temp_files_beside(path: &std::path::Path) -> usize {
        std::fs::read_dir(path.parent().unwrap())
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn each_write_of_one_path_has_a_temp_file_of_its_own_beside_it() {
        let path = std::path::Path::new("/cache/night/+37-123/lit_16_25312_10528.cache");
        let (a, b) = (temp_path_for(path), temp_path_for(path));
        assert_ne!(a, b);
        for temp in [&a, &b] {
            assert_eq!(temp.parent(), path.parent());
            let name = temp.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("lit_16_25312_10528.cache."), "{name}");
            assert!(name.ends_with(".tmp"), "{name}");
        }
    }

    #[test]
    fn two_interleaved_writes_of_one_path_leave_one_complete_entry_and_no_temp_file() {
        // The first write's file is written, then the second's, before
        // either is renamed: with one shared temp name the second truncated
        // the first's, the first rename published the second's bytes so
        // far, and the second rename found no file.
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("+37-123").join("lit_16_25312_10528.cache");
        let first = vec![Bytes::from(vec![1u8; 4096]), Bytes::from(vec![2u8; 4096])];
        let second = vec![Bytes::from(vec![3u8; 8192])];
        let (temp_a, temp_b) = (temp_path_for(&path), temp_path_for(&path));
        write_temp(&path, &temp_a, &first).unwrap();
        write_temp(&path, &temp_b, &second).unwrap();
        std::fs::rename(&temp_a, &path).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [&first[0][..], &first[1][..]].concat()
        );
        std::fs::rename(&temp_b, &path).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            &second[0][..],
            "the later write, whole"
        );
        assert_eq!(temp_files_beside(&path), 0);
    }

    #[tokio::test]
    async fn concurrent_writes_of_one_key_leave_one_complete_entry_and_no_temp_file() {
        let temp = TempDir::new().unwrap();
        let provider = lit_provider(&temp);
        let key = "lit:16:25312:10528";
        let entry = |b: u8| Bytes::from([b"XELL".as_slice(), &vec![b; 1 << 16]].concat());
        let writes = (0..8u8).map(|b| {
            let provider = Arc::clone(&provider);
            async move { provider.set(key, entry(b)).await }
        });
        for written in futures::future::join_all(writes).await {
            written.unwrap();
        }
        let stored = provider.get(key).await.unwrap().expect("an entry");
        assert!((0..8u8).any(|b| stored == entry(b)), "one write's, whole");
        assert_eq!(temp_files_beside(&provider.key_path(key)), 0);
    }

    #[test]
    fn a_leftover_temp_file_is_a_cache_entry_to_clear_and_none_to_index() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("+37-123").join("tile_12_1_2.cache");
        let leftover = temp_path_for(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&leftover, b"half").unwrap();
        assert!(crate::cache::layout::holds_only_cache_entries(
            path.parent().unwrap()
        ));
        let name = leftover.file_name().unwrap().to_str().unwrap();
        assert_eq!(LruIndex::filename_to_key(name), None);
    }

    #[tokio::test]
    async fn an_entry_written_in_parts_is_their_concatenation() {
        let temp = TempDir::new().unwrap();
        let provider = lit_provider(&temp);
        let key = "lit:16:25312:10528";
        let (header, payload) = (Bytes::from_static(b"XELL"), Bytes::from(vec![7u8; 64]));
        provider
            .set_parts(key, vec![header.clone(), payload.clone()])
            .await
            .unwrap();
        let joined = Bytes::from([&header[..], &payload[..]].concat());
        assert_eq!(provider.get(key).await.unwrap(), Some(joined.clone()));
        assert_eq!(provider.size_bytes(), joined.len() as u64);
        let path = provider.key_path(key);
        assert_eq!(temp_files_beside(&path), 0, "renamed into place");
    }

    #[tokio::test]
    async fn a_write_that_fails_leaves_no_temp_file_and_no_entry() {
        let temp = TempDir::new().unwrap();
        let provider = lit_provider(&temp);
        let key = "lit:16:25312:10528";
        // A directory where the entry goes: the rename over it fails.
        let path = provider.key_path(key);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        let parts = vec![Bytes::from_static(b"XELL"), Bytes::from(vec![7u8; 64])];
        assert!(provider.set_parts(key, parts).await.is_err());
        assert_eq!(temp_files_beside(&path), 0);
        assert_eq!(provider.entry_count(), 0);
    }

    #[tokio::test]
    async fn corrupt_dds_entry_is_discarded_and_reported_as_a_miss() {
        let temp = TempDir::new().unwrap();
        let provider = dds_provider(&temp);
        let key = "tile:15:12754:5279";

        // Put a file with the right name but wrong content on disk.
        let path = provider.key_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a dds tile").unwrap();

        assert!(provider.get(key).await.unwrap().is_none(), "must be a miss");
        assert!(
            !path.exists(),
            "a rejected entry must be deleted so it regenerates"
        );
    }

    #[tokio::test]
    async fn empty_chunk_entry_is_discarded_and_reported_as_a_miss() {
        let temp = TempDir::new().unwrap();
        let provider = chunk_provider(&temp);
        let key = "chunk:19:204064:84464";

        let path = provider.key_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();

        assert!(provider.get(key).await.unwrap().is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_valid_entry_survives_validation() {
        let temp = TempDir::new().unwrap();
        let provider = dds_provider(&temp);
        let key = "tile:15:12754:5279";

        let mut tile = vec![0u8; EXPECTED_DDS_SIZE];
        tile[0..4].copy_from_slice(b"DDS ");
        provider.set(key, Bytes::from(tile.clone())).await.unwrap();

        assert_eq!(provider.get(key).await.unwrap(), Some(Bytes::from(tile)));
        assert!(provider.key_path(key).exists());
    }

    async fn create_test_provider(max_size: u64) -> (TempDir, Arc<DiskCacheProvider>) {
        let temp_dir = TempDir::new().unwrap();
        let config = DiskProviderConfig {
            directory: temp_dir.path().to_path_buf(),
            max_size_bytes: max_size,
            gc_interval: Duration::from_secs(3600), // Not used anymore
            provider_name: "test".to_string(),
            metrics_client: None,
            tier: DiskTier::Chunk,
        };

        let provider = DiskCacheProvider::start(config).await.unwrap();

        (temp_dir, provider)
    }

    #[tokio::test]
    async fn test_disk_provider_set_and_get() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(Bytes::from(vec![1, 2, 3])));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_get_missing() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        let value = provider.get("nonexistent").await.unwrap();
        assert!(value.is_none());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_delete() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();

        let deleted = provider.delete("key1").await.unwrap();
        assert!(deleted);

        let value = provider.get("key1").await.unwrap();
        assert!(value.is_none());

        // Delete non-existent
        let deleted = provider.delete("key1").await.unwrap();
        assert!(!deleted);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_contains() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        assert!(!provider.contains("key1").await.unwrap());

        provider.set("key1", vec![1].into()).await.unwrap();

        assert!(provider.contains("key1").await.unwrap());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_contains_sync_agrees_with_contains() {
        let (_dir, provider) = create_test_provider(10 * 1024 * 1024).await;
        let key = "tile:15:12754:5279";

        assert!(!provider.contains_sync(key), "absent key must be false");
        assert_eq!(
            provider.contains(key).await.unwrap(),
            provider.contains_sync(key)
        );

        provider.set(key, vec![0u8; 64].into()).await.unwrap();

        assert!(provider.contains_sync(key), "present key must be true");
        assert_eq!(
            provider.contains(key).await.unwrap(),
            provider.contains_sync(key)
        );

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_replace_existing() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();
        provider.set("key1", vec![4, 5, 6, 7].into()).await.unwrap();

        let value = provider.get("key1").await.unwrap();
        assert_eq!(value, Some(Bytes::from(vec![4, 5, 6, 7])));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_size_tracking() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        assert_eq!(provider.size_bytes(), 0);
        assert_eq!(provider.entry_count(), 0);

        provider.set("key1", vec![0u8; 1000].into()).await.unwrap();

        assert_eq!(provider.size_bytes(), 1000);
        assert_eq!(provider.entry_count(), 1);

        provider.set("key2", vec![0u8; 2000].into()).await.unwrap();

        assert_eq!(provider.size_bytes(), 3000);
        assert_eq!(provider.entry_count(), 2);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_needs_gc() {
        let (_temp_dir, provider) = create_test_provider(1000).await;

        assert!(!provider.needs_gc());

        // Add data to exceed 95% threshold (950 bytes)
        provider.set("key1", vec![0u8; 960].into()).await.unwrap();

        assert!(provider.needs_gc());

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_lru_index_access() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();

        let lru_index = provider.lru_index();
        assert!(lru_index.contains("key1"));

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_atomic_write() {
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        // Write should be atomic (temp file + rename)
        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();

        // No temp files should remain (check in the region directory)
        let cache_path = provider.lru_index().key_to_path("key1");
        let parent = cache_path.parent().unwrap();
        let files: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();

        assert!(files.is_empty(), "Temp files should not remain");

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_stale_index_cleanup_on_get() {
        // Post-#172: `contains()` is index-only for performance (no per-call
        // filesystem verify). Stale index entries (file missing but index
        // still has it) are instead cleaned up reactively on `get()`: if
        // the read fails with NotFound, the entry is removed. This moves
        // the cleanup cost from "every contains() call, always" to "only
        // when a get() actually discovers the missing file."
        let (_temp_dir, provider) = create_test_provider(1_000_000).await;

        provider.set("key1", vec![1, 2, 3].into()).await.unwrap();
        assert!(provider.lru_index().contains("key1"));

        // External mutation: delete the file behind the cache's back.
        let path = provider.lru_index().key_to_path("key1");
        std::fs::remove_file(path).unwrap();

        // `contains()` still reports true — it trusts the canonical
        // index and doesn't do a per-call fs verify.
        assert!(
            provider.contains("key1").await.unwrap(),
            "contains() trusts the index; no fs verify"
        );

        // `get()` discovers the file is missing and cleans up reactively.
        let result = provider.get("key1").await.unwrap();
        assert!(result.is_none(), "get() returns None for missing file");
        assert!(
            !provider.lru_index().contains("key1"),
            "Stale index entry must be removed after failed get()"
        );

        // A subsequent contains() now correctly returns false.
        assert!(
            !provider.contains("key1").await.unwrap(),
            "contains() returns false after reactive cleanup"
        );

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_scan_initial_size_does_not_double_count() {
        let temp_dir = TempDir::new().unwrap();

        // Pre-populate disk with cache files in region subdirectories
        let path1 = crate::cache::key_to_full_path(temp_dir.path(), "chunk:15:100:200:8:12");
        let path2 = crate::cache::key_to_full_path(temp_dir.path(), "chunk:15:100:201:0:0");
        std::fs::create_dir_all(path1.parent().unwrap()).unwrap();
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path1, vec![0u8; 1000]).unwrap();
        std::fs::write(&path2, vec![0u8; 2000]).unwrap();

        let config = DiskProviderConfig {
            directory: temp_dir.path().to_path_buf(),
            max_size_bytes: 1_000_000,
            gc_interval: Duration::from_secs(3600),
            provider_name: "test".to_string(),
            metrics_client: None,
            tier: DiskTier::Chunk,
        };

        // start() internally calls populate_from_disk()
        let provider = DiskCacheProvider::start(config).await.unwrap();

        // Verify initial state is correct
        assert_eq!(provider.size_bytes(), 3000);
        assert_eq!(provider.entry_count(), 2);

        // scan_initial_size() must return the same value, NOT double it
        let scanned_size = provider.scan_initial_size().await.unwrap();
        assert_eq!(
            scanned_size, 3000,
            "scan_initial_size() must not double-count after start()"
        );

        // And the internal state must remain consistent
        assert_eq!(
            provider.size_bytes(),
            3000,
            "size_bytes() must remain consistent after scan_initial_size()"
        );
        assert_eq!(provider.entry_count(), 2);

        provider.shutdown().await;
    }

    #[tokio::test]
    async fn test_disk_provider_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let key = "tile:15:100:200";

        // Create provider, add data, shutdown
        {
            let config = DiskProviderConfig {
                directory: temp_dir.path().to_path_buf(),
                max_size_bytes: 1_000_000,
                gc_interval: Duration::from_secs(3600),
                provider_name: "test".to_string(),
                metrics_client: None,
                tier: DiskTier::Chunk,
            };
            let provider = DiskCacheProvider::start(config).await.unwrap();

            provider.set(key, vec![1, 2, 3, 4, 5].into()).await.unwrap();
            provider.shutdown().await;
        }

        // Create new provider, verify data persists
        {
            let config = DiskProviderConfig {
                directory: temp_dir.path().to_path_buf(),
                max_size_bytes: 1_000_000,
                gc_interval: Duration::from_secs(3600),
                provider_name: "test".to_string(),
                metrics_client: None,
                tier: DiskTier::Chunk,
            };
            let provider = DiskCacheProvider::start(config).await.unwrap();

            // Index should be populated from disk
            assert!(provider.lru_index().contains(key));
            assert_eq!(provider.entry_count(), 1);

            let value = provider.get(key).await.unwrap();
            assert_eq!(value, Some(Bytes::from(vec![1, 2, 3, 4, 5])));

            provider.shutdown().await;
        }
    }
}
