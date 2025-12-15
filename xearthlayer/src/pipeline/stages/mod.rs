//! Pipeline stages for tile generation.
//!
//! Each stage is responsible for a single step in the tile generation process:
//!
//! 1. **Download** - Fetch 256 chunks from the imagery provider
//! 2. **Assembly** - Combine chunks into a 4096x4096 RGBA image
//! 3. **Encode** - Compress to DDS format with mipmaps
//! 4. **Cache** - Store in memory and disk caches

mod assembly;
mod cache;
mod download;
mod encode;

pub use assembly::assembly_stage;
pub use cache::{cache_stage, check_memory_cache};
pub use download::{download_stage, download_stage_with_limiter};
pub use encode::encode_stage;
