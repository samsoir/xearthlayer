use super::types::{DsfAnalysis, DsfError, TerrainDef};
use std::collections::HashMap;
use std::io::BufRead;

pub fn parse_terrain_def_zoom(name: &str) -> Option<u8> {
    let filename = name.rsplit('/').next().unwrap_or(name);
    let stem = filename.strip_suffix(".ter")?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 3 {
        return None;
    }
    let provider_zoom = parts[2];
    if provider_zoom.len() < 3 {
        return None;
    }
    let zoom_str = &provider_zoom[provider_zoom.len() - 2..];
    zoom_str.parse::<u8>().ok()
}

pub struct DsfTextParser;

impl DsfTextParser {
    pub fn analyze(reader: impl BufRead) -> Result<DsfAnalysis, DsfError> {
        let mut terrain_defs = Vec::new();
        let mut terrain_index: usize = 0;
        let mut total_patches: usize = 0;
        let mut patches_by_zoom: HashMap<u8, usize> = HashMap::new();
        let mut index_to_zoom: HashMap<usize, u8> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

            if let Some(name) = trimmed.strip_prefix("TERRAIN_DEF ") {
                let zoom_level = parse_terrain_def_zoom(name);
                terrain_defs.push(TerrainDef {
                    index: terrain_index,
                    name: name.to_string(),
                    zoom_level,
                });
                if let Some(zl) = zoom_level {
                    index_to_zoom.insert(terrain_index, zl);
                }
                terrain_index += 1;
            } else if trimmed.starts_with("BEGIN_PATCH ") {
                total_patches += 1;
                if let Some(idx) = parse_patch_terrain_index(trimmed) {
                    if let Some(&zl) = index_to_zoom.get(&idx) {
                        *patches_by_zoom.entry(zl).or_insert(0) += 1;
                    }
                }
            }
        }

        Ok(DsfAnalysis {
            terrain_defs,
            total_patches,
            patches_by_zoom,
        })
    }
}

/// Extract the terrain index from a BEGIN_PATCH line.
fn parse_patch_terrain_index(line: &str) -> Option<usize> {
    let after_prefix = line.strip_prefix("BEGIN_PATCH ")?;
    let index_str = after_prefix.split_whitespace().next()?;
    index_str.parse().ok()
}
