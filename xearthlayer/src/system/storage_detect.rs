//! Storage device detection for the hardware report.
//!
//! Detects whether the cache path lives on an NVMe drive, a SATA SSD, or a
//! spinning disk, for display in `xearthlayer setup` and diagnostics.
//!
//! # Detection method (Linux)
//!
//! 1. Find the mount point for the given path
//! 2. Identify the block device for that mount
//! 3. Check `/sys/block/<device>/queue/rotational`
//! 4. For non-rotational devices, check whether the name marks it NVMe
//!
//! Detection failure yields `None`, which the caller renders as `Unknown`.
//!
//! This used to size disk I/O concurrency via a `DiskIoProfile` enum and a
//! `cache.disk_io_profile` setting. Neither ever reached a live limiter — see
//! issue #227 — so both were removed in v0.4.7 and pool sizing now derives
//! from the executor's own capacities. What remains is detection for display.

use super::hardware::StorageType;
use std::path::Path;
use tracing::debug;

#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Detect the storage type for the given path.
///
/// Returns `None` if detection fails.
#[cfg(target_os = "linux")]
pub(crate) fn detect_storage_type(path: &Path) -> Option<StorageType> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    // Get the device ID for the path
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to get metadata for {:?}: {}", path, e);
            // Try parent directory if path doesn't exist yet
            let parent = path.parent()?;
            match fs::metadata(parent) {
                Ok(m) => m,
                Err(e) => {
                    debug!("Failed to get metadata for parent {:?}: {}", parent, e);
                    return None;
                }
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
        return Some(StorageType::Nvme);
    }

    // Check rotational status
    let rotational_path = format!("/sys/block/{}/queue/rotational", block_device);
    match fs::read_to_string(&rotational_path) {
        Ok(content) => {
            let is_rotational = content.trim() == "1";
            if is_rotational {
                debug!("Detected rotational (HDD) device");
                Some(StorageType::Hdd)
            } else {
                debug!("Detected non-rotational (SSD) device");
                Some(StorageType::Ssd)
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
pub(crate) fn detect_storage_type(path: &Path) -> Option<StorageType> {
    warn!(
        "Storage type detection not supported on this platform, using default profile for {:?}",
        path
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detection must not panic or hang on a path that does not exist; the
    /// caller renders `None` as `Unknown`.
    #[test]
    fn detection_tolerates_a_missing_path() {
        let _ = detect_storage_type(Path::new("/nonexistent/path/for/test"));
    }

    /// On Linux a real path should resolve to a concrete storage type or to
    /// `None`; it must never yield a value the display layer cannot render.
    #[cfg(target_os = "linux")]
    #[test]
    fn detection_on_a_real_path_yields_a_renderable_result() {
        if let Some(kind) = detect_storage_type(Path::new("/tmp")) {
            assert!(matches!(
                kind,
                StorageType::Nvme | StorageType::Ssd | StorageType::Hdd
            ));
        }
    }
}
