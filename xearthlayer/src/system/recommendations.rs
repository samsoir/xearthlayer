//! Configuration recommendations based on system hardware.
//!
//! Provides functions to calculate optimal XEarthLayer settings based on
//! detected hardware capabilities.

use crate::config::{format_size, DiskIoProfile, GB, MB};

/// Floor for the memory-cache recommendation.
///
/// The memory cache is intentionally a small request absorber rather than a
/// working set holder (the on-disk DDS cache is the working set). Even on
/// very small RAM systems, dropping below 500 MB starves the absorber. See
/// `xearthlayer/src/cache/providers/memory.rs` for the rationale.
pub const MIN_MEMORY_CACHE_BYTES: usize = 500 * MB;

/// Floor for the disk-cache recommendation. Below this, the disk cache is
/// thrashing constantly and provides little benefit.
pub const MIN_DISK_CACHE_BYTES: usize = 10 * GB;

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
    /// * `available_disk` - Bytes available at the cache directory's filesystem
    /// * `detected_profile` - Storage type detected for cache location
    pub fn for_system(
        total_memory: usize,
        available_disk: u64,
        detected_profile: DiskIoProfile,
    ) -> Self {
        Self {
            memory_cache: recommended_memory_cache(total_memory),
            disk_cache: recommended_disk_cache(available_disk),
            disk_io_profile: detected_profile,
        }
    }

    /// Get formatted memory cache size (e.g., "1.4 GB").
    pub fn memory_cache_display(&self) -> String {
        format_size(self.memory_cache)
    }

    /// Get formatted disk cache size (e.g., "50 GB").
    pub fn disk_cache_display(&self) -> String {
        format_size(self.disk_cache)
    }

    /// Get disk I/O profile string for configuration.
    pub fn disk_io_profile_str(&self) -> &'static str {
        recommended_disk_io_profile(self.disk_io_profile)
    }
}

/// Calculate recommended memory cache size from total system memory.
///
/// The memory cache is intentionally a small request absorber, not a working
/// set holder — the on-disk DDS cache is the working set. The formula is
/// `RAM / 12`, clamped to `[MIN_MEMORY_CACHE_BYTES, RAM / 4]`.
///
/// # Examples
///
/// | System RAM | Recommended cache |
/// |------------|------------------|
/// | 4 GB       | 500 MB (floor)   |
/// | 8 GB       | ~683 MB          |
/// | 16 GB      | ~1.3 GB          |
/// | 32 GB      | ~2.7 GB          |
/// | 64 GB      | ~5.3 GB          |
/// | 128 GB     | ~10.7 GB         |
///
/// ```
/// use xearthlayer::system::recommended_memory_cache;
///
/// let cache = recommended_memory_cache(16 * 1024 * 1024 * 1024); // 16 GB RAM
/// assert!(cache >= 1_300_000_000 && cache <= 1_500_000_000); // ~1.3 GB
/// ```
pub fn recommended_memory_cache(total_memory: usize) -> usize {
    let raw = total_memory / 12;
    let ceiling = total_memory / 4;
    raw.clamp(MIN_MEMORY_CACHE_BYTES, ceiling.max(MIN_MEMORY_CACHE_BYTES))
}

