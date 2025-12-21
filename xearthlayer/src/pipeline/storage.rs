//! Storage type detection for optimal I/O concurrency tuning.
//!
//! This module provides automatic detection of storage device types (HDD, SSD, NVMe)
//! to optimize disk I/O concurrency limits. Different storage types have vastly
//! different optimal concurrency:
//!
//! - **HDD**: Seek-bound, optimal at 1-4 concurrent operations
//! - **SSD**: Queue depth ~32, optimal at 32-64 concurrent operations
//! - **NVMe**: Multiple queues, optimal at 128-256+ concurrent operations
//!
//! # Detection Method (Linux)
//!
//! On Linux, detection works by:
//! 1. Finding the mount point for the given path
//! 2. Identifying the block device for that mount
//! 3. Checking `/sys/block/<device>/queue/rotational`
//! 4. For non-rotational devices, checking if it's NVMe via device path
//!
//! # Fallback Behavior
//!
//! If detection fails (unsupported OS, permission issues, etc.), the system
//! defaults to SSD profile as a safe middle-ground.

use std::path::Path;
use tracing::debug;

#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Storage device profile for I/O concurrency tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskIoProfile {
    /// Automatic detection (default)
    /// Falls back to SSD if detection fails
    Auto,
    /// Spinning disk - very conservative concurrency
    /// Optimal: 1-4 concurrent operations (seek-bound)
    Hdd,
    /// SATA/AHCI SSD - moderate concurrency
    /// Optimal: 32-64 concurrent operations
    #[default]
    Ssd,
    /// NVMe SSD - aggressive concurrency
    /// Optimal: 128-256 concurrent operations
    Nvme,
}

impl DiskIoProfile {
    /// Convert profile to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hdd => "hdd",
            Self::Ssd => "ssd",
            Self::Nvme => "nvme",
        }
    }

    /// Get the recommended maximum concurrent I/O operations for this profile.
    ///
    /// The actual limit is calculated as `min(num_cpus * scaling_factor, ceiling)`.
    pub fn concurrency_params(&self) -> (usize, usize) {
        match self {
            // For Auto, use SSD as default (will be resolved by detect_for_path)
            Self::Auto | Self::Ssd => (4, 64), // num_cpus * 4, max 64
            Self::Hdd => (1, 4),               // num_cpus * 1, max 4
            Self::Nvme => (8, 256),            // num_cpus * 8, max 256
        }
    }

    /// Calculate the actual concurrency limit for this profile.
    pub fn max_concurrent(&self) -> usize {
        let (scaling_factor, ceiling) = self.concurrency_params();
        let cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);
        (cpus * scaling_factor).min(ceiling).max(1)
    }

    /// Detect the appropriate profile for the given path.
    ///
    /// If `self` is `Auto`, attempts to detect the storage type.
    /// Otherwise, returns `self` unchanged.
    ///
    /// On detection failure, falls back to `Ssd`.
    pub fn resolve_for_path(&self, path: &Path) -> Self {
        match self {
            Self::Auto => detect_storage_type(path).unwrap_or_else(|| {
                debug!("Storage detection failed, defaulting to SSD profile");
                Self::Ssd
            }),
            other => *other,
        }
    }
}

impl std::fmt::Display for DiskIoProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for DiskIoProfile {
    type Err = ();

    /// Parse a profile from a string (case-insensitive).
    ///
    /// Valid values: "auto", "hdd", "ssd", "nvme"
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "hdd" => Ok(Self::Hdd),
            "ssd" => Ok(Self::Ssd),
            "nvme" => Ok(Self::Nvme),
            _ => Err(()),
        }
    }
}

/// Detect the storage type for the given path.
///
/// Returns `None` if detection fails.
#[cfg(target_os = "linux")]
fn detect_storage_type(path: &Path) -> Option<DiskIoProfile> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    // Get the device ID for the path
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to get metadata for {:?}: {}", path, e);
            // Try parent directory if path doesn't exist yet
            if let Some(parent) = path.parent() {
                match fs::metadata(parent) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("Failed to get metadata for parent {:?}: {}", parent, e);
                        return None;
                    }
                }
            } else {
                return None;
            }
        }
    };

    let dev_id = metadata.dev();
    let major = (dev_id >> 8) & 0xff;
    let minor = dev_id & 0xff;

    debug!(
        "Path {:?} is on device {}:{} (dev_id: {})",
        path, major, minor, dev_id
    );

    // Find the block device name by scanning /sys/block
    let block_device = find_block_device(major as u32, minor as u32)?;
    debug!("Found block device: {}", block_device);

    // Check if it's NVMe first (by device name pattern)
    if block_device.starts_with("nvme") {
        debug!("Detected NVMe device");
        return Some(DiskIoProfile::Nvme);
    }

    // Check rotational status
    let rotational_path = format!("/sys/block/{}/queue/rotational", block_device);
    match fs::read_to_string(&rotational_path) {
        Ok(content) => {
            let is_rotational = content.trim() == "1";
            if is_rotational {
                debug!("Detected rotational (HDD) device");
                Some(DiskIoProfile::Hdd)
            } else {
                debug!("Detected non-rotational (SSD) device");
                Some(DiskIoProfile::Ssd)
            }
        }
        Err(e) => {
            debug!(
                "Failed to read rotational status from {}: {}",
                rotational_path, e
            );
            None
        }
    }
}

