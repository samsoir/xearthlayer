use std::io::{BufReader, BufWriter};
use std::path::Path;

use super::filter::DsfZoomFilter;
use super::parser::DsfTextParser;
use super::tool::DsfTool;
use super::types::{DsfError, FilterResult};

/// Orchestrates the decode → analyze → filter → encode pipeline for a single DSF file.
pub struct DsfProcessor<'a> {
    tool: &'a dyn DsfTool,
}

impl<'a> DsfProcessor<'a> {
    pub fn new(tool: &'a dyn DsfTool) -> Self {
        Self { tool }
    }

    /// Process a single DSF file, removing the target zoom level.
    /// Returns Some(FilterResult) if modified, None if no target ZL found.
    pub fn process_dsf_file(
        &self,
        dsf_path: &Path,
        target_zoom: u8,
        dry_run: bool,
    ) -> Result<Option<FilterResult>, DsfError> {
        let tmp_dir = tempfile::tempdir()?;
        let decoded_path = tmp_dir.path().join("decoded.txt");
        let filtered_path = tmp_dir.path().join("filtered.txt");

        // Decode DSF to text
        self.tool.decode(dsf_path, &decoded_path)?;

        // Quick analysis: does this DSF contain the target ZL?
        let analysis = {
            let file = std::fs::File::open(&decoded_path)?;
            let reader = BufReader::new(file);
            DsfTextParser::analyze(reader)?
        };

        let has_target = analysis
            .terrain_defs
            .iter()
            .any(|td| td.zoom_level == Some(target_zoom));

        if !has_target {
            return Ok(None);
        }

        // Filter the decoded text
        let filter_result = {
            let input = std::fs::File::open(&decoded_path)?;
            let reader = BufReader::new(input);
            let output = std::fs::File::create(&filtered_path)?;
            let writer = BufWriter::new(output);
            DsfZoomFilter::filter(reader, writer, target_zoom)?
        };

        if dry_run {
            return Ok(Some(filter_result));
        }

        // Backup original DSF
        let backup_path = dsf_path.with_extension("dsf.bak");
        std::fs::copy(dsf_path, &backup_path)?;

        // Encode filtered text back to DSF
        match self.tool.encode(&filtered_path, dsf_path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&backup_path);
                Ok(Some(filter_result))
            }
            Err(e) => {
                let _ = std::fs::rename(&backup_path, dsf_path);
                Err(e)
            }
        }
    }
}
