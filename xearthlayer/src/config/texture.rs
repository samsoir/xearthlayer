//! Texture encoding configuration.

use crate::dds::DdsFormat;

/// Configuration for texture encoding.
///
/// Groups all parameters needed to configure a texture encoder,
/// providing sensible defaults while allowing customization.
///
/// # Mipmap levels
///
/// `mipmap_count` is an [`Option`]: `None` — the default — means the full
/// chain, derived from the texture dimensions at encode time (13 levels for
/// 4096×4096, down to 1×1). `Some(n)` truncates the chain to `n` levels and
/// exists for tests and callers that deliberately want a partial chain.
///
/// A truncated chain makes X-Plane clamp sampling at the last declared level,
/// which undersamples sloped terrain at grazing angles and shows up as banding
/// along contour lines — so there is no user-facing setting for this.
///
/// # Example
///
/// ```
/// use xearthlayer::config::TextureConfig;
/// use xearthlayer::dds::DdsFormat;
///
/// // Using defaults (BC1 format, full mipmap chain)
/// let config = TextureConfig::default();
/// assert_eq!(config.format(), DdsFormat::BC1);
/// assert_eq!(config.mipmap_count(), None);
///
/// // Custom configuration
/// let config = TextureConfig::new(DdsFormat::BC3)
///     .with_mipmap_count(3);
/// assert_eq!(config.format(), DdsFormat::BC3);
/// assert_eq!(config.mipmap_count(), Some(3));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureConfig {
    /// DDS compression format (BC1 or BC3)
    format: DdsFormat,
    /// Mipmap levels to emit; `None` means the full chain for the texture size
    mipmap_count: Option<usize>,
    /// Compressor backend: "software", "ispc", or "gpu"
    compressor: String,
    /// GPU device selector: "integrated", "discrete", or adapter name substring
    gpu_device: String,
}

impl TextureConfig {
    /// Create a new texture configuration with the specified format.
    ///
    /// Emits the full mipmap chain for whatever texture size is encoded.
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: None,
            compressor: crate::config::defaults::DEFAULT_COMPRESSOR.to_string(),
            gpu_device: crate::config::defaults::DEFAULT_GPU_DEVICE.to_string(),
        }
    }

    /// Truncate the mipmap chain to a fixed number of levels.
    ///
    /// Intended for tests and special-purpose callers. Production code should
    /// leave this unset so the chain is derived from the texture dimensions.
    /// The count is clamped to the full chain length at encode time, so it can
    /// never make the DDS header over-declare what the payload holds.
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = Some(count);
        self
    }

    /// Get the DDS compression format.
    pub fn format(&self) -> DdsFormat {
        self.format
    }

    /// Get the mipmap level override, or `None` for the full chain.
    pub fn mipmap_count(&self) -> Option<usize> {
        self.mipmap_count
    }

    /// Set the compressor backend.
    pub fn with_compressor(mut self, compressor: String) -> Self {
        self.compressor = compressor;
        self
    }

    /// Set the GPU device selector.
    pub fn with_gpu_device(mut self, gpu_device: String) -> Self {
        self.gpu_device = gpu_device;
        self
    }

    /// Get the compressor backend.
    pub fn compressor(&self) -> &str {
        &self.compressor
    }

    /// Get the GPU device selector.
    pub fn gpu_device(&self) -> &str {
        &self.gpu_device
    }
}

impl Default for TextureConfig {
    fn default() -> Self {
        Self {
            format: DdsFormat::BC1,
            mipmap_count: None,
            compressor: crate::config::defaults::DEFAULT_COMPRESSOR.to_string(),
            gpu_device: crate::config::defaults::DEFAULT_GPU_DEVICE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TextureConfig::default();
        assert_eq!(config.format(), DdsFormat::BC1);
        assert_eq!(config.mipmap_count(), None, "default is the full chain");
    }

    #[test]
    fn test_new_with_format() {
        let config = TextureConfig::new(DdsFormat::BC3);
        assert_eq!(config.format(), DdsFormat::BC3);
        assert_eq!(config.mipmap_count(), None, "default is the full chain");
    }

    #[test]
    fn test_with_mipmap_count() {
        let config = TextureConfig::new(DdsFormat::BC1).with_mipmap_count(10);
        assert_eq!(config.format(), DdsFormat::BC1);
        assert_eq!(config.mipmap_count(), Some(10));
    }

    #[test]
    fn test_builder_chain() {
        let config = TextureConfig::new(DdsFormat::BC3).with_mipmap_count(3);
        assert_eq!(config.format(), DdsFormat::BC3);
        assert_eq!(config.mipmap_count(), Some(3));
    }

    #[test]
    fn test_clone_semantics() {
        let config1 = TextureConfig::new(DdsFormat::BC1);
        let config2 = config1.clone();
        assert_eq!(config1.format(), config2.format());
    }

    #[test]
    fn test_equality() {
        let config1 = TextureConfig::new(DdsFormat::BC1).with_mipmap_count(5);
        let config2 = TextureConfig::new(DdsFormat::BC1).with_mipmap_count(5);
        let config3 = TextureConfig::new(DdsFormat::BC3).with_mipmap_count(5);

        assert_eq!(config1, config2);
        assert_ne!(config1, config3);
    }

    #[test]
    fn test_debug_impl() {
        let config = TextureConfig::new(DdsFormat::BC1);
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("TextureConfig"));
        assert!(debug_str.contains("BC1"));
    }

    #[test]
    fn test_texture_config_compressor_default() {
        let config = TextureConfig::default();
        assert_eq!(config.compressor(), "ispc");
        assert_eq!(config.gpu_device(), "integrated");
    }

    #[test]
    fn test_texture_config_with_compressor() {
        let config = TextureConfig::default()
            .with_compressor("gpu".to_string())
            .with_gpu_device("Radeon".to_string());
        assert_eq!(config.compressor(), "gpu");
        assert_eq!(config.gpu_device(), "Radeon");
    }
}
