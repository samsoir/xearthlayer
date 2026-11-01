//! First task of a prefetch-lane tile job: skip the work if the tile is
//! already cached when the job is admitted (#296).
//!
//! While the prefetch lane is paused, FUSE demand generates tiles prefetch
//! had queued. Re-checking at admission turns those queued copies into
//! no-ops instead of duplicate downloads and encodes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::coord::TileCoord;
use crate::executor::{
    DdsDiskCache, MemoryCache, ResourceType, Task, TaskContext, TaskOutput, TaskResult,
};

pub const CACHED_TILE_PROBE_TASK: &str = "CachedTileProbe";
pub const TILE_CACHED_KEY: &str = "tile_cached";

/// True when an earlier probe in this job found the tile cached.
pub fn tile_already_cached(ctx: &TaskContext) -> bool {
    ctx.get_output::<bool>(CACHED_TILE_PROBE_TASK, TILE_CACHED_KEY)
        .unwrap_or(false)
}

/// Checks the memory and DDS disk caches for the tile.
///
/// On a hit it records [`TILE_CACHED_KEY`] so the download and build tasks
/// that follow return at once. A miss is a plain success.
pub struct CachedTileProbeTask<M, DD> {
    tile: TileCoord,
    memory_cache: Arc<M>,
    dds_disk_cache: Arc<DD>,
}

impl<M: MemoryCache, DD: DdsDiskCache> CachedTileProbeTask<M, DD> {
    pub fn new(tile: TileCoord, memory_cache: Arc<M>, dds_disk_cache: Arc<DD>) -> Self {
        Self {
            tile,
            memory_cache,
            dds_disk_cache,
        }
    }
}

impl<M: MemoryCache, DD: DdsDiskCache> Task for CachedTileProbeTask<M, DD> {
    fn name(&self) -> &str {
        CACHED_TILE_PROBE_TASK
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::DiskIO
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            let TileCoord { row, col, zoom } = self.tile;
            let cached = self.memory_cache.get(row, col, zoom).await.is_some()
                || self.dds_disk_cache.contains(row, col, zoom).await;
            if !cached {
                return TaskResult::Success;
            }
            if let Some(metrics) = ctx.metrics_clone() {
                metrics.prefetch_skipped_cached();
            }
            let mut output = TaskOutput::new();
            output.set(TILE_CACHED_KEY, true);
            TaskResult::SuccessWithOutput(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{JobId, TaskOutput};
    use bytes::Bytes;
    use std::future::Future;
    use tokio_util::sync::CancellationToken;

    struct FixedMemoryCache {
        hit: bool,
    }
    impl MemoryCache for FixedMemoryCache {
        fn get(&self, _r: u32, _c: u32, _z: u8) -> impl Future<Output = Option<Bytes>> + Send {
            let hit = self.hit;
            async move { hit.then(|| Bytes::from_static(b"x")) }
        }
        fn put(&self, _r: u32, _c: u32, _z: u8, _d: Bytes) -> impl Future<Output = ()> + Send {
            async {}
        }
        fn size_bytes(&self) -> usize {
            0
        }
        fn entry_count(&self) -> usize {
            0
        }
    }

    struct FixedDdsDisk {
        contains: bool,
    }
    impl DdsDiskCache for FixedDdsDisk {
        fn get(&self, _r: u32, _c: u32, _z: u8) -> impl Future<Output = Option<Bytes>> + Send {
            async { None }
        }
        fn put(&self, _r: u32, _c: u32, _z: u8, _d: Bytes) -> impl Future<Output = ()> + Send {
            async {}
        }
        fn contains(&self, _r: u32, _c: u32, _z: u8) -> impl Future<Output = bool> + Send {
            let contains = self.contains;
            async move { contains }
        }
    }

    fn tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 15,
        }
    }

    fn ctx() -> TaskContext {
        TaskContext::new(JobId::new("probe"), CancellationToken::new())
    }

    #[tokio::test]
    async fn memory_hit_records_tile_cached() {
        let task = CachedTileProbeTask::new(
            tile(),
            Arc::new(FixedMemoryCache { hit: true }),
            Arc::new(FixedDdsDisk { contains: false }),
        );
        let result = task.execute(&mut ctx()).await;
        assert!(
            matches!(result, TaskResult::SuccessWithOutput(ref o) if o.contains(TILE_CACHED_KEY))
        );
    }

    #[tokio::test]
    async fn dds_disk_hit_records_tile_cached() {
        let task = CachedTileProbeTask::new(
            tile(),
            Arc::new(FixedMemoryCache { hit: false }),
            Arc::new(FixedDdsDisk { contains: true }),
        );
        let result = task.execute(&mut ctx()).await;
        assert!(
            matches!(result, TaskResult::SuccessWithOutput(ref o) if o.contains(TILE_CACHED_KEY))
        );
    }

    #[tokio::test]
    async fn miss_is_plain_success() {
        let task = CachedTileProbeTask::new(
            tile(),
            Arc::new(FixedMemoryCache { hit: false }),
            Arc::new(FixedDdsDisk { contains: false }),
        );
        assert!(matches!(
            task.execute(&mut ctx()).await,
            TaskResult::Success
        ));
    }

    #[test]
    fn tile_already_cached_reads_the_probe_output() {
        let ctx = ctx();
        assert!(!tile_already_cached(&ctx));
        let mut out = TaskOutput::new();
        out.set(TILE_CACHED_KEY, true);
        ctx.add_task_output(CACHED_TILE_PROBE_TASK.to_string(), out);
        assert!(tile_already_cached(&ctx));
    }
}
