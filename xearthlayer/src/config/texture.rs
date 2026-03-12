//! Texture encoding configuration.

use super::file::DEFAULT_MIPMAP_COUNT;
use crate::dds::DdsFormat;

/// Configuration for texture encoding.
///
/// Groups all parameters needed to configure a texture encoder,
/// providing sensible defaults while allowing customization.
///
/// # Example
///
/// ```
/// use xearthlayer::config::TextureConfig;
/// use xearthlayer::dds::DdsFormat;
///
/// // Using defaults (BC1 format, 5 mipmaps)
/// let config = TextureConfig::default();
/// assert_eq!(config.format(), DdsFormat::BC1);
/// assert_eq!(config.mipmap_count(), 5);
///
/// // Custom configuration
/// let config = TextureConfig::new(DdsFormat::BC3)
///     .with_mipmap_count(3);
/// assert_eq!(config.format(), DdsFormat::BC3);
/// assert_eq!(config.mipmap_count(), 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureConfig {
    /// DDS compression format (BC1 or BC3)
    format: DdsFormat,
    /// Number of mipmap levels (1-10, default: 5 for 4096→256)
    mipmap_count: usize,
    /// Compressor backend: "software", "ispc", or "gpu"
    compressor: String,
    /// GPU device selector: "integrated", "discrete", or adapter name substring
    gpu_device: String,
}

impl TextureConfig {
    /// Create a new texture configuration with the specified format.
    ///
    /// Uses default mipmap count (suitable for 4096×4096 → 256×256 chain).
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: DEFAULT_MIPMAP_COUNT,
            compressor: crate::config::defaults::DEFAULT_COMPRESSOR.to_string(),
            gpu_device: crate::config::defaults::DEFAULT_GPU_DEVICE.to_string(),
        }
    }

    /// Set the number of mipmap levels.
    ///
    /// For a 4096×4096 source image, common values are:
    /// - 1: No mipmaps (only full resolution)
    /// - 5: 4096 → 2048 → 1024 → 512 → 256 (default)
    /// - 10: Full chain down to 4×4
    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = count;
        self
    }

    /// Get the DDS compression format.
    pub fn format(&self) -> DdsFormat {
        self.format
    }

    /// Get the number of mipmap levels.
    pub fn mipmap_count(&self) -> usize {
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
            mipmap_count: DEFAULT_MIPMAP_COUNT,
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
        assert_eq!(config.mipmap_count(), DEFAULT_MIPMAP_COUNT);
    }

    #[test]
    fn test_new_with_format() {
        let config = TextureConfig::new(DdsFormat::BC3);
        assert_eq!(config.format(), DdsFormat::BC3);
        assert_eq!(config.mipmap_count(), DEFAULT_MIPMAP_COUNT); // Default mipmaps
    }

    #[test]
    fn test_with_mipmap_count() {
        let config = TextureConfig::new(DdsFormat::BC1).with_mipmap_count(10);
        assert_eq!(config.format(), DdsFormat::BC1);
        assert_eq!(config.mipmap_count(), 10);
    }

    #[test]
    fn test_builder_chain() {
        let config = TextureConfig::new(DdsFormat::BC3).with_mipmap_count(3);
        assert_eq!(config.format(), DdsFormat::BC3);
        assert_eq!(config.mipmap_count(), 3);
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
