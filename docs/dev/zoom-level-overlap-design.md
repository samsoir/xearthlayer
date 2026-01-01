# Zoom Level Overlap Management Design

This document describes the design for detecting and resolving overlapping zoom level tiles in XEarthLayer scenery packages.

## Problem Statement

Ortho4XP generates terrain at different zoom levels (ZL) for different areas. The supported range is **ZL12 through ZL19**, with common configurations being:

| Zoom Level | Ground Resolution | Typical Use |
|------------|------------------|-------------|
| ZL12 | ~4.8km | Remote/ocean areas |
| ZL14 | ~1.2km | Rural areas |
| ZL16 | ~300m | Default terrain |
| ZL17 | ~150m | Cities, POIs |
| ZL18 | ~75m | Airports, detailed areas |
| ZL19 | ~38m | Ultra-high detail |

When multiple zoom levels exist for the same geographic area, X-Plane loads all terrain triangles. This causes **Z-fighting** (depth buffer conflicts) that manifests as visual striping artifacts, particularly noticeable at low altitudes.

### Root Cause

Each `.ter` file defines terrain patches with mesh triangles. When two `.ter` files at different zoom levels cover the same area:

```
ZL18 tile: 100032_42688_BI18.ter  → LOAD_CENTER 39.15562 -121.36597
ZL16 tile: 25008_10672_BI16.ter   → LOAD_CENTER 39.13006 -121.33301
```

Both reference overlapping geographic areas. X-Plane renders both, causing Z-buffer thrash.

### Measured Impact

In the NA ortho package (as of December 2024):
- Total tiles: 616,980
- ZL16 tiles: ~600,000+
- ZL18 tiles: ~16,000+
- **ZL16 tiles with partial ZL18 overlap: 2,165**
- **Missing ZL18 tiles to complete coverage: 32,475**

### Critical Discovery: Gap Protection

Initial implementation revealed a critical issue: **blindly removing ZL16 tiles when any ZL18 child exists creates gaps**. Each ZL16 tile has 16 potential ZL18 children (4×4 grid). If only 1-15 children exist, removing the ZL16 parent creates visible holes in the scenery.

| Coverage | Safe to Remove Parent? | Reason |
|----------|----------------------|--------|
| 16/16 (100%) | ✅ Yes | All children exist, no gaps |
| 1-15/16 (6-94%) | ❌ No | Missing children = visible gaps |
| 0/16 (0%) | N/A | No overlap, nothing to dedupe |

The deduplication algorithm now uses "parent-centric" detection: for each lower ZL tile, count ALL existing higher ZL children before marking an overlap as safe to resolve.

## Design Goals

1. **Detection**: Identify overlapping zoom level tiles across the full ZL12-ZL19 range
2. **Resolution**: Remove redundant lower-resolution tiles where higher-resolution exists
3. **Flexibility**: Allow content creators to choose priority zoom level
4. **Non-destructive**: Never modify Ortho4XP source files
5. **Auditability**: Report all changes made during processing

## Coordinate Relationship

Adjacent zoom levels follow a 4:1 coordinate ratio (2× in each axis):

```
Tile at ZL(N) (row, col) → ZL(N-2) (row ÷ 4, col ÷ 4)
```

This relationship cascades across the full range:

```
ZL19 → ZL17 → ZL15 → ZL13
ZL18 → ZL16 → ZL14 → ZL12

Example chain:
  ZL18 (100032, 42688)
    → ZL16 (25008, 10672)
    → ZL14 (6252, 2668)
    → ZL12 (1563, 667)
```

A single lower-ZL tile covers the same area as 16 tiles at ZL+2 (4×4 grid).

### Detecting Overlaps Between Any Two Zoom Levels

For tiles at ZL(high) and ZL(low) where high > low:

```
scale_factor = 4^((high - low) / 2)

ZL(high) (row, col) overlaps ZL(low) if:
  ZL(low) exists at (row ÷ scale_factor, col ÷ scale_factor)
```

Example: ZL18 tile at (100032, 42688) vs ZL14:
```
scale = 4^((18-14)/2) = 4^2 = 16
ZL14 coords = (100032 ÷ 16, 42688 ÷ 16) = (6252, 2668)
```

