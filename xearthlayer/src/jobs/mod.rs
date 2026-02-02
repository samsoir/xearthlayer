//! Job implementations for the executor framework.
//!
//! This module provides job implementations that integrate with the generic
//! job executor from [`crate::executor`].
//!
//! # Jobs
//!
//! - [`DdsGenerateJob`] - Generates a single DDS texture from satellite imagery
//! - [`TilePrefetchJob`] - Prefetches all DDS tiles in a geographic area (hierarchical)
//!
//! # Factories
//!
//! - [`DdsJobFactory`] - Trait for creating DDS generation jobs
//! - [`DefaultDdsJobFactory`] - Default factory implementation
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::jobs::{DdsGenerateJob, TilePrefetchJob, DefaultDdsJobFactory};
//! use xearthlayer::executor::Priority;
//!
//! // Single DDS generation
//! let job = DdsGenerateJob::new(tile, priority, provider, encoder, memory_cache, disk_cache, executor);
//!
//! // Tile prefetch (spawns many DdsGenerateJob children)
//! let factory = DefaultDdsJobFactory::new(provider, encoder, memory_cache, disk_cache, executor);
//! let prefetch_job = TilePrefetchJob::new(lat, lon, zoom, radius, Arc::new(factory));
//! ```

mod cache_gc;
mod dds_generate;
mod factory;
mod tile_prefetch;

pub use cache_gc::{CacheGcJob, DEFAULT_BATCH_SIZE, DEFAULT_MIN_AGE_SECS};
pub use dds_generate::DdsGenerateJob;
pub use factory::{DdsJobFactory, DefaultDdsJobFactory};
pub use tile_prefetch::{TilePrefetchJob, DEFAULT_SUCCESS_THRESHOLD};
