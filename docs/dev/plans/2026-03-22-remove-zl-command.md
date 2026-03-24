# Remove Zoom Level Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `publish remove-zl` command that strips a specified zoom level from scenery packages by modifying DSF files via DSFTool text pipeline and cleaning up orphaned `.ter`/`.png` files.

**Architecture:** New `publisher::dsf` module handles DSF text parsing and filtering. CLI follows existing `publish dedupe` patterns — `RemoveZlHandler` dispatches to `PublisherService::remove_zoom_level()`, which orchestrates DSFTool decode → filter → encode per DSF file with Rayon parallelism. `DsfTool` trait enables mocking the external tool dependency.

**Tech Stack:** Rust, clap (CLI), rayon (parallelism), tempfile (temp dirs), thiserror (errors). External: DSFTool CLI.

**Spec:** `docs/dev/specs/2026-03-22-remove-zl-command.md`

---

## File Structure

### New Files (library — `xearthlayer/src/publisher/dsf/`)

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module declarations and public exports |
| `types.rs` | `DsfError`, `TerrainDef`, `DsfAnalysis`, `FilterResult`, `RemoveZlReport` |
| `parser.rs` | `parse_terrain_def_zoom()` utility, `DsfTextParser::analyze()` |
| `filter.rs` | `DsfZoomFilter::filter()` — index remapping and patch removal |
| `tool.rs` | `DsfTool` trait + `DsfToolRunner` implementation |
| `processor.rs` | `DsfProcessor` — orchestrates the full pipeline per DSF file |
| `tests.rs` | Unit tests for parser, filter, and processor |

### Modified Files (CLI — `xearthlayer-cli/src/commands/publish/`)

| File | Change |
|------|--------|
| `args.rs` | Add `RemoveZl` variant to `PublishCommands`, add `RemoveZlArgs` struct |
| `handlers.rs` | Add `RemoveZlHandler` struct and `CommandHandler` impl |
| `traits.rs` | Add `RemoveZlReport` type, add `remove_zoom_level()` to `PublisherService` trait |
| `services.rs` | Implement `remove_zoom_level()` on `DefaultPublisherService` |
| `output.rs` | Add `print_remove_zl_result()` function |
| `mod.rs` | Add dispatch arm for `PublishCommands::RemoveZl` |

### Modified Files (library)

| File | Change |
|------|--------|
| `xearthlayer/src/publisher/mod.rs` | Add `pub mod dsf;` declaration and re-exports |

---

## Task 1: DSF Error Types and Core Structs

**Files:**
- Create: `xearthlayer/src/publisher/dsf/types.rs`
- Create: `xearthlayer/src/publisher/dsf/mod.rs`
- Modify: `xearthlayer/src/publisher/mod.rs`

- [ ] **Step 1: Create `types.rs` with `DsfError` and data structs**

```rust
// xearthlayer/src/publisher/dsf/types.rs
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
```

- [ ] **Step 2: Create `mod.rs` with module declarations**

```rust
// xearthlayer/src/publisher/dsf/mod.rs
mod filter;
mod parser;
mod processor;
mod tool;
mod types;

pub use parser::{parse_terrain_def_zoom, DsfTextParser};
pub use filter::DsfZoomFilter;
pub use processor::DsfProcessor;
pub use tool::{DsfTool, DsfToolRunner};
pub use types::{DsfAnalysis, DsfError, FilterResult, RemoveZlReport, TerrainDef};

#[cfg(test)]
mod tests;
```

Note: Create empty stub files for `filter.rs`, `parser.rs`, `processor.rs`, `tool.rs`, and `tests.rs` so the module compiles. Each stub can just be `// TODO: implement`.

- [ ] **Step 3: Add `pub mod dsf;` to `xearthlayer/src/publisher/mod.rs`**

Add after the existing `pub mod dedupe;` line:

```rust
pub mod dsf;
```

And add re-exports:

```rust
pub use dsf::{DsfError, RemoveZlReport};
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p xearthlayer 2>&1 | head -20`
Expected: Compiles (warnings about unused imports/dead code are OK at this stage)

- [ ] **Step 5: Commit**

```
feat(publisher): add DSF module types and error definitions

Foundation types for the publish remove-zl command: DsfError,
TerrainDef, DsfAnalysis, FilterResult, and RemoveZlReport.
```

---

## Task 2: Terrain Def Zoom Parser

**Files:**
- Create: `xearthlayer/src/publisher/dsf/parser.rs`
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write failing tests for `parse_terrain_def_zoom`**

```rust
// xearthlayer/src/publisher/dsf/tests.rs
use super::parser::parse_terrain_def_zoom;

#[test]
fn test_parse_bing_zl16() {
    assert_eq!(parse_terrain_def_zoom("terrain/22112_34224_BI16.ter"), Some(16));
}

#[test]
fn test_parse_bing_zl18() {
    assert_eq!(parse_terrain_def_zoom("terrain/88416_136896_BI18.ter"), Some(18));
}

#[test]
fn test_parse_bing_zl18_sea() {
    assert_eq!(parse_terrain_def_zoom("terrain/88416_136896_BI18_sea.ter"), Some(18));
}

#[test]
fn test_parse_bing_zl18_sea_overlay() {
    assert_eq!(parse_terrain_def_zoom("terrain/88416_136896_BI18_sea_overlay.ter"), Some(18));
}

#[test]
fn test_parse_go2_zl18() {
    assert_eq!(parse_terrain_def_zoom("terrain/38800_80512_GO218.ter"), Some(18));
}

#[test]
fn test_parse_go2_zl16() {
    assert_eq!(parse_terrain_def_zoom("terrain/25264_10368_GO216.ter"), Some(16));
}

#[test]
fn test_parse_terrain_water() {
    assert_eq!(parse_terrain_def_zoom("terrain_Water"), None);
}

#[test]
fn test_parse_google_zl16() {
    assert_eq!(parse_terrain_def_zoom("terrain/25264_10368_GO16.ter"), Some(16));
}

#[test]
fn test_parse_no_zoom() {
    assert_eq!(parse_terrain_def_zoom("terrain/unknown_format.ter"), None);
}

#[test]
fn test_parse_bare_filename_matches() {
    // ter_matches_zoom wraps with "terrain/" — verify the function handles it
    let full = format!("terrain/{}", "88416_136896_BI18_sea.ter");
    assert_eq!(parse_terrain_def_zoom(&full), Some(18));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer dsf::tests::test_parse_ -- --no-capture 2>&1 | tail -20`
