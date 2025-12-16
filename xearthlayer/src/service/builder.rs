//! Service builder for constructing XEarthLayerService with smaller, focused units.
//!
//! This module extracts the complex initialization logic from the facade into
//! discrete, testable builder functions.

use super::config::ServiceConfig;
use super::error::ServiceError;
use super::network_logger::NetworkStatsLogger;
use crate::cache::{Cache, CacheConfig, CacheSystem, MemoryCache, NoOpCache};
use crate::log::Logger;
use crate::log_info;
use crate::orchestrator::{NetworkStats, TileOrchestrator};
use crate::provider::{
    AsyncProviderFactory, AsyncProviderType, AsyncReqwestClient, Provider, ProviderConfig,
    ProviderFactory, ReqwestClient,
};
use crate::telemetry::PipelineMetrics;
use crate::texture::{DdsTextureEncoder, TextureEncoder};
use crate::tile::{DefaultTileGenerator, ParallelConfig, ParallelTileGenerator, TileGenerator};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Result of provider initialization.
pub struct ProviderComponents {
    /// Sync provider for legacy pipeline
    pub sync_provider: Arc<dyn Provider>,
    /// Async provider for async pipeline (optional)
    pub async_provider: Option<Arc<AsyncProviderType>>,
    /// Provider name for cache directories
    pub name: String,
    /// Maximum zoom level supported
    pub max_zoom: u8,
}

/// Result of cache initialization.
pub struct CacheComponents {
    /// Cache implementation (CacheSystem or NoOpCache)
    pub cache: Arc<dyn Cache>,
    /// Shared memory cache for async pipeline
    pub memory_cache: Option<Arc<MemoryCache>>,
    /// Cache directory path for disk cache
    pub cache_dir: Option<PathBuf>,
}

/// Result of generator initialization.
pub struct GeneratorComponents {
    /// Tile generator (parallel wrapper around base generator)
    pub generator: Arc<dyn TileGenerator>,
    /// Network stats tracker
    pub network_stats: Arc<NetworkStats>,
}

/// Create sync and async providers from configuration.
pub fn create_providers(
    config: &ProviderConfig,
    runtime_handle: &Handle,
) -> Result<ProviderComponents, ServiceError> {
    // Create sync HTTP client for legacy pipeline
    let http_client =
        ReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

    // Create sync provider using factory
    let factory = ProviderFactory::new(http_client);
    let (sync_provider, name, max_zoom) = factory.create(config)?;

    // Create async HTTP client for async pipeline
    let async_http_client =
        AsyncReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

    // Create async provider (optional - gracefully handle failures)
    let async_factory = AsyncProviderFactory::new(async_http_client);
    let async_provider = runtime_handle
        .block_on(async_factory.create(config))
        .map(|(provider, _, _)| Arc::new(provider))
        .ok();

    Ok(ProviderComponents {
        sync_provider,
        async_provider,
        name,
        max_zoom,
    })
}

/// Create texture encoder from configuration.
pub fn create_encoder(config: &ServiceConfig) -> Arc<DdsTextureEncoder> {
    Arc::new(
        DdsTextureEncoder::new(config.texture().format())
            .with_mipmap_count(config.texture().mipmap_count()),
    )
}

/// Create tile generator pipeline (orchestrator -> base generator -> parallel wrapper).
pub fn create_generator(
    config: &ServiceConfig,
    provider: Arc<dyn Provider>,
    encoder: Arc<dyn TextureEncoder>,
    logger: Arc<dyn Logger>,
) -> GeneratorComponents {
    // Create network stats tracker
    let network_stats = Arc::new(NetworkStats::new());

    // Create orchestrator with download config and network stats
    let orchestrator = TileOrchestrator::with_config(Arc::clone(&provider), *config.download())
        .with_network_stats(Arc::clone(&network_stats));

    // Create base tile generator
    let base_generator: Arc<dyn TileGenerator> = Arc::new(DefaultTileGenerator::new(
        orchestrator,
        encoder,
        logger.clone(),
    ));

    // Configure parallel wrapper
    let parallel_config = ParallelConfig::default()
        .with_threads(config.generation_threads().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        }))
        .with_timeout_secs(config.generation_timeout().unwrap_or(10))
        .with_dds_format(config.texture().format())
        .with_mipmap_count(config.texture().mipmap_count());

    log_info!(
        logger,
        "Tile generation: {} threads, {}s timeout",
        parallel_config.threads,
        parallel_config.timeout_secs
    );

    // Wrap with parallel generator
    let generator: Arc<dyn TileGenerator> = Arc::new(ParallelTileGenerator::new(
        base_generator,
        parallel_config,
        logger,
    ));

    GeneratorComponents {
        generator,
        network_stats,
    }
}

