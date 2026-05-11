//! Hardware detection for CPU, memory, and storage.
//!
//! Provides cross-platform detection of system hardware with fallbacks
//! for unsupported platforms.

use std::path::Path;

use crate::config::format_size;
use crate::config::DiskIoProfile;

use super::recommendations::{
    recommended_disk_cache, recommended_disk_io_profile, recommended_memory_cache,
};

/// Detected storage type classification.
///
/// This is a simplified view of [`DiskIoProfile`] for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageType {
    /// NVMe SSD with multiple queues
    Nvme,
    /// SATA/AHCI SSD
    Ssd,
    /// Traditional spinning hard drive
    Hdd,
    /// Could not determine storage type
    Unknown,
}

impl StorageType {
    /// Convert from DiskIoProfile.
    pub fn from_disk_io_profile(profile: DiskIoProfile) -> Self {
        match profile {
            DiskIoProfile::Nvme => StorageType::Nvme,
            DiskIoProfile::Ssd => StorageType::Ssd,
            DiskIoProfile::Hdd => StorageType::Hdd,
            DiskIoProfile::Auto => StorageType::Unknown,
        }
    }

    /// Get human-readable display string.
    pub fn display(&self) -> &'static str {
        match self {
            StorageType::Nvme => "NVMe SSD",
            StorageType::Ssd => "SATA SSD",
            StorageType::Hdd => "HDD",
            StorageType::Unknown => "Unknown (defaulting to SSD)",
        }
    }
}

/// Detected system hardware information.
///
/// Contains CPU, memory, and storage detection results along with
/// convenience methods for recommendations and display formatting.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    /// Number of logical CPU cores
    pub cpu_cores: usize,
    /// Total system memory in bytes
    pub total_memory: usize,
    /// Detected storage type for the cache path
    pub storage_type: StorageType,
    /// The underlying DiskIoProfile for configuration
    pub disk_io_profile: DiskIoProfile,
    /// Bytes available to a non-privileged user at the cache path's
    /// filesystem. Used to size the recommended disk cache. Zero if
    /// detection failed (e.g., path does not exist yet).
    pub cache_path_available_bytes: u64,
}

impl SystemInfo {
    /// Detect system information for a given cache path.
    ///
    /// # Arguments
    ///
    /// * `cache_path` - Path where the cache will be stored (used for storage detection)
    ///
    /// # Example
    ///
    /// ```
    /// use std::path::Path;
    /// use xearthlayer::system::SystemInfo;
    ///
    /// let info = SystemInfo::detect(Path::new("/tmp"));
    /// println!("Detected {} CPU cores", info.cpu_cores);
    /// ```
    pub fn detect(cache_path: &Path) -> Self {
        let cpu_cores = detect_cpu_cores();
        let total_memory = detect_total_memory();
        let disk_io_profile = DiskIoProfile::Auto.resolve_for_path(cache_path);
        let storage_type = StorageType::from_disk_io_profile(disk_io_profile);
        let cache_path_available_bytes = available_bytes_for(cache_path);

        Self {
            cpu_cores,
            total_memory,
            storage_type,
            disk_io_profile,
            cache_path_available_bytes,
        }
    }

    /// Create SystemInfo with specific values (for testing).
    #[cfg(test)]
    pub fn new(cpu_cores: usize, total_memory: usize, storage_type: StorageType) -> Self {
        Self::new_with_disk(cpu_cores, total_memory, storage_type, 0)
    }

    /// Create SystemInfo with specific values including available disk (for testing).
    #[cfg(test)]
    pub fn new_with_disk(
        cpu_cores: usize,
        total_memory: usize,
        storage_type: StorageType,
        cache_path_available_bytes: u64,
    ) -> Self {
        let disk_io_profile = match storage_type {
            StorageType::Nvme => DiskIoProfile::Nvme,
            StorageType::Ssd => DiskIoProfile::Ssd,
            StorageType::Hdd => DiskIoProfile::Hdd,
            StorageType::Unknown => DiskIoProfile::Auto,
        };

        Self {
            cpu_cores,
            total_memory,
            storage_type,
            disk_io_profile,
            cache_path_available_bytes,
        }
    }

    // =========================================================================
    // Recommendations (delegated to recommendations module)
    // =========================================================================

    /// Get recommended memory cache size in bytes.
    ///
    /// Computed as `RAM / 12`, clamped to a 500 MB floor and a `RAM / 4`
    /// ceiling. The cache is intentionally a small request absorber, not a
    /// working set holder; the on-disk DDS cache holds the working set.
    pub fn recommended_memory_cache(&self) -> usize {
        recommended_memory_cache(self.total_memory)
    }

    /// Get recommended disk cache size in bytes.
    ///
    /// Computed as 25% of available space at the cache path's filesystem,
    /// floored to the nearest 10 GB. Minimum 10 GB to avoid thrashing.
    pub fn recommended_disk_cache(&self) -> usize {
        recommended_disk_cache(self.cache_path_available_bytes)
    }