Expected: FAIL — `parse_terrain_def_zoom` not defined

- [ ] **Step 3: Implement `parse_terrain_def_zoom`**

```rust
// xearthlayer/src/publisher/dsf/parser.rs
use std::io::BufRead;
use super::types::{DsfAnalysis, DsfError, TerrainDef};
use std::collections::HashMap;

/// Extract zoom level from a TERRAIN_DEF path string.
///
/// Handles formats:
/// - `terrain/<row>_<col>_<provider><zoom>.ter` (e.g., `terrain/22112_34224_BI16.ter`)
/// - `terrain/<row>_<col>_<provider><zoom>_sea.ter`
/// - `terrain/<row>_<col>_<provider><zoom>_sea_overlay.ter`
/// - `terrain_Water` → None
///
/// Provider codes: BI (Bing, 2 chars), GO2 (Google GO2, 3 chars), GO (Google, 2 chars)
/// Zoom is always the last 2 digits of the provider+zoom segment.
pub fn parse_terrain_def_zoom(name: &str) -> Option<u8> {
    // Strip directory prefix — get filename
    let filename = name.rsplit('/').next().unwrap_or(name);

    // Strip .ter extension
    let stem = filename.strip_suffix(".ter")?;

    // Split on underscore
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 3 {
        return None;
    }

    // The provider+zoom segment is parts[2] (e.g., "BI16", "GO218", "BI18")
    // Parts after that are suffixes like "sea", "sea_overlay"
    let provider_zoom = parts[2];

    // Need at least 3 chars (min 1 char provider + 2 digit zoom)
    if provider_zoom.len() < 3 {
        return None;
    }

    // Last 2 chars are the zoom level
    let zoom_str = &provider_zoom[provider_zoom.len() - 2..];
    zoom_str.parse::<u8>().ok()
}

/// Parser for DSF text files produced by DSFTool --dsf2text.
pub struct DsfTextParser;

impl DsfTextParser {
    /// Analyze a decoded DSF text stream.
    ///
    /// Extracts terrain definitions and counts patches by zoom level.
    pub fn analyze(reader: impl BufRead) -> Result<DsfAnalysis, DsfError> {
        // Stub — implemented in Task 3
        todo!()
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer dsf::tests::test_parse_ -- --no-capture 2>&1 | tail -20`
Expected: All 8 tests PASS

- [ ] **Step 5: Commit**

```
feat(publisher/dsf): add terrain def zoom level parser

Extracts zoom level from TERRAIN_DEF path strings in DSF text output.
Handles BI, GO2, GO provider prefixes and _sea/_sea_overlay suffixes.
```

---

## Task 3: DSF Text Analyzer

**Files:**
- Modify: `xearthlayer/src/publisher/dsf/parser.rs`
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write failing tests for `DsfTextParser::analyze`**

```rust
// Add to xearthlayer/src/publisher/dsf/tests.rs
use super::parser::DsfTextParser;
use std::io::BufReader;

fn make_dsf_text(terrain_defs: &[&str], patches: &[(usize, &str)]) -> String {
    let mut lines = vec![
        "I".to_string(),
        "800 written by DSFTool 2.4.0-b1".to_string(),
        "DSF2TEXT".to_string(),
        "".to_string(),
        "PROPERTY sim/west 8".to_string(),
        "PROPERTY sim/east 9".to_string(),
        "PROPERTY sim/south 50".to_string(),
        "PROPERTY sim/north 51".to_string(),
    ];

    for def in terrain_defs {
        lines.push(format!("TERRAIN_DEF {}", def));
    }

    for (index, vertex_block) in patches {
        lines.push(format!("BEGIN_PATCH {} 0.000000 -1.000000 1 7", index));
        lines.push("BEGIN_PRIMITIVE 0".to_string());
        lines.push(vertex_block.to_string());
        lines.push("END_PRIMITIVE".to_string());
        lines.push("END_PATCH".to_string());
    }

    lines.join("\n")
}

#[test]
fn test_analyze_counts_terrain_defs() {
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[],
    );
    let reader = BufReader::new(text.as_bytes());
    let analysis = DsfTextParser::analyze(reader).unwrap();

    assert_eq!(analysis.terrain_defs.len(), 3);
    assert_eq!(analysis.terrain_defs[0].name, "terrain_Water");
    assert_eq!(analysis.terrain_defs[0].zoom_level, None);
    assert_eq!(analysis.terrain_defs[1].zoom_level, Some(16));
    assert_eq!(analysis.terrain_defs[2].zoom_level, Some(18));
}

#[test]
fn test_analyze_counts_patches_by_zoom() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[(1, vertex), (1, vertex), (2, vertex)],
    );
    let reader = BufReader::new(text.as_bytes());
    let analysis = DsfTextParser::analyze(reader).unwrap();

    assert_eq!(analysis.total_patches, 3);
    assert_eq!(analysis.patches_by_zoom[&16], 2);
    assert_eq!(analysis.patches_by_zoom[&18], 1);
}

#[test]
fn test_analyze_empty_dsf() {
    let text = make_dsf_text(&["terrain_Water"], &[]);
    let reader = BufReader::new(text.as_bytes());
    let analysis = DsfTextParser::analyze(reader).unwrap();

    assert_eq!(analysis.terrain_defs.len(), 1);
    assert_eq!(analysis.total_patches, 0);
    assert!(analysis.patches_by_zoom.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer dsf::tests::test_analyze_ -- --no-capture 2>&1 | tail -20`