/// Create cache system from configuration.
pub fn create_cache(
    config: &ServiceConfig,
    provider_name: &str,
    logger: Arc<dyn Logger>,
) -> Result<CacheComponents, ServiceError> {
    if !config.cache_enabled() {
        return Ok(CacheComponents {
            cache: Arc::new(NoOpCache::new(provider_name)),
            memory_cache: None,
            cache_dir: None,
        });
    }

    let mut cache_config = CacheConfig::new(provider_name);

    // Disable stats logging in quiet mode (e.g., TUI active)
    if config.quiet_mode() {
        cache_config = cache_config.with_stats_interval(0);
    }

    // Track cache directory and memory size for async pipeline
    let mut cache_dir = cache_config.disk.cache_dir.clone();
    let mut mem_size = cache_config.memory.max_size_bytes;

    // Apply user-configured overrides
    if let Some(dir) = config.cache_directory() {
        cache_config = cache_config.with_cache_dir(dir.clone());
        cache_dir = dir.clone();
    }
    if let Some(size) = config.cache_memory_size() {
        cache_config = cache_config.with_memory_size(size);
        mem_size = size;
    }
    if let Some(size) = config.cache_disk_size() {
        cache_config = cache_config.with_disk_size(size);
    }

    // Create shared memory cache for async pipeline
    let memory_cache = Arc::new(MemoryCache::new(mem_size));

    // Create the full cache system
    let cache = CacheSystem::new(cache_config, logger)
        .map_err(|e| ServiceError::CacheError(e.to_string()))?;

    Ok(CacheComponents {
        cache: Arc::new(cache),
        memory_cache: Some(memory_cache),
        cache_dir: Some(cache_dir),
    })
}

/// Create network stats logger (if not in quiet mode).
pub fn create_network_logger(
    config: &ServiceConfig,
    network_stats: Arc<NetworkStats>,
    logger: Arc<dyn Logger>,
) -> Option<NetworkStatsLogger> {
    if config.quiet_mode() {
        None
    } else {
        Some(NetworkStatsLogger::start(
            network_stats,
            logger,
            60, // Log every 60 seconds
        ))
    }
}

/// Initialize disk cache size metric from existing cache directory.
///
/// This runs asynchronously to avoid blocking startup for large caches.
pub fn init_disk_cache_metrics(
    cache_dir: Option<&PathBuf>,
    metrics: &Arc<PipelineMetrics>,
    runtime_handle: &Handle,
) {
    let Some(dir) = cache_dir else { return };

    let chunks_dir = dir.join("chunks");
    if !chunks_dir.exists() {
        return;
    }

    let metrics_clone = Arc::clone(metrics);
    let chunks_dir_clone = chunks_dir.clone();

    runtime_handle.spawn(async move {
        match calculate_directory_size(&chunks_dir_clone).await {
            Ok(size) => {
                metrics_clone.set_disk_cache_size(size);
                tracing::info!(
                    size_bytes = size,
                    size_human = %crate::config::format_size(size as usize),
                    "Initialized disk cache size from existing cache"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to calculate disk cache size");
            }
        }
    });
}

/// Calculate the total size of a directory recursively.
async fn calculate_directory_size(path: &std::path::Path) -> std::io::Result<u64> {
    use tokio::fs;

    let mut total_size = 0u64;
    let mut dirs_to_scan = vec![path.to_path_buf()];

    while let Some(dir) = dirs_to_scan.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.is_dir() {
                dirs_to_scan.push(entry.path());
            } else if metadata.is_file() {
                total_size += metadata.len();
            }
        }
    }

    Ok(total_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dds::DdsFormat;

    #[test]
    fn test_create_encoder_with_defaults() {
        let config = ServiceConfig::default();
        let encoder = create_encoder(&config);
        assert_eq!(encoder.format(), DdsFormat::BC1);
    }

    #[test]
    fn test_cache_disabled_returns_noop() {
        let config = ServiceConfig::builder().cache_enabled(false).build();
        let logger = Arc::new(crate::log::NoOpLogger);
        let result = create_cache(&config, "test", logger).unwrap();
        assert!(result.memory_cache.is_none());
        assert!(result.cache_dir.is_none());
    }
}
