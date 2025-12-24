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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureConfig {
    /// DDS compression format (BC1 or BC3)
    format: DdsFormat,
    /// Number of mipmap levels (1-10, default: 5 for 4096→256)
    mipmap_count: usize,
}

impl TextureConfig {
    /// Create a new texture configuration with the specified format.
    ///
    /// Uses default mipmap count (suitable for 4096×4096 → 256×256 chain).
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: DEFAULT_MIPMAP_COUNT,
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
}

impl Default for TextureConfig {
    fn default() -> Self {
        Self {
            format: DdsFormat::BC1,
            mipmap_count: DEFAULT_MIPMAP_COUNT,
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
    fn test_copy_semantics() {
        let config1 = TextureConfig::new(DdsFormat::BC1);
        let config2 = config1; // Copy, not move
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
}