Expected: FAIL — `todo!()` panics

- [ ] **Step 3: Implement `DsfTextParser::analyze`**

Replace the `todo!()` in `parser.rs`:

```rust
impl DsfTextParser {
    pub fn analyze(reader: impl BufRead) -> Result<DsfAnalysis, DsfError> {
        let mut terrain_defs = Vec::new();
        let mut terrain_index: usize = 0;
        let mut total_patches: usize = 0;
        let mut patches_by_zoom: HashMap<u8, usize> = HashMap::new();

        // Build terrain def table for reverse lookups
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
                // Parse terrain index from BEGIN_PATCH line
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
/// Format: "BEGIN_PATCH <index> <rest...>"
fn parse_patch_terrain_index(line: &str) -> Option<usize> {
    let after_prefix = line.strip_prefix("BEGIN_PATCH ")?;
    let index_str = after_prefix.split_whitespace().next()?;
    index_str.parse().ok()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer dsf::tests::test_analyze_ -- --no-capture 2>&1 | tail -20`
Expected: All 3 tests PASS

- [ ] **Step 5: Commit**

```
feat(publisher/dsf): add DSF text analyzer

DsfTextParser::analyze() reads decoded DSF text and extracts terrain
definitions with zoom levels and patch counts per zoom.
```

---

## Task 4: DSF Zoom Filter

**Files:**
- Create: `xearthlayer/src/publisher/dsf/filter.rs`
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write failing tests for `DsfZoomFilter::filter`**

```rust
// Add to xearthlayer/src/publisher/dsf/tests.rs
use super::filter::DsfZoomFilter;

#[test]
fn test_filter_removes_zl18_terrain_defs() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[(1, vertex), (2, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 1);
    assert_eq!(result.patches_removed, 1);

    let output_str = String::from_utf8(output).unwrap();
    assert!(output_str.contains("TERRAIN_DEF terrain_Water"));
    assert!(output_str.contains("TERRAIN_DEF terrain/100_200_BI16.ter"));
    assert!(!output_str.contains("BI18"));
}

#[test]
fn test_filter_remaps_patch_indices() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &[
            "terrain_Water",          // index 0
            "terrain/100_200_BI18.ter", // index 1 — REMOVE
            "terrain/100_200_BI16.ter", // index 2 → becomes 1
        ],
        &[(2, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    let output_str = String::from_utf8(output).unwrap();
    // Index 2 should be remapped to 1
    assert!(output_str.contains("BEGIN_PATCH 1 "));
    assert!(!output_str.contains("BEGIN_PATCH 2 "));
}

#[test]
fn test_filter_preserves_non_target_patches() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[(0, vertex), (1, vertex), (2, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.patches_removed, 1);
    let output_str = String::from_utf8(output).unwrap();
    // Water patch (index 0) and ZL16 patch (index 1) should survive
    assert_eq!(output_str.matches("BEGIN_PATCH").count(), 2);
}

#[test]
fn test_filter_no_target_zl_is_noop() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter"],
        &[(0, vertex), (1, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 0);
    assert_eq!(result.patches_removed, 0);
}

#[test]
fn test_filter_consecutive_removals() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &[
            "terrain_Water",            // 0
            "terrain/a_b_BI18.ter",     // 1 — REMOVE
            "terrain/c_d_BI18_sea.ter", // 2 — REMOVE
            "terrain/e_f_BI16.ter",     // 3 → becomes 1
        ],
        &[(3, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 2);
    let output_str = String::from_utf8(output).unwrap();
    assert!(output_str.contains("BEGIN_PATCH 1 "));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer dsf::tests::test_filter_ -- --no-capture 2>&1 | tail -20`
Expected: FAIL — `DsfZoomFilter` not implemented

- [ ] **Step 3: Implement `DsfZoomFilter::filter`**

```rust
// xearthlayer/src/publisher/dsf/filter.rs
use std::collections::HashMap;
use std::io::{BufRead, Write};
use super::parser::parse_terrain_def_zoom;
use super::types::{DsfError, FilterResult};

/// Filters decoded DSF text to remove a target zoom level.
pub struct DsfZoomFilter;

impl DsfZoomFilter {
    /// Filter decoded DSF text, removing all TERRAIN_DEF entries and patches
    /// at the target zoom level. Remaps surviving patch indices.
    pub fn filter(
        reader: impl BufRead,
        mut writer: impl Write,
        target_zoom: u8,
    ) -> Result<FilterResult, DsfError> {
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

        // Pass 1: identify terrain def indices to remove and build remap table
        let mut remove_indices: Vec<usize> = Vec::new();
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
            // No target ZL found — write through unchanged
            for line in &lines {
                writeln!(writer, "{}", line)?;
            }
            return Ok(FilterResult {
                terrain_defs_removed: 0,
                patches_removed: 0,
            });
        }

        // Build remap table: old_index → new_index (only for surviving indices)
        let remove_set: std::collections::HashSet<usize> = remove_indices.iter().copied().collect();
        let total_terrain_defs = terrain_index;
        let mut remap: HashMap<usize, usize> = HashMap::new();
        let mut new_index: usize = 0;
        for old_index in 0..total_terrain_defs {
            if !remove_set.contains(&old_index) {
                remap.insert(old_index, new_index);
                new_index += 1;
            }
        }

        // Pass 2: write output, skipping removed terrain defs and their patches
        let mut terrain_def_counter: usize = 0;
        let mut skip_patch = false;
        let mut patches_removed: usize = 0;

        for line in &lines {
            let trimmed = line.trim();

            if let Some(_name) = trimmed.strip_prefix("TERRAIN_DEF ") {
                if remove_set.contains(&terrain_def_counter) {
                    terrain_def_counter += 1;
                    continue; // Skip this TERRAIN_DEF
                }
                terrain_def_counter += 1;
                writeln!(writer, "{}", line)?;
            } else if trimmed.starts_with("BEGIN_PATCH ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if let Some(idx_str) = parts.get(1) {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if remove_set.contains(&idx) {
                            skip_patch = true;
                            patches_removed += 1;
                            continue;
                        }
                        // Remap the index
                        if let Some(&new_idx) = remap.get(&idx) {
                            let remapped = format!(
                                "BEGIN_PATCH {}{}",
                                new_idx,
                                &trimmed["BEGIN_PATCH ".len() + idx_str.len()..]
                            );
                            writeln!(writer, "{}", remapped)?;
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
                continue; // Skip lines inside a removed patch
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer dsf::tests::test_filter_ -- --no-capture 2>&1 | tail -20`
Expected: All 5 tests PASS

