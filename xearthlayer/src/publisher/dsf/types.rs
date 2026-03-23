use std::collections::HashMap;
use std::path::PathBuf;

/// Errors that can occur during DSF processing.
#[derive(Debug, thiserror::Error)]
pub enum DsfError {
    #[error("DSFTool not found on PATH. Install from https://developer.x-plane.com/tools/")]
    ToolNotFound,

    #[error("DSFTool decode failed for {path}: {reason}")]
    DecodeFailed { path: PathBuf, reason: String },

    #[error("DSFTool encode failed for {path}: {reason}")]
    EncodeFailed { path: PathBuf, reason: String },

    #[error("Failed to parse DSF text: {0}")]
    ParseError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// A terrain definition entry from a DSF text file.
#[derive(Debug, Clone)]
pub struct TerrainDef {
    /// Zero-based index in the TERRAIN_DEF table.
    pub index: usize,
    /// Full name as it appears in the DSF (e.g., "terrain/88416_136896_BI18_sea.ter").
    pub name: String,
    /// Zoom level extracted from the name, if parseable. None for terrain_Water.
    pub zoom_level: Option<u8>,
}

/// Analysis of a decoded DSF text file.
#[derive(Debug)]
pub struct DsfAnalysis {
    /// All terrain definitions found.
    pub terrain_defs: Vec<TerrainDef>,
    /// Total number of patches (BEGIN_PATCH...END_PATCH blocks).
    pub total_patches: usize,
    /// Count of patches grouped by zoom level.
    pub patches_by_zoom: HashMap<u8, usize>,
}

/// Result of filtering a DSF text file.
#[derive(Debug)]
pub struct FilterResult {
    /// Number of TERRAIN_DEF entries removed.
    pub terrain_defs_removed: usize,
    /// Number of BEGIN_PATCH...END_PATCH blocks removed.
    pub patches_removed: usize,
}

/// Report from a remove-zl operation across a package.
#[derive(Debug)]
pub struct RemoveZlReport {
    /// Region code processed.
    pub region: String,
    /// Zoom level that was targeted for removal.
    pub target_zoom: u8,
    /// Number of DSF files scanned.
    pub dsf_files_scanned: usize,
    /// Number of DSF files that were modified (contained target ZL).
    pub dsf_files_modified: usize,
    /// DSF files that failed processing, with error messages.
    pub dsf_files_failed: Vec<(PathBuf, String)>,
    /// Total TERRAIN_DEF entries removed across all DSFs.
    pub terrain_defs_removed: usize,
    /// Total patches removed across all DSFs.
    pub patches_removed: usize,
    /// Number of .ter files removed from terrain/.
    pub ter_files_removed: usize,
    /// Number of .png files removed from textures/.
    pub png_files_removed: usize,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

impl RemoveZlReport {
    /// Note: region is validated to be alphanumeric by the CLI arg parser,
    /// so no JSON escaping is needed.
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{\n",
                "  \"region\": \"{}\",\n",
                "  \"target_zoom\": {},\n",
                "  \"dsf_files_scanned\": {},\n",
                "  \"dsf_files_modified\": {},\n",
                "  \"dsf_files_failed\": {},\n",
                "  \"terrain_defs_removed\": {},\n",
                "  \"patches_removed\": {},\n",
                "  \"ter_files_removed\": {},\n",
                "  \"png_files_removed\": {},\n",
                "  \"dry_run\": {}\n",
                "}}"
            ),
            self.region,
            self.target_zoom,
            self.dsf_files_scanned,
            self.dsf_files_modified,
            self.dsf_files_failed.len(),
            self.terrain_defs_removed,
            self.patches_removed,
            self.ter_files_removed,
            self.png_files_removed,
            self.dry_run,
        )
    }

    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!(
                "Remove ZL{} Report — {}",
                self.target_zoom,
                self.region.to_uppercase()
            ),
            format!("DSF files scanned: {}", self.dsf_files_scanned),
            format!("DSF files modified: {}", self.dsf_files_modified),
            format!("Terrain defs removed: {}", self.terrain_defs_removed),
            format!("Patches removed: {}", self.patches_removed),
            format!(".ter files removed: {}", self.ter_files_removed),
            format!(".png files removed: {}", self.png_files_removed),
            format!("Dry run: {}", self.dry_run),
        ];
        if !self.dsf_files_failed.is_empty() {
            lines.push(format!("\nFailures ({}):", self.dsf_files_failed.len()));
            for (path, err) in &self.dsf_files_failed {
                lines.push(format!("  {}: {}", path.display(), err));
            }
        }
        lines.join("\n")
    }
}
