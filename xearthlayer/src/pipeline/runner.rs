//! Pipeline runner for processing DDS requests.
//!
//! This module provides the connection between the FUSE filesystem's
//! `DdsHandler` callback and the async pipeline processor.

use crate::fuse::{DdsHandler, DdsRequest, DdsResponse};
use crate::pipeline::{
    process_tile, BlockingExecutor, ChunkProvider, DiskCache, MemoryCache, PipelineConfig,
    TextureEncoderAsync,
};
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{error, instrument};

/// Creates a `DdsHandler` that processes requests through the async pipeline.
///
/// This function creates the bridge between FUSE's synchronous callback model
/// and the async pipeline. Each DDS request spawns a task on the Tokio runtime
/// that processes the tile and sends the result back via oneshot channel.
///
/// # Type Parameters
///
/// * `P` - Chunk provider (downloads satellite imagery)
/// * `E` - Texture encoder (DDS compression)
/// * `M` - Memory cache (tile-level)
/// * `D` - Disk cache (chunk-level)
/// * `X` - Blocking executor
///
/// # Arguments
///
/// * `provider` - The chunk provider
/// * `encoder` - The texture encoder
/// * `memory_cache` - Memory cache for tiles
/// * `disk_cache` - Disk cache for chunks
/// * `executor` - Executor for blocking operations
/// * `config` - Pipeline configuration
/// * `runtime_handle` - Handle to the Tokio runtime
///
/// # Example
///
/// ```ignore
/// use xearthlayer::pipeline::{create_dds_handler, TokioExecutor, PipelineConfig};
/// use xearthlayer::pipeline::adapters::*;
///
/// let handler = create_dds_handler(
///     Arc::new(provider_adapter),
///     Arc::new(encoder_adapter),
///     Arc::new(memory_cache_adapter),
///     Arc::new(disk_cache_adapter),
///     Arc::new(TokioExecutor::new()),
///     PipelineConfig::default(),
///     runtime.handle().clone(),
/// );
///
/// // Use handler with AsyncPassthroughFS
/// let fs = AsyncPassthroughFS::new(source_dir, handler, ...);
/// ```
pub fn create_dds_handler<P, E, M, D, X>(
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    config: PipelineConfig,
    runtime_handle: Handle,
) -> DdsHandler
where
    P: ChunkProvider + 'static,
    E: TextureEncoderAsync + 'static,
    M: MemoryCache + 'static,
    D: DiskCache + 'static,
    X: BlockingExecutor + 'static,
{
    Arc::new(move |request: DdsRequest| {
        let provider = Arc::clone(&provider);
        let encoder = Arc::clone(&encoder);
        let memory_cache = Arc::clone(&memory_cache);
        let disk_cache = Arc::clone(&disk_cache);
        let executor = Arc::clone(&executor);
        let config = config.clone();

        // Spawn the processing task on the Tokio runtime
        runtime_handle.spawn(async move {
            process_dds_request(
                request,
                provider,
                encoder,
                memory_cache,
                disk_cache,
                executor,
                config,
            )
            .await;
        });
    })
}