/// Find the block device name for the given major:minor device numbers.
#[cfg(target_os = "linux")]
fn find_block_device(major: u32, minor: u32) -> Option<String> {
    use std::fs;

    // Read /sys/block to find matching device
    let block_dir = match fs::read_dir("/sys/block") {
        Ok(dir) => dir,
        Err(e) => {
            debug!("Failed to read /sys/block: {}", e);
            return None;
        }
    };

    for entry in block_dir.flatten() {
        let device_name = entry.file_name().to_string_lossy().to_string();

        // Check if this device matches
        if check_device_match(&device_name, major, minor) {
            return Some(device_name);
        }

        // Check partitions (e.g., sda1, nvme0n1p1)
        let partitions_path = entry.path();
        if let Ok(partitions) = fs::read_dir(&partitions_path) {
            for partition in partitions.flatten() {
                let partition_name = partition.file_name().to_string_lossy().to_string();
                // Partitions are subdirectories that start with the device name
                if partition_name.starts_with(&device_name)
                    && check_device_match(&partition_name, major, minor)
                {
                    // Return the base device, not the partition
                    return Some(device_name);
                }
            }
        }
    }

    None
}

/// Check if a device matches the given major:minor numbers.
#[cfg(target_os = "linux")]
fn check_device_match(device_name: &str, major: u32, minor: u32) -> bool {
    use std::fs;

    let dev_path = format!("/sys/block/{}/dev", device_name);
    if let Ok(content) = fs::read_to_string(&dev_path) {
        let expected = format!("{}:{}", major, minor);
        if content.trim() == expected {
            return true;
        }
    }

    // Also check in partition subdirectory
    let partition_dev_path = format!(
        "/sys/block/{}/{}/dev",
        device_name
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect::<String>(),
        device_name
    );
    if let Ok(content) = fs::read_to_string(&partition_dev_path) {
        let expected = format!("{}:{}", major, minor);
        if content.trim() == expected {
            return true;
        }
    }

    false
}

/// Fallback for non-Linux platforms - always returns None.
#[cfg(not(target_os = "linux"))]
fn detect_storage_type(path: &Path) -> Option<DiskIoProfile> {
    warn!(
        "Storage type detection not supported on this platform, using default profile for {:?}",
        path
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_from_str() {
        assert_eq!("auto".parse(), Ok(DiskIoProfile::Auto));
        assert_eq!("AUTO".parse(), Ok(DiskIoProfile::Auto));
        assert_eq!("hdd".parse(), Ok(DiskIoProfile::Hdd));
        assert_eq!("HDD".parse(), Ok(DiskIoProfile::Hdd));
        assert_eq!("ssd".parse(), Ok(DiskIoProfile::Ssd));
        assert_eq!("SSD".parse(), Ok(DiskIoProfile::Ssd));
        assert_eq!("nvme".parse(), Ok(DiskIoProfile::Nvme));
        assert_eq!("NVMe".parse(), Ok(DiskIoProfile::Nvme));
        assert_eq!("invalid".parse::<DiskIoProfile>(), Err(()));
    }

    #[test]
    fn test_profile_as_str() {
        assert_eq!(DiskIoProfile::Auto.as_str(), "auto");
        assert_eq!(DiskIoProfile::Hdd.as_str(), "hdd");
        assert_eq!(DiskIoProfile::Ssd.as_str(), "ssd");
        assert_eq!(DiskIoProfile::Nvme.as_str(), "nvme");
    }

    #[test]
    fn test_concurrency_params() {
        assert_eq!(DiskIoProfile::Hdd.concurrency_params(), (1, 4));
        assert_eq!(DiskIoProfile::Ssd.concurrency_params(), (4, 64));
        assert_eq!(DiskIoProfile::Nvme.concurrency_params(), (8, 256));
        // Auto uses SSD defaults
        assert_eq!(DiskIoProfile::Auto.concurrency_params(), (4, 64));
    }

    #[test]
    fn test_max_concurrent_respects_ceiling() {
        // HDD ceiling is 4, so even with many CPUs it won't exceed 4
        let hdd_max = DiskIoProfile::Hdd.max_concurrent();
        assert!(hdd_max >= 1 && hdd_max <= 4);

        // SSD ceiling is 64
        let ssd_max = DiskIoProfile::Ssd.max_concurrent();
        assert!(ssd_max >= 1 && ssd_max <= 64);

        // NVMe ceiling is 256
        let nvme_max = DiskIoProfile::Nvme.max_concurrent();
        assert!(nvme_max >= 1 && nvme_max <= 256);
    }

    #[test]
    fn test_resolve_non_auto_returns_self() {
        let path = Path::new("/tmp");
        assert_eq!(
            DiskIoProfile::Hdd.resolve_for_path(path),
            DiskIoProfile::Hdd
        );
        assert_eq!(
            DiskIoProfile::Ssd.resolve_for_path(path),
            DiskIoProfile::Ssd
        );
        assert_eq!(
            DiskIoProfile::Nvme.resolve_for_path(path),
            DiskIoProfile::Nvme
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", DiskIoProfile::Auto), "auto");
        assert_eq!(format!("{}", DiskIoProfile::Hdd), "hdd");
        assert_eq!(format!("{}", DiskIoProfile::Ssd), "ssd");
        assert_eq!(format!("{}", DiskIoProfile::Nvme), "nvme");
    }

    #[test]
    fn test_default_is_ssd() {
        assert_eq!(DiskIoProfile::default(), DiskIoProfile::Ssd);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_detect_storage_type_current_dir() {
        // This test just verifies detection doesn't panic
        // The actual result depends on the system
        let result = detect_storage_type(Path::new("."));
        // Result could be Some or None depending on system
        if let Some(profile) = result {
            println!("Detected profile for current directory: {:?}", profile);
        }
    }
}
