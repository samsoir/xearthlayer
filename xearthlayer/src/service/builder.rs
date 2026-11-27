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
    /// The demand lane's texture encoder (and the service's, for sizes).
    pub encoder: Arc<DdsTextureEncoder>,
    /// The prefetch lane's texture encoder (#421). With a CPU backend it is
    /// [`encoder`](Self::encoder) itself; with the GPU it sends on the
    /// prefetch channel, which the worker reads only while the demand
    /// channel is empty.
    pub prefetch_encoder: Arc<DdsTextureEncoder>,
    /// The GPU worker (present when `texture.compressor = gpu`): its
    /// thread and shutdown token. Stop it at shutdown
    /// ([`GpuWorkerHandle::stop`](crate::dds::gpu_channel::GpuWorkerHandle::stop))
    /// so the GPU's resources are released: the channels stay open while
    /// anything holds an encoder, so the worker does not end by itself.
    pub gpu_worker_handle: Option<crate::dds::gpu_channel::GpuWorkerHandle>,
    /// The GPU the encoder selected (present when `texture.compressor = gpu`).
    pub gpu_adapter: Option<crate::system::GpuAdapter>,
    /// The GPU encoder's health (present when `texture.compressor = gpu`):
    /// Failed once the GPU worker has stopped and tiles are encoded on the
    /// CPU (#420). The dashboard reads it every tick.
    pub gpu_health: Option<crate::dds::gpu_health::SharedGpuHealth>,
    /// The encoder policy's warning when `gpu` was honoured on a host whose
    /// only GPU is the one X-Plane renders with (#203). Logged at creation
    /// and shown on the dashboard's Status pane.
    pub gpu_warning: Option<String>,
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

/// The prefetch lane's GPU channel capacity (#421): its CPU pool, as no
/// more of its encode tasks can run at once.
fn prefetch_gpu_capacity(config: &ServiceConfig) -> usize {
    crate::executor::LaneSizing::from_configured_share(config.prefetch_capacity_share())
        .prefetch
        .cpu
}

/// The demand and prefetch lanes' encoders on the two GPU channels (#421),
/// each with ISPC as the fallback for the images the GPU hands back. Named,
/// not left to the encoder's default: the WARN and the Status pane say a
/// failed GPU's tiles are encoded on ISPC (#420).
pub(crate) fn gpu_lane_encoders(
    channels: crate::dds::gpu_channel::GpuEncoderChannels,
    format: crate::dds::DdsFormat,
    mipmap_count: Option<usize>,
) -> (Arc<DdsTextureEncoder>, Arc<DdsTextureEncoder>) {
    let on = |channel: crate::dds::gpu_channel::GpuEncoderChannel| {
        Arc::new(
            apply_mipmap_count(DdsTextureEncoder::new(format), mipmap_count)
                .with_compressor(Arc::new(crate::dds::IspcCompressor))
                .with_mipmap_compressor(Arc::new(channel)),
        )
    };
    (on(channels.demand), on(channels.prefetch))
}

