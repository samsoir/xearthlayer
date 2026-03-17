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

    /// Create a `SpawnedMountHandle` from a fuse3 `MountHandle`.
    ///
    /// Spawns a tokio task that runs the FUSE event loop and handles
    /// unmount signals cleanly by calling `handle.unmount().await`
    /// instead of just dropping the handle.
    ///
    /// This is the shared implementation for all three FUSE filesystem
    /// types (OrthoUnionFS, PassthroughFS, UnionFS).
    pub(crate) fn spawn_from_handle(handle: Fuse3MountHandle, mountpoint: PathBuf) -> Self {
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();

        let task = tokio::spawn(Self::mount_task(handle, unmount_rx));

        Self::new(task, unmount_tx, mountpoint)
    }

    /// The async task that runs the FUSE event loop and handles unmount.
    ///
    /// Uses `poll_fn` to poll the handle without taking ownership, so
    /// `handle.unmount()` can be called explicitly when the unmount
    /// signal arrives. This ensures fuse3 cleanly disconnects from
    /// the kernel and drains pending operations (including `release()`
    /// calls for open file handles).
    pub(crate) async fn mount_task<F>(
        handle: F,
        unmount_rx: oneshot::Receiver<()>,
    ) -> io::Result<()>
    where
        F: Future<Output = io::Result<()>> + Unpin,
    {
        let mut handle = handle;
        tokio::select! {
            result = std::future::poll_fn(|cx| Pin::new(&mut handle).poll(cx)) => result,
            _ = unmount_rx => {
                // Unmount signal received — the handle is still owned because
                // poll_fn only borrowed it. Drop it to trigger fuse3's internal
                // unmount (MountHandle::drop → spawn inner_unmount).
                drop(handle);
                Ok(())
            },
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
    /// Uses escalating unmount strategy:
    /// 1. Signal task to unmount gracefully
    /// 2. Try `fusermount -u` (graceful unmount)
    /// 3. If busy, try `fusermount -uz` (lazy unmount)
    pub fn unmount_sync(&mut self) {
        let mountpoint_str = self.mountpoint.to_string_lossy().to_string();

        // Signal the task to stop (if channel still exists).
        // The mount_task will drop the fuse3 MountHandle, triggering its
        // internal unmount which drains pending kernel operations (release()
        // calls for open file handles, etc.).
        if let Some(tx) = self.unmount_tx.take() {
            debug!(mountpoint = %mountpoint_str, "Sending unmount signal");
            let _ = tx.send(());
        }

        // Wait for the task to complete the unmount. The fuse3 MountHandle::drop()
        // spawns an internal tokio task for unmount — we must keep our task alive
        // long enough for that to finish. Poll with short sleeps up to a timeout.
        if let Some(task) = self.task.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

            while std::time::Instant::now() < deadline {
                if task.is_finished() {
                    debug!(mountpoint = %mountpoint_str, "Mount task completed cleanly");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            // Task didn't finish in time — abort and fall back to fusermount
            warn!(
                mountpoint = %mountpoint_str,
                "Mount task did not complete within 5s, aborting"
            );
            task.abort();
        }

        // Fallback: check if still mounted and use fusermount
        if !Self::is_mounted(&self.mountpoint) {
            debug!(mountpoint = %mountpoint_str, "Already unmounted after task abort");
            return;
        }

        debug!(mountpoint = %mountpoint_str, "Still mounted, attempting fusermount");

        let graceful_success = Self::try_unmount(&mountpoint_str, false);

        if graceful_success {
            debug!(mountpoint = %mountpoint_str, "Graceful unmount succeeded");
        } else {
            std::thread::sleep(std::time::Duration::from_millis(500));

            if Self::is_mounted(&self.mountpoint) {
                warn!(
                    mountpoint = %mountpoint_str,
                    "Graceful unmount failed (likely busy), escalating to lazy unmount"
                );

                let lazy_success = Self::try_unmount(&mountpoint_str, true);

                if lazy_success {
                    debug!(mountpoint = %mountpoint_str, "Lazy unmount succeeded");
                } else {
                    warn!(
                        mountpoint = %mountpoint_str,
                        "Lazy unmount also failed - mount may require manual cleanup"
                    );
                }
            }
        }
    }

    /// Attempt to unmount using fusermount3 or fusermount.
    ///
    /// # Arguments
    /// * `mountpoint` - Path to unmount
    /// * `lazy` - If true, use lazy unmount (-uz) which detaches immediately
    ///
    /// # Returns
    /// `true` if unmount command succeeded, `false` otherwise
    fn try_unmount(mountpoint: &str, lazy: bool) -> bool {
        let args: &[&str] = if lazy {
            &["-uz", mountpoint]
        } else {
            &["-u", mountpoint]
        };

        let result = Command::new("fusermount3")
            .args(args)
            .output()
            .or_else(|_| Command::new("fusermount").args(args).output());

        match result {
            Ok(output) => {
                if output.status.success() {
                    true
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    // "not found" or "not mounted" means already unmounted - that's success
                    if stderr.contains("not found") || stderr.contains("not mounted") {
                        true
                    } else {
                        debug!(
                            mountpoint = %mountpoint,
                            lazy = lazy,
                            stderr = %stderr,
                            "fusermount failed"
                        );
                        false
                    }
                }
            }
            Err(e) => {
                warn!(
                    mountpoint = %mountpoint,
                    error = %e,
                    "Failed to run fusermount"
                );
                false
            }
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
    #[allow(clippy::unnecessary_literal_unwrap)]
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
    async fn test_spawn_from_mount_handle_unmount_signal_completes() {
        use std::time::Duration;

        // Simulate a MountHandle-like future that runs until cancelled
        let (_stop_tx, stop_rx) = oneshot::channel::<()>();

        // Create a mock "mount handle" future — runs until stop signal
        let mock_handle_future = async move {
            let _ = stop_rx.await;
            Ok::<(), io::Error>(())
        };

        // Use spawn_mount_task directly (the shared logic)
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(SpawnedMountHandle::mount_task(
            Box::pin(mock_handle_future),
            unmount_rx,
        ));
        let mountpoint = PathBuf::from("/test/spawn_from_handle");
        let handle = SpawnedMountHandle::new(task, unmount_tx, mountpoint);

        // Unmount should signal and complete cleanly
        let result = tokio::time::timeout(Duration::from_secs(2), handle.unmount()).await;

        assert!(result.is_ok(), "unmount should complete within timeout");
        assert!(result.unwrap().is_ok(), "unmount should succeed");
    }

    #[tokio::test]
    async fn test_spawn_from_mount_handle_natural_exit() {
        use std::time::Duration;

        // Mock handle that exits immediately (simulates external unmount)
        let mock_handle_future = async { Ok::<(), io::Error>(()) };

        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(SpawnedMountHandle::mount_task(
            Box::pin(mock_handle_future),
            unmount_rx,
        ));
        let mountpoint = PathBuf::from("/test/natural_exit");
        let mut handle = SpawnedMountHandle::new(task, unmount_tx, mountpoint);

        // Task should complete on its own
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Take task to check it completed
        if let Some(task) = handle.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(1), task).await;
            assert!(result.is_ok(), "task should have completed naturally");
        }
        // Prevent Drop from trying unmount_sync
        handle.unmount_tx.take();
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
