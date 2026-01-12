//! Adapters for connecting executor traits to existing implementations.
//!
//! This module provides adapter types that bridge the executor's abstract traits
//! to the concrete implementations in other modules. This follows the Adapter
//! pattern, allowing the executor to work with existing code without modification.
//!
//! # Adapters
//!
//! - [`ProviderAdapter`] - Adapts sync `Provider` to `ChunkProvider` (legacy)
//! - [`AsyncProviderAdapter`] - Adapts `AsyncProvider` to `ChunkProvider` (preferred)
//! - [`TextureEncoderAdapter`] - Adapts `TextureEncoder` to `TextureEncoderAsync`
//! - [`MemoryCacheAdapter`] - Adapts `cache::MemoryCache` to executor `MemoryCache`
//! - [`DiskCacheAdapter`] - Adapts disk chunk caching to executor `DiskCache`
//! - [`NullDiskCache`] - No-op disk cache for testing

mod disk_cache;
mod memory_cache;
mod provider;
mod texture_encoder;

pub use disk_cache::{DiskCacheAdapter, NullDiskCache};
pub use memory_cache::MemoryCacheAdapter;
pub use provider::{AsyncProviderAdapter, ProviderAdapter};
pub use texture_encoder::TextureEncoderAdapter;
