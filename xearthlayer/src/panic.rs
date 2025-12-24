//! Panic handler for graceful cleanup and state logging.
//!
//! This module provides a custom panic hook that:
//! - Logs useful state information (telemetry, mount points)
//! - Attempts to unmount FUSE filesystems to prevent X-Plane SIGBUS
//! - Preserves the original panic behavior after cleanup
//!
//! The panic handler uses a global mount registry since panic hooks must be
//! `'static`. Mount points are registered when created and unregistered on
//! normal unmount.

use std::io::Write;
use std::panic::{self, PanicHookInfo};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::telemetry::TelemetrySnapshot;

/// Global mount point registry for panic cleanup.
static MOUNT_REGISTRY: OnceLock<Mutex<MountRegistry>> = OnceLock::new();

/// Registry of active mount points for panic cleanup.
#[derive(Default)]
struct MountRegistry {
    /// Active mount points that need cleanup on panic.
    mount_points: Vec<PathBuf>,
    /// Callback to capture telemetry snapshot.
    telemetry_callback: Option<Box<dyn Fn() -> TelemetrySnapshot + Send + Sync>>,
}

/// Initialize the panic handler.
///
/// This should be called once early in the application startup. It sets up
/// a custom panic hook that will:
///
/// 1. Log the panic location and message
/// 2. Capture and log telemetry state
/// 3. Attempt to unmount all registered FUSE mount points
/// 4. Chain to the default panic handler
///
/// # Safety
///
/// This function should only be called once. Subsequent calls will be ignored.
pub fn init() {
    // Initialize the global registry
    let _ = MOUNT_REGISTRY.get_or_init(|| Mutex::new(MountRegistry::default()));

    // Store the original panic hook
    let original_hook = panic::take_hook();

    // Set our custom panic hook
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        // First, run our cleanup
        handle_panic(info);

        // Then chain to the original hook
        original_hook(info);
    }));
}

/// Register a mount point for panic cleanup.
///
/// When a panic occurs, registered mount points will be unmounted using
/// `fusermount -u` to prevent the FUSE filesystem from becoming stuck
/// and causing SIGBUS in X-Plane.
pub fn register_mount(path: PathBuf) {
    if let Some(registry) = MOUNT_REGISTRY.get() {
        if let Ok(mut guard) = registry.lock() {
            guard.mount_points.push(path);
        }
    }
}

/// Unregister a mount point after normal unmount.
///
/// This removes the mount point from the registry so it won't be
/// attempted during panic cleanup.
pub fn unregister_mount(path: &PathBuf) {
    if let Some(registry) = MOUNT_REGISTRY.get() {
        if let Ok(mut guard) = registry.lock() {
            guard.mount_points.retain(|p| p != path);
        }
    }
}

/// Set the telemetry callback for state capture on panic.
///
/// This callback will be invoked during panic handling to capture
/// the current telemetry state for logging.
pub fn set_telemetry_callback<F>(callback: F)
where
    F: Fn() -> TelemetrySnapshot + Send + Sync + 'static,
{
    if let Some(registry) = MOUNT_REGISTRY.get() {
        if let Ok(mut guard) = registry.lock() {
            guard.telemetry_callback = Some(Box::new(callback));
        }
    }
}