## CLI Commands

### Enhanced: `publish scan`

Add overlap detection to the existing scan command:

```bash
xearthlayer publish scan --source /path/to/Ortho4XP/Tiles [--tile <lat>,<lon>]

# Output includes new section:
Zoom Level Analysis:
  ZL14 tiles: 234
  ZL16 tiles: 12,456
  ZL17 tiles: 1,203
  ZL18 tiles: 847

  Overlaps Detected:
    ZL18 overlaps ZL16: 312 tiles
    ZL18 overlaps ZL14: 28 tiles
    ZL17 overlaps ZL16: 156 tiles
    Total redundant tiles: 496

  Recommendation: Use --dedupe during 'publish add' to remove 496 redundant tiles
```

### Enhanced: `publish add`

Add deduplication option during tile import:

```bash
xearthlayer publish add \
  --source /path/to/Ortho4XP/Tiles \
  --region na \
  --type ortho \
  --dedupe [--priority <highest|lowest|zl##>]
```

| Option | Description |
|--------|-------------|
| `--dedupe` | Enable zoom level deduplication |
| `--priority <mode>` | Priority when overlap detected (see Priority Modes) |

### Enhanced: `publish build`

Add deduplication option during archive creation:

```bash
xearthlayer publish build \
  --region na \
  --type ortho \
  --dedupe [--priority <mode>] [--tile <lat>,<lon>]
```

This excludes overlapping tiles from the archive without modifying the package directory. Useful for testing deduplication before committing changes.

### New: `publish dedupe`

Process an existing package to remove overlapping tiles:

```bash
xearthlayer publish dedupe \
  --region na \
  --type ortho \
  [--priority <mode>] \
  [--dry-run] \
  [--tile <lat>,<lon>] \
  [<repo_path>]
```

| Option | Description |
|--------|-------------|
| `--priority <mode>` | Resolution strategy (default: `highest`) |
| `--dry-run` | Report what would be removed without making changes |
| `--tile <lat>,<lon>` | Target a specific 1°×1° tile by coordinates |

**Naming Alternatives Considered:**
- `conform` - Adjust to a standard (ambiguous)
- `normalize` - Good but implies standardization, not optimization
- `prune` - Remove unwanted items (good but generic)
- `optimize` - Too broad (could mean compression, etc.)
- `dedupe` - **Chosen**: Clear meaning (de-duplicate), familiar term

### New: `publish gaps`

Analyze coverage gaps where higher zoom level tiles only partially cover lower zoom level areas:

```bash
xearthlayer publish gaps \
  --region na \
  --type ortho \
  [--tile <lat>,<lon>] \
  [--output <file>] \
  [--format <text|json|ortho4xp|summary>] \
  [<repo_path>]
```

| Option | Description |
|--------|-------------|
| `--tile <lat>,<lon>` | Target a specific 1°×1° tile by coordinates |
| `--output <file>` | Write report to file instead of stdout |
| `--format <mode>` | Output format (default: `text`) |

**Output Formats:**

| Format | Description |
|--------|-------------|
| `text` | Human-readable detailed report |
| `json` | Machine-readable JSON structure |
| `ortho4xp` | Coordinates for Ortho4XP tile regeneration |
| `summary` | Compact per-tile summary |

**Example Output (text format):**
```
Coverage Gap Analysis
=====================
Tiles analyzed: 616,980
Zoom levels:    [16, 18]

Gaps Found
  2,165 parent tiles have incomplete ZL18 coverage
  32,475 ZL18 tiles needed to complete coverage

Estimated download: ~396 GB (to generate missing tiles)

Example Gaps
  ZL16 (25008, 10672) at 39.15,-121.34: 4/16 coverage (25.0%)
  ZL16 (25012, 10676) at 39.28,-121.12: 8/16 coverage (50.0%)
  ...
```

**Example Output (ortho4xp format):**
```
# Ortho4XP Missing Tile Coordinates
# Format: latitude, longitude, zoom_level

41.1563, -72.6533, 18
41.1563, -72.5283, 18
41.2813, -72.6533, 18
...
```

**Use Case:**
The `gaps` command helps package publishers identify what tiles need to be generated in Ortho4XP to achieve complete ZL18 coverage. Only with complete coverage can deduplication safely remove lower ZL tiles without creating visible gaps.

