//! Reader for OXO night recipes (`.oxn`), format version 1.
//!
//! Implements the recipe format's reading rules exactly: the `OXN<digit>`
//! magic, one CBOR map and nothing after it, unknown keys and layers
//! ignored, missing layers read as zeros, and a layer rejected when it does
//! not inflate to `width x height` bytes or indexes past the palette.

use std::io::Read;

use ciborium::value::Value;
use flate2::read::ZlibDecoder;

/// The recipe major version this reader accepts.
pub const SUPPORTED_MAJOR: u8 = 1;

/// Largest layer accepted, in cells. Version 1 layers are 1200 x 1200; the
/// ceiling only stops a hostile header from allocating without bound.
const MAX_LAYER_CELLS: usize = 4096 * 4096;

/// One raster layer, row 0 at the north edge, column 0 at the west edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<u8>,
}

impl Grid {
    /// A missing layer: one zero cell, which samples as zero everywhere.
    pub fn empty() -> Self {
        Self {
            width: 1,
            height: 1,
            cells: vec![0],
        }
    }

    /// The cell at `(row, col)`.
    pub fn get(&self, row: usize, col: usize) -> u8 {
        self.cells[row * self.width + col]
    }
}

/// How a brightness byte maps back to radiance in nW/cm2/sr (`log1p`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrightnessScale {
    pub r0_nw: f64,
    pub rmax_nw: f64,
}

impl Default for BrightnessScale {
    /// The version 1 writer's values, used when a recipe has no brightness layer.
    fn default() -> Self {
        Self {
            r0_nw: 1.0,
            rmax_nw: 300.0,
        }
    }
}

impl BrightnessScale {
    /// `R = r0 * (exp(b / 255 * ln(1 + rmax / r0)) - 1)`.
    pub fn radiance(&self, b: u8) -> f64 {
        self.radiance_at(f64::from(b))
    }

    /// `radiance` for a fractional byte, as bilinear sampling produces.
    pub fn radiance_at(&self, b: f64) -> f64 {
        let span = (1.0 + self.rmax_nw / self.r0_nw).ln();
        self.r0_nw * ((b / 255.0 * span).exp() - 1.0)
    }
}

/// One decoded recipe.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    pub tile: String,
    pub palette: Vec<String>,
    pub class: Grid,
    pub brightness: Grid,
    pub scale: BrightnessScale,
}

/// Why a recipe was rejected. Every rejection makes its tile dark.
#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    #[error("not a night recipe (bad magic)")]
    BadMagic,
    #[error("recipe major version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("format {format} differs from the magic's version {magic}")]
    FormatMismatch { magic: u8, format: u64 },
    #[error("invalid CBOR: {0}")]
    Cbor(String),
    #[error("{0} trailing bytes after the recipe")]
    TrailingBytes(usize),
    #[error("malformed recipe: {0}")]
    Malformed(&'static str),
    #[error("layer {layer} inflates to {actual} bytes, expected {expected}")]
    LayerSize {
        layer: String,
        expected: usize,
        actual: usize,
    },
    #[error("class cell {cell} indexes past a palette of {palette}")]
    ClassIndex { cell: u8, palette: usize },
}

