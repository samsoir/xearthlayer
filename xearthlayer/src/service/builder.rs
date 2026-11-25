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
    network_level: crate::config::NetworkLevel,
) -> Result<AsyncProviderComponents, ServiceError> {
    // Create async HTTP client, its idle pool sized by network.connections (#367)
    let async_http_client = AsyncReqwestClient::at_network_level(network_level)
        .map_err(|e| ServiceError::HttpClientError(e.to_string()))?;

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
    /// GPU worker handle (present when `texture.compressor` is `gpu` or
    /// `hybrid`).
    /// Must be awaited during shutdown to ensure GPU resources are released.
    pub gpu_worker_handle: Option<tokio::task::JoinHandle<()>>,
    /// The GPU the encoder selected (present when `texture.compressor` is
    /// `gpu` or `hybrid`).
    pub gpu_adapter: Option<crate::system::GpuAdapter>,
    /// The encoder policy's warning when `gpu` or `hybrid` was honoured on a host whose
    /// only GPU is the one X-Plane renders with (#203). Logged at creation
    /// and shown on the dashboard's Status pane.
    pub gpu_warning: Option<String>,
    /// Shutdown token for the GPU worker (present when `texture.compressor`
    /// is `gpu` or `hybrid`).
    /// Cancelling this token causes the worker to exit its loop regardless of
    /// the channel state. Required because `spawn_blocking` threads are not
    /// cancelled by tokio runtime shutdown.
    pub gpu_shutdown: Option<tokio_util::sync::CancellationToken>,
}

/// Apply an explicit mipmap level override, if the config carries one.
///
/// `None` leaves the encoder on its default: the full chain derived from the
/// texture dimensions at encode time.
fn apply_mipmap_count(encoder: DdsTextureEncoder, count: Option<usize>) -> DdsTextureEncoder {
    match count {
        Some(count) => encoder.with_mipmap_count(count),
        None => encoder,
    }
}