/// Process a single DDS request through the pipeline.
#[instrument(skip_all, fields(job_id = %request.job_id, tile = ?request.tile))]
async fn process_dds_request<P, E, M, D, X>(
    request: DdsRequest,
    provider: Arc<P>,
    encoder: Arc<E>,
    memory_cache: Arc<M>,
    disk_cache: Arc<D>,
    executor: Arc<X>,
    config: PipelineConfig,
) where
    P: ChunkProvider,
    E: TextureEncoderAsync,
    M: MemoryCache,
    D: DiskCache,
    X: BlockingExecutor,
{
    let result = process_tile(
        request.job_id,
        request.tile,
        provider,
        encoder,
        memory_cache,
        disk_cache,
        executor.as_ref(),
        &config,
    )
    .await;

    let response = match result {
        Ok(job_result) => DdsResponse::from(job_result),
        Err(e) => {
            error!(error = %e, "Pipeline processing failed");
            // Return placeholder on error
            DdsResponse {
                data: crate::fuse::generate_default_placeholder().unwrap_or_default(),
                cache_hit: false,
                duration: std::time::Duration::ZERO,
            }
        }
    };

    // Send response back to FUSE handler
    // Ignore error if receiver dropped (FUSE handler timed out)
    let _ = request.result_tx.send(response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::pipeline::{ChunkDownloadError, JobId, TextureEncodeError, TokioExecutor};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    // Mock implementations for testing

    struct MockProvider;

    impl ChunkProvider for MockProvider {
        async fn download_chunk(
            &self,
            _row: u32,
            _col: u32,
            _zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            // Return a minimal valid PNG
            Ok(create_test_png())
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    struct MockEncoder;

    impl TextureEncoderAsync for MockEncoder {
        fn encode(&self, _image: &image::RgbaImage) -> Result<Vec<u8>, TextureEncodeError> {
            Ok(vec![0xDD, 0x53, 0x20, 0x00])
        }

        fn expected_size(&self, _width: u32, _height: u32) -> usize {
            4
        }
    }

    struct MockMemoryCache {
        data: Mutex<HashMap<(u32, u32, u8), Vec<u8>>>,
    }

    impl MockMemoryCache {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    impl MemoryCache for MockMemoryCache {
        fn get(&self, row: u32, col: u32, zoom: u8) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(&(row, col, zoom)).cloned()
        }

        fn put(&self, row: u32, col: u32, zoom: u8, data: Vec<u8>) {
            self.data.lock().unwrap().insert((row, col, zoom), data);
        }

        fn size_bytes(&self) -> usize {
            0
        }

        fn entry_count(&self) -> usize {
            self.data.lock().unwrap().len()
        }
    }

    struct NullDiskCache;

    impl DiskCache for NullDiskCache {
        async fn get(
            &self,
            _tile_row: u32,
            _tile_col: u32,
            _zoom: u8,
            _chunk_row: u8,
            _chunk_col: u8,
        ) -> Option<Vec<u8>> {
            None
        }

        async fn put(
            &self,
            _tile_row: u32,
            _tile_col: u32,
            _zoom: u8,
            _chunk_row: u8,
            _chunk_col: u8,
            _data: Vec<u8>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    fn create_test_png() -> Vec<u8> {
        use image::{Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(256, 256, Rgba([255, 0, 0, 255]));
        let mut buffer = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buffer
    }

    #[tokio::test]
    async fn test_create_dds_handler() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let handler = create_dds_handler(
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
            Handle::current(),
        );

        // Create a request
        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
        };

        // Call the handler
        handler(request);

        // Wait for response with timeout
        let response = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .expect("timeout")
            .expect("channel closed");

        assert!(!response.data.is_empty());
        assert!(!response.cache_hit);
    }

    #[tokio::test]
    async fn test_process_dds_request_success() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
        };

        process_dds_request(
            request,
            provider,
            encoder,
            memory_cache,
            disk_cache,
            executor,
            config,
        )
        .await;

        let response = rx.await.expect("channel closed");
        assert!(!response.data.is_empty());
    }

    #[tokio::test]
    async fn test_memory_cache_populated_after_request() {
        let provider = Arc::new(MockProvider);
        let encoder = Arc::new(MockEncoder);
        let memory_cache = Arc::new(MockMemoryCache::new());
        let disk_cache = Arc::new(NullDiskCache);
        let executor = Arc::new(TokioExecutor::new());
        let config = PipelineConfig {
            max_retries: 1,
            ..Default::default()
        };

        // Initially empty
        assert_eq!(memory_cache.entry_count(), 0);

        let (tx, rx) = oneshot::channel();
        let request = DdsRequest {
            job_id: JobId::new(),
            tile: TileCoord {
                row: 100,
                col: 200,
                zoom: 16,
            },
            result_tx: tx,
        };

        process_dds_request(
            request,
            provider,
            encoder,
            Arc::clone(&memory_cache),
            disk_cache,
            executor,
            config,
        )
        .await;

        let _ = rx.await;

        // Cache should now have an entry
        assert_eq!(memory_cache.entry_count(), 1);
        assert!(memory_cache.get(100, 200, 16).is_some());
    }
}
