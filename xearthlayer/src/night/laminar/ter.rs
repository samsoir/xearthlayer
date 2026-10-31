//! The lit lines of a Laminar terrain (`.ter`) file.

use std::path::{Path, PathBuf};

/// Ground scale used when a lit `.ter` gives no `LIT_FADE_PARAMS`.
pub const DEFAULT_GROUND_SCALE_M: f64 = 4096.0;

/// A terrain that draws light: its lit texture and the pattern's ground scale.
#[derive(Debug, Clone, PartialEq)]
pub struct LitTerrain {
    /// The lit texture, resolved against the `.ter`'s folder.
    pub texture: PathBuf,
    /// Metres of ground one copy of the texture covers (`LIT_FADE_PARAMS` first value).
    pub ground_scale_m: f64,
}

/// The lit terrain a `.ter` describes, or `None` when it has no lit texture.
pub fn parse_lit_terrain(ter_text: &str, ter_dir: &Path) -> Option<LitTerrain> {
    let mut texture = None;
    let mut scale = None;
    for line in ter_text.lines() {
        let mut tokens = line.split_whitespace();
        match tokens.next() {
            Some("TEXTURE_LIT") | Some("LIT_TEX_NOWRAP") => {
                texture = texture.or_else(|| tokens.next().map(|t| ter_dir.join(t)));
            }
            Some("LIT_FADE_PARAMS") => {
                scale = scale.or_else(|| tokens.next().and_then(|t| t.parse::<f64>().ok()));
            }
            _ => {}
        }
    }
    Some(LitTerrain {
        texture: texture?,
        ground_scale_m: scale
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(DEFAULT_GROUND_SCALE_M),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const URBAN: &str = "A\n800\nTERRAIN\n\nBASE_TEX ../textures12/x.dds\n\
        TEXTURE_LIT\t../textures12/urban_FAR/uw5_FAR_LIT.dds\n\
        LIT_FADE_PARAMS 4096 7000 15000\nLUMINANCE 600\nPROJECTED\t256 256\n";

    #[test]
    fn reads_the_lit_texture_and_ground_scale() {
        let lit = parse_lit_terrain(URBAN, Path::new("/x/terrain12")).unwrap();
        assert_eq!(
            lit.texture,
            Path::new("/x/terrain12/../textures12/urban_FAR/uw5_FAR_LIT.dds")
        );
        assert_eq!(lit.ground_scale_m, 4096.0);
    }

    #[test]
    fn accepts_lit_tex_nowrap() {
        let lit = parse_lit_terrain(
            "LIT_TEX_NOWRAP a.dds\nLIT_FADE_PARAMS 8192 1 2\n",
            Path::new("/t"),
        )
        .unwrap();
        assert_eq!(
            (lit.texture, lit.ground_scale_m),
            (PathBuf::from("/t/a.dds"), 8192.0)
        );
    }

    #[test]
    fn a_park_without_a_lit_texture_is_dark() {
        assert!(parse_lit_terrain("BASE_TEX park.dds\nLUMINANCE 1\n", Path::new("/t")).is_none());
    }

    #[test]
    fn missing_fade_params_use_the_default_scale() {
        let lit = parse_lit_terrain("TEXTURE_LIT a.dds\n", Path::new("/t")).unwrap();
        assert_eq!(lit.ground_scale_m, DEFAULT_GROUND_SCALE_M);
    }
}
