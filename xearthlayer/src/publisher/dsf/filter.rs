use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};

use super::parser::parse_terrain_def_zoom;
use super::types::{DsfError, FilterResult};

pub struct DsfZoomFilter;

impl DsfZoomFilter {
    pub fn filter(
        reader: impl BufRead,
        mut writer: impl Write,
        target_zoom: u8,
    ) -> Result<FilterResult, DsfError> {
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

        // Pass 1: identify terrain def indices to remove
        let mut remove_indices = Vec::new();
        let mut terrain_index: usize = 0;
        for line in &lines {
            let trimmed = line.trim();
            if let Some(name) = trimmed.strip_prefix("TERRAIN_DEF ") {
                if parse_terrain_def_zoom(name) == Some(target_zoom) {
                    remove_indices.push(terrain_index);
                }
                terrain_index += 1;
            }
        }

        if remove_indices.is_empty() {
            for line in &lines {
                writeln!(writer, "{}", line)?;
            }
            return Ok(FilterResult {
                terrain_defs_removed: 0,
                patches_removed: 0,
            });
        }

        // Build remap table
        let remove_set: HashSet<usize> = remove_indices.iter().copied().collect();
        let total_terrain_defs = terrain_index;
        let mut remap: HashMap<usize, usize> = HashMap::new();
        let mut new_index: usize = 0;
        for old_index in 0..total_terrain_defs {
            if !remove_set.contains(&old_index) {
                remap.insert(old_index, new_index);
                new_index += 1;
            }
        }

        // Pass 2: write output
        let mut terrain_def_counter: usize = 0;
        let mut skip_patch = false;
        let mut patches_removed: usize = 0;

        for line in &lines {
            let trimmed = line.trim();

            if trimmed.starts_with("TERRAIN_DEF ") {
                if remove_set.contains(&terrain_def_counter) {
                    terrain_def_counter += 1;
                    continue;
                }
                terrain_def_counter += 1;
                writeln!(writer, "{}", line)?;
            } else if trimmed.starts_with("BEGIN_PATCH ") {
                let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
                if let Some(idx_str) = parts.get(1) {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if remove_set.contains(&idx) {
                            skip_patch = true;
                            patches_removed += 1;
                            continue;
                        }
                        if let Some(&new_idx) = remap.get(&idx) {
                            let rest = parts.get(2).copied().unwrap_or("");
                            writeln!(writer, "BEGIN_PATCH {} {}", new_idx, rest)?;
                        } else {
                            writeln!(writer, "{}", line)?;
                        }
                    } else {
                        writeln!(writer, "{}", line)?;
                    }
                } else {
                    writeln!(writer, "{}", line)?;
                }
            } else if trimmed == "END_PATCH" {
                if skip_patch {
                    skip_patch = false;
                    continue;
                }
                writeln!(writer, "{}", line)?;
            } else if skip_patch {
                continue;
            } else {
                writeln!(writer, "{}", line)?;
            }
        }

        Ok(FilterResult {
            terrain_defs_removed: remove_indices.len(),
            patches_removed,
        })
    }
}