- [ ] **Step 5: Commit**

```
feat(publisher/dsf): add DSF zoom level filter

DsfZoomFilter reads decoded DSF text, removes TERRAIN_DEF entries and
patches at the target zoom level, and remaps surviving patch indices.
Operates on in-memory readers/writers for testability.
```

---

## Task 5: DsfTool Trait and Runner

**Files:**
- Create: `xearthlayer/src/publisher/dsf/tool.rs`
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write failing test for `DsfToolRunner::check_available`**

```rust
// Add to xearthlayer/src/publisher/dsf/tests.rs
use super::tool::{DsfTool, DsfToolRunner};

#[test]
fn test_dsftool_check_available() {
    // This test verifies the runner can find DSFTool on PATH.
    // Will pass on dev machines with DSFTool installed, fail in CI.
    let runner = DsfToolRunner;
    // We just test it doesn't panic — actual availability depends on env
    let _result = runner.check_available();
}
```

- [ ] **Step 2: Implement `DsfTool` trait and `DsfToolRunner`**

```rust
// xearthlayer/src/publisher/dsf/tool.rs
use std::path::Path;
use std::process::Command;
use super::types::DsfError;

/// Abstraction over DSF binary ↔ text conversion.
/// Trait enables mocking DSFTool in tests.
pub trait DsfTool: Send + Sync {
    /// Check that the DSF tool is available.
    fn check_available(&self) -> Result<(), DsfError>;

    /// Decode a DSF binary file to text.
    fn decode(&self, dsf_path: &Path, text_path: &Path) -> Result<(), DsfError>;

    /// Encode a text file back to DSF binary.
    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError>;
}

/// Production implementation that shells out to DSFTool CLI.
pub struct DsfToolRunner;

impl DsfTool for DsfToolRunner {
    fn check_available(&self) -> Result<(), DsfError> {
        let result = Command::new("DSFTool")
            .arg("--version")
            .output();

        match result {
            Ok(output) if output.status.success() => Ok(()),
            // DSFTool may not have --version but still be findable
            Ok(_) => Ok(()),
            Err(_) => Err(DsfError::ToolNotFound),
        }
    }

    fn decode(&self, dsf_path: &Path, text_path: &Path) -> Result<(), DsfError> {
        let output = Command::new("DSFTool")
            .arg("--dsf2text")
            .arg(dsf_path)
            .arg(text_path)
            .output()
            .map_err(|_| DsfError::ToolNotFound)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DsfError::DecodeFailed {
                path: dsf_path.to_path_buf(),
                reason: stderr.to_string(),
            });
        }
        Ok(())
    }

    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError> {
        let output = Command::new("DSFTool")
            .arg("--text2dsf")
            .arg(text_path)
            .arg(dsf_path)
            .output()
            .map_err(|_| DsfError::ToolNotFound)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DsfError::EncodeFailed {
                path: dsf_path.to_path_buf(),
                reason: stderr.to_string(),
            });
        }
        Ok(())
    }
}
```

- [ ] **Step 3: Run full test suite to verify nothing is broken**

Run: `cargo test -p xearthlayer dsf:: 2>&1 | tail -20`
Expected: All previous tests still pass

- [ ] **Step 4: Commit**

```
feat(publisher/dsf): add DsfTool trait and DsfToolRunner

Trait abstraction over DSFTool CLI for decode/encode operations.
DsfToolRunner is the production implementation; trait enables
mock injection for testing without DSFTool installed.
```

---

## Task 6: DSF Processor (Pipeline Orchestrator)

**Files:**
- Create: `xearthlayer/src/publisher/dsf/processor.rs`
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write failing test for `DsfProcessor` using mock DsfTool**

