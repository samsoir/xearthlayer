//! Road lights for lit textures: the road network the sim presents, read
//! from the DSFs X-Plane loads for a 1 degree tile, drawn as a glow.
//! See `docs/dev/night-lighting-design.md` (Road lights).

pub mod class;
pub mod codec;
pub mod dsf;
pub mod exclusion;
pub mod field;
pub mod merge;
pub mod model;
pub mod packs;
pub mod service;
pub mod stats;

use sha2::{Digest, Sha256};

pub use class::{is_lit_subtype, RoadClass};
pub use model::{ClassParams, LampParams, RoadModel, ROAD_MODEL_VERSION};

/// Bump when the same DSFs and the same `RoadModel` can produce different
/// road light: the reader, the class table, the merge and source rules,
/// quantisation, the kernel. Hashed with the model's digest into the lit
/// session fingerprint ([`roads_digest`]), so every lit payload on disk
/// misses after a bump. Road disk entries have their own version
/// (`service::ROAD_EXTRACT_VERSION`).
pub const ROADS_VERSION: u32 = 2;

/// The road part of the lit session fingerprint: [`ROADS_VERSION`] and
/// every parameter of `model`.
pub fn roads_digest(model: &RoadModel) -> [u8; 32] {
    roads_digest_at(ROADS_VERSION, model)
}

fn roads_digest_at(version: u32, model: &RoadModel) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(version.to_le_bytes());
    h.update(model.digest());
    h.finalize().into()
}

/// The 1 degree tile of a DSF file name such as `+46+006.dsf`.
pub fn dsf_tile_of_name(name: &str) -> Option<(i32, i32)> {
    let stem = name
        .strip_suffix(".dsf")
        .or_else(|| name.strip_suffix(".DSF"))?;
    let b = stem.as_bytes();
    if b.len() != 7 || !matches!(b[0], b'+' | b'-') || !matches!(b[3], b'+' | b'-') {
        return None;
    }
    let lat: i32 = stem[0..3].parse().ok()?;
    let lon: i32 = stem[3..7].parse().ok()?;
    ((-90..90).contains(&lat) && (-180..180).contains(&lon)).then_some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_roads_version_and_the_model_move_the_session_fingerprint() {
        let m = RoadModel::DEFAULT;
        let session = |roads| crate::night::session_fingerprint(1.0, [0; 32], roads);
        let base = session(roads_digest_at(1, &m));
        assert_eq!(roads_digest(&m), roads_digest_at(ROADS_VERSION, &m));
        assert_ne!(base, session(roads_digest_at(2, &m)), "another version");
        let mut other = m;
        other.peak += 0.01;
        assert_ne!(base, session(roads_digest_at(1, &other)), "another model");
        assert_ne!(roads_digest(&m), m.digest(), "not the model alone");
    }

    #[test]
    fn dsf_names_parse_to_their_tile() {
        assert_eq!(dsf_tile_of_name("+46+006.dsf"), Some((46, 6)));
        assert_eq!(dsf_tile_of_name("-34-071.dsf"), Some((-34, -71)));
        assert_eq!(dsf_tile_of_name("+40-111.DSF"), Some((40, -111)));
        assert_eq!(dsf_tile_of_name("+46+006.ter"), None);
        assert_eq!(dsf_tile_of_name("46006.dsf"), None);
        assert_eq!(dsf_tile_of_name("+91+006.dsf"), None);
    }
}
