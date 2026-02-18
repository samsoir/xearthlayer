# GeoIndex Design

**Status**: Implemented
**Created**: 2026-02-17
**Issue**: #51

## Overview

GeoIndex is an in-memory geospatial reference database that provides region-level spatial queries for XEarthLayer. It was introduced to solve a fundamental architectural problem: X-Plane scenery patches and packages use **different filenames** for the same geographic area, but the FUSE union filesystem resolved files by **filename**, not geography.

GeoIndex separates geographic ownership ("which regions are patched?") from file resolution ("where is this file on disk?"), enabling FUSE, prewarm, and prefetch to make geospatially-aware decisions without coupling geographic knowledge into the file index.

### Problem Statement

When scenery patches are installed alongside regional packages, both provide `.ter` (terrain) files for the same 1x1 DSF region. However, patches use naming conventions like `GO218.ter` while packages use `BI16.ter` for overlapping areas. The previous filename-based union approach could not detect this overlap:

```
Patch:   _patches/LIPX_Mesh/terrain/1234_5678_GO218.ter   --> DSF region (+45+011)
Package: eu/terrain/1234_5678_BI16.ter                     --> DSF region (+45+011)
```

Package `.ter` files reference `.dds` textures that FUSE won't generate in patched regions, causing X-Plane to crash with missing texture errors.

### Goals

1. **Separation of concerns** — Geographic queries decoupled from file index
2. **Thread safety** — Concurrent reads from FUSE, prewarm, and prefetch
3. **Extensibility** — Support future layer types beyond patch coverage
4. **Minimal overhead** — Fast region lookups (O(1) via DashMap)

### Non-Goals

- Persistent storage (in-memory only; rebuild on startup)
- Sub-region granularity (1x1 DSF regions are the atomic unit)
- Runtime modification of layers during flights (populated at startup)

---

## Architecture

### Separation of Concerns

```
GeoIndex (geography)              OrthoUnionIndex (files)
────────────────────              ─────────────────────────
contains::<PatchCoverage>()       resolve_lazy_filtered(predicate)
insert / populate / clear         resolve() / resolve_lazy()
get / regions / count
                     \              /
                      \            /
                       ↓          ↓
                 FUSE (composition layer)
                 ────────────────────────
                 is_geo_filtered()     →  GeoIndex
                 resolve_lazy_geo()    →  OrthoUnionIndex
```

The three components have distinct responsibilities:

| Component | Knows About | Does Not Know About |
|-----------|------------|-------------------|
| **GeoIndex** | DSF regions, patch coverage | Files, FUSE, DDS generation |
| **OrthoUnionIndex** | Virtual paths, source priority, file resolution | Why sources are filtered |
| **FUSE** | How to compose geographic and file queries | Geographic internals, index internals |

### Module Structure

```
xearthlayer/src/geo_index/
├── mod.rs       # Module definition, re-exports
├── region.rs    # DsfRegion type (1x1 coordinate)
├── index.rs     # GeoIndex implementation
└── layers.rs    # GeoLayer trait, PatchCoverage
```

### Data Model

GeoIndex is a **type-keyed heterogeneous map** of **region-indexed layers**:

```
GeoIndex
├── Layer: PatchCoverage
│   ├── DsfRegion(+45, +011) → PatchCoverage { patch_name: "LIPX_Mesh" }
│   ├── DsfRegion(+45, +012) → PatchCoverage { patch_name: "LIPX_Mesh" }
│   └── DsfRegion(+33, -119) → PatchCoverage { patch_name: "KLAX_Ortho" }
│
└── (future layers added here without modifying existing code)
```

Each layer type (`PatchCoverage`, etc.) is an independent `DashMap<DsfRegion, T>` accessed via Rust's `TypeId` for type-safe heterogeneous storage.

---

## Thread Safety

### Locking Strategy

```
                    ┌──────────────────────────────────────────┐
                    │          GeoIndex                        │
                    │                                          │
                    │  layers: RwLock<HashMap<TypeId, Layer>>  │
                    │                                          │
                    │  ┌────────────────────────────────────┐  │
                    │  │ Layer: PatchCoverage               │  │
                    │  │                                    │  │
                    │  │  DashMap<DsfRegion, PatchCoverage> │  │
                    │  │  ┌───────┐ ┌───────┐ ┌───────┐     │  │
                    │  │  │Shard 0│ │Shard 1│ │Shard N│     │  │
                    │  │  └───────┘ └───────┘ └───────┘     │  │
                    │  └────────────────────────────────────┘  │
                    └──────────────────────────────────────────┘
```