```rust
// Add to xearthlayer/src/publisher/dsf/tests.rs
use super::processor::DsfProcessor;
use std::sync::Mutex;
use tempfile::TempDir;

/// Mock DsfTool that writes pre-canned DSF text during decode
/// and records encode calls.
struct MockDsfTool {
    /// DSF text content to write when decode() is called.
    decode_content: String,
    /// Records paths passed to encode().
    encode_calls: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl MockDsfTool {
    fn new(content: &str) -> Self {
        Self {
            decode_content: content.to_string(),
            encode_calls: Mutex::new(Vec::new()),
        }
    }
}

impl DsfTool for MockDsfTool {
    fn check_available(&self) -> Result<(), DsfError> {
        Ok(())
    }

    fn decode(&self, _dsf_path: &Path, text_path: &Path) -> Result<(), DsfError> {
        std::fs::write(text_path, &self.decode_content)?;
        Ok(())
    }

    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError> {
        // Copy the filtered text as-is to simulate encoding
        std::fs::copy(text_path, dsf_path)
            .map_err(|e| DsfError::EncodeFailed {
                path: dsf_path.to_path_buf(),
                reason: e.to_string(),
            })?;
        self.encode_calls
            .lock()
            .unwrap()
            .push((text_path.to_path_buf(), dsf_path.to_path_buf()));
        Ok(())
    }
}

#[test]
fn test_processor_modifies_dsf_with_target_zl() {
    let tmp = TempDir::new().unwrap();
    let dsf_path = tmp.path().join("test.dsf");
    std::fs::write(&dsf_path, b"fake dsf binary").unwrap();

    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let dsf_text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[(1, vertex), (2, vertex)],
    );

    let tool = MockDsfTool::new(&dsf_text);
    let processor = DsfProcessor::new(&tool);
    let result = processor.process_dsf_file(&dsf_path, 18, false).unwrap();

    assert!(result.is_some());
    let filter_result = result.unwrap();
    assert_eq!(filter_result.terrain_defs_removed, 1);
    assert_eq!(filter_result.patches_removed, 1);
    assert_eq!(tool.encode_calls.lock().unwrap().len(), 1);
}

#[test]
fn test_processor_skips_dsf_without_target_zl() {
    let tmp = TempDir::new().unwrap();
    let dsf_path = tmp.path().join("test.dsf");
    std::fs::write(&dsf_path, b"fake dsf binary").unwrap();

    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let dsf_text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter"],
        &[(1, vertex)],
    );

    let tool = MockDsfTool::new(&dsf_text);
    let processor = DsfProcessor::new(&tool);
    let result = processor.process_dsf_file(&dsf_path, 18, false).unwrap();

    assert!(result.is_none()); // No modifications needed
    assert_eq!(tool.encode_calls.lock().unwrap().len(), 0);
}

#[test]
fn test_processor_dry_run_does_not_modify() {
    let tmp = TempDir::new().unwrap();
    let dsf_path = tmp.path().join("test.dsf");
    std::fs::write(&dsf_path, b"fake dsf binary").unwrap();

    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let dsf_text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter", "terrain/400_800_BI18.ter"],
        &[(1, vertex), (2, vertex)],
    );

    let tool = MockDsfTool::new(&dsf_text);
    let processor = DsfProcessor::new(&tool);
    let result = processor.process_dsf_file(&dsf_path, 18, true).unwrap();

    assert!(result.is_some());
    // Dry run: should NOT encode
    assert_eq!(tool.encode_calls.lock().unwrap().len(), 0);
    // Original file should be unchanged
    assert_eq!(std::fs::read_to_string(&dsf_path).unwrap(), "fake dsf binary");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xearthlayer dsf::tests::test_processor_ -- --no-capture 2>&1 | tail -20`
Expected: FAIL — `DsfProcessor` not implemented

- [ ] **Step 3: Implement `DsfProcessor`**

```rust
// xearthlayer/src/publisher/dsf/processor.rs
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
    ///
    /// Returns `Some(FilterResult)` if the DSF contained the target ZL,
    /// `None` if no modifications were needed.
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
                // Success — remove backup
                let _ = std::fs::remove_file(&backup_path);
                Ok(Some(filter_result))
            }
            Err(e) => {
                // Restore backup
                let _ = std::fs::rename(&backup_path, dsf_path);
                Err(e)
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xearthlayer dsf::tests::test_processor_ -- --no-capture 2>&1 | tail -20`
Expected: All 3 tests PASS

- [ ] **Step 5: Run full DSF module test suite**

Run: `cargo test -p xearthlayer dsf:: 2>&1 | tail -20`
Expected: All tests PASS (parser + filter + processor)

- [ ] **Step 6: Commit**

```
feat(publisher/dsf): add DSF processor pipeline

DsfProcessor orchestrates decode → analyze → filter → encode per DSF
file with backup/restore safety. Uses DsfTool trait for external tool
dependency injection.
```

---

## Task 7: CLI Wiring and Service Implementation

**Files:**
- Modify: `xearthlayer-cli/src/commands/publish/args.rs`
- Modify: `xearthlayer-cli/src/commands/publish/handlers.rs`
- Modify: `xearthlayer-cli/src/commands/publish/output.rs`
- Modify: `xearthlayer-cli/src/commands/publish/mod.rs`
- Modify: `xearthlayer-cli/src/commands/publish/traits.rs`
- Modify: `xearthlayer-cli/src/commands/publish/services.rs`

Note: Tasks 7 and 8 from the original plan are merged into a single task to ensure
each commit compiles. Adding the handler without the trait method would break compilation.

- [ ] **Step 1: Add `RemoveZl` variant to `PublishCommands` enum in `args.rs`**

Add after the `Gaps` variant (around line 362):

```rust
    /// Remove a specific zoom level from a scenery package
    ///
    /// Modifies DSF files to strip terrain definitions and patches at the
    /// target zoom level, then removes orphaned .ter and .png files.
    /// Requires DSFTool on PATH.
    RemoveZl {
        /// Region code (e.g., "na", "eur", "asia")
        #[arg(long)]
        region: String,

        /// Zoom level to remove (12-19)
        #[arg(long, value_parser = clap::value_parser!(u8).range(12..=19))]
        zoom: u8,

        /// Limit operation to a specific 1°×1° tile (format: "lat,lon", e.g., "50,8")
        #[arg(long)]
        tile: Option<String>,

        /// Preview changes without modifying files
        #[arg(long)]
        dry_run: bool,

        /// Output a report file with details of changes
        #[arg(long)]
        report: Option<PathBuf>,

        /// Format for the report file (text or json)
        #[arg(long, value_enum, default_value = "text")]
        report_format: ReportFormatArg,

        /// Repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
```

- [ ] **Step 2: Add `RemoveZlArgs` struct in `args.rs`**

Add after `GapsArgs` (around line 495):

```rust
/// Arguments for the remove-zl command.
pub struct RemoveZlArgs {
    pub region: String,
    pub zoom: u8,
    pub tile: Option<String>,
    pub dry_run: bool,
    pub report: Option<PathBuf>,
    pub report_format: ReportFormatArg,
    pub repo: PathBuf,
}
```