**Tile Size Reference:**

| Zoom Level | Approximate Area at 37°N | Tiles per Parent |
|------------|--------------------------|------------------|
| ZL16 | ~5 × 5 NM (22 NM²) | - |
| ZL18 | ~1.2 × 1.2 NM (1.4 NM²) | 16 per ZL16 |

Each ZL16 tile has 16 potential ZL18 children arranged in a 4×4 grid. Partial coverage (1-15 of 16) creates striping artifacts if the parent is removed.

## Targeted Tile Operations

All deduplication commands support targeting a specific 1°×1° tile using latitude/longitude coordinates. This enables:

1. **Testing**: Validate deduplication behavior on a single tile before running package-wide
2. **Surgical fixes**: Address specific problem areas without processing entire packages
3. **Debugging**: Isolate and investigate overlap issues in particular regions

### Tile Coordinate Format

Tiles are specified using the `--tile <lat>,<lon>` option with integer degree coordinates:

```bash
# Target the tile at +37°, -118° (Eastern Sierra, CA)
xearthlayer publish dedupe --region na --tile 37,-118 --dry-run

# Target the tile at +48°, -122° (Seattle area)
xearthlayer publish scan --source /path/to/tiles --tile 48,-122
```

The coordinates map directly to X-Plane's tile naming convention:
- `--tile 37,-118` → Tile `+37-118` (covers 37°N to 38°N, 118°W to 117°W)
- `--tile -33,151` → Tile `-33+151` (covers 33°S to 32°S, 151°E to 152°E)

### Commands Supporting `--tile`

| Command | With `--tile` Behavior |
|---------|----------------------|
| `publish scan` | Report overlaps only within specified tile |
| `publish dedupe` | Remove overlapping tiles only within specified tile |
| `publish gaps` | Analyze coverage gaps only within specified tile |
| `publish build --dedupe` | Exclude overlaps only within specified tile from archive |

### Usage Examples

```bash
# Check overlaps in a specific problem tile (dry run)
xearthlayer publish dedupe --region na --tile 39,-122 --dry-run

# Fix overlaps in a single tile
xearthlayer publish dedupe --region na --tile 39,-122 --priority highest

# Scan a specific tile from Ortho4XP source
xearthlayer publish scan --source ~/Ortho4XP/Tiles --tile 47,-123

# Build with deduplication for only one tile (testing)
xearthlayer publish build --region na --dedupe --tile 39,-122
```

### Tile Selection Logic

When `--tile` is specified, the operation filters terrain files by their `LOAD_CENTER` coordinates:

```rust
/// Check if a tile falls within a 1°×1° degree cell
fn tile_in_cell(tile: &TileReference, lat: i32, lon: i32) -> bool {
    let tile_lat = tile.lat.floor() as i32;
    let tile_lon = tile.lon.floor() as i32;
    tile_lat == lat && tile_lon == lon
}
```

This matches X-Plane's DSF tile boundaries exactly, ensuring consistency with the scenery file organization in `Earth nav data/`.

## Resolution Strategy

### Priority Modes

| Mode | Behavior | Use Case |
|------|----------|----------|
| `highest` (default) | Keep highest ZL, remove all lower | Best visual quality |
| `lowest` | Keep lowest ZL, remove all higher | Smallest package size |
| `zl##` (e.g., `zl16`) | Keep specified ZL, remove others | Explicit control |

### Algorithm

```
1. Group all tiles by their geographic coverage area
2. For each group with multiple zoom levels:
   a. Determine which ZL to keep based on priority mode
   b. Mark all other ZLs in that group for removal
3. Generate removal list with audit trail
```

### Multi-Level Resolution Example

Given tiles at ZL14, ZL16, and ZL18 covering the same area:

| Priority | Keeps | Removes |
|----------|-------|---------|
| `highest` | ZL18 | ZL14, ZL16 |
| `lowest` | ZL14 | ZL16, ZL18 |
| `zl16` | ZL16 | ZL14, ZL18 |

### Edge Cases

1. **Partial coverage**: Higher ZL covers only part of lower ZL tile's area
   - Keep both tiles; lower ZL provides fallback for uncovered areas
   - Report as "partial overlap" in audit