Two levels of synchronization:

1. **Layer-level**: `RwLock<HashMap<TypeId, Box<dyn Any>>>` — protects the layer registry. Read locks for queries (common), write locks only for layer creation or bulk population (rare, startup only).

2. **Region-level**: `DashMap<DsfRegion, T>` per layer — concurrent region access with per-shard locking. Multiple FUSE threads can query different regions simultaneously without contention.

### ACID Properties

| Property | Implementation |
|----------|---------------|
| **Atomicity** | `populate()` builds the new `DashMap` outside any lock, then swaps it in with a brief write lock. Readers never see a partially-populated layer. |
| **Consistency** | Readers see either the old or new state, never an intermediate. The `RwLock` ensures the layer pointer swap is atomic. |
| **Isolation** | Multiple readers (FUSE, prewarm, prefetch) access concurrently via `RwLock` read guards. Writers are serialized. |
| **Durability** | N/A — in-memory only. Rebuilt from patch source directories at startup. |

### Double-Checked Locking

The `insert()` method uses double-checked locking to minimize write lock contention:

```rust
// 1. Try read lock first (common path — layer already exists)
{
    let layers = self.layers.read();
    if let Some(layer) = layers.get(&TypeId::of::<T>()) {
        layer.insert(region, value);   // DashMap handles its own locking
        return;
    }
}
// 2. Layer doesn't exist — acquire write lock, re-check
let mut layers = self.layers.write();
if let Some(layer) = layers.get(&TypeId::of::<T>()) {
    layer.insert(region, value);       // Another thread created it
    return;
}
// 3. Create new layer
layers.insert(TypeId::of::<T>(), new_layer);
```

---

## API Reference

### Core Types

#### `DsfRegion`

A 1x1 DSF region coordinate, identified by the floor of latitude and longitude:

```rust
let region = DsfRegion::new(45, 11);           // From integers
let region = DsfRegion::from_lat_lon(45.7, 11.3); // From float coords (floors)
println!("{}", region);                         // "+45+011"
```

#### `GeoLayer` (trait)

Marker trait for data types storable in GeoIndex. Requires `Clone + Send + Sync + 'static`:

```rust
pub trait GeoLayer: Clone + Send + Sync + 'static {}
```

#### `PatchCoverage`

Indicates a DSF region is owned by a scenery patch:

```rust
pub struct PatchCoverage {
    pub patch_name: String,  // e.g., "LIPX_Mesh"
}
```

### GeoIndex Methods

| Method | Description | Lock Behavior |
|--------|-------------|---------------|
| `new()` | Create empty GeoIndex | None |
| `contains::<T>(region)` | Check if region has data of type T | Read lock → DashMap read |
| `get::<T>(region)` | Get cloned value for region | Read lock → DashMap read + clone |
| `insert::<T>(region, value)` | Insert/update single entry | Read lock (existing layer) or write lock (new layer) |
| `remove::<T>(region)` | Remove a region entry | Read lock → DashMap remove |
| `populate::<T>(entries)` | Atomic bulk population (replaces entire layer) | Build outside → brief write lock to swap |
| `count::<T>()` | Number of regions in a layer | Read lock |
| `regions::<T>()` | All regions in a layer (copied) | Read lock |
| `clear::<T>()` | Clear an entire layer | Write lock |

---

## Integration Points

### Population (Startup)

GeoIndex is populated in `MountManager::mount_consolidated_ortho_with_progress()` after building the `OrthoUnionIndex`. Patch sources are scanned for DSF files to extract region coordinates:

```
MountManager
  └── build OrthoUnionIndex
  └── scan patch sources for DSF regions (extract_dsf_regions)
  └── geo_index.populate(entries)        ← atomic bulk load
  └── pass Arc<GeoIndex> to FUSE, orchestrator
```

### FUSE Composition

`Fuse3OrthoUnionFS` composes GeoIndex and OrthoUnionIndex via two helper methods:

