//! Where night recipes live, shared by the publisher and the runtime.
//!
//! OXO delivers one recipe per 1 degree tile at
//! `yOrtho4XP_Night/<block>/<tile>.oxn`; a package carries it at
//! `night/<block>/<tile>.oxn`. `night/` is outside XEL's root allowlist,
//! so X-Plane never sees it (XEL 0.3.0 and later).

use std::path::PathBuf;

/// The package folder for recipes.
pub const PACKAGE_DIR: &str = "night";

/// OXO's recipe tree beside the `zOrtho4XP_<tile>` folders.
pub const SOURCE_DIR: &str = "yOrtho4XP_Night";

/// The first four bytes of a version 1 recipe.
pub const MAGIC_V1: [u8; 4] = *b"OXN1";

/// Ortho4XP's tile name: `+37-123`.
pub fn tile_name(lat: i32, lon: i32) -> String {
    format!("{:+03}{:+04}", lat, lon)
}

/// Ortho4XP's 10 degree block: each coordinate floored to a multiple of 10.
pub fn block_name(lat: i32, lon: i32) -> String {
    tile_name(lat.div_euclid(10) * 10, lon.div_euclid(10) * 10)
}

/// `<block>/<tile>.oxn`, relative to `PACKAGE_DIR` or `SOURCE_DIR`.
pub fn recipe_relative_path(lat: i32, lon: i32) -> PathBuf {
    PathBuf::from(block_name(lat, lon)).join(format!("{}.oxn", tile_name(lat, lon)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_ortho4xp_including_negative_coordinates() {
        assert_eq!(tile_name(37, -123), "+37-123");
        assert_eq!(block_name(37, -123), "+30-130");
        assert_eq!(tile_name(-12, 45), "-12+045");
        assert_eq!(block_name(-12, 45), "-20+040");
        assert_eq!(tile_name(-1, -1), "-01-001");
        assert_eq!(block_name(-1, -1), "-10-010");
        assert_eq!(tile_name(0, 0), "+00+000");
        assert_eq!(block_name(0, 0), "+00+000");
    }

    #[test]
    fn relative_path_is_block_then_tile() {
        assert_eq!(
            recipe_relative_path(37, -123),
            PathBuf::from("+30-130/+37-123.oxn")
        );
    }

    #[test]
    fn magic_is_the_version_1_recipe_magic() {
        assert_eq!(&MAGIC_V1, b"OXN1");
        assert_eq!(PACKAGE_DIR, "night");
        assert_eq!(SOURCE_DIR, "yOrtho4XP_Night");
    }
}
