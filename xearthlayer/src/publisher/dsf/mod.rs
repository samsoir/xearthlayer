mod parser;
mod types;

pub use parser::{parse_terrain_def_zoom, DsfTextParser};
pub use types::{DsfAnalysis, DsfError, FilterResult, RemoveZlReport, TerrainDef};

#[cfg(test)]
mod tests;