/// Parse one `.oxn` file.
pub fn parse_recipe(bytes: &[u8]) -> Result<Recipe, RecipeError> {
    if bytes.len() < 4 || &bytes[..3] != b"OXN" || !bytes[3].is_ascii_digit() {
        return Err(RecipeError::BadMagic);
    }
    let major = bytes[3] - b'0';
    if major != SUPPORTED_MAJOR {
        return Err(RecipeError::UnsupportedVersion(major));
    }

    let mut rest = &bytes[4..];
    let value: Value =
        ciborium::de::from_reader(&mut rest).map_err(|e| RecipeError::Cbor(e.to_string()))?;
    if !rest.is_empty() {
        return Err(RecipeError::TrailingBytes(rest.len()));
    }

    let map = as_map(&value).ok_or(RecipeError::Malformed("recipe is not a map"))?;
    let format = get(map, "format")
        .and_then(as_u64)
        .ok_or(RecipeError::Malformed("format"))?;
    if format != u64::from(major) {
        return Err(RecipeError::FormatMismatch {
            magic: major,
            format,
        });
    }
    let tile = get(map, "tile")
        .and_then(as_text)
        .ok_or(RecipeError::Malformed("tile"))?
        .to_string();
    let palette = match get(map, "palette") {
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|e| {
                as_text(e)
                    .map(str::to_string)
                    .ok_or(RecipeError::Malformed("palette entry"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(RecipeError::Malformed("palette")),
    };
    if palette.is_empty() {
        return Err(RecipeError::Malformed("palette is empty"));
    }

    let mut class = Grid::empty();
    let mut brightness = Grid::empty();
    let mut scale = BrightnessScale::default();
    let layers = match get(map, "layers") {
        Some(Value::Array(layers)) => layers,
        _ => return Err(RecipeError::Malformed("layers")),
    };
    for layer in layers {
        let layer = as_map(layer).ok_or(RecipeError::Malformed("layer is not a map"))?;
        match get(layer, "name").and_then(as_text) {
            Some("class") => class = read_grid("class", layer)?,
            Some("brightness") => {
                brightness = read_grid("brightness", layer)?;
                scale = read_scale(layer)?;
            }
            Some(_) => {} // a later minor version's layer
            None => return Err(RecipeError::Malformed("layer name")),
        }
    }

    if let Some(&cell) = class
        .cells
        .iter()
        .find(|&&c| usize::from(c) >= palette.len())
    {
        return Err(RecipeError::ClassIndex {
            cell,
            palette: palette.len(),
        });
    }

    Ok(Recipe {
        tile,
        palette,
        class,
        brightness,
        scale,
    })
}

fn read_grid(name: &str, layer: &[(Value, Value)]) -> Result<Grid, RecipeError> {
    let width = get(layer, "width")
        .and_then(as_u64)
        .ok_or(RecipeError::Malformed("width"))? as usize;
    let height = get(layer, "height")
        .and_then(as_u64)
        .ok_or(RecipeError::Malformed("height"))? as usize;
    if get(layer, "type").and_then(as_text) != Some("u8") {
        return Err(RecipeError::Malformed("layer type"));
    }
    let expected = width
        .checked_mul(height)
        .filter(|&n| n > 0 && n <= MAX_LAYER_CELLS)
        .ok_or(RecipeError::Malformed("layer size"))?;
    let compressed = match get(layer, "zlib") {
        Some(Value::Bytes(b)) => b,
        _ => return Err(RecipeError::Malformed("zlib")),
    };

    // Bounded by the declared size: one byte over proves an oversize stream.
    let mut cells = Vec::with_capacity(expected);
    ZlibDecoder::new(compressed.as_slice())
        .take(expected as u64 + 1)
        .read_to_end(&mut cells)
        .map_err(|_| RecipeError::Malformed("zlib stream"))?;
    if cells.len() != expected {
        return Err(RecipeError::LayerSize {
            layer: name.to_string(),
            expected,
            actual: cells.len(),
        });
    }
    Ok(Grid {
        width,
        height,
        cells,
    })
}

fn read_scale(layer: &[(Value, Value)]) -> Result<BrightnessScale, RecipeError> {
    let scale = get(layer, "scale")
        .and_then(as_map)
        .ok_or(RecipeError::Malformed("scale"))?;
    if get(scale, "kind").and_then(as_text) != Some("log1p") {
        return Err(RecipeError::Malformed("scale kind"));
    }
    let r0_nw = get(scale, "r0_nw")
        .and_then(as_f64)
        .ok_or(RecipeError::Malformed("r0_nw"))?;
    let rmax_nw = get(scale, "rmax_nw")
        .and_then(as_f64)
        .ok_or(RecipeError::Malformed("rmax_nw"))?;
    if !r0_nw.is_finite() || r0_nw <= 0.0 || !rmax_nw.is_finite() || rmax_nw <= 0.0 {
        return Err(RecipeError::Malformed("scale values"));
    }
    Ok(BrightnessScale { r0_nw, rmax_nw })
}

fn get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| matches!(k, Value::Text(t) if t == key))
        .map(|(_, v)| v)
}

fn as_map(v: &Value) -> Option<&[(Value, Value)]> {
    match v {
        Value::Map(m) => Some(m),
        _ => None,
    }
}

fn as_text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Integer(i) => u64::try_from(*i).ok(),
        _ => None,
    }
}