/// Create texture encoder from configuration.
///
/// The backend is `texture.compressor`, parsed as a [`CompressorKind`]:
/// - `"ispc"` — SIMD-optimized via Intel ISPC (default)
/// - `"software"` — Pure-Rust fallback
/// - `"gpu"` — GPU compute via wgpu
///
/// The GPU path runs the choice past the [`EncoderPolicy`] with the
/// adapters its own enumeration produced, so a host whose only GPU is the
/// one X-Plane renders with gets its contention warning without a second
/// enumeration (#203).
pub fn create_encoder(config: &ServiceConfig) -> Result<EncoderComponents, ServiceError> {
    use crate::dds::gpu_channel::GpuLane;
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
            "Unknown texture compressor '{}'. Valid options: software, ispc, gpu",
            config.texture().compressor()
        ))
    })?;

    let cpu_components = |compressor: Arc<dyn crate::dds::ImageCompressor>| {
        let encoder = Arc::new(
            apply_mipmap_count(
                DdsTextureEncoder::new(config.texture().format()),
                config.texture().mipmap_count(),
            )
            .with_compressor(compressor),
        );
        EncoderComponents {
            // Both lanes encode on their own CPU pools: one encoder.
            prefetch_encoder: Arc::clone(&encoder),
            encoder,
            gpu_worker_handle: None,
            gpu_adapter: None,
            gpu_health: None,
            gpu_warning: None,
        }
    };

    match backend {
        CompressorKind::Gpu => {
            use crate::dds::gpu_channel::create_gpu_encoder_channels;

            let shutdown_token = tokio_util::sync::CancellationToken::new();
            let (channels, worker_handle, selection) = create_gpu_encoder_channels(
                config.texture().gpu_device(),
                prefetch_gpu_capacity(config),
                shutdown_token.clone(),
            )
            .map_err(|e| ServiceError::ConfigError(format!("GPU compressor: {e}")))?;

            let decision = DefaultEncoderPolicy.select(&selection.host, Some(CompressorKind::Gpu));
            if let Some(warning) = &decision.warning {
                tracing::warn!(adapter = %selection.adapter, "{warning}");
            }

            // The per-minute `GPU encode sample` line of each lane, ended
            // with the worker by the shutdown token or when its channel is
            // dropped (#419, #421).
            for (lane, channel) in [
                (GpuLane::Demand, &channels.demand),
                (GpuLane::Prefetch, &channels.prefetch),
            ] {
                crate::dds::gpu_stats::start_sample_log(
                    lane,
                    Arc::downgrade(channel.stats()),
                    channel.health().clone(),
                    shutdown_token.clone(),
                );
            }
            let gpu_health = channels.demand.health().clone();

            tracing::info!("GPU pipeline encoder created with dedicated worker");

            let (encoder, prefetch_encoder) = gpu_lane_encoders(
                channels,
                config.texture().format(),
                config.texture().mipmap_count(),
            );
            Ok(EncoderComponents {
                encoder,
                prefetch_encoder,
                gpu_worker_handle: Some(worker_handle),
                gpu_adapter: Some(selection.adapter),
                gpu_health: Some(gpu_health),
                gpu_warning: decision.warning,
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
        for compressor in ["software", "ispc", "gpu"] {
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
        use crate::texture::TextureEncoder;

        // Verify the GPU code path compiles and handles missing GPU gracefully.
        // On CI without GPU, this returns an error — that's fine.
        // Needs a Tokio runtime because GpuEncoderChannel spawns a worker task.
        let config = ServiceConfig::builder()
            .texture(TextureConfig::default().with_compressor("gpu".to_string()))
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
                let health = components
                    .gpu_health
                    .expect("a GPU encoder reports its health");
                assert!(!health.is_failed(), "a new GPU is healthy");
                // The WARN and the Status pane say a failed GPU falls back
                // to ISPC: the encoder must name it.
                let debug = format!("{:?}", components.encoder);
                assert!(debug.contains(r#"fallback: "ispc""#), "{debug}");
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
                "error should mention GPU, got: {e}"
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

        let mut worker_handle = components
            .gpu_worker_handle
            .expect("GPU compressor should return a worker handle");

        // Drop both lanes' encoders: this drops both GpuEncoderChannels
        // (senders), closing the channels and causing the worker to exit.
        drop(components.encoder);
        drop(components.prefetch_encoder);

        // The worker must end. The bound is a guard against a hang, not a
        // measurement: dropping the device can take seconds on a discrete
        // GPU (5 s timed out once on a discrete NVIDIA GPU, #471).
        let ended = worker_handle
            .wait(std::time::Duration::from_secs(30))
            .await
            .expect("GPU worker should complete within 30s after channel close");
        assert!(ended.is_ok(), "GPU worker should not panic during shutdown");
    }

    /// With a CPU encoder both lanes encode on their own CPU pools, so they
    /// share one encoder (#421).
    #[test]
    fn a_cpu_encoder_is_shared_by_both_lanes() {
        for compressor in ["software", "ispc"] {
            let config = ServiceConfig::builder()
                .texture(TextureConfig::default().with_compressor(compressor.to_string()))
                .build();
            let components = create_encoder(&config).unwrap();
            assert!(
                Arc::ptr_eq(&components.encoder, &components.prefetch_encoder),
                "{compressor}"
            );
        }
    }

    /// The prefetch lane's GPU channel holds its CPU pool's worth of
    /// requests (#421).
    #[test]
    fn the_prefetch_gpu_channel_is_sized_by_the_prefetch_cpu_pool() {
        let config = ServiceConfig::builder()
            .prefetch_capacity_share(0.2)
            .build();
        assert_eq!(
            prefetch_gpu_capacity(&config),
            crate::executor::LaneSizing::from_configured_share(0.2)
                .prefetch
                .cpu
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
        assert!(components.gpu_health.is_none());
        assert!(
            components.gpu_warning.is_none(),
            "a CPU encoder never contends with the sim's GPU"
        );
    }
}
