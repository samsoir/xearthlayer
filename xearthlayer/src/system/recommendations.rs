//! Configuration recommendations based on system hardware.
//!
//! Provides functions to calculate optimal XEarthLayer settings based on
//! detected hardware capabilities.

use crate::config::format_size;
use crate::pipeline::DiskIoProfile;

/// Size constants for clarity.
const GB: usize = 1024 * 1024 * 1024;

/// Recommended configuration settings based on system hardware.
///
/// This struct collects all recommendations in one place for easy
/// consumption by UI code.
#[derive(Debug, Clone)]
pub struct RecommendedSettings {
    /// Recommended memory cache size in bytes
    pub memory_cache: usize,
    /// Recommended disk cache size in bytes
    pub disk_cache: usize,
    /// Recommended disk I/O profile
    pub disk_io_profile: DiskIoProfile,
}

impl RecommendedSettings {
    /// Create recommendations based on system info.
    ///
    /// # Arguments
    ///
    /// * `total_memory` - Total system memory in bytes
    /// * `detected_profile` - Storage type detected for cache location
    pub fn for_system(total_memory: usize, detected_profile: DiskIoProfile) -> Self {
        Self {
            memory_cache: recommended_memory_cache(total_memory),
            disk_cache: recommended_disk_cache(),
            disk_io_profile: detected_profile,
        }
    }

    /// Get formatted memory cache size (e.g., "8 GB").
    pub fn memory_cache_display(&self) -> String {
        format_size(self.memory_cache)
    }

    /// Get formatted disk cache size (e.g., "40 GB").
    pub fn disk_cache_display(&self) -> String {
        format_size(self.disk_cache)
    }

    /// Get disk I/O profile string for configuration.
    pub fn disk_io_profile_str(&self) -> &'static str {
        recommended_disk_io_profile(self.disk_io_profile)
    }
}

/// Calculate recommended memory cache size based on total system memory.
///
/// # Memory Cache Sizing Rules
///
/// | System RAM | Cache Size |
/// |------------|------------|
/// | < 8 GB     | 2 GB       |
/// | 8-31 GB    | 8 GB       |
/// | 32-63 GB   | 12 GB      |
/// | 64+ GB     | 16 GB      |
///
/// # Arguments
///
/// * `total_memory` - Total system memory in bytes
///
/// # Returns
///
/// Recommended memory cache size in bytes
///
/// # Example
///
/// ```
/// use xearthlayer::system::recommended_memory_cache;
///
/// let cache_size = recommended_memory_cache(32 * 1024 * 1024 * 1024); // 32 GB RAM
/// assert_eq!(cache_size, 12 * 1024 * 1024 * 1024); // 12 GB cache
/// ```
pub fn recommended_memory_cache(total_memory: usize) -> usize {
    match total_memory {
        m if m < 8 * GB => 2 * GB,
        m if m < 32 * GB => 8 * GB,
        m if m < 64 * GB => 12 * GB,
        _ => 16 * GB,
    }
}

/// Get recommended disk cache size.
///
/// Currently returns a fixed 40GB recommendation, which provides good
/// coverage for typical flight sessions without consuming excessive disk space.
///
/// # Returns
///
/// Recommended disk cache size in bytes (40 GB)
pub fn recommended_disk_cache() -> usize {
    40 * GB
}

/// Get recommended disk I/O profile string for configuration.
///
/// # Arguments
///
/// * `detected_profile` - The detected storage profile
///
/// # Returns
///
/// - `"nvme"` if NVMe storage detected (enables aggressive concurrency)
/// - `"auto"` otherwise (safe default that re-detects at runtime)
pub fn recommended_disk_io_profile(detected_profile: DiskIoProfile) -> &'static str {
    match detected_profile {
        DiskIoProfile::Nvme => "nvme",
        _ => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_cache_under_8gb() {
        // 4GB system -> 2GB cache
        assert_eq!(recommended_memory_cache(4 * GB), 2 * GB);
        // 7GB system -> 2GB cache
        assert_eq!(recommended_memory_cache(7 * GB), 2 * GB);
    }

    #[test]
    fn test_memory_cache_8_to_32gb() {
        // 8GB system -> 8GB cache
        assert_eq!(recommended_memory_cache(8 * GB), 8 * GB);
        // 16GB system -> 8GB cache
        assert_eq!(recommended_memory_cache(16 * GB), 8 * GB);
        // 31GB system -> 8GB cache
        assert_eq!(recommended_memory_cache(31 * GB), 8 * GB);
    }

    #[test]
    fn test_memory_cache_32_to_64gb() {
        // 32GB system -> 12GB cache
        assert_eq!(recommended_memory_cache(32 * GB), 12 * GB);
        // 48GB system -> 12GB cache
        assert_eq!(recommended_memory_cache(48 * GB), 12 * GB);
        // 63GB system -> 12GB cache
        assert_eq!(recommended_memory_cache(63 * GB), 12 * GB);
    }

    #[test]
    fn test_memory_cache_64gb_plus() {
        // 64GB system -> 16GB cache
        assert_eq!(recommended_memory_cache(64 * GB), 16 * GB);
        // 128GB system -> 16GB cache
        assert_eq!(recommended_memory_cache(128 * GB), 16 * GB);
    }

    #[test]
    fn test_disk_cache_fixed() {
        assert_eq!(recommended_disk_cache(), 40 * GB);
    }

    #[test]
    fn test_io_profile_nvme() {
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Nvme), "nvme");
    }

    #[test]
    fn test_io_profile_others_return_auto() {
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Ssd), "auto");
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Hdd), "auto");
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Auto), "auto");
    }

    #[test]
    fn test_recommended_settings() {
        let settings = RecommendedSettings::for_system(32 * GB, DiskIoProfile::Nvme);
        assert_eq!(settings.memory_cache, 12 * GB);
        assert_eq!(settings.disk_cache, 40 * GB);
        assert_eq!(settings.disk_io_profile, DiskIoProfile::Nvme);
        assert_eq!(settings.disk_io_profile_str(), "nvme");
    }
}
