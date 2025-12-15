//! Async tile generation pipeline.
//!
//! This module implements a multi-stage asynchronous pipeline for generating
//! DDS textures from satellite imagery. The pipeline is designed to maximize
//! throughput by matching X-Plane's concurrent file request patterns.
//!
//! # Architecture
//!
//! ```text
//! FUSE Request → Job → Download Stage → Assembly Stage → Encode Stage → Cache Stage → Response
//! ```
//!
//! # Request Coalescing
//!
//! The pipeline includes automatic request coalescing via [`RequestCoalescer`].
//! When multiple requests for the same tile arrive simultaneously, only one
//! processing task runs - all waiters receive the same result. This prevents
//! duplicate work during X-Plane's burst loading patterns.
//!
//! # Key Components
//!
//! - [`Job`] - Represents a request for a DDS tile
//! - [`JobId`] - Unique identifier for tracking jobs through the pipeline
//! - [`JobResult`] - The result of processing a job
//! - [`RequestCoalescer`] - Coalesces duplicate requests for efficiency
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::pipeline::{Job, JobId, Priority};
//! use xearthlayer::coord::TileCoord;
//! use tokio::sync::oneshot;
//!
//! let (tx, rx) = oneshot::channel();
//! let job = Job::new(
//!     TileCoord { row: 100, col: 200, zoom: 16 },
//!     Priority::Normal,
//!     tx,
//! );
//!
//! // Submit job to pipeline...
//! let result = rx.await?;
//! ```

pub mod adapters;
mod coalesce;
mod context;
mod error;
mod executor;
mod http_limiter;
mod job;
mod processor;
mod runner;
pub mod stages;

pub use coalesce::{CoalescerStats, RequestCoalescer};
pub use context::{
    ChunkDownloadError, ChunkProvider, DiskCache, MemoryCache, PipelineConfig, PipelineContext,
    TextureEncodeError, TextureEncoderAsync,
};
pub use error::{ChunkFailure, ChunkResults, ChunkSuccess, JobError, StageError};
pub use executor::{BlockingExecutor, ConcurrentRunner, ExecutorError, Timer, TokioExecutor};
pub use http_limiter::HttpConcurrencyLimiter;
pub use job::{Job, JobId, JobResult, Priority};
pub use processor::{process_job, process_tile, process_tile_cancellable};
pub use runner::{create_dds_handler, create_dds_handler_with_metrics};
pub use stages::{
    assembly_stage, cache_stage, download_stage, download_stage_cancellable,
    download_stage_with_limiter, encode_stage,
};