/// Handle a panic by logging state and cleaning up mounts.
fn handle_panic(info: &PanicHookInfo<'_>) {
    // Try to write to stderr directly since logging may be broken
    let mut stderr = std::io::stderr().lock();

    // Write panic header
    let _ = writeln!(stderr, "\n");
    let _ = writeln!(
        stderr,
        "╔══════════════════════════════════════════════════════════════════╗"
    );
    let _ = writeln!(
        stderr,
        "║                    XEARTHLAYER PANIC HANDLER                     ║"
    );
    let _ = writeln!(
        stderr,
        "╚══════════════════════════════════════════════════════════════════╝"
    );
    let _ = writeln!(stderr);

    // Write panic location and message
    let _ = writeln!(stderr, "━━━ Panic Information ━━━");
    if let Some(location) = info.location() {
        let _ = writeln!(
            stderr,
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        let _ = writeln!(stderr, "Message: {}", message);
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        let _ = writeln!(stderr, "Message: {}", message);
    }
    let _ = writeln!(stderr);

    // Try to capture telemetry
    if let Some(registry) = MOUNT_REGISTRY.get() {
        if let Ok(guard) = registry.lock() {
            if let Some(ref callback) = guard.telemetry_callback {
                let _ = writeln!(stderr, "━━━ Telemetry State ━━━");
                let snapshot = callback();
                let _ = writeln!(stderr, "Uptime:           {:?}", snapshot.uptime);
                let _ = writeln!(stderr, "Jobs active:      {}", snapshot.jobs_active);
                let _ = writeln!(stderr, "Jobs completed:   {}", snapshot.jobs_completed);
                let _ = writeln!(stderr, "Jobs failed:      {}", snapshot.jobs_failed);
                let _ = writeln!(stderr, "Jobs timed out:   {}", snapshot.jobs_timed_out);
                let _ = writeln!(stderr, "Downloads active: {}", snapshot.downloads_active);
                let _ = writeln!(stderr, "Encodes active:   {}", snapshot.encodes_active);
                let _ = writeln!(
                    stderr,
                    "Memory cache:     {} bytes",
                    snapshot.memory_cache_size_bytes
                );
                let _ = writeln!(
                    stderr,
                    "Disk cache:       {} bytes",
                    snapshot.disk_cache_size_bytes
                );
                let _ = writeln!(stderr);
            }
        }
    }

    // Attempt mount cleanup
    let _ = writeln!(stderr, "━━━ Mount Cleanup ━━━");
    if let Some(registry) = MOUNT_REGISTRY.get() {
        if let Ok(guard) = registry.lock() {
            if guard.mount_points.is_empty() {
                let _ = writeln!(stderr, "No mount points registered for cleanup.");
            } else {
                let _ = writeln!(
                    stderr,
                    "Attempting to unmount {} FUSE mount(s)...",
                    guard.mount_points.len()
                );

                for mount_point in &guard.mount_points {
                    let _ = write!(stderr, "  Unmounting {}... ", mount_point.display());

                    // Try fusermount -u first (graceful unmount)
                    let result = Command::new("fusermount")
                        .arg("-u")
                        .arg(mount_point)
                        .output();

                    match result {
                        Ok(output) if output.status.success() => {
                            let _ = writeln!(stderr, "OK");
                        }
                        Ok(_) => {
                            // Try fusermount3 -u as fallback
                            let result3 = Command::new("fusermount3")
                                .arg("-u")
                                .arg(mount_point)
                                .output();

                            match result3 {
                                Ok(output) if output.status.success() => {
                                    let _ = writeln!(stderr, "OK (fusermount3)");
                                }
                                _ => {
                                    // Last resort: lazy unmount
                                    let lazy_result = Command::new("fusermount")
                                        .arg("-uz")
                                        .arg(mount_point)
                                        .output();

                                    match lazy_result {
                                        Ok(output) if output.status.success() => {
                                            let _ = writeln!(stderr, "OK (lazy)");
                                        }
                                        _ => {
                                            let _ = writeln!(stderr, "FAILED");
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(stderr, "ERROR: {}", e);
                        }
                    }
                }
            }
        }
    }

    let _ = writeln!(stderr);
    let _ = writeln!(stderr, "━━━ End of XEarthLayer Panic Handler ━━━");
    let _ = writeln!(stderr);

    // Ensure output is flushed
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_registry_register_unregister() {
        // Initialize registry
        init();

        let path1 = PathBuf::from("/tmp/test_mount_1");
        let path2 = PathBuf::from("/tmp/test_mount_2");

        // Register mounts
        register_mount(path1.clone());
        register_mount(path2.clone());

        // Verify they're registered
        if let Some(registry) = MOUNT_REGISTRY.get() {
            let guard = registry.lock().unwrap();
            assert!(guard.mount_points.contains(&path1));
            assert!(guard.mount_points.contains(&path2));
        }

        // Unregister one
        unregister_mount(&path1);

        // Verify it's removed
        if let Some(registry) = MOUNT_REGISTRY.get() {
            let guard = registry.lock().unwrap();
            assert!(!guard.mount_points.contains(&path1));
            assert!(guard.mount_points.contains(&path2));
        }
    }
}
