//! Road lights for lit textures: the road network the sim presents, read
//! from the DSFs X-Plane loads for a 1 degree tile, drawn as a glow.
//! See `docs/dev/night-lighting-design.md` (Road lights).

pub mod class;
pub mod model;

pub use class::{is_lit_subtype, RoadClass};
pub use model::{ClassParams, LampParams, RoadModel, ROAD_MODEL_VERSION};

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
    fn dsf_names_parse_to_their_tile() {
        assert_eq!(dsf_tile_of_name("+46+006.dsf"), Some((46, 6)));
        assert_eq!(dsf_tile_of_name("-34-071.dsf"), Some((-34, -71)));
        assert_eq!(dsf_tile_of_name("+40-111.DSF"), Some((40, -111)));
        assert_eq!(dsf_tile_of_name("+46+006.ter"), None);
        assert_eq!(dsf_tile_of_name("46006.dsf"), None);
        assert_eq!(dsf_tile_of_name("+91+006.dsf"), None);
    }
}