- [ ] **Step 3: Add `print_remove_zl_result` to `output.rs`**

```rust
use xearthlayer::publisher::dsf::RemoveZlReport;

/// Print remove-zl results to the output.
pub fn print_remove_zl_result(out: &dyn Output, report: &RemoveZlReport) {
    out.header("Remove Zoom Level Results");
    out.newline();

    out.println(&format!("Scanned {} DSF files", report.dsf_files_scanned));
    out.indented(&format!(
        "Files containing ZL{}: {}",
        report.target_zoom, report.dsf_files_modified
    ));
    out.indented(&format!(
        "Terrain definitions removed: {}",
        report.terrain_defs_removed
    ));
    out.indented(&format!("Patches removed: {}", report.patches_removed));
    out.indented(&format!(".ter files removed: {}", report.ter_files_removed));
    out.indented(&format!(".png files removed: {}", report.png_files_removed));

    if !report.dsf_files_failed.is_empty() {
        out.newline();
        out.println(&format!("Failures: {}", report.dsf_files_failed.len()));
        for (path, error) in &report.dsf_files_failed {
            out.indented(&format!(
                "{}: {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                error
            ));
        }
    }
}
```

- [ ] **Step 4: Add `RemoveZlHandler` to `handlers.rs`**

```rust
use super::args::RemoveZlArgs;
use super::output::print_remove_zl_result;
use xearthlayer::publisher::dedupe::TileCoord;

/// Handler for the `publish remove-zl` command.
pub struct RemoveZlHandler;

impl CommandHandler for RemoveZlHandler {
    type Args = RemoveZlArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        // Parse tile filter if specified
        let tile = if let Some(ref tile_str) = args.tile {
            Some(
                TileCoord::parse(tile_str)
                    .map_err(|e| CliError::Publish(format!("Invalid tile coordinate: {}", e)))?,
            )
        } else {
            None
        };

        let repo = ctx.publisher.open_repository(&args.repo)?;

        ctx.output.println(&format!(
            "Removing ZL{} from {} ortho package",
            args.zoom,
            args.region.to_uppercase()
        ));
        if args.dry_run {
            ctx.output.indented("(dry run - no files will be modified)");
        }
        if let Some(ref tile_str) = args.tile {
            ctx.output
                .indented(&format!("Targeting tile: {}", tile_str));
        }
        ctx.output.newline();

        let report = ctx.publisher.remove_zoom_level(
            repo.as_ref(),
            &args.region,
            args.zoom,
            tile,
            args.dry_run,
        )?;

        // Print results
        print_remove_zl_result(ctx.output, &report);

        // Write report file if requested
        if let Some(ref report_path) = args.report {
            ctx.output.newline();
            ctx.output
                .println(&format!("Writing report to: {}", report_path.display()));

            let content = match args.report_format {
                ReportFormatArg::Json => report.to_json(),
                ReportFormatArg::Text => report.to_text(),
            };

            std::fs::write(report_path, content)
                .map_err(|e| CliError::Publish(format!("Failed to write report: {}", e)))?;
        }

        // Next steps
        ctx.output.newline();
        if report.dsf_files_modified > 0 && report.dry_run {
            ctx.output.println("Next steps:");
            ctx.output
                .indented("Run without --dry-run to apply changes");
        } else if report.dsf_files_modified > 0 && !report.dry_run {
            ctx.output.println("Note:");
            ctx.output
                .indented("Run 'publish build' to recreate archives with updated tiles");
        }

        Ok(())
    }
}
```

Note: The report serialization uses custom `to_json()`/`to_text()` methods on `RemoveZlReport`,
matching the existing `DedupeAuditReport` pattern — NOT serde. Add these methods to `RemoveZlReport`
in `types.rs`:

```rust
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
            format!("Remove ZL{} Report — {}", self.target_zoom, self.region.to_uppercase()),
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
```

- [ ] **Step 5: Add `remove_zoom_level` to `PublisherService` trait in `traits.rs`**

Add after the `analyze_gaps` method:

```rust
    /// Remove a specific zoom level from a scenery package.
    fn remove_zoom_level(
        &self,
        repo: &dyn RepositoryOperations,
        region: &str,
        target_zoom: u8,
        tile: Option<TileCoord>,
        dry_run: bool,
    ) -> Result<RemoveZlReport, CliError>;
```

Add import: `use xearthlayer::publisher::dsf::RemoveZlReport;`

- [ ] **Step 6: Add mock stub for `MockPublisherService`**

Find the mock implementation in `xearthlayer-cli/src/commands/publish/tests.rs` and add:

```rust
fn remove_zoom_level(
    &self,
    _repo: &dyn RepositoryOperations,
    _region: &str,
    _target_zoom: u8,
    _tile: Option<TileCoord>,
    dry_run: bool,
) -> Result<RemoveZlReport, CliError> {
    Ok(RemoveZlReport {
        region: String::new(),
        target_zoom: 18,
        dsf_files_scanned: 0,
        dsf_files_modified: 0,
        dsf_files_failed: vec![],
        terrain_defs_removed: 0,
        patches_removed: 0,
        ter_files_removed: 0,
        png_files_removed: 0,
        dry_run,
    })
}
```

- [ ] **Step 7: Add dispatch arm in `mod.rs`**

Add after the `Gaps` dispatch arm (around line 253):

```rust
        PublishCommands::RemoveZl {
            region,
            zoom,
            tile,
            dry_run,
            report,
            report_format,
            repo,
        } => RemoveZlHandler::execute(
            RemoveZlArgs {
                region,
                zoom,
                tile,
                dry_run,
                report,
                report_format,
                repo,
            },
            &ctx,
        ),
```

Add the imports at the top of `mod.rs` and update the re-export blocks:
```rust
use handlers::RemoveZlHandler;
use args::RemoveZlArgs;
```

