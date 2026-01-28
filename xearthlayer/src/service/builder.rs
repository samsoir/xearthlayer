//! Service builder for constructing XEarthLayerService with smaller, focused units.
//!
//! This module extracts the complex initialization logic from the facade into
//! discrete, testable builder functions.

use super::config::ServiceConfig;
use super::error::ServiceError;
use crate::cache::{DiskCacheConfig, MemoryCache, MemoryCacheConfig};
use crate::provider::{
    AsyncProviderFactory, AsyncProviderType, AsyncReqwestClient, Provider, ProviderConfig,
    ProviderFactory, ReqwestClient,
};
use crate::texture::DdsTextureEncoder;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Result of provider initialization (sync constructors).
pub struct ProviderComponents {
    /// Sync provider (unused - kept for API compatibility, will be removed)
    #[allow(dead_code)]
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
    /// Shared memory cache for async pipeline (DDS tiles in memory with LRU eviction).
    pub memory_cache: Option<Arc<MemoryCache>>,
    /// Cache directory path for disk cache (chunks stored via ParallelDiskCache).
    pub cache_dir: Option<PathBuf>,
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

/// Result of async-only provider initialization.
///
/// This struct is returned by `create_async_provider()` which only creates
/// the async provider without the legacy sync provider. This is used by
/// `XEarthLayerService::start()` where the sync provider is dead code.
pub struct AsyncProviderComponents {
    /// Async provider for the modern pipeline
    pub async_provider: Arc<AsyncProviderType>,
    /// Provider name for cache directories
    pub name: String,
    /// Maximum zoom level supported
    pub max_zoom: u8,
}

/// Create only the async provider from configuration.
///
/// This version is safe to call from within an async context and doesn't
/// create the sync provider (which would create its own internal Tokio runtime
/// via reqwest::blocking::Client).
///
/// Use this for `XEarthLayerService::start()` where the legacy sync pipeline
/// is not needed.
pub async fn create_async_provider(
    config: &ProviderConfig,
) -> Result<AsyncProviderComponents, ServiceError> {
    // Create async HTTP client
    let async_http_client =
        AsyncReqwestClient::new().map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

    // Create async provider
    let async_factory = AsyncProviderFactory::new(async_http_client);
    let (async_provider, name, max_zoom) = async_factory
        .create(config)
        .await
        .map_err(ServiceError::ProviderError)?;

    Ok(AsyncProviderComponents {
        async_provider: Arc::new(async_provider),
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

/// Create cache components from configuration.
///
/// The async pipeline uses:
/// - `MemoryCache` for DDS tiles (LRU eviction, shared across requests)
/// - `ParallelDiskCache` for chunks (configured via `DdsHandlerBuilder::with_disk_cache`)
pub fn create_cache(
    config: &ServiceConfig,
    _provider_name: &str,
) -> Result<CacheComponents, ServiceError> {
    if !config.cache_enabled() {
        return Ok(CacheComponents {
            memory_cache: None,
            cache_dir: None,
        });
    }

    // Get defaults from config types
    let disk_defaults = DiskCacheConfig::default();
    let memory_defaults = MemoryCacheConfig::default();

    let mut cache_dir = disk_defaults.cache_dir;
    let mut mem_size = memory_defaults.max_size_bytes;

    // Apply user-configured overrides
    if let Some(dir) = config.cache_directory() {
        cache_dir = dir.clone();
    }
    if let Some(size) = config.cache_memory_size() {
        mem_size = size;
    }

    // Create shared memory cache for async pipeline
    let memory_cache = Arc::new(MemoryCache::new(mem_size));

    Ok(CacheComponents {
        memory_cache: Some(memory_cache),
        cache_dir: Some(cache_dir),
    })
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
        let result = create_cache(&config, "test").unwrap();
        assert!(result.memory_cache.is_none());
        assert!(result.cache_dir.is_none());
    }
}