2. **Odd zoom level gaps**: ZL18 and ZL14 exist, but no ZL16
   - Still detect overlap (ZL18 → ZL14 via scale factor 16)
   - Apply priority resolution normally

3. **Sea tiles**: `*_sea.ter` and `*_sea_overlay.ter` follow same rules

4. **Provider mixing**: BI18 (Bing ZL18) and GO216 (Google ZL16)
   - Treat as overlap if same geographic area regardless of provider
   - Priority resolves which provider's tile is kept

## Audit Report

All deduplication operations produce an audit report:

```
Zoom Level Deduplication Report
================================
Package: zzXEL_na_ortho
Priority: highest
Mode: dedupe command

Zoom Levels Present: ZL14, ZL16, ZL17, ZL18

Summary:
  Tiles analyzed: 392,738
  Overlaps detected: 1,583
    ZL18 → ZL16: 1,083
    ZL18 → ZL14: 28
    ZL17 → ZL16: 456
    ZL17 → ZL14: 16
  Tiles removed: 1,583
  Tiles preserved: 391,155

Removed Files (by zoom level):
  ZL14 (44 tiles):
    terrain/6252_2668_BI14.ter
    ...
  ZL16 (1,539 tiles):
    terrain/25008_10672_BI16.ter
    terrain/25008_10672_BI16_sea.ter
    ...

Preserved Higher-Resolution Coverage:
  ZL18: 19,051 tiles
  ZL17: 1,203 tiles (no overlap)

Partial Overlaps (no action taken): 12
  terrain/100048_42704_BI18.ter (partial coverage of 25012_10676_BI16)
  ...
```

### Report Output Options

```bash
# Print to console (default)
xearthlayer publish dedupe --region na

# Save to file
xearthlayer publish dedupe --region na --report dedupe-report.txt

# JSON format for programmatic use
xearthlayer publish dedupe --region na --report-format json
```

## Architecture

### New Types

```rust
/// Supported zoom level range
pub const MIN_ZOOM_LEVEL: u8 = 12;
pub const MAX_ZOOM_LEVEL: u8 = 19;

/// Represents a detected zoom level overlap
#[derive(Debug, Clone)]
pub struct ZoomOverlap {
    /// Higher resolution tile (to keep by default)
    pub higher_zl: TileReference,
    /// Lower resolution tile (redundant)
    pub lower_zl: TileReference,
    /// Zoom level difference (e.g., 2 for ZL18→ZL16)
    pub zl_diff: u8,
    /// Whether overlap is complete or partial
    pub coverage: OverlapCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverlapCoverage {
    /// Higher ZL tile completely covers lower ZL tile's area
    Complete,
    /// Higher ZL tile only partially covers lower ZL tile's area
    Partial,
}

/// Reference to a terrain tile
#[derive(Debug, Clone)]
pub struct TileReference {
    pub row: u32,
    pub col: u32,
    pub zoom: u8,
    pub provider: String,  // "BI", "GO2", "GO", etc.
    pub ter_path: PathBuf,
}

impl TileReference {
    /// Calculate parent tile coordinates at a lower zoom level
    pub fn parent_at(&self, target_zl: u8) -> Option<(u32, u32)> {
        if target_zl >= self.zoom || (self.zoom - target_zl) % 2 != 0 {
            return None;  // Invalid: must be lower ZL and even difference
        }
        let scale = 4u32.pow(((self.zoom - target_zl) / 2) as u32);
        Some((self.row / scale, self.col / scale))
    }
}

/// Priority strategy for resolving overlaps
#[derive(Debug, Clone, Copy)]
pub enum ZoomPriority {
    Highest,
    Lowest,
    Specific(u8),
}

impl Default for ZoomPriority {
    fn default() -> Self {
        Self::Highest
    }
}

/// Result of deduplication operation
#[derive(Debug)]
pub struct DedupeResult {
    pub tiles_analyzed: usize,
    pub zoom_levels_present: Vec<u8>,
    pub overlaps_by_pair: HashMap<(u8, u8), usize>,  // (higher, lower) → count
    pub tiles_removed: Vec<TileReference>,
    pub tiles_preserved: Vec<TileReference>,
    pub partial_overlaps: Vec<ZoomOverlap>,
}

/// A missing tile that would complete coverage
#[derive(Debug, Clone)]
pub struct MissingTile {
    pub row: u32,
    pub col: u32,
    pub zoom: u8,
    pub lat: f32,  // Center latitude for Ortho4XP
    pub lon: f32,  // Center longitude for Ortho4XP
}

/// A coverage gap representing incomplete higher-ZL coverage
#[derive(Debug, Clone)]
pub struct CoverageGap {
    pub parent: TileReference,          // The lower-ZL parent tile
    pub existing_children: Vec<TileReference>,  // Children that exist
    pub missing_tiles: Vec<MissingTile>,        // Children that don't exist
    pub expected_count: u32,            // Always 16 for ZL+2
}

impl CoverageGap {
    /// Calculate coverage percentage
    pub fn coverage_percent(&self) -> f32 {
        (self.existing_children.len() as f32 / self.expected_count as f32) * 100.0
    }
}

/// Result of gap analysis operation
#[derive(Debug, Clone, Default)]
pub struct GapAnalysisResult {
    pub tiles_analyzed: usize,
    pub zoom_levels_present: Vec<u8>,
    pub gaps: Vec<CoverageGap>,
    pub total_missing_tiles: usize,
    pub tile_filter: Option<(i32, i32)>,  // Applied tile filter
}
```

