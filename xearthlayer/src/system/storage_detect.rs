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
//! # Detection method (macOS)
//!
//! macOS has no `/sys/block`. The path is resolved to its backing device node
//! with `df`, then `diskutil info` reports whether that device is solid state
//! and over what bus.
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// Detect the storage type for the given path on macOS.
///
/// Resolves the path to its backing device node with `df`, then asks
/// `diskutil info` whether that device is solid state and over what protocol.
/// Returns `None` if any step fails or the device cannot be classified, which
/// the caller renders as `Unknown`.
#[cfg(target_os = "macos")]
pub(crate) fn detect_storage_type(path: &Path) -> Option<StorageType> {
    let existing = nearest_existing_ancestor(path)?;
    let df_out = run_command("df", &[existing.to_str()?])?;
    let device = parse_df_device(&df_out)?;
    debug!("Path {:?} is backed by device {}", path, device);

    let info = run_command("diskutil", &["info", &device])?;
    let kind = classify_diskutil_info(&info);
    debug!("diskutil classified {} as {:?}", device, kind);
    kind
}

/// Walk up from `path` to the nearest ancestor that exists.
///
/// The cache directory may not exist yet during setup, but `df` needs a real
/// path. The filesystem of the nearest existing ancestor is the same one the
/// cache will live on, so it answers the storage-type question correctly.
#[cfg(target_os = "macos")]
fn nearest_existing_ancestor(path: &Path) -> Option<std::path::PathBuf> {
    let mut probe: Option<&Path> = Some(path);
    while let Some(p) = probe {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        probe = p.parent();
    }
    None
}

/// Run a command and return its stdout as a string, or `None` on failure.
#[cfg(target_os = "macos")]
fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    use std::process::Command;

    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        debug!("`{} {}` failed", cmd, args.join(" "));
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extract the `/dev/...` device node from `df <path>` output.
///
/// Pure function (no I/O) so the parsing is unit-testable. `df` prints a header
/// line followed by one data line whose first column is the device node, e.g.
/// `/dev/disk3s5`. Returns `None` if there is no data line or it isn't a device.
#[cfg(target_os = "macos")]
fn parse_df_device(df_output: &str) -> Option<String> {
    df_output
        .lines()
        .nth(1)? // skip the header row
        .split_whitespace()
        .next()
        .filter(|dev| dev.starts_with("/dev/"))
        .map(|dev| dev.to_string())
}

/// Map `diskutil info <device>` output to a [`StorageType`].
///
/// Pure function (no I/O) so the classification is unit-testable. Keys on two
/// fields:
/// - `Solid State: No` maps to [`StorageType::Hdd`]
/// - `Solid State: Yes` over a PCI / Apple Fabric / NVMe protocol maps to [`StorageType::Nvme`]
/// - `Solid State: Yes` over any other protocol (SATA, USB) maps to [`StorageType::Ssd`]
///
/// Returns `None` if the `Solid State` field is absent, so the caller renders
/// `Unknown` rather than guessing.
#[cfg(target_os = "macos")]
fn classify_diskutil_info(info: &str) -> Option<StorageType> {
    let field = |name: &str| {
        info.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim() == name)
                .map(|(_, value)| value.trim())
        })
    };

    if field("Solid State")?.eq_ignore_ascii_case("No") {
        return Some(StorageType::Hdd);
    }

    // Solid state: distinguish NVMe-class from SATA by the bus protocol.
    let protocol = field("Protocol").unwrap_or("").to_ascii_lowercase();
    if protocol.contains("pci") || protocol.contains("apple fabric") || protocol.contains("nvme") {
        Some(StorageType::Nvme)
    } else {
        Some(StorageType::Ssd)
    }
}

/// Fallback for platforms with no detection - always returns None.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// macOS classification is pure string parsing, so it is testable without a Mac
/// for the parsing itself; the end-to-end probe only runs on macOS.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn classify_apple_fabric_ssd_as_nvme() {
        // Apple Silicon internal storage.
        let info =
            "   Protocol:                  Apple Fabric\n   Solid State:               Yes\n";
        assert_eq!(classify_diskutil_info(info), Some(StorageType::Nvme));
    }

    #[test]
    fn classify_pci_express_ssd_as_nvme() {
        // Intel Mac internal NVMe.
        let info = "   Protocol:                  PCI-Express\n   Solid State:               Yes\n";
        assert_eq!(classify_diskutil_info(info), Some(StorageType::Nvme));
    }

    #[test]
    fn classify_sata_ssd_as_ssd() {
        let info = "   Protocol:                  SATA\n   Solid State:               Yes\n";
        assert_eq!(classify_diskutil_info(info), Some(StorageType::Ssd));
    }

    #[test]
    fn classify_spinning_disk_as_hdd() {
        let info = "   Protocol:                  USB\n   Solid State:               No\n";
        assert_eq!(classify_diskutil_info(info), Some(StorageType::Hdd));
    }

    #[test]
    fn classify_missing_solid_state_field_is_none() {
        let info = "   Protocol:                  USB\n   Device Node:   /dev/disk9\n";
        assert_eq!(classify_diskutil_info(info), None);
    }

    #[test]
    fn parse_df_device_extracts_dev_node() {
        let df = "Filesystem    512-blocks      Used Available Capacity  Mounted on\n\
                  /dev/disk3s5  3906971480 100000000 50000000    67%  /System/Volumes/Data\n";
        assert_eq!(parse_df_device(df).as_deref(), Some("/dev/disk3s5"));
    }

    #[test]
    fn parse_df_device_rejects_missing_or_nondevice() {
        assert_eq!(parse_df_device(""), None);
        assert_eq!(parse_df_device("only a header line\n"), None);
        assert_eq!(
            parse_df_device("header\nmap auto_home 0 0 ... /home\n"),
            None
        );
    }

    #[test]
    fn detect_storage_type_for_root_does_not_panic() {
        // End-to-end exercise of df + diskutil on a path that always exists.
        // No specific type is asserted (it varies by host), only that the
        // pipeline runs without panicking and yields a sane Option.
        let _ = detect_storage_type(Path::new("/"));
    }
}
