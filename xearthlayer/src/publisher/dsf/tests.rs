use super::filter::DsfZoomFilter;
use super::parser::parse_terrain_def_zoom;
use super::parser::DsfTextParser;
use super::processor::DsfProcessor;
use super::tool::{DsfTool, DsfToolRunner};
use super::types::DsfError;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

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
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
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
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
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

#[test]
fn test_parse_bing_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/22112_34224_BI16.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_bing_zl18() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_bing_zl18_sea() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18_sea.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_bing_zl18_sea_overlay() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18_sea_overlay.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_go2_zl18() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/38800_80512_GO218.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_go2_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/25264_10368_GO216.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_google_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/25264_10368_GO16.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_terrain_water() {
    assert_eq!(parse_terrain_def_zoom("terrain_Water"), None);
}

#[test]
fn test_parse_no_zoom() {
    assert_eq!(parse_terrain_def_zoom("terrain/unknown_format.ter"), None);
}

#[test]
fn test_parse_bare_filename_matches() {
    let full = format!("terrain/{}", "88416_136896_BI18_sea.ter");
    assert_eq!(parse_terrain_def_zoom(&full), Some(18));
}

#[test]
fn test_filter_removes_zl18_terrain_defs() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
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
            "terrain_Water",
            "terrain/100_200_BI18.ter",
            "terrain/100_200_BI16.ter",
        ],
        &[(2, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    let output_str = String::from_utf8(output).unwrap();
    assert!(output_str.contains("BEGIN_PATCH 1 "));
    assert!(!output_str.contains("BEGIN_PATCH 2 "));
}

#[test]
fn test_filter_preserves_non_target_patches() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
        &[(0, vertex), (1, vertex), (2, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.patches_removed, 1);
    let output_str = String::from_utf8(output).unwrap();
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
            "terrain_Water",
            "terrain/a_b_BI18.ter",
            "terrain/c_d_BI18_sea.ter",
            "terrain/e_f_BI16.ter",
        ],
        &[(3, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 2);
    let output_str = String::from_utf8(output).unwrap();
    // Index 3 (terrain/e_f_BI16.ter) should be remapped to index 1
    assert!(output_str.contains("BEGIN_PATCH 1 "));
}

// ─── DsfProcessor tests ──────────────────────────────────────────────────────

struct MockDsfTool {
    decode_content: String,
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
        std::fs::copy(text_path, dsf_path).map_err(|e| DsfError::EncodeFailed {
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
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
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

    assert!(result.is_none());
    assert_eq!(tool.encode_calls.lock().unwrap().len(), 0);
}

#[test]
fn test_processor_dry_run_does_not_modify() {
    let tmp = TempDir::new().unwrap();
    let dsf_path = tmp.path().join("test.dsf");
    std::fs::write(&dsf_path, b"fake dsf binary").unwrap();

    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let dsf_text = make_dsf_text(
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
        ],
        &[(1, vertex), (2, vertex)],
    );

    let tool = MockDsfTool::new(&dsf_text);
    let processor = DsfProcessor::new(&tool);
    let result = processor.process_dsf_file(&dsf_path, 18, true).unwrap();

    assert!(result.is_some());
    assert_eq!(tool.encode_calls.lock().unwrap().len(), 0);
    assert_eq!(
        std::fs::read_to_string(&dsf_path).unwrap(),
        "fake dsf binary"
    );
}

/// Integration test using real DSFTool binary.
/// Run with: cargo test -p xearthlayer test_real_dsftool -- --ignored --nocapture
#[test]
#[ignore] // Requires DSFTool on PATH
fn test_real_dsftool_roundtrip() {
    let tool = DsfToolRunner;
    if tool.check_available().is_err() {
        eprintln!("DSFTool not available, skipping integration test");
        return;
    }

    // Use a known DSF from the NA package for testing
    let test_dsf = Path::new(
        "/media/FlightSim/XEarthLayer Packages/zzXEL_na_ortho/Earth nav data/+30-100/+30-096.dsf",
    );
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
        assert!(
            filter_result.terrain_defs_removed > 0,
            "Expected ZL18 terrain defs"
        );
        assert!(filter_result.patches_removed > 0, "Expected ZL18 patches");
        eprintln!(
            "Dry run: would remove {} terrain defs, {} patches",
            filter_result.terrain_defs_removed, filter_result.patches_removed
        );
    } else {
        eprintln!("No ZL18 found in this DSF — test inconclusive");
        return;
    }

    // Actual run — verify DSF is modified and still valid
    let result = processor.process_dsf_file(&work_dsf, 18, false).unwrap();
    assert!(result.is_some());

    // Verify the modified DSF can be decoded again (valid binary)
    let verify_text = tmp.path().join("verify.txt");
    tool.decode(&work_dsf, &verify_text).unwrap();

    let content = std::fs::read_to_string(&verify_text).unwrap();
    assert!(
        !content.contains("BI18"),
        "ZL18 terrain should be removed from output"
    );
    assert!(
        content.contains("BI16") || content.contains("GO216"),
        "ZL16 terrain should be preserved"
    );
    assert!(
        content.contains("TERRAIN_DEF"),
        "Should still have terrain definitions"
    );
    eprintln!("Integration test passed: DSF roundtrip successful");
}

/// Regression test: FilterResult must return the names of removed TERRAIN_DEFs.
/// This is used by the service layer to scope .ter/.png cleanup to only the
/// files referenced by processed DSFs (critical for --tile mode).
#[test]
fn test_filter_returns_removed_terrain_names() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &[
            "terrain_Water",
            "terrain/100_200_BI16.ter",
            "terrain/400_800_BI18.ter",
            "terrain/500_900_BI18_sea.ter",
        ],
        &[(1, vertex), (2, vertex), (3, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 2);
    assert_eq!(result.removed_terrain_names.len(), 2);
    assert!(result
        .removed_terrain_names
        .contains(&"terrain/400_800_BI18.ter".to_string()));
    assert!(result
        .removed_terrain_names
        .contains(&"terrain/500_900_BI18_sea.ter".to_string()));
}

/// Regression test: when no target ZL is found, removed_terrain_names must be empty.
#[test]
fn test_filter_noop_returns_empty_terrain_names() {
    let vertex = "PATCH_VERTEX 8.5 50.5 100.0 0.0 0.0 1.0 0.5";
    let text = make_dsf_text(
        &["terrain_Water", "terrain/100_200_BI16.ter"],
        &[(1, vertex)],
    );

    let reader = BufReader::new(text.as_bytes());
    let mut output = Vec::new();
    let result = DsfZoomFilter::filter(reader, &mut output, 18).unwrap();

    assert_eq!(result.terrain_defs_removed, 0);
    assert!(result.removed_terrain_names.is_empty());
}