### Module Structure

```
xearthlayer/src/publisher/
├── mod.rs
├── dedupe/
│   ├── mod.rs           # Module exports
│   ├── detector.rs      # Overlap detection + gap analysis
│   ├── resolver.rs      # Resolution strategy implementation
│   ├── report.rs        # DedupeAuditReport + GapAuditReport
│   ├── types.rs         # ZoomOverlap, TileReference, CoverageGap, etc.
│   └── tests.rs         # Unit tests for detection and resolution
└── ...
```

### Core Detection Algorithm

```rust
impl OverlapDetector {
    /// Detect all overlaps in a collection of tiles
    pub fn detect(&self, tiles: &[TileReference]) -> Vec<ZoomOverlap> {
        let mut overlaps = Vec::new();

        // Group tiles by zoom level
        let by_zoom: HashMap<u8, Vec<&TileReference>> = tiles
            .iter()
            .fold(HashMap::new(), |mut acc, tile| {
                acc.entry(tile.zoom).or_default().push(tile);
                acc
            });

        // Get sorted zoom levels (highest first)
        let mut zoom_levels: Vec<u8> = by_zoom.keys().copied().collect();
        zoom_levels.sort_by(|a, b| b.cmp(a));

        // For each higher ZL, check against all lower ZLs
        for (i, &high_zl) in zoom_levels.iter().enumerate() {
            for &low_zl in &zoom_levels[i + 1..] {
                // Skip if zoom difference is odd (not aligned grid)
                if (high_zl - low_zl) % 2 != 0 {
                    continue;
                }

                overlaps.extend(
                    self.detect_between_levels(&by_zoom, high_zl, low_zl)
                );
            }
        }

        overlaps
    }

    fn detect_between_levels(
        &self,
        by_zoom: &HashMap<u8, Vec<&TileReference>>,
        high_zl: u8,
        low_zl: u8,
    ) -> Vec<ZoomOverlap> {
        let high_tiles = by_zoom.get(&high_zl).unwrap();
        let low_tiles = by_zoom.get(&low_zl).unwrap();

        // Build lookup for low ZL tiles
        let low_lookup: HashSet<(u32, u32)> = low_tiles
            .iter()
            .map(|t| (t.row, t.col))
            .collect();

        high_tiles
            .iter()
            .filter_map(|high| {
                let (parent_row, parent_col) = high.parent_at(low_zl)?;
                if low_lookup.contains(&(parent_row, parent_col)) {
                    // Find the actual low tile for the overlap record
                    let low = low_tiles.iter()
                        .find(|t| t.row == parent_row && t.col == parent_col)?;
                    Some(ZoomOverlap {
                        higher_zl: (*high).clone(),
                        lower_zl: (*low).clone(),
                        zl_diff: high_zl - low_zl,
                        coverage: self.determine_coverage(high, low),
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}
```

