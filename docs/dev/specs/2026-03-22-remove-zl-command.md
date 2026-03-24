# Remove Zoom Level Command Design

**Date:** 2026-03-22
**Status:** Draft

## Problem

XEarthLayer scenery packages include ZL18 tiles around airfields, inherited from Ortho4XP's default behavior of generating higher zoom levels near airports. With XEarthLayer streaming tiles on-demand, these pre-baked ZL18 tiles are redundant — they consume unnecessary download bandwidth and can introduce visual artifacts (seam lines at zoom level boundaries) during flight.

The ZL18 tiles are **overlays** on top of a complete ZL16 mesh. The DSF files contain both ZL16 and ZL18 terrain patches, with ZL18 drawn on top. Removing ZL18 requires modifying the DSF binary files — simply deleting `.ter` files causes X-Plane to abort loading entire 1°×1° DSF regions (`DSF canceled due to missing art assets`).

## Solution

A new `publish remove-zl` command that strips a specified zoom level from scenery packages by:

1. Decoding DSF files to text via `DSFTool --dsf2text`
2. Removing the target zoom level's `TERRAIN_DEF` entries and associated `BEGIN_PATCH...END_PATCH` blocks
3. Reindexing remaining `BEGIN_PATCH` terrain references to account for removed definitions
4. Re-encoding via `DSFTool --text2dsf`
5. Cleaning up orphaned `.ter` and `.png` files

## CLI Interface

```
xearthlayer publish remove-zl \
    --region <code> \
    --zoom <level> \
    [--tile <lat,lon>] \
    [--dry-run] \
    [--report <path>] \
    [--report-format <text|json>] \
    [--repo <path>]
```

### Arguments

| Argument | Required | Default | Description |
|----------|----------|---------|-------------|
| `--region` | Yes | — | Target package region code (e.g., `eu`, `na`, `sa`) |
| `--zoom` | Yes | — | Zoom level to remove (validated: 12-19) |
| `--tile` | No | — | Limit to single 1°×1° DSF tile (`lat,lon` format) |
| `--dry-run` | No | `false` | Preview changes without modifying files |
| `--report` | No | — | Write audit report to file |
| `--report-format` | No | `text` | Report format (`text` or `json`) |
| `--repo` | No | `.` | Repository path |

### Behavior

- **`--dry-run` is opt-in** — consistent with `dedupe` command. Omitting it executes the removal.
- **`--zoom` is validated** against `MIN_ZOOM_LEVEL` (12) and `MAX_ZOOM_LEVEL` (19) from `publisher::dedupe::types`.
- **`--tile` filter** — enables testing on a single DSF before processing the full package.
- Package type is always `ortho` — overlays don't contain zoom-level terrain.

### `RemoveZlArgs`

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

## Architecture

### Components

```
CLI Layer (xearthlayer-cli)
  └─ RemoveZlHandler (commands/publish/handlers.rs)
       └─ PublisherService::remove_zoom_level() (commands/publish/traits.rs)
            └─ DefaultPublisherService (commands/publish/services.rs)
                 └─ publisher::dsf module (xearthlayer/src/publisher/dsf/)
                      ├─ DsfTool trait + DsfToolRunner — shell out to DSFTool
                      ├─ DsfTextParser — parse decoded DSF text from reader
                      ├─ DsfZoomFilter — identify and remove target ZL in memory
                      └─ DsfError — error types
```

### `PublisherService` trait method

```rust
/// Remove a specific zoom level from a scenery package.
///
/// Modifies DSF files to strip terrain definitions and patches at the target
/// zoom level, then removes orphaned .ter and .png files.
fn remove_zoom_level(
    &self,
    repo: &dyn RepositoryOperations,
    region: &str,
    target_zoom: u8,
    tile: Option<TileCoord>,
    dry_run: bool,
) -> Result<RemoveZlReport, CliError>;
```

### New Module: `publisher::dsf`

Located at `xearthlayer/src/publisher/dsf/`. Responsible for DSF text manipulation.

#### `DsfError`

```rust
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
```

#### `DsfTool` trait + `DsfToolRunner`

Trait abstraction for testability — allows mocking DSFTool in unit tests:

```rust
pub trait DsfTool: Send + Sync {
    /// Check that DSFTool is available.
    fn check_available(&self) -> Result<(), DsfError>;

    /// Decode DSF binary to text file.
    fn decode(&self, dsf_path: &Path, text_path: &Path) -> Result<(), DsfError>;

    /// Encode text file back to DSF binary.
    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError>;
}

/// Production implementation that shells out to DSFTool CLI.
pub struct DsfToolRunner;

impl DsfTool for DsfToolRunner {
    // ... shells out to `DSFTool --dsf2text` / `DSFTool --text2dsf`
}
```

#### `DsfTextParser`

Parses the text format produced by `DSFTool --dsf2text`. Accepts `impl BufRead` for testability:

```rust
pub struct TerrainDef {
    pub index: usize,
    pub name: String,       // e.g., "terrain/88416_136896_BI18_sea.ter"
    pub zoom_level: Option<u8>,  // Extracted from name, None for terrain_Water
}

pub struct DsfAnalysis {
    pub terrain_defs: Vec<TerrainDef>,
    pub total_patches: usize,
    pub patches_by_zoom: HashMap<u8, usize>,
}

impl DsfTextParser {
    /// Analyze a decoded DSF text stream without modifying it.
    pub fn analyze(reader: impl BufRead) -> Result<DsfAnalysis, DsfError>;
}
```

#### `DsfZoomFilter`

Performs the text transformation in memory. Reads all lines into a `Vec<String>`, filters, and writes output:

```rust
pub struct FilterResult {
    pub terrain_defs_removed: usize,
    pub patches_removed: usize,
}

impl DsfZoomFilter {
    /// Filter decoded DSF text, removing all references to the target zoom level.
    ///
    /// Reads from `reader`, writes filtered output to `writer`.
    ///
    /// Algorithm:
    /// 1. Read all lines into memory
    /// 2. Identify TERRAIN_DEF indices that match the target ZL
    /// 3. Build an index remapping table (old index → new index)
    /// 4. Write output, skipping target ZL TERRAIN_DEFs and their patches,
    ///    remapping BEGIN_PATCH indices for surviving patches
    pub fn filter(
        reader: impl BufRead,
        writer: impl Write,
        target_zoom: u8,
    ) -> Result<FilterResult, DsfError>;
}
```

### Zoom Level Detection from TERRAIN_DEF Paths

The zoom level is extracted from `TERRAIN_DEF` path strings in the DSF text output. This is **not** the same as parsing `.ter` file contents — it operates on the filename component of the path.

**Pattern:** `terrain/<row>_<col>_<provider><zoom>[_sea][_sea_overlay].ter`

Examples:
- `terrain/88416_136896_BI18_sea.ter` → zoom 18 (Bing)
- `terrain/38800_80512_GO218.ter` → zoom 18 (Google GO2)
- `terrain/22112_34224_BI16.ter` → zoom 16 (Bing)
- `terrain_Water` → no zoom level (always preserved)

A new shared utility `parse_terrain_def_zoom(name: &str) -> Option<u8>` will be added to `publisher::dsf::types`. This extracts the zoom level from the TERRAIN_DEF path string using the same provider/zoom regex pattern as the dedupe module's DDS filename parser, adapted for `.ter` naming conventions (handling `_sea`, `_sea_overlay` suffixes and `.ter` extension).

### Index Remapping

When `TERRAIN_DEF` entries are removed, all subsequent indices shift. The `BEGIN_PATCH <index>` references must be remapped:

```
Before removal (indices 0-5):        After removing index 2 and 3:
  0: terrain_Water                      0: terrain_Water        (unchanged)
  1: terrain/tile_A_BI16.ter            1: terrain/tile_A_BI16.ter (unchanged)
  2: terrain/tile_B_BI18.ter  ← remove
  3: terrain/tile_C_BI18.ter  ← remove
  4: terrain/tile_D_BI16.ter            2: terrain/tile_D_BI16.ter (was 4 → now 2)
  5: terrain/tile_E_BI16.ter            3: terrain/tile_E_BI16.ter (was 5 → now 3)

Remap table: {0→0, 1→1, 4→2, 5→3}
```

Patches referencing index 2 or 3 are removed entirely (the full `BEGIN_PATCH...END_PATCH` block). Patches referencing index 4 become `BEGIN_PATCH 2 ...`.

### File Cleanup

After DSF processing, orphaned files are removed:

- `.ter` files matching the target zoom level provider pattern in `terrain/` (e.g., `*BI18*.ter`, `*GO218*.ter`)
- `.png` files matching `*_ZL{zoom}.png` in `textures/` (e.g., `*_ZL18.png`)

Note: `.ter` files use provider-based naming (`BI18`, `GO218`) while `.png` files use generic zoom naming (`ZL18`). Both patterns must be handled.

### Processing Pipeline (per DSF file)

```
1. DSFTool --dsf2text  input.dsf  →  /tmp/xel_work_XXXX/decoded.txt
2. DsfZoomFilter::filter(decoded.txt → filtered.txt, target_zl)
3. cp input.dsf  input.dsf.bak
4. DSFTool --text2dsf  filtered.txt  →  input.dsf
5. If text2dsf fails: mv input.dsf.bak input.dsf (restore)
6. On success: remove input.dsf.bak
7. Clean up temp directory
```

### Parallelism

DSF files within a package are independent. Processing uses Rayon `par_iter()` over the list of DSF files that contain target-ZL terrain definitions. The `--tile` filter reduces the set to a single DSF for testing.

Each parallel worker uses its own temp directory to avoid collisions.

## Report Output

### `RemoveZlReport`

```rust
pub struct RemoveZlReport {
    pub region: String,
    pub target_zoom: u8,
    pub dsf_files_scanned: usize,
    pub dsf_files_modified: usize,
    pub dsf_files_failed: Vec<(PathBuf, String)>,  // (path, error message)
    pub terrain_defs_removed: usize,
    pub patches_removed: usize,
    pub ter_files_removed: usize,
    pub png_files_removed: usize,
    pub dry_run: bool,
}
```

### Console Output Example (dry-run)

```
Removing ZL18 from EU ortho package
  (dry run - no files will be modified)

Scanned 847 DSF files
  Files containing ZL18: 312
  Terrain definitions to remove: 4,821
  Patches to remove: 6,142
  .ter files to remove: 21,280
  .png files to remove: 11,706

Next steps:
  Run without --dry-run to apply changes
```

### Console Output Example (execution with failures)

```
Removing ZL18 from EU ortho package

Processed 312 / 312 DSF files
  Terrain definitions removed: 4,821
  Patches removed: 6,142
  .ter files removed: 21,280
  .png files removed: 11,706
  Failures: 2
    +50+008.dsf: DSFTool encode failed (exit code 1)
    +51+003.dsf: I/O error: permission denied

Note:
  Run 'publish build' to recreate archives with updated tiles
```

## Error Handling

| Error | Behavior |
|-------|----------|
| `DSFTool` not on PATH | Fail immediately with clear message |
| `--zoom` out of range (12-19) | Fail at argument parsing with validation error |
| `dsf2text` fails | Skip DSF file, record in `dsf_files_failed`, continue |
| `text2dsf` fails | Restore `.dsf.bak`, record in `dsf_files_failed`, continue |
| No target ZL found in DSF | Skip silently (not an error) |
| Package directory not found | Fail with error |

Processing is **continue-on-error** per DSF file — a failure in one DSF doesn't abort the entire package. Failed DSFs are tracked in `RemoveZlReport::dsf_files_failed`.

## Testing Strategy

- **Unit tests** for `parse_terrain_def_zoom()` with all provider/suffix combinations
- **Unit tests** for `DsfTextParser::analyze()` and `DsfZoomFilter::filter()` using in-memory readers/writers with synthetic DSF text
- **Unit tests** for index remapping correctness (edge cases: consecutive removals, first/last index, all removed)
- **Integration tests** for the full pipeline using a mock `DsfTool` trait implementation
- **CLI tests** for argument parsing, zoom validation, and dry-run output

DSFTool-dependent integration tests (real decode → filter → encode) are gated behind `#[ignore]` since DSFTool may not be available in CI.

## Future Considerations

- **Native Rust DSF parser**: Replace `DsfToolRunner` with a native implementation using the [xptools C spec](https://github.com/X-Plane/xptools/tree/master/src/DSF). The `DsfTool` trait boundary makes this a drop-in replacement.
- **Multiple zoom levels**: Extend `--zoom` to accept a comma-separated list. Internal API already uses `u8` which can be extended to `&[u8]`.
- **Integration with `publish build`**: Optionally run `remove-zl` as part of the build pipeline.
- **Progress reporting**: Add progress indicator for large packages (counter of DSF files processed).
