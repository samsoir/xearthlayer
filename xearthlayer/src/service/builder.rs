//! Service builder for constructing XEarthLayerService with smaller, focused units.
//!
//! This module extracts the complex initialization logic from the facade into
//! discrete, testable builder functions.

use super::config::ServiceConfig;
use super::error::ServiceError;
use crate::provider::{
    AsyncProviderFactory, AsyncProviderType, AsyncReqwestClient, ProviderConfig,
};
use crate::texture::DdsTextureEncoder;
use std::sync::Arc;

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

/// Result of [`create_encoder`], including optional GPU worker handle.
pub struct EncoderComponents {
    /// The texture encoder.
    pub encoder: Arc<DdsTextureEncoder>,
    /// GPU worker handle (present when `texture.compressor = gpu`).
    /// Must be awaited during shutdown to ensure GPU resources are released.
    pub gpu_worker_handle: Option<tokio::task::JoinHandle<()>>,
    /// Shutdown token for the GPU worker (present when `texture.compressor = gpu`).
    /// Cancelling this token causes the worker to exit its loop regardless of
    /// the channel state. Required because `spawn_blocking` threads are not
    /// cancelled by tokio runtime shutdown.
    pub gpu_shutdown: Option<tokio_util::sync::CancellationToken>,
}

/// Create texture encoder from configuration.
///
/// Selects the block compressor backend based on the `texture.compressor`
/// config setting:
/// - `"ispc"` — SIMD-optimized via Intel ISPC (default)
/// - `"software"` — Pure-Rust fallback
/// - `"gpu"` — GPU compute via wgpu (requires `gpu-encode` feature)
pub fn create_encoder(config: &ServiceConfig) -> Result<EncoderComponents, ServiceError> {
    use crate::dds::{ImageCompressor, IspcCompressor, SoftwareCompressor};

    type GpuHandles = (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio_util::sync::CancellationToken>,
    );

    let (compressor, (gpu_worker_handle, gpu_shutdown)): (Arc<dyn ImageCompressor>, GpuHandles) =
        match config.texture().compressor() {
            "software" => (Arc::new(SoftwareCompressor), (None, None)),
            "ispc" => (Arc::new(IspcCompressor), (None, None)),
            #[cfg(feature = "gpu-encode")]
            "gpu" => {
                use crate::dds::gpu_channel::create_gpu_encoder_channel;

                let shutdown_token = tokio_util::sync::CancellationToken::new();
                let (channel, worker_handle) = create_gpu_encoder_channel(
                    config.texture().gpu_device(),
                    shutdown_token.clone(),
                )
                .map_err(|e| ServiceError::ConfigError(format!("GPU compressor: {e}")))?;

                tracing::info!("GPU pipeline encoder created with dedicated worker");
                (
                    Arc::new(channel) as Arc<dyn ImageCompressor>,
                    (Some(worker_handle), Some(shutdown_token)),
                )
            }
            #[cfg(not(feature = "gpu-encode"))]
            "gpu" => {
                return Err(ServiceError::ConfigError(
                    "GPU compression requires the `gpu-encode` feature. \
                     Rebuild with `cargo build --features gpu-encode` \
                     or set texture.compressor = ispc"
                        .to_string(),
                ));
            }
            other => {
                return Err(ServiceError::ConfigError(format!(
                    "Unknown texture compressor '{}'. Valid options: software, ispc, gpu",
                    other
                )));
            }
        };

    Ok(EncoderComponents {
        encoder: Arc::new(
            DdsTextureEncoder::new(config.texture().format())
                .with_mipmap_count(config.texture().mipmap_count())
                .with_compressor(compressor),
        ),
        gpu_worker_handle,
        gpu_shutdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TextureConfig;
    use crate::dds::DdsFormat;

    #[test]
    fn test_create_encoder_default_is_ispc() {
        let config = ServiceConfig::default();
        let components = create_encoder(&config).unwrap();
        assert_eq!(components.encoder.format(), DdsFormat::BC1);
    }

    #[test]
    fn test_create_encoder_software_compressor() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("software".to_string()))
            .build();
        let components = create_encoder(&config).unwrap();
        assert_eq!(components.encoder.format(), DdsFormat::BC1);
    }

    #[test]
    fn test_create_encoder_unknown_compressor_fails() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("invalid".to_string()))
            .build();
        assert!(create_encoder(&config).is_err());
    }

    #[test]
    #[cfg(not(feature = "gpu-encode"))]
    fn test_create_encoder_gpu_without_feature_fails() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("gpu".to_string()))
            .build();
        assert!(create_encoder(&config).is_err());
    }

    #[tokio::test]
    #[cfg(feature = "gpu-encode")]
    async fn test_create_encoder_gpu_compiles() {
        use crate::texture::TextureEncoder;

        // Verify the GPU code path compiles and handles missing GPU gracefully.
        // On CI without GPU, this returns an error — that's fine.
        // Needs a Tokio runtime because GpuEncoderChannel spawns a worker task.
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("gpu".to_string()))
            .build();
        let result = create_encoder(&config);
        match result {
            Ok(components) => assert_eq!(components.encoder.extension(), "dds"),
            Err(e) => assert!(
                e.to_string().contains("GPU"),
                "error should mention GPU, got: {e}"
            ),
        }
    }

    /// GPU worker handle must complete when the encoder (channel sender) is dropped.
    /// This is the shutdown contract: store the handle, drop the encoder, await the handle.
    /// If the handle doesn't complete, the process hangs on exit.
    #[tokio::test]
    #[cfg(feature = "gpu-encode")]
    #[ignore] // Requires GPU hardware
    async fn test_gpu_worker_handle_completes_on_encoder_drop() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("gpu".to_string()))
            .build();

        let components = match create_encoder(&config) {
            Ok(c) => c,
            Err(_) => return, // No GPU available
        };

        let worker_handle = components
            .gpu_worker_handle
            .expect("GPU compressor should return a worker handle");

        // Drop the encoder — this drops the GpuEncoderChannel (sender),
        // closing the mpsc channel and causing the worker to exit.
        drop(components.encoder);

        // The worker handle must complete within a reasonable timeout.
        // If it doesn't, GPU resources are leaked and the process will hang.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker_handle).await;

        assert!(
            result.is_ok(),
            "GPU worker should complete within 5s after channel close"
        );
        assert!(
            result.unwrap().is_ok(),
            "GPU worker should not panic during shutdown"
        );
    }

    /// Non-GPU encoders should not return a worker handle.
    #[test]
    fn test_ispc_encoder_has_no_worker_handle() {
        let config = ServiceConfig::default(); // defaults to ispc
        let components = create_encoder(&config).unwrap();
        assert!(
            components.gpu_worker_handle.is_none(),
            "ISPC compressor should not have a GPU worker handle"
        );
    }
}
