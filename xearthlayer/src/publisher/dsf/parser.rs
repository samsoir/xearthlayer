use super::types::{DsfAnalysis, DsfError};
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
    pub fn analyze(_reader: impl BufRead) -> Result<DsfAnalysis, DsfError> {
        todo!()
    }
}