Also update the `pub use handlers::{...}` block to include `RemoveZlHandler`,
and the `use args::{...}` block to include `RemoveZlArgs`.

- [ ] **Step 8: Implement `remove_zoom_level` on `DefaultPublisherService` in `services.rs`**

```rust
use xearthlayer::publisher::dsf::{DsfProcessor, DsfToolRunner, DsfTool, RemoveZlReport};
use xearthlayer::publisher::dedupe::TileCoord;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn remove_zoom_level(
    &self,
    repo: &dyn RepositoryOperations,
    region: &str,
    target_zoom: u8,
    tile: Option<TileCoord>,
    dry_run: bool,
) -> Result<RemoveZlReport, CliError> {
    let package_dir = repo.package_dir(region, PackageType::Ortho);
    if !package_dir.exists() {
        return Err(CliError::Publish(format!(
            "Package not found: {} ortho",
            region.to_uppercase()
        )));
    }

    // Check DSFTool availability
    let tool = DsfToolRunner;
    tool.check_available()
        .map_err(|e| CliError::Publish(e.to_string()))?;

    // Find all DSF files in Earth nav data/
    let earth_nav = package_dir.join("Earth nav data");
    if !earth_nav.exists() {
        return Err(CliError::Publish(format!(
            "Earth nav data directory not found in {}",
            package_dir.display()
        )));
    }

    let dsf_files: Vec<PathBuf> = Self::find_dsf_files(&earth_nav, tile.as_ref())?;
    let dsf_files_scanned = dsf_files.len();

    // Process DSF files in parallel
    let dsf_files_modified = AtomicUsize::new(0);
    let terrain_defs_removed = AtomicUsize::new(0);
    let patches_removed = AtomicUsize::new(0);
    let dsf_files_failed: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

    dsf_files.par_iter().for_each(|dsf_path| {
        let processor = DsfProcessor::new(&tool);
        match processor.process_dsf_file(dsf_path, target_zoom, dry_run) {
            Ok(Some(result)) => {
                dsf_files_modified.fetch_add(1, Ordering::Relaxed);
                terrain_defs_removed.fetch_add(result.terrain_defs_removed, Ordering::Relaxed);
                patches_removed.fetch_add(result.patches_removed, Ordering::Relaxed);
            }
            Ok(None) => {} // No target ZL in this DSF
            Err(e) => {
                dsf_files_failed
                    .lock()
                    .unwrap()
                    .push((dsf_path.clone(), e.to_string()));
            }
        }
    });

    // Clean up (or count) orphaned .ter and .png files
    let (ter_removed, png_removed) = Self::process_orphan_files(&package_dir, target_zoom, dry_run)?;

    Ok(RemoveZlReport {
        region: region.to_string(),
        target_zoom,
        dsf_files_scanned,
        dsf_files_modified: dsf_files_modified.load(Ordering::Relaxed),
        dsf_files_failed: dsf_files_failed.into_inner().unwrap(),
        terrain_defs_removed: terrain_defs_removed.load(Ordering::Relaxed),
        patches_removed: patches_removed.load(Ordering::Relaxed),
        ter_files_removed: ter_removed,
        png_files_removed: png_removed,
        dry_run,
    })
}
```

- [ ] **Step 9: Add helper methods for DSF file discovery and cleanup**

Add as private methods on `DefaultPublisherService`:

```rust
/// Find all .dsf files, optionally filtering by tile coordinate.
fn find_dsf_files(
    earth_nav: &Path,
    tile: Option<&TileCoord>,
) -> Result<Vec<PathBuf>, CliError> {
    let mut dsf_files = Vec::new();

    for region_entry in std::fs::read_dir(earth_nav)
        .map_err(|e| CliError::Publish(format!("Failed to read Earth nav data: {}", e)))?
    {
        let region_dir = region_entry
            .map_err(|e| CliError::Publish(e.to_string()))?
            .path();
        if !region_dir.is_dir() {
            continue;
        }

        for dsf_entry in std::fs::read_dir(&region_dir)
            .map_err(|e| CliError::Publish(e.to_string()))?
        {
            let dsf_path = dsf_entry
                .map_err(|e| CliError::Publish(e.to_string()))?
                .path();
            if dsf_path.extension().is_some_and(|e| e == "dsf") {
                // Apply tile filter if specified
                if let Some(tile_coord) = tile {
                    if let Some(name) = dsf_path.file_stem().and_then(|n| n.to_str()) {
                        if name != tile_coord.to_xplane_name() {
                            continue;
                        }
                    }
                }
                dsf_files.push(dsf_path);
            }
        }
    }

    Ok(dsf_files)
}

/// Remove or count orphaned .ter and .png files for a zoom level.
/// When dry_run is true, counts without deleting.
fn process_orphan_files(
    package_dir: &Path,
    target_zoom: u8,
    dry_run: bool,
) -> Result<(usize, usize), CliError> {
    let terrain_dir = package_dir.join("terrain");
    let textures_dir = package_dir.join("textures");

    let mut ter_count = 0;
    if terrain_dir.exists() {
        for entry in std::fs::read_dir(&terrain_dir)
            .map_err(|e| CliError::Publish(e.to_string()))?
        {
            let path = entry.map_err(|e| CliError::Publish(e.to_string()))?.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".ter") && Self::ter_matches_zoom(name, target_zoom) {
                    if !dry_run {
                        std::fs::remove_file(&path)
                            .map_err(|e| CliError::Publish(e.to_string()))?;
                    }
                    ter_count += 1;
                }
            }
        }
    }

    let mut png_count = 0;
    if textures_dir.exists() {
        let zl_pattern = format!("_ZL{}.png", target_zoom);
        for entry in std::fs::read_dir(&textures_dir)
            .map_err(|e| CliError::Publish(e.to_string()))?
        {
            let path = entry.map_err(|e| CliError::Publish(e.to_string()))?.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(&zl_pattern) {
                    if !dry_run {
                        std::fs::remove_file(&path)
                            .map_err(|e| CliError::Publish(e.to_string()))?;
                    }
                    png_count += 1;
                }
            }
        }
    }

    Ok((ter_count, png_count))
}

/// Check if a .ter filename matches a target zoom level.
/// Handles provider-based naming: BI18, GO218, etc.
fn ter_matches_zoom(filename: &str, target_zoom: u8) -> bool {
    use xearthlayer::publisher::dsf::parse_terrain_def_zoom;
    // Wrap with "terrain/" prefix since parse_terrain_def_zoom expects it
    let full_name = format!("terrain/{}", filename);
    parse_terrain_def_zoom(&full_name) == Some(target_zoom)
}
```