/// Calculate recommended disk cache size from available disk space.
///
/// Targets 25% of free space, floored to the nearest 10 GB so the value
/// is round and predictable in the wizard. Will not recommend less than
/// `MIN_DISK_CACHE_BYTES` even if available space is very low — below
/// that threshold the cache thrashes and provides little benefit.
///
/// # Examples
///
/// | Available | Recommended |
/// |-----------|-------------|
/// | 8 GB      | 10 GB (floor) |
/// | 80 GB     | 20 GB       |
/// | 230 GB    | 50 GB       |
/// | 1 TB      | 250 GB      |
///
/// ```
/// use xearthlayer::system::recommended_disk_cache;
///
/// let cache = recommended_disk_cache(230 * 1024 * 1024 * 1024); // 230 GB free
/// assert_eq!(cache, 50 * 1024 * 1024 * 1024); // 50 GB
/// ```
pub fn recommended_disk_cache(available_bytes: u64) -> usize {
    let quarter = available_bytes / 4;
    let floor_step = MIN_DISK_CACHE_BYTES as u64;
    let floored = (quarter / floor_step) * floor_step;
    let bytes = floored.max(floor_step);
    usize::try_from(bytes).unwrap_or(usize::MAX)
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
    fn memory_cache_floored_at_500mb_for_tiny_systems() {
        // 4GB / 12 ~= 341MB, below the 500MB floor.
        assert_eq!(recommended_memory_cache(4 * GB), MIN_MEMORY_CACHE_BYTES);
        // 1GB / 12 ~= 85MB, also clamped — but ceiling (1GB/4 = 256MB) is
        // even lower; we still must honor the absolute floor for sanity.
        assert_eq!(recommended_memory_cache(1 * GB), MIN_MEMORY_CACHE_BYTES);
    }

    #[test]
    fn memory_cache_uses_ram_div_12_in_normal_range() {
        // 16GB / 12 = 1.333... GB
        assert_eq!(recommended_memory_cache(16 * GB), 16 * GB / 12);
        // 32GB / 12 = 2.666... GB
        assert_eq!(recommended_memory_cache(32 * GB), 32 * GB / 12);
        // 64GB / 12 = 5.333... GB
        assert_eq!(recommended_memory_cache(64 * GB), 64 * GB / 12);
        // 128GB / 12 = 10.666... GB
        assert_eq!(recommended_memory_cache(128 * GB), 128 * GB / 12);
    }

    #[test]
    fn memory_cache_capped_at_quarter_ram() {
        // RAM / 12 < RAM / 4 always, so the ceiling never bites in normal
        // operation. The clamp only matters at the floor (tiny systems).
        // We still verify it doesn't accidentally exceed the cap.
        for ram in &[8 * GB, 16 * GB, 64 * GB, 256 * GB] {
            assert!(
                recommended_memory_cache(*ram) <= ram / 4,
                "{} GB RAM cache exceeded RAM/4 ceiling",
                ram / GB
            );
        }
    }

    #[test]
    fn disk_cache_floored_at_10gb() {
        // Tiny disk → still recommend 10GB so we don't hand the user a
        // useless thrashing-cache config.
        assert_eq!(recommended_disk_cache(0), MIN_DISK_CACHE_BYTES);
        assert_eq!(recommended_disk_cache(8 * GB as u64), MIN_DISK_CACHE_BYTES);
    }

    #[test]
    fn disk_cache_25_percent_floored_to_10gb_step() {
        // 80GB free → 25% = 20GB → already a 10GB multiple
        assert_eq!(recommended_disk_cache(80 * GB as u64), 20 * GB);
        // 230GB free → 25% = 57.5GB → floored to 50GB
        assert_eq!(recommended_disk_cache(230 * GB as u64), 50 * GB);
        // 1TB free → 25% = 256GB → floored to 250GB
        assert_eq!(recommended_disk_cache(1024 * GB as u64), 250 * GB);
    }

    #[test]
    fn io_profile_nvme_is_explicit() {
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Nvme), "nvme");
    }

    #[test]
    fn io_profile_others_default_to_auto() {
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Ssd), "auto");
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Hdd), "auto");
        assert_eq!(recommended_disk_io_profile(DiskIoProfile::Auto), "auto");
    }

    #[test]
    fn recommended_settings_combines_inputs() {
        let settings =
            RecommendedSettings::for_system(32 * GB, 230 * GB as u64, DiskIoProfile::Nvme);
        assert_eq!(settings.memory_cache, 32 * GB / 12);
        assert_eq!(settings.disk_cache, 50 * GB);
        assert_eq!(settings.disk_io_profile, DiskIoProfile::Nvme);
        assert_eq!(settings.disk_io_profile_str(), "nvme");
    }
}