### Integration Points

#### Ortho4XPProcessor

Add overlap detection during tile scanning:

```rust
impl Ortho4XPProcessor {
    /// Scan with overlap detection
    pub fn scan_with_overlaps(&self, source: &Path) -> Result<ScanResult, ProcessorError> {
        let tiles = self.scan_tiles(source)?;
        let detector = OverlapDetector::new();
        let overlaps = detector.detect(&tiles);
        Ok(ScanResult { tiles, overlaps })
    }
}
```

#### PublisherService Trait

Extend with deduplication and gap analysis methods:

```rust
pub trait PublisherService: Send + Sync {
    // ... existing methods ...

    /// Detect zoom level overlaps in a package
    fn detect_overlaps(
        &self,
        packages_dir: &Path,
        filter: DedupeFilter,
    ) -> Result<Vec<ZoomOverlap>, CliError>;

    /// Remove overlapping tiles from a package
    fn dedupe_package(
        &self,
        packages_dir: &Path,
        priority: ZoomPriority,
        filter: DedupeFilter,
        dry_run: bool,
    ) -> Result<DedupeResult, CliError>;

    /// Analyze coverage gaps in a package
    fn analyze_gaps(
        &self,
        packages_dir: &Path,
        region: &str,
        scenery_type: &str,
        filter: DedupeFilter,
    ) -> Result<GapAnalysisResult, CliError>;
}
```

## Non-Destructive Guarantees

| Location | Behavior |
|----------|----------|
| Ortho4XP source | **Never modified** - read-only scanning |
| XEL package directory | May be modified by `dedupe` command |
| XEL dist archives | Controlled by `--dedupe` flag at build time |
| Audit reports | Written to separate files, never overwrite source |

### Workflow Safety

```
Ortho4XP/Tiles/          (SOURCE - never touched)
       ↓
xearthlayer publish add --dedupe
       ↓
packages/zzXEL_na_ortho/ (PACKAGE - deduped on import)
       ↓
xearthlayer publish build
       ↓
dist/zzXEL_na-*.tar.gz   (ARCHIVE - clean output)
```

## CLI Handler Structure

Following the existing Command Pattern:

```rust
// args.rs
#[derive(Parser)]
pub struct DedupeArgs {
    #[arg(long)]
    pub region: String,

    #[arg(long, default_value = "ortho")]
    pub r#type: String,

    /// Priority mode: highest, lowest, or zl## (e.g., zl16)
    #[arg(long, default_value = "highest")]
    pub priority: String,

    #[arg(long)]
    pub dry_run: bool,

    /// Target a specific 1°×1° tile (format: lat,lon e.g., 37,-118)
    #[arg(long, value_parser = parse_tile_coord)]
    pub tile: Option<TileCoord>,

    #[arg(long)]
    pub report: Option<PathBuf>,

    #[arg(long, default_value = "text")]
    pub report_format: String,

    #[arg(default_value = ".")]
    pub repo: PathBuf,
}

/// 1°×1° tile coordinate (matches X-Plane DSF tile boundaries)
#[derive(Debug, Clone, Copy)]
pub struct TileCoord {
    pub lat: i32,
    pub lon: i32,
}

impl TileCoord {
    /// Format as X-Plane tile name (e.g., "+37-118")
    pub fn to_xplane_name(&self) -> String {
        format!("{:+03}{:+04}", self.lat, self.lon)
    }
}

fn parse_tile_coord(s: &str) -> Result<TileCoord, String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        return Err("expected format: lat,lon (e.g., 37,-118)".to_string());
    }
    let lat: i32 = parts[0].trim().parse()
        .map_err(|_| format!("invalid latitude: {}", parts[0]))?;
    let lon: i32 = parts[1].trim().parse()
        .map_err(|_| format!("invalid longitude: {}", parts[1]))?;

    if !(-90..=89).contains(&lat) {
        return Err(format!("latitude must be between -90 and 89, got {}", lat));
    }
    if !(-180..=179).contains(&lon) {
        return Err(format!("longitude must be between -180 and 179, got {}", lon));
    }

    Ok(TileCoord { lat, lon })
}

// handlers.rs
pub struct DedupeHandler;

impl CommandHandler for DedupeHandler {
    type Args = DedupeArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        let priority = parse_priority(&args.priority)?;

        // Build filter options
        let filter = DedupeFilter {
            tile: args.tile,
        };

        // Report targeted operation if tile specified
        if let Some(tile) = &args.tile {
            ctx.output.println(&format!(
                "Targeting tile {} ({}, {})",
                tile.to_xplane_name(),
                tile.lat,
                tile.lon
            ));
        }

        let result = ctx.publisher.dedupe_package(
            &args.repo.join("packages"),
            priority,
            filter,
            args.dry_run,
        )?;

        // Generate and output report
        let report = generate_report(&result, &args);
        output_report(ctx.output, &report, &args)?;

        Ok(())
    }
}

/// Filter options for deduplication operations
#[derive(Debug, Clone, Default)]
pub struct DedupeFilter {
    /// Limit operation to tiles within this 1°×1° cell
    pub tile: Option<TileCoord>,
}

fn parse_priority(s: &str) -> Result<ZoomPriority, CliError> {
    match s.to_lowercase().as_str() {
        "highest" => Ok(ZoomPriority::Highest),
        "lowest" => Ok(ZoomPriority::Lowest),
        s if s.starts_with("zl") => {
            let zl: u8 = s[2..].parse()
                .map_err(|_| CliError::InvalidArgument("priority", s.to_string()))?;
            if zl < MIN_ZOOM_LEVEL || zl > MAX_ZOOM_LEVEL {
                return Err(CliError::InvalidArgument(
                    "priority",
                    format!("zoom level must be between {} and {}", MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL)
                ));
            }
            Ok(ZoomPriority::Specific(zl))
        }
        _ => Err(CliError::InvalidArgument("priority", s.to_string())),
    }
}
```