```rust
fn is_geo_filtered(&self, filename: &str) -> bool {
    // Parse filename → DsfTileCoord → DsfRegion
    // Query: geo_index.contains::<PatchCoverage>(&region)
}

fn resolve_lazy_geo(&self, virtual_path: &Path, filename: &str) -> Option<PathBuf> {
    if self.is_geo_filtered(filename) {
        // Patched region: only serve from patch sources
        self.index.resolve_lazy_filtered(path, |source| source.is_patch())
    } else {
        // Normal region: serve from any source
        self.index.resolve_lazy(path)
    }
}
```

This pattern is used in `lookup()`, `getattr()`, and `read()` — the three FUSE operations that resolve virtual paths to real files.

### Prewarm Filtering

`PrewarmContext` receives `Option<Arc<GeoIndex>>` and filters generated tiles:

```rust
if let Some(ref geo_index) = self.geo_index {
    tiles.retain(|tile| {
        let dsf = DsfTileCoord::from_lat_lon(lat, lon);
        !geo_index.contains::<PatchCoverage>(&DsfRegion::new(dsf.lat, dsf.lon))
    });
}
```

Tiles in patched regions are skipped since patches provide their own pre-built textures.

### Prefetch Filtering

`AdaptivePrefetchCoordinator` applies the same pattern independently of the disk filter:

```
Step 1: GeoIndex filter  — skip tiles in patch-owned regions (geographic)
Step 2: Disk filter       — skip tiles already on disk (file existence)
```

The two filters are independent: you may have patches without packages, or vice versa.

---

## Extensibility

Adding a new layer type requires:

1. Define a struct implementing `GeoLayer`:
   ```rust
   #[derive(Debug, Clone)]
   pub struct SeasonalCoverage { pub season: String }
   impl GeoLayer for SeasonalCoverage {}
   ```

2. Populate it:
   ```rust
   geo_index.populate::<SeasonalCoverage>(entries);
   ```

3. Query it:
   ```rust
   geo_index.contains::<SeasonalCoverage>(&region);
   ```

No modifications to GeoIndex itself are needed. The `TypeId`-based dispatch handles routing automatically.

---

## Design Decisions

### Why Not Extend OrthoUnionIndex?

The previous approach stored `patched_regions: HashSet<(i32, i32)>` directly on `OrthoUnionIndex`. This violated SRP:

- The file index had to know about geographic regions
- Methods like `is_resource_in_patched_region()` mixed file parsing with geographic queries
- The `Serialize`/`Deserialize` implementation had to accommodate region data
- No path to supporting additional geographic data types

### Why Type-Keyed (TypeId) Instead of Enum?

An enum (`enum GeoLayerType { PatchCoverage, ... }`) would require modifying the enum for every new layer type. The `TypeId` approach is open for extension — new layer types can be added without touching GeoIndex internals. This follows the Open-Closed Principle.

### Why DashMap Instead of RwLock<HashMap>?

Region lookups happen on every FUSE operation (potentially thousands per second during scene loading). `DashMap` provides per-shard locking, allowing concurrent reads to different regions without any lock contention. A single `RwLock<HashMap>` would serialize all lookups.

### Why Separate populate() Instead of Repeated insert()?

Bulk population via `populate()` provides atomicity: readers never see a partially-populated layer. This matters at startup when FUSE mounts may already be serving requests while the index is being built. The `populate()` method builds the complete `DashMap` outside any lock, then swaps it in with a brief write lock.

---

## Key Files

| File | Purpose |
|------|---------|
| `xearthlayer/src/geo_index/mod.rs` | Module definition, re-exports |
| `xearthlayer/src/geo_index/region.rs` | `DsfRegion` type |
| `xearthlayer/src/geo_index/index.rs` | `GeoIndex` implementation |
| `xearthlayer/src/geo_index/layers.rs` | `GeoLayer` trait, `PatchCoverage` |
| `xearthlayer/src/ortho_union/index.rs` | `resolve_lazy_filtered()` |
| `xearthlayer/src/fuse/fuse3/ortho_union_fs.rs` | FUSE composition (`is_geo_filtered`, `resolve_lazy_geo`) |
| `xearthlayer/src/manager/mounts.rs` | GeoIndex creation and population |
| `xearthlayer/src/prefetch/prewarm/context.rs` | Prewarm patched region filtering |
| `xearthlayer/src/prefetch/adaptive/coordinator/core.rs` | Prefetch patched region filtering |
