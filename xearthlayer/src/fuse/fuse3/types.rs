//! Types for the fuse3 filesystem implementation.

use fuse3::raw::MountHandle as Fuse3MountHandle;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Result type for fuse3 operations.
pub type Fuse3Result<T> = Result<T, Fuse3Error>;

/// Errors that can occur in the fuse3 filesystem.
#[derive(Debug, Error)]
pub enum Fuse3Error {
    /// I/O error during filesystem operations
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Mount operation failed
    #[error("Mount failed: {0}")]
    MountFailed(String),

    /// Invalid path
    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// Handle to a mounted fuse3 filesystem.
///
/// When dropped, the filesystem is automatically unmounted.
/// This is a wrapper around fuse3's MountHandle that provides
/// a cleaner API for XEarthLayer.
///
/// The handle can be awaited - it will resolve when the filesystem
/// is unmounted (e.g., via Ctrl+C or `fusermount -u`).
pub struct MountHandle {
    inner: Fuse3MountHandle,
}

impl MountHandle {
    /// Create a new mount handle from a fuse3 mount handle.
    pub(crate) fn new(inner: Fuse3MountHandle) -> Self {
        Self { inner }
    }

    /// Unmount the filesystem.
    ///
    /// This is called automatically when the handle is dropped,
    /// but can be called explicitly for more control.
    pub async fn unmount(self) -> io::Result<()> {
        self.inner.unmount().await
    }
}

/// Implement Future so the handle can be awaited.
/// Resolves when the filesystem is unmounted.
impl Future for MountHandle {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Delegate to the inner MountHandle's Future implementation
        Pin::new(&mut self.inner).poll(cx)
    }
}

/// Handle to a spawned fuse3 filesystem task.
///
/// This wraps a `JoinHandle` for the fuse3 mount task, allowing the mount
/// to run in the background while providing control over unmounting.
///
/// Unlike `MountHandle`, this can be safely stored and dropped outside
/// of an async context because the actual fuse3 handle is managed by
/// the spawned task.
pub struct SpawnedMountHandle {
    /// The spawned task handle
    task: Option<JoinHandle<io::Result<()>>>,
    /// Channel to signal unmount
    unmount_tx: Option<oneshot::Sender<()>>,
    /// Mountpoint for fallback unmount via fusermount
    mountpoint: PathBuf,
}

impl SpawnedMountHandle {
    /// Create a new spawned mount handle.
    pub(crate) fn new(
        task: JoinHandle<io::Result<()>>,
        unmount_tx: oneshot::Sender<()>,
        mountpoint: PathBuf,
    ) -> Self {
        Self {
            task: Some(task),
            unmount_tx: Some(unmount_tx),
            mountpoint,
        }
    }

    /// Unmount the filesystem asynchronously.
    ///
    /// Signals the mount task to unmount and waits for it to complete.
    pub async fn unmount(mut self) -> io::Result<()> {
        // Signal the task to unmount
        if let Some(tx) = self.unmount_tx.take() {
            let _ = tx.send(());
        }

        // Wait for the task to complete
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(result) => result,
                Err(e) => Err(io::Error::other(format!("Mount task panicked: {}", e))),
            }
        } else {
            Ok(())
        }
    }

    /// Unmount the filesystem synchronously using fusermount.
    ///
    /// This is a fallback for when we can't use async unmount.
    pub fn unmount_sync(&mut self) {
        // Signal the task to stop (if channel still exists)
        if let Some(tx) = self.unmount_tx.take() {
            let _ = tx.send(());
        }

        // Check if mount is still active before trying fusermount
        let mountpoint_str = self.mountpoint.to_string_lossy();
        if !Self::is_mounted(&self.mountpoint) {
            debug!(mountpoint = %mountpoint_str, "Already unmounted, skipping fusermount");
            // Still cancel the task if it exists
            if let Some(task) = self.task.take() {
                task.abort();
            }
            return;
        }

        debug!(mountpoint = %mountpoint_str, "Unmounting via fusermount");

        // Try fusermount3 first (fuse3), then fall back to fusermount (fuse2)
        let result = Command::new("fusermount3")
            .args(["-u", &mountpoint_str])
            .output()
            .or_else(|_| {
                Command::new("fusermount")
                    .args(["-u", &mountpoint_str])
                    .output()
            });

        match result {
            Ok(output) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    // Don't warn for expected "not found" or "not mounted" errors
                    if !stderr.contains("not found") && !stderr.contains("not mounted") {
                        warn!(
                            mountpoint = %mountpoint_str,
                            stderr = %stderr,
                            "fusermount -u failed"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    mountpoint = %mountpoint_str,
                    error = %e,
                    "Failed to run fusermount"
                );
            }
        }

        // Cancel the task
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// Check if a path is currently mounted.
    fn is_mounted(path: &Path) -> bool {
        // Read /proc/mounts to check if the path is mounted
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            let path_str = path.to_string_lossy();
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == path_str {
                    return true;
                }
            }
        }
        false
    }
}

impl Drop for SpawnedMountHandle {
    fn drop(&mut self) {
        // If we're being dropped without explicit unmount, try fusermount
        if self.task.is_some() {
            self.unmount_sync();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse3_error_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: Fuse3Error = io_err.into();
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_fuse3_error_mount_failed() {
        let err = Fuse3Error::MountFailed("permission denied".to_string());
        assert!(err.to_string().contains("Mount failed"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn test_fuse3_error_invalid_path() {
        let err = Fuse3Error::InvalidPath("/invalid/path".to_string());
        assert!(err.to_string().contains("Invalid path"));
        assert!(err.to_string().contains("/invalid/path"));
    }

    #[test]
    fn test_fuse3_result_type() {
        // Test that Fuse3Result works as expected
        let ok_result: Fuse3Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        let err_result: Fuse3Result<i32> = Err(Fuse3Error::InvalidPath("test".to_string()));
        assert!(err_result.is_err());
    }

    #[test]
    fn test_spawned_mount_handle_creation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        rt.block_on(async {
            let (tx, _rx) = oneshot::channel();
            let task = tokio::spawn(async { Ok(()) });
            let mountpoint = PathBuf::from("/test/mount");

            let handle = SpawnedMountHandle::new(task, tx, mountpoint.clone());

            // Handle should have task and channel
            assert!(handle.task.is_some());
            assert!(handle.unmount_tx.is_some());
            assert_eq!(handle.mountpoint, mountpoint);
        });
    }

    #[tokio::test]
    async fn test_spawned_mount_handle_unmount_async() {
        let (tx, rx) = oneshot::channel();

        // Create a task that waits for unmount signal
        let task = tokio::spawn(async move {
            let _ = rx.await;
            Ok(())
        });

        let mountpoint = PathBuf::from("/test/mount");
        let handle = SpawnedMountHandle::new(task, tx, mountpoint);

        // Unmount should complete successfully
        let result = handle.unmount().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_spawned_mount_handle_unmount_task_already_done() {
        let (tx, _rx) = oneshot::channel();

        // Create a task that completes immediately
        let task = tokio::spawn(async { Ok(()) });

        // Wait for task to complete
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mountpoint = PathBuf::from("/test/mount");
        let mut handle = SpawnedMountHandle::new(task, tx, mountpoint);

        // Take the unmount_tx before calling unmount
        handle.unmount_tx.take();
        handle.task.take();

        // Unmount with no task should succeed
        let result = handle.unmount().await;
        assert!(result.is_ok());
    }
}
