//! Night lighting: lit textures generated from OXO night recipes.
//!
//! See `docs/dev/night-lighting-design.md`. Phase 1 reads recipes and
//! serves a minimal black texture for every lit name. Phase 2a answers lit
//! `stat`/`open` with the real file size (footprints from recipes, lit art
//! from the user's Laminar installation, texture setting from the preferences)
//! but still serves black pixels. Phase 2b generates the light, in the
//! `generator` module behind the `LitGenerator` trait.

pub(crate) mod admission;
pub mod disk;
pub mod footprint;
pub mod generator;
pub mod laminar;
pub mod layout;
pub(crate) mod math;
pub(crate) mod payloads;
pub mod prefs;
pub(crate) mod pregen;
pub mod recipe;
pub(crate) mod retry;
pub mod roads;
pub mod service;
pub(crate) mod stats;
pub mod store;
#[cfg(test)]
pub(crate) mod test_log;

pub use disk::{
    check_entry, decode_entry_unsized, encode_entry, entry_header, footprint_fingerprint,
    lit_cache_key, session_fingerprint, EntryReject, LitFingerprint,
};
pub use footprint::Footprint;
pub use generator::{
    CpuLitGenerator, LedModel, LedRegion, LitGenerator, LitRequest, LIT_GENERATOR_VERSION,
};
pub use laminar::{
    ArtDoubt, ArtDoubtKind, ArtFiles, ArtRecord, DiskArtFiles, LaminarArt, LitPattern, LitTerrain,
};
pub use layout::{minimal_black_dds, LitLayout, LitParts, PayloadPart};
pub use prefs::{
    FixedTextureSetting, StartupTextureSetting, TextureSettingSource, FULL_TEXTURE_SETTING,
};
pub use recipe::{parse_recipe, BrightnessScale, Grid, Recipe, RecipeError, SUPPORTED_MAJOR};
pub use service::{
    BlackLitSource, LitDiskPermits, LitTextureSource, NightLitSource, DEFAULT_NIGHT_BRIGHTNESS,
};
pub use store::{block_name, recipe_path, tile_name, FileIdentity, RecipeStore};

/// What night lighting is actually doing this session, as decided when the
/// ortho mount was built. Differs from `night.enabled` when the X-Plane
/// installation behind the scenery path cannot be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NightLighting {
    /// `night.enabled` is off: every lit texture is a minimal black DDS.
    #[default]
    Off,
    /// Lit textures are generated from recipes and the user's Laminar art.
    Lit,
    /// Enabled, but the X-Plane installation was not found, so lit textures
    /// stay black.
    Dark,
}
