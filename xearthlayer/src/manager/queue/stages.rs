//! The stages a queue drives, so the queue can be tested without a network
//! or a package store.

use std::path::Path;

use xearthlayer_package::{PackageMetadata, PackageType};

use crate::manager::cancel::CancelToken;
use crate::manager::download::DownloadProgressCallback;
use crate::manager::error::ManagerResult;
use crate::manager::installer::{InstallResult, InstallStage};

use super::ledger::Capacity;

/// The work behind each job, split where the queue hands over between its
/// threads.
pub trait Stages: Send + Sync + 'static {
    /// A package validated and sized, not yet downloaded.
    type Prepared: Send + 'static;
    /// A package downloaded and waiting to install.
    type Downloaded: Send + 'static;

    /// Where downloads are written.
    fn temp_dir(&self) -> &Path;

    /// Where installed packages live; swept for aside folders.
    fn packages_root(&self) -> &Path;

    /// Free space and device of the temp and packages filesystems, read
    /// when the queue goes busy.
    fn capacity(&self) -> ManagerResult<Capacity>;

    /// Validate and size; no bytes downloaded.
    fn prepare(&self, metadata: &PackageMetadata) -> ManagerResult<Self::Prepared>;

    /// The bytes `prepared` downloads.
    fn download_size(&self, prepared: &Self::Prepared) -> u64;

    /// Download `prepared`. Must return `ManagerError::Cancelled` promptly
    /// once `cancel` is set.
    fn download(
        &self,
        prepared: Self::Prepared,
        cancel: &CancelToken,
        progress: DownloadProgressCallback,
    ) -> ManagerResult<Self::Downloaded>;

    /// Install `downloaded`. Must call `ctx.begin_swap()` before touching
    /// the installed package and return `ManagerError::Cancelled` if it
    /// returns false; after that, the swap ignores cancellation. Should
    /// call `ctx.end_swap()` once the renames are done, so an abandoned
    /// queue does not wait for the cleanup after them.
    fn install(
        &self,
        downloaded: Self::Downloaded,
        ctx: &dyn InstallContext,
    ) -> ManagerResult<InstallResult>;

    /// Remove an installed package.
    fn remove(&self, region: &str, package_type: PackageType) -> ManagerResult<()>;
}

/// What an install sees of its queue.
pub trait InstallContext {
    /// Report reaching `stage`.
    fn stage(&self, stage: InstallStage);

    /// The job's cancellation token.
    fn cancel_token(&self) -> &CancelToken;

    /// Claim the swap. False if the job was cancelled first; once true,
    /// the swap runs to completion.
    fn begin_swap(&self) -> bool;

    /// The renames are done: what remains (deleting the replaced version)
    /// may be abandoned. Clears the queue's swap-in-progress flag.
    fn end_swap(&self);
}