/// Create texture encoder from configuration.
///
/// The backend is `texture.compressor`, parsed as a [`CompressorKind`]:
/// - `"ispc"` — SIMD-optimized via Intel ISPC (default)
/// - `"software"` — Pure-Rust fallback
/// - `"gpu"` — GPU compute via wgpu
/// - `"hybrid"` — the GPU worker, and ISPC on the calling thread for a tile
///   that finds the worker's one-tile queue taken
///
/// The GPU path runs the choice past the [`EncoderPolicy`] with the
/// adapters its own enumeration produced, so a host whose only GPU is the
/// one X-Plane renders with gets its contention warning without a second
/// enumeration (#203).
pub fn create_encoder(config: &ServiceConfig) -> Result<EncoderComponents, ServiceError> {
    use crate::dds::{IspcCompressor, SoftwareCompressor};
    use crate::system::{CompressorKind, DefaultEncoderPolicy, EncoderPolicy};

    // Before anything else, the GPU device in particular: an unsupported
    // format fails at startup rather than once per tile (#395).
    config
        .texture()
        .format()
        .ensure_supported()
        .map_err(|e| ServiceError::ConfigError(format!("Texture format: {e}")))?;

    let backend = CompressorKind::parse(config.texture().compressor()).ok_or_else(|| {
        ServiceError::ConfigError(format!(
            "Unknown texture compressor '{}'. Valid options: {}",
            config.texture().compressor(),
            CompressorKind::VALUES.join(", ")
        ))
    })?;

    let cpu_components = |compressor: Arc<dyn crate::dds::ImageCompressor>| EncoderComponents {
        encoder: Arc::new(
            apply_mipmap_count(
                DdsTextureEncoder::new(config.texture().format()),
                config.texture().mipmap_count(),
            )
            .with_compressor(compressor),
        ),
        gpu_worker_handle: None,
        gpu_adapter: None,
        gpu_warning: None,
        gpu_shutdown: None,
    };

    match backend {
        CompressorKind::Gpu | CompressorKind::Hybrid => {
            use crate::dds::gpu_channel::{create_gpu_encoder_channel, WhenBusy};

            let shutdown_token = tokio_util::sync::CancellationToken::new();
            let when_busy = match backend {
                CompressorKind::Hybrid => WhenBusy::EncodeWithIspc,
                _ => WhenBusy::Wait,
            };
            let (channel, worker_handle, selection) = create_gpu_encoder_channel(
                config.texture().gpu_device(),
                when_busy,
                shutdown_token.clone(),
            )
            .map_err(|e| ServiceError::ConfigError(format!("GPU compressor: {e}")))?;

            let decision = DefaultEncoderPolicy.select(&selection.host, Some(backend));
            if let Some(warning) = &decision.warning {
                tracing::warn!(adapter = %selection.adapter, "{warning}");
            }

            // The per-minute `GPU encode sample` line, ended with the worker
            // by the shutdown token or when the channel is dropped (#419).
            crate::dds::gpu_stats::start_sample_log(
                Arc::downgrade(channel.stats()),
                shutdown_token.clone(),
            );

            tracing::info!(compressor = %backend, "GPU pipeline encoder created with dedicated worker");

            Ok(EncoderComponents {
                encoder: Arc::new(
                    apply_mipmap_count(
                        DdsTextureEncoder::new(config.texture().format()),
                        config.texture().mipmap_count(),
                    )
                    .with_mipmap_compressor(Arc::new(channel)),
                ),
                gpu_worker_handle: Some(worker_handle),
                gpu_adapter: Some(selection.adapter),
                gpu_warning: decision.warning,
                gpu_shutdown: Some(shutdown_token),
            })
        }
        CompressorKind::Software => Ok(cpu_components(Arc::new(SoftwareCompressor))),
        CompressorKind::Ispc => Ok(cpu_components(Arc::new(IspcCompressor))),
    }
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

    /// An unsupported format fails when the encoder is built, for every
    /// backend, and before the GPU path enumerates or opens a device: the
    /// error names the format whether or not this host has a GPU.
    #[tokio::test]
    async fn test_create_encoder_rejects_unsupported_format() {
        for compressor in ["software", "ispc", "gpu", "hybrid"] {
            let config = ServiceConfig::builder()
                .texture(TextureConfig::new(DdsFormat::BC3).with_compressor(compressor.to_string()))
                .build();
            match create_encoder(&config) {
                Err(ServiceError::ConfigError(msg)) => assert!(
                    msg.contains("BC3") && msg.contains("BC1"),
                    "{compressor}: the error names the format: {msg}"
                ),
                Err(other) => panic!("{compressor}: expected a config error, got {other:?}"),
                Ok(_) => panic!("{compressor}: an unsupported format must be refused"),
            }
        }
    }

    #[test]
    fn test_create_encoder_unknown_compressor_fails() {
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("invalid".to_string()))
            .build();
        assert!(create_encoder(&config).is_err());
    }

    #[tokio::test]
    async fn test_create_encoder_gpu_compiles() {
        for compressor in ["gpu", "hybrid"] {
            create_gpu_encoder_or_report_why_not(compressor);
        }
    }

    /// Verify the GPU code path compiles and handles a missing GPU
    /// gracefully: on CI without a GPU this is an error, which is fine.
    /// Needs a Tokio runtime because GpuEncoderChannel spawns a worker task.
    fn create_gpu_encoder_or_report_why_not(compressor: &str) {
        use crate::texture::TextureEncoder;

        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor(compressor.to_string()))
            .build();
        let result = create_encoder(&config);
        match result {
            Ok(components) => {
                assert_eq!(components.encoder.extension(), "dds");
                // The dashboard names the adapter the worker actually chose.
                let adapter = components
                    .gpu_adapter
                    .expect("a GPU encoder reports its adapter");
                assert!(!adapter.name.is_empty());
                // The policy's verdict must match the host: a lone GPU is the
                // one X-Plane renders with and warns, two or more do not (#203).
                let host = crate::system::HardwareProfile::new(crate::system::enumerate_gpus());
                assert_eq!(
                    components.gpu_warning.is_some(),
                    host.shares_render_gpu(),
                    "warning {:?} on host {host:?}",
                    components.gpu_warning
                );
            }
            Err(e) => assert!(
                e.to_string().contains("GPU"),
                "{compressor}: error should mention GPU, got: {e}"
            ),
        }
    }

    /// GPU worker handle must complete when the encoder (channel sender) is dropped.
    /// This is the shutdown contract: store the handle, drop the encoder, await the handle.
    /// If the handle doesn't complete, the process hangs on exit.
    #[tokio::test]
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
        assert!(components.gpu_adapter.is_none());
        assert!(
            components.gpu_warning.is_none(),
            "a CPU encoder never contends with the sim's GPU"
        );
    }
}