    /// Get recommended disk I/O profile string for configuration.
    ///
    /// Returns "nvme" if NVMe detected, otherwise "auto".
    pub fn recommended_disk_io_profile(&self) -> &'static str {
        recommended_disk_io_profile(self.disk_io_profile)
    }

    // =========================================================================
    // Display formatting
    // =========================================================================

    /// Get formatted memory string (e.g., "32 GB").
    pub fn memory_display(&self) -> String {
        format_size(self.total_memory)
    }

    /// Get formatted recommended memory cache size (e.g., "8 GB").
    pub fn recommended_memory_cache_display(&self) -> String {
        format_size(self.recommended_memory_cache())
    }

    /// Get formatted recommended disk cache size (e.g., "40 GB").
    pub fn recommended_disk_cache_display(&self) -> String {
        format_size(self.recommended_disk_cache())
    }

    /// Get storage type display string (e.g., "NVMe SSD").
    pub fn storage_display(&self) -> &'static str {
        self.storage_type.display()
    }
}

/// Return the bytes available at the filesystem holding `path`.
///
/// Walks up the path until it finds an existing ancestor — this matters
/// during setup, when the cache directory may not exist yet but its
/// parent chain does. Returns 0 if no ancestor can be statted.
fn available_bytes_for(path: &Path) -> u64 {
    use crate::system::filesystem::fs_info;
    let mut probe: Option<&Path> = Some(path);
    while let Some(p) = probe {
        if p.exists() {
            return fs_info(p).map(|i| i.available_bytes).unwrap_or(0);
        }
        probe = p.parent();
    }
    0
}

/// Detect the number of logical CPU cores.
///
/// Falls back to 4 if detection fails.
pub fn detect_cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
}

/// Detect total system memory in bytes.
///
/// # Platform Support
///
/// - **Linux**: Parses `/proc/meminfo`
/// - **Other platforms**: Returns fallback of 8GB
#[cfg(target_os = "linux")]
pub fn detect_total_memory() -> usize {
    use std::fs;

    // Parse /proc/meminfo
    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                // Format: "MemTotal:       16384000 kB"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<usize>() {
                        return kb * 1024; // Convert to bytes
                    }
                }
            }
        }
    }

    // Fallback: 8GB
    fallback_memory()
}

#[cfg(not(target_os = "linux"))]
pub fn detect_total_memory() -> usize {
    // Fallback for non-Linux: 8GB
    fallback_memory()
}

/// Fallback memory value when detection fails.
const fn fallback_memory() -> usize {
    8 * 1024 * 1024 * 1024 // 8GB
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GB;

    #[test]
    fn test_detect_cpu_cores_returns_positive() {
        let cores = detect_cpu_cores();
        assert!(cores > 0, "Should detect at least 1 CPU core");
    }

    #[test]
    fn test_detect_total_memory_returns_positive() {
        let memory = detect_total_memory();
        assert!(memory > 0, "Should detect some memory");
    }

    #[test]
    fn test_storage_type_display() {
        assert_eq!(StorageType::Nvme.display(), "NVMe SSD");
        assert_eq!(StorageType::Ssd.display(), "SATA SSD");
        assert_eq!(StorageType::Hdd.display(), "HDD");
        assert_eq!(
            StorageType::Unknown.display(),
            "Unknown (defaulting to SSD)"
        );
    }

    #[test]
    fn test_system_info_recommendations() {
        // 16GB system with SSD, 230GB available disk
        let info = SystemInfo::new_with_disk(8, 16 * GB, StorageType::Ssd, 230 * GB as u64);
        // 16GB / 12 = ~1.33GB, well above 500MB floor and well below 4GB ceiling
        assert_eq!(info.recommended_memory_cache(), 16 * GB / 12);
        assert_eq!(info.recommended_disk_cache(), 50 * GB);
        assert_eq!(info.recommended_disk_io_profile(), "auto");
    }

    #[test]
    fn test_system_info_nvme_recommendation() {
        let info = SystemInfo::new(8, 16 * GB, StorageType::Nvme);
        assert_eq!(info.recommended_disk_io_profile(), "nvme");
    }

    #[test]
    fn test_system_info_display_formatting() {
        let info = SystemInfo::new(8, 16 * GB, StorageType::Ssd);
        assert_eq!(info.memory_display(), "16 GB");
        // 16GB / 12 = 1.333... GB → format_size renders as "1.3 GB"
        assert_eq!(info.recommended_memory_cache_display(), "1.3 GB");
        assert_eq!(info.storage_display(), "SATA SSD");
    }

    #[test]
    fn available_bytes_for_walks_up_to_existing_ancestor() {
        // /tmp definitely exists; a child path under it does not. The
        // helper should still return a non-zero number by falling back
        // to the existing ancestor.
        let phantom = Path::new("/tmp/xearthlayer-test-nonexistent-child-path-12345/cache");
        let bytes = available_bytes_for(phantom);
        assert!(bytes > 0, "Should report bytes from existing ancestor /tmp");
    }
}