## Testing Strategy

### Unit Tests

- Overlap detection with known tile coordinates across all ZL pairs
- Parent coordinate calculation for various ZL differences
- Priority resolution for each mode with multi-level overlaps
- Partial coverage detection
- Edge cases (empty packages, single ZL, odd ZL gaps)
- Tile coordinate parsing (valid formats, edge cases, error handling)
- Tile filtering logic (tiles inside/outside target cell)

### Integration Tests

- Full scan → dedupe → build workflow
- Verify correct files removed
- Verify no source file modifications
- Report accuracy across all ZL combinations
- **Targeted tile operations**:
  - Dedupe only affects tiles within specified 1°×1° cell
  - Tiles outside target cell remain untouched
  - Report reflects targeted scope accurately

### Test Fixtures

Create minimal test packages with controlled overlap scenarios:

```
test_fixtures/overlapping_tiles/
├── terrain/
│   ├── 1563_667_BI12.ter         # ZL12
│   ├── 6252_2668_BI14.ter        # ZL14 (overlaps ZL12)
│   ├── 25008_10672_BI16.ter      # ZL16 (overlaps ZL14, ZL12)
│   ├── 100032_42688_BI18.ter     # ZL18 (overlaps ZL16, ZL14, ZL12)
│   └── 25012_10676_BI16.ter      # ZL16 with no higher overlap
└── textures/
    └── ...
```

## Implementation Status

| Feature | Status | Notes |
|---------|--------|-------|
| Core overlap detection | ✅ Complete | Parent-centric algorithm with gap protection |
| `publish dedupe` command | ✅ Complete | All priority modes, dry-run, tile filter |
| `publish gaps` command | ✅ Complete | Multiple output formats including Ortho4XP |
| `publish scan` overlap reporting | ✅ Complete | Integrated with existing scan |
| `publish add --dedupe` | ⏳ Planned | Phase 4 of original design |
| `publish build --dedupe` | ⏳ Planned | Phase 4 of original design |

## Future Considerations

1. **Automatic detection in `xearthlayer run`**: Warn if mounted packages have overlaps
2. **Package metadata**: Store dedupe status in package manifest
3. **Undo support**: Track removed tiles for potential restoration
4. **Visualization**: Integrate with coverage map to show overlap regions by ZL
5. **ZL-aware coverage maps**: Color tiles by zoom level in coverage generator
6. **Ortho4XP integration**: Script to automate tile regeneration from gaps output