/// Any valid encoding is accepted, so an integral float may arrive as an integer.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Integer(i) => Some(i128::from(*i) as f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::value::Value;
    use std::io::Write;

    const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");
    const EMPTY: &[u8] = include_bytes!("../../tests/fixtures/night/empty.oxn");
    const NO_VIIRS: &[u8] = include_bytes!("../../tests/fixtures/night/no_viirs.oxn");

    const FAKE_GRANULE: [u8; 16] = [
        0, 255, 176, 177, 0, 0, 0, 202, 217, 218, 218, 218, 229, 230, 230, 230,
    ];

    fn count(
        grid: &Grid,
        index: u8,
        rows: std::ops::RangeInclusive<usize>,
        cols: std::ops::RangeInclusive<usize>,
    ) -> usize {
        rows.flat_map(|r| cols.clone().map(move |c| (r, c)))
            .filter(|&(r, c)| grid.get(r, c) == index)
            .count()
    }

    #[test]
    fn urban_golden_decodes_to_the_documented_cells() {
        let r = parse_recipe(URBAN).unwrap();
        assert_eq!(r.tile, "+37-123");
        assert_eq!(
            r.palette,
            vec![
                "",
                "lib/g10/terrain10/ind_tmp_sdry1.ter",
                "lib/g10/terrain10/urban_tmp_sdry3.ter"
            ]
        );
        assert_eq!((r.class.width, r.class.height), (1200, 1200));
        assert_eq!(count(&r.class, 1, 600..=659, 600..=719), 7200);
        assert_eq!(count(&r.class, 2, 120..=179, 120..=239), 7200);
        assert_eq!(count(&r.class, 2, 900..=1019, 300..=359), 7200);
        let nonzero = r.class.cells.iter().filter(|&&c| c != 0).count();
        assert_eq!(nonzero, 7200 + 14400, "every other cell is 0");
        assert_eq!((r.brightness.width, r.brightness.height), (4, 4));
        assert_eq!(r.brightness.cells, FAKE_GRANULE.to_vec());
        assert_eq!(
            r.scale,
            BrightnessScale {
                r0_nw: 1.0,
                rmax_nw: 300.0
            }
        );
    }

    #[test]
    fn empty_golden_has_no_lit_class() {
        let r = parse_recipe(EMPTY).unwrap();
        assert_eq!(r.palette, vec![""]);
        assert_eq!(r.class.cells.len(), 1_440_000);
        assert!(r.class.cells.iter().all(|&c| c == 0));
        assert_eq!(r.brightness.cells, FAKE_GRANULE.to_vec());
    }

    #[test]
    fn no_viirs_golden_has_a_dark_brightness_layer() {
        let r = parse_recipe(NO_VIIRS).unwrap();
        assert_eq!(r.palette.len(), 3);
        assert_eq!((r.brightness.width, r.brightness.height), (240, 240));
        assert!(r.brightness.cells.iter().all(|&b| b == 0));
    }

    #[test]
    fn radiance_at_matches_radiance_on_whole_bytes() {
        let s = BrightnessScale::default();
        for b in [0u8, 1, 97, 230, 255] {
            assert!((s.radiance_at(f64::from(b)) - s.radiance(b)).abs() < 1e-12);
        }
    }

    #[test]
    fn radiance_inverts_the_log_scale() {
        let s = BrightnessScale {
            r0_nw: 1.0,
            rmax_nw: 300.0,
        };
        assert_eq!(s.radiance(0), 0.0);
        assert!((s.radiance(255) - 300.0).abs() < 1e-9);
    }

    // --- Reading rules -----------------------------------------------------

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    fn zlib(cells: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(cells).unwrap();
        e.finish().unwrap()
    }

    fn layer(name: &str, w: u64, h: u64, cells: &[u8]) -> Value {
        Value::Map(vec![
            (text("name"), text(name)),
            (text("width"), Value::Integer(w.into())),
            (text("height"), Value::Integer(h.into())),
            (text("type"), text("u8")),
            (text("zlib"), Value::Bytes(zlib(cells))),
        ])
    }

    fn recipe(format: u64, palette: &[&str], layers: Vec<Value>) -> Value {
        Value::Map(vec![
            (text("format"), Value::Integer(format.into())),
            (text("tile"), text("+37-123")),
            (
                text("palette"),
                Value::Array(palette.iter().map(|p| text(p)).collect()),
            ),
            (text("layers"), Value::Array(layers)),
            (text("future_key"), text("ignored")),
        ])
    }

    fn file(magic: &[u8; 4], v: &Value) -> Vec<u8> {
        let mut out = magic.to_vec();
        ciborium::ser::into_writer(v, &mut out).unwrap();
        out
    }

    #[test]
    fn rejects_a_bad_magic() {
        let mut bytes = URBAN.to_vec();
        bytes[0] = b'X';
        assert!(matches!(parse_recipe(&bytes), Err(RecipeError::BadMagic)));
    }

    #[test]
    fn rejects_an_unsupported_major_version() {
        let bytes = file(b"OXN2", &recipe(2, &[""], vec![]));
        assert!(matches!(
            parse_recipe(&bytes),
            Err(RecipeError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn rejects_a_format_that_differs_from_the_magic() {
        let bytes = file(b"OXN1", &recipe(2, &[""], vec![]));
        assert!(matches!(
            parse_recipe(&bytes),
            Err(RecipeError::FormatMismatch { .. })
        ));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = URBAN.to_vec();
        bytes.push(0);
        assert!(matches!(
            parse_recipe(&bytes),
            Err(RecipeError::TrailingBytes(1))
        ));
    }

    #[test]
    fn rejects_a_layer_that_inflates_to_the_wrong_size() {
        let v = recipe(1, &[""], vec![layer("class", 2, 2, &[0, 0, 0])]);
        assert!(matches!(
            parse_recipe(&file(b"OXN1", &v)),
            Err(RecipeError::LayerSize { .. })
        ));
    }

    #[test]
    fn rejects_a_class_index_past_the_palette() {
        let v = recipe(
            1,
            &["", "lib/g10/terrain10/urban_x.ter"],
            vec![layer("class", 2, 1, &[0, 2])],
        );
        assert!(matches!(
            parse_recipe(&file(b"OXN1", &v)),
            Err(RecipeError::ClassIndex { .. })
        ));
    }

    fn brightness_with_scale(r0: f64, rmax: f64) -> Value {
        let mut l = layer("brightness", 1, 1, &[5]);
        if let Value::Map(m) = &mut l {
            m.push((
                text("scale"),
                Value::Map(vec![
                    (text("kind"), text("log1p")),
                    (text("r0_nw"), Value::Float(r0)),
                    (text("rmax_nw"), Value::Float(rmax)),
                ]),
            ));
        }
        l
    }

    #[test]
    fn rejects_a_non_finite_or_nan_scale() {
        for (r0, rmax) in [
            (1.0, f64::INFINITY),
            (f64::INFINITY, 300.0),
            (1.0, f64::NAN),
        ] {
            let v = recipe(1, &[""], vec![brightness_with_scale(r0, rmax)]);
            assert!(
                matches!(
                    parse_recipe(&file(b"OXN1", &v)),
                    Err(RecipeError::Malformed("scale values"))
                ),
                "r0={r0} rmax={rmax}"
            );
        }
        let ok = recipe(1, &[""], vec![brightness_with_scale(1.0, 300.0)]);
        assert!(parse_recipe(&file(b"OXN1", &ok)).is_ok());
    }

    #[test]
    fn missing_layers_read_as_zero_and_unknown_ones_are_ignored() {
        let v = recipe(1, &[""], vec![layer("roads", 1, 1, &[9])]);
        let r = parse_recipe(&file(b"OXN1", &v)).unwrap();
        assert_eq!(r.class, Grid::empty());
        assert_eq!(r.brightness, Grid::empty());
        assert_eq!(r.scale, BrightnessScale::default());
    }

    #[test]
    fn rejects_a_layer_declaring_more_cells_than_the_ceiling() {
        let v = recipe(1, &[""], vec![layer("class", 100_000, 100_000, &[0])]);
        assert!(matches!(
            parse_recipe(&file(b"OXN1", &v)),
            Err(RecipeError::Malformed(_))
        ));
    }
}
