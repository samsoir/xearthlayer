//! Job implementations for the executor framework.
//!
//! This module provides job implementations that integrate with the generic
//! job executor from [`crate::executor`].
//!
//! # Jobs
//!
//! - [`DdsGenerateJob`] - Generates a single DDS texture from satellite imagery
//!
//! # Factories
//!
//! - [`DdsJobFactory`] - Trait for creating DDS generation jobs
//! - [`DefaultDdsJobFactory`] - Default factory implementation
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::jobs::{DdsGenerateJob, DefaultDdsJobFactory};
//! use xearthlayer::executor::Priority;
//!
//! // Single DDS generation
//! let job = DdsGenerateJob::new(tile, priority, provider, encoder, memory_cache, disk_cache, executor);
//! ```

mod cache_gc;
mod dds_generate;
mod factory;

pub use cache_gc::{CacheGcJob, DEFAULT_BATCH_SIZE, DEFAULT_MIN_AGE_SECS};
pub use dds_generate::DdsGenerateJob;
pub use factory::{DdsJobFactory, DefaultDdsJobFactory};