Update the `remove_zoom_level` implementation to call `process_orphan_files` instead of
separate cleanup/count functions:

```rust
let (ter_removed, png_removed) = Self::process_orphan_files(&package_dir, target_zoom, dry_run)?;
```

- [ ] **Step 10: Verify it compiles**

Run: `cargo check -p xearthlayer-cli 2>&1 | head -30`
Expected: Compiles

- [ ] **Step 11: Commit**

```
feat(cli): add publish remove-zl command with service implementation

CLI args, handler, output formatting, dispatch, trait method, and
DefaultPublisherService::remove_zoom_level. Orchestrates parallel DSF
processing via Rayon with file cleanup for orphaned .ter/.png files.
```

---

## Task 8: Integration Test with Real DSFTool

**Files:**
- Modify: `xearthlayer/src/publisher/dsf/tests.rs`

- [ ] **Step 1: Write an integration test that uses real DSFTool**

```rust
// Add to xearthlayer/src/publisher/dsf/tests.rs

/// Integration test using real DSFTool binary.
/// Run with: cargo test -p xearthlayer test_real_dsftool -- --ignored
#[test]
#[ignore] // Requires DSFTool on PATH
fn test_real_dsftool_roundtrip() {
    let tool = DsfToolRunner;
    if tool.check_available().is_err() {
        eprintln!("DSFTool not available, skipping integration test");
        return;
    }

    // Use a known DSF from the EU package for testing
    let test_dsf = Path::new("/media/FlightSim/XEarthLayer Packages/zzXEL_na_ortho/Earth nav data/+30-100/+30-096.dsf");
    if !test_dsf.exists() {
        eprintln!("Test DSF not found, skipping");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let work_dsf = tmp.path().join("test.dsf");
    std::fs::copy(test_dsf, &work_dsf).unwrap();

    let processor = DsfProcessor::new(&tool);

    // Dry run first — verify analysis
    let result = processor.process_dsf_file(&work_dsf, 18, true).unwrap();
    if let Some(ref filter_result) = result {
        assert!(filter_result.terrain_defs_removed > 0, "Expected ZL18 terrain defs");
        assert!(filter_result.patches_removed > 0, "Expected ZL18 patches");
    }

    // Actual run — verify DSF is modified and still valid
    let result = processor.process_dsf_file(&work_dsf, 18, false).unwrap();
    assert!(result.is_some());

    // Verify the modified DSF can be decoded again
    let verify_text = tmp.path().join("verify.txt");
    tool.decode(&work_dsf, &verify_text).unwrap();

    let content = std::fs::read_to_string(&verify_text).unwrap();
    assert!(!content.contains("BI18"), "ZL18 terrain should be removed");
    assert!(content.contains("BI16"), "ZL16 terrain should be preserved");
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p xearthlayer test_real_dsftool_roundtrip -- --ignored --no-capture 2>&1 | tail -30`
Expected: PASS (on your machine with DSFTool installed)

- [ ] **Step 3: Commit**

```
test(publisher/dsf): add DSFTool integration test

Integration test verifies full decode → filter → encode roundtrip
using real DSFTool binary. Gated behind #[ignore] for CI.
```

---

## Task 9: Full Pre-Commit Verification and Documentation

**Files:**
- Modify: `/media/Disk6/Projects/xearthlayer/CLAUDE.md`

- [ ] **Step 1: Run `make pre-commit`**

Run: `make pre-commit`
Expected: Format + lint + all tests pass

- [ ] **Step 2: Fix any clippy warnings or test failures**

Address any issues found by clippy or failing tests.

- [ ] **Step 3: Update CLAUDE.md CLI commands section**

Add the new command to the CLI commands table:

```
# Publisher commands
xearthlayer publish remove-zl --region <code> --zoom <level> [--tile <lat,lon>] [--dry-run]  # Remove zoom level from package
```

- [ ] **Step 4: Update CLAUDE.md architecture section**

Add brief description of the `publisher::dsf` module to the Implemented Components section and update the Key Files table.

- [ ] **Step 5: Final commit**

```
docs: add publish remove-zl to CLAUDE.md

Document new command and publisher::dsf module in project reference.
```

---

## Task 10: Manual Validation on EU Package

This task is manual — not automated.

- [ ] **Step 1: Restore the EU package from archives** (you deleted the ZL18 .ter files earlier)

- [ ] **Step 2: Run dry-run on a single tile**

```bash
xearthlayer publish remove-zl --region eu --zoom 18 --tile 50,8 --dry-run --repo "/path/to/repo"
```

Verify the output shows correct counts for that 1°×1° tile.

- [ ] **Step 3: Run actual removal on the single tile**

```bash
xearthlayer publish remove-zl --region eu --zoom 18 --tile 50,8 --repo "/path/to/repo"
```

- [ ] **Step 4: Test in X-Plane**

Load a flight near the modified tile (e.g., Frankfurt EDDF at 50°N, 8°E). Verify scenery loads without errors.

- [ ] **Step 5: If successful, run on full EU package**

```bash
xearthlayer publish remove-zl --region eu --zoom 18 --repo "/path/to/repo"
```
