# Adaptive Tile-Based Prefetch System

**Technical Design Document**

**Project**: XEarthLayer
**Date**: January 2026, revised October 2026
**Version**: 2.4
**Status**: Implemented (sliding box since PR #102)

---

## Table of Contents

1. [Overview](#overview)
2. [Goals and Non-Goals](#goals-and-non-goals)
3. [Background Research](#background-research)
4. [System Architecture](#system-architecture)
   - [Demand and prefetch lanes](#demand-and-prefetch-lanes)
5. [Three-Window Model](#three-window-model)
6. [SceneryWindow](#scenerywindow)
7. [BoundaryStrategy](#boundarystrategy)
8. [Strategy Mode](#strategy-mode)
9. [Integration Points](#integration-points)
10. [Configuration](#configuration)
11. [Error Handling & Edge Cases](#error-handling--edge-cases)
12. [Testing Strategy](#testing-strategy)

---

## Overview

### Purpose

The Adaptive Prefetch System predicts which scenery tiles X-Plane will request next and pre-loads them before X-Plane needs them. This eliminates loading stutters during flight by ensuring tiles are already in memory or on disk when X-Plane requests them.

### Key Innovation

A **sliding prefetch box** moves with the aircraft, biased in the direction of travel and sized by ground speed. Every cycle, the DSF regions inside the box that XEL has not yet prefetched are expanded to their tiles and submitted. The box is deliberately larger than X-Plane's ~3° scenery window, so the rows and columns X-Plane loads at each boundary crossing are already in the cache when it asks for them.

The first version of this system (v2.0) mirrored X-Plane's loading model directly, with two boundary monitors that fired row and column loads as the aircraft neared the window edge. Flight testing showed that fired too late (#86), and PR #102 replaced it with the box. The research findings below still describe X-Plane's behaviour accurately; the box is a different response to them.

### Design Principles

- **Observe, don't assume**: Ground the model in measured X-Plane behaviour and FUSE request data, not hardcoded assumptions
- **Stay ahead of X-Plane**: Cover more than X-Plane will load next, rather than predicting exactly what it will load
- **Three windows**: Track X-Plane's window, XEL's prefetched window, and the target window separately
- **Performance-aware**: Don't prefetch if the connection can't complete before X-Plane needs it
- **Non-interfering**: Respect executor backpressure and transition throttling

---

## Goals and Non-Goals

### Goals

1. **Cover what X-Plane loads next**: keep every DSF region X-Plane will load at its next boundary crossing inside the prefetch box, including the 3-deep row and 2-deep column loads it makes
2. **Size the lookahead to speed**: look further ahead in cruise than on approach, so prefetch depth tracks how far the aircraft will travel in the next few minutes
3. **Detect world rebuilds**: recognise a teleport or rendering change from FUSE request bursts and reset prefetch state
4. **Stay ahead**: have tiles generated before X-Plane's reactive loading asks for them
5. **Handle all flight phases** — turns, diagonal flight, teleports, rendering changes, sparse/ocean regions

### Non-Goals

1. **Predict X-Plane's internal memory state precisely** — we infer with a buffer zone
2. **Flight plan parsing** — future enhancement
3. **Prewarm replacement** — prewarm (loading entire regions) remains separate from prefetch (loading ahead during flight)
4. **Solve extremely slow connections** — if throughput is too low, prefetch won't help

---

## Background Research

This design is based on empirical research documented in:

- **[X-Plane 12 Scenery Loading Behavior v1.1](xplane-scenery-loading-whitepaper.md)** — White paper on X-Plane's loading patterns (5 flights, 10.5+ hours, 4M+ FUSE requests)
- **[Flight Test Data](../../tests/flight-data/)** — Compressed flight logs used for analysis

### Key Findings Informing Design

The findings are X-Plane's measured behaviour and still hold. The **Design Impact** column records the v2.0 boundary-monitor response, kept for the reasoning; where it names monitors, the current design answers the same finding with the sliding box instead (see [Sliding Prefetch Box](#sliding-prefetch-box-cruise-phase)).

| Finding | Design Impact (v2.0) |
|---------|---------------|
| X-Plane uses position-only loading (no heading/speed/track) | Use position-based boundary monitors, not track projection |
| Two independent boundary monitors (lat and lon) | Mirror with dual `BoundaryMonitor` structs |
| ROW loads on latitude crossings, COLUMN loads on longitude crossings | Generate rows/columns, not rectangular bands |
| Scenery window: ~3° lat × ~3°/cos(lat) lon (~330km × 330km) | Fixed lat rows, latitude-dependent lon columns |
| ROW loads: 3 DSF rows deep (73% of events), 3-4 cols wide | `load_depth = 3` for latitude crossings |
| COL loads: 2 DSF cols deep (64% of events), 3-4 rows wide | `load_depth = 2` for longitude crossings |
| Loading depth is asymmetric (rows deeper than columns) | Axis-specific depth defaults, not a single value |
| Trigger at ~0.6° into DSF toward boundary | Prefetch when aircraft within `trigger_distance` of window edge |
| Aircraft typically 1-1.5° from nearest edge | Tight prefetch timing — trigger early |
| Speed doesn't affect trigger position | Position-based monitors, not time-based |
| No turn detection — purely position vs. loaded area | No TurnDetector needed; monitors handle turns naturally |
| Window biased toward departure/heading direction | AC not always centered; bias affects which edge is closest |
| Post-turn radius fill over 10-20 minutes | Low-priority lateral gap filling |
| Initial load reveals window dimensions | Window size is a measured constant; live derivation from FUSE reads was never wired, and its SceneTracker was deleted in #204 |
| Diagonal flight fires both monitors independently | Both monitors check independently each cycle |
| Map grid squares are ~0.5°, NOT 1° DSF tiles | Don't confuse X-Plane map grid with DSF boundaries |

### Test Flight Data

| Flight | Route | Key Learning |
|--------|-------|--------------|
| EDDH → EDDF | Southbound | Band loading pattern, 0.6° trigger position |
| EDDH → EKCH | Northeast diagonal | Both lat/lon bands load simultaneously, 29s separation |
| EDDH → LFMN | Southbound with turns | Turn adaptation timing (20-40s), position-only |
| KJFK → EGLL | High-speed transatlantic | Speed independence, ocean behavior |
| LFLL diagonal orbit | Multi-heading (317°/47°/139°) | Dual boundary monitors, row/column separation, post-turn radius fill |

---

## System Architecture

### Component Diagram

```
┌───────────────────────────────────────────────────────────────────────────┐
│                      AdaptivePrefetchCoordinator                          │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │ • Picks the box shape by flight phase (PhaseDetector)               │  │
│  │ • Plans new regions, or drains pending tiles first                  │  │
│  │ • Filters plans (memory, patch, package, DDS disk)                  │  │
│  │ • Submits via PrefetchLane under backpressure + transition throttle │  │
│  │ • Runs region maintenance every cycle                               │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│         │                    │                      │                     │
│         ▼                    ▼                      ▼                     │
│  ┌─────────────┐     ┌─────────────────┐     ┌─────────────┐            │
│  │ Prefetch    │     │ Boundary        │     │ Scenery     │            │
│  │ Box         │     │ Strategy        │     │ Index       │            │
│  ├─────────────┤     ├─────────────────┤     ├─────────────┤            │
│  │ Box bounds, │     │ Region states:  │     │ Tiles per   │            │
│  │ new regions,│     │ mark, promote,  │     │ DSF region  │            │
│  │ retention   │     │ defer, evict    │     │             │            │
│  └──────┬──────┘     └────────┬────────┘     └─────────────┘            │
│         └──────────┬──────────┘                                          │
│                    ▼                                                     │
│             ┌─────────────┐      ┌──────────────┐                        │
│             │  GeoIndex   │      │ SceneryWindow│                        │
│             │(spatial DB) │      │ (rebuild     │                        │
│             └─────────────┘      │  detection)  │                        │
│                                  └──────────────┘                        │
└───────────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│    PrefetchLane     │────▶│  JobExecutor (the    │────▶│   CacheLayer     │
│  (submit prefetch   │     │  prefetch lane)      │     │  (memory + disk) │
│   tiles)            │     │                      │     │                  │
└─────────────────────┘     └──────────────────────┘     └──────────────────┘
```

### Module Structure

```
xearthlayer/src/prefetch/
├── mod.rs                       # Module exports
├── strategy.rs                  # Prefetcher trait (run loop interface)
├── adaptive/
│   ├── mod.rs                   # Adaptive module exports
│   ├── coordinator/
│   │   ├── core.rs              # AdaptivePrefetchCoordinator (plan, drain, settle)
│   │   ├── filtering.rs         # Four-tier filter pipeline
│   │   ├── plan_executor.rs     # Submission under backpressure and throttle
│   │   ├── runner.rs            # Prefetcher impl (async loop)
│   │   ├── constants.rs         # Timing constants (MIN_CYCLE_INTERVAL, ...)
│   │   ├── telemetry.rs         # Telemetry processing helpers
│   │   ├── time_budget.rs       # Plan time budget validation
│   │   ├── status.rs            # Coordinator status types
│   │   └── status_updater.rs    # Shared status for the TUI
│   ├── prefetch_box.rs          # PrefetchBox (heading-biased sliding box)
│   ├── extent.rs                # Speed-proportional box extent
│   ├── boundary_strategy.rs     # BoundaryStrategy (region lifecycle)
│   ├── scenery_window.rs        # SceneryWindow (rebuild detection, dimensions)
│   ├── strategy.rs              # AdaptivePrefetchStrategy trait, PrefetchPlan
│   ├── phase_detector.rs        # PhaseDetector (Ground/Transition/Cruise)
│   ├── transition_throttle.rs   # TransitionThrottle (takeoff ramp-up)
│   ├── config.rs                # AdaptivePrefetchConfig
│   └── calibration.rs           # StrategyMode, PerformanceCalibration
```

### Removed Components

| Component | File | Reason |
|-----------|------|--------|
| `CruiseStrategy` | `cruise_strategy.rs` | Replaced by `BoundaryStrategy` |
| `BandCalculator` | `band_calculator.rs` | Band model replaced by boundary prediction |
| `BoundaryPrioritizer` | `boundary_prioritizer.rs` | Boundary logic absorbed into `SceneryWindow` monitors |
| `TurnDetector` | `turn_detector.rs` | Not needed: the box's heading bias follows the track every cycle |
| `BoundaryMonitor`, `BoundaryCrossing` | `boundary_monitor.rs` | Fired too late at DSF boundaries (#86); replaced by `PrefetchBox` in PR #102 |
| `GroundStrategy` | `ground_strategy.rs` | Ring-based ground prefetch; the ground phase now uses the box, unbiased (#172) |
| `PerformanceCalibrator`, `RollingCalibrator`, `ThroughputObserver` | `calibration/` | Never wired: nothing fed them samples and nothing read the result (#277) |

### Demand and prefetch lanes

Prefetch used to share every contention point with FUSE: the request channel,
the daemon coalescer, the job channel, the `tile_generation` group, the resource
pools and the HTTP semaphore. At a boundary crossing or a turn, prefetch
legitimately queues thousands of tiles, and X-Plane ended up waiting behind them
until the generation timeout (#296). Prefetch and FUSE now run in separate lanes.

**The no-harm contract.** A FUSE request is never slower because prefetch exists
than it would be with prefetch disabled. It is measured as FUSE demand-generation
latency (p95 and max) and timeout count, prefetch on against prefetch off.

**Two lanes.**

- The **demand lane** is always local: an `ExecutorDaemon` and `JobExecutor` with
  their own coalescer, request channel, job channel, pools, `tile_generation`
  group and HTTP semaphore. FUSE reaches it through `DdsClient::request_dds`,
  which is demand only.
- The **prefetch lane** sits behind the `PrefetchLane` trait. `LocalPrefetchLane`
  is a second `ExecutorDaemon` and `JobExecutor` with their own everything. The
  adaptive coordinator and prewarm are its consumers.

Lanes are handed to consumers at construction rather than routed by
`RequestOrigin` at run time, so FUSE cannot land in the prefetch lane by type.
`RequestOrigin` remains for metrics. The two lanes share the memory cache, the DDS
disk cache and the chunk disk cache by `Arc`; writes are idempotent. Coalescing is
per lane. The coordinator, the region state machine and the demand lane stay on
the primary in every mode.

**What the lanes share.** Besides the three caches, both lanes' job factories hold
the same texture encoder. With the CPU backends (`software`, `ispc`) that is only
shared code, but with `texture.compressor = gpu` every encode from both lanes goes
through one channel to one GPU worker (`dds/gpu_channel.rs`), so a prefetch encode
can sit ahead of a FUSE encode there.

**Admission pause.** `DemandTracker` (`executor/demand.rs`) counts demand jobs in
flight, a job being a FUSE cache miss that has a generation running. The count is
held by an RAII `DemandGuard` released on every exit (success, failure, the FUSE
timeout cancelling the job, a failed submit). It publishes a `watch<bool>`: paused
while any demand job is in flight, resumed when the count returns to zero. Cache
hits never pause. The prefetch lane's executor is built with that receiver as its
admission gate: while paused it admits no queued job, jobs already admitted run all
their tasks, and admission retries on resume. See the job executor design for the
gate itself.

**Always claim.** FUSE always starts its own job in the demand lane. There is no
cross-lane coalescing or cancellation, so the only duplicate work is a prefetch
job already running when FUSE asks for the same tile.

**Cache probe.** Every prefetch-lane tile job starts with `CachedTileProbeTask`
(`tasks/cached_tile_probe.rs`). It checks the memory and DDS disk caches and, on a
hit, records that fact so `DownloadChunksTask` and `BuildAndCacheDdsTask` return at
once. This turns tiles that demand generated during a pause into no-ops. Hits are
counted as `skipped_cached`.

**Sizing.** Demand keeps the full derived capacity, and the prefetch lane is added
on top, scaled by `prefetch.capacity_share` (default 0.35, range 0.05 to 0.50).
`LaneSizing` (`executor/lane_sizing.rs`) derives it from the demand lane's
`ResourcePoolConfig::default()`; each figure is rounded up and never below one:

| Resource | Demand lane | Prefetch lane | Prefetch formula |
|----------|-------------|---------------|------------------|
| CPU pool | 40 | 14 | `ceil(demand cpu x share)` |
| Network pool | 60 | 21 | `ceil(demand network x share)` |
| Disk I/O pool | 64 | 23 | `ceil(demand disk_io x share)` |
| `tile_generation` group | 512 | 70 | `2 x (prefetch cpu + prefetch network)` |
| HTTP semaphore | 1,024 | 1,024 | `DEFAULT_MAX_CONCURRENT_HTTP`, not scaled |

The figures are for a 32 core machine at the default share. The share scales only
the pools whose work competes with X-Plane for cores: encoding, and the TLS and
JPEG decoding network tasks do. HTTP connections are not scaled. A connection
waiting on the network costs the simulator nothing, and prefetch downloads are
bound by connections in flight: on a 5 Gbit link, a prewarm with 359 permits
(the share applied to 1,024) moved about 2,000 chunks/s at about 180 ms per
chunk while CPU sat near 5% and the link near 4%, and raising the permits to
512 raised the rate in proportion. Before the lanes, prefetch drew on the whole
shared 1,024, so scaling them made prewarm slower than 0.4.x.

The tokio blocking pool
covers both lanes: `LaneSizing::blocking_threads_required()` is demand cpu +
demand disk_io + prefetch cpu + prefetch disk_io + `BLOCKING_POOL_HEADROOM`
(173 on the reference machine). Admitted prefetch jobs finish after a pause, so
transient overlap with demand can reach 135% of the old permit totals at the
default share (150% at the 0.50 ceiling). Under #227 the glibc arena settles at the
worst peak, so the memory floor should be checked on a flight after changing the
share.

**The `PrefetchLane` contract.** The lane is written to be met over a network, so
prefetch can later run on another machine:

```rust
pub trait PrefetchLane: Send + Sync {
    fn submit(&self, tile: TileCoord) -> Result<(), LaneError>;
    fn submit_with_receipt(&self, tile: TileCoord) -> Result<LaneReceipt, LaneError>;
    fn load(&self) -> f64;
}
```

`submit` is fire-and-forget. `submit_with_receipt` is for prewarm, which needs
per-tile progress: the `LaneReceipt` resolves to a `LaneOutcome` (`Generated`,
`AlreadyCached`, `Failed`, `Dropped`) and never carries tile bytes. `load()` is
0.0 to 1.0 and feeds coordinator backpressure: how saturated the lane's queue is,
measured as jobs waiting for admission against the `tile_generation` group size.
`LaneError` is `Full` (retry later) or `Closed`. A submission the lane accepts is
never dropped afterwards: the local lane's daemon waits for room in its executor's
queue instead of shedding, so a saturated executor backs up the request channel,
`submit` returns `Full`, and the coordinator keeps the tile pending. Shedding there
used to lose tiles the coordinator had already counted as submitted, and nothing
asked for them again. The result of a submission is a complete tile appearing in the
primary's DDS disk cache, which is how the coordinator already learns of completion
(`promote_completed_regions`). Pause is not a trait method: the lane is built with
the demand state and stands aside on its own.

"A tile appears in the cache" carries four obligations that `LocalPrefetchLane`
meets by writing through `CacheLayer`, and a remote lane would have to meet too:

1. **Completeness:** only tiles with all 256 chunks persist (#180).
2. **Integrity:** the write is atomic and never visible half-written (`write_atomic`).
3. **Identical encoding:** format and provider match the primary's configuration.
4. **Survival:** disk GC may evict a prefetched tile before it is used.

Cache GC housekeeping jobs run on the local prefetch lane through
`LocalPrefetchLane::job_submitter()`, a local-only method outside the trait, so
they pause with prefetch. Offloading would have to move GC back to a local executor
because it maintains the primary's caches. GC jobs count among the lane's running
jobs, so a GC run shows as lane activity in the TUI and the sample line even when
no prefetch is in flight.

Cancelling prewarm stops it issuing tiles, but work already submitted to the lane
runs to completion: a receipt carries no cancellation, so tiles in flight finish
and land in the cache.

---

## Three-Window Model

The prefetch system maintains three logical windows that drive all prefetch decisions:

### Window Definitions

```
Direction of flight →

┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   ┌───────────────────────────────┐                                 │
│   │   X-Plane Window              │   ┌───────────────────┐        │
│   │   (assumed from measurements) │   │  Target Window     │        │
│   │                               │   │  (what X-Plane     │        │
│   │          ✈ aircraft           │───▶  will load next)   │        │
│   │                               │   │                    │        │
│   │                               │   │  THIS is what we   │        │
│   └───────────────────────────────┘   │  prefetch          │        │
│                                       └───────────────────┘        │
│   ┌─────────────────────────────────────────────────────────┐      │
│   │   XEL Window                                             │      │
│   │   (what we've already prefetched / cached)               │      │
│   │   Should always CONTAIN the Target Window                │      │
│   └─────────────────────────────────────────────────────────┘      │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

| Window | Source of Truth | Purpose |
|--------|-----------------|---------|
| **X-Plane Window** | `PrefetchBox::update_retention` (`RetainedRegion` layer): a box around the aircraft with a buffer. Inference from FUSE reads was deleted in #204 | What X-Plane currently holds in memory (assumed) |
| **XEL Window** | GeoIndex `PrefetchedRegion` layer | What XEL has confirmed as cached (DSF region granularity) |
| **Target Window** | The prefetch box (`PrefetchBox::bounds_with_shape`) | Everything X-Plane may load next; deliberately larger than the next row/column, so it contains them whichever edge the aircraft approaches |

### The Prefetch Decision

```
Prefetch work = Target Window \ XEL Window

If empty → idle (we're ahead of X-Plane)
If non-empty → expand to DDS tiles, filter, submit
```

### Region States (GeoIndex PrefetchedRegion layer)

Each DSF region in the XEL Window has one of five states:

| State | Meaning | Prefetch Behaviour |
|-------|---------|-------------------|
| *absent* | Never evaluated | Include in target diff → submit |
| `InProgress` | Tiles submitted, awaiting completion | Skip bulk submission (prevents duplicate work) |
| `Prefetched` | All tiles confirmed cached | Exclude from target diff entirely |
| `NoCoverage` | Scenery index attributes zero tiles to this region | Exclude from target diff, skip silently |
| `Deferred { retry_after }` | Has indexed tiles but made no progress; non-terminal | Excluded from target diff until `retry_after`, then eligible again — the one state that *expires* |

This follows a **two-phase commit** pattern:
1. **Reserve**: Region marked `InProgress` when tiles are submitted (prevents duplicate bulk submissions)
2. **Confirm**: Region marked `Prefetched` when tiles are confirmed in cache
3. **Timeout**: If `InProgress` exceeds `stale_region_timeout`, `evaluate_stale_regions`
   consults `BoundaryStrategy::region_disk_state` — the same completeness predicate the
   fast path uses — and picks one of four outcomes:

   | `RegionDiskState` | Outcome |
   |--------------------|---------|
   | `Complete` | Promote straight to `Prefetched` (the rescue path; see [Region Tile-Set SSOT](#region-tile-set-ssot) below); clear the region's strike count |
   | `Incomplete { coverage }` | Exact `covered`/`total` counts on this path — it asks for `CoverageDetail::ExactCounts`. **Progress-based, not time-based.** If `covered` grew since the previous evaluation, the region is advancing: clear strikes, revert to *absent* and retry immediately. If `covered` did not grow, it's a strike: increment the strike count and mark `Deferred { retry_after: now + deferral_for(strikes) }` |
   | `NoTiles` | The *only* path to `NoCoverage` — the scenery index attributes zero tiles to this region, which is definitive on first look (the index is fully built before the coordinator sees it). Retire to `NoCoverage`, clear the region's strike count |
   | `Unknown` | No authoritative source to consult (no `DdsDiskCacheChecker` and/or no `SceneryIndex`) → left `InProgress` untouched; this does **not** count as a strike, since "cannot tell" must not be treated the same as "checked and it is absent" |

   `Incomplete` reaches `NoCoverage` by no path — this is the fix for #226. Before it,
   `Incomplete` and `NoTiles` shared a retirement arm keyed on a fixed count of three
   timing-based strikes, so three consecutive slow evaluations could permanently retire a
   region that genuinely had scenery, just slow to complete. Real field evidence: two
   Baden-Württemberg regions were retired to `NoCoverage` this way, then disproved
   2m43s later when on-demand FUSE generation served tiles from the very regions
   prefetch had given up on.

   **The deferral ladder** (`DEFERRAL_LADDER` in
   `xearthlayer/src/prefetch/adaptive/coordinator/core.rs`) is **20s / 30s / 40s / 60s**,
   1-based on the strike count; the 60s entry repeats for every strike beyond the
   fourth — the ladder never grows past it. The 60s ceiling is a hard requirement, not
   a tuning knob: it comes directly from the 2m43s field evidence above — the window in
   which a region is skipped with nothing in flight must stay well inside the gap
   between "prefetch gives up" and "the sim demands it anyway".

   **`Deferred` re-enters through the planner, not through `evaluate_stale_regions`.**
   A `Deferred` region's `GeoIndex` entry is left in place (unlike the `Incomplete`
   advancing case, which removes it outright); `PrefetchedRegion::should_prefetch`
   is the gate that lets it back in once `retry_after` passes, at which point the
   normal target-diff submission path picks it up like any other absent region.
   `evaluate_stale_regions` never revisits it directly — a `Deferred` region is not
   `InProgress`, so `is_stale()` (which is `InProgress`-only; see
   `test_deferred_is_never_stale`) never matches it. Submission is lossy — a plan can
   drop regions under backpressure — so relying on `evaluate_stale_regions` to also
   re-drive `Deferred` regions would risk a region that never actually gets
   re-submitted.

A region can reach step 1 without submitting anything. When the filter
pipeline removes a region's *entire* planned tile set as already cached, there
is no submission for the mark-after-submit rule to hang off — and if that is
true of every region in the plan, the plan is empty and the submit path is
skipped outright. Such regions are marked `InProgress` at the end of
filtering, so step 2's disk-verified check confirms them on the maintenance
pass. Without this they stayed *absent*, re-entered the target diff every 2s
cycle, and could never return to `Prefetched` — which under-reported
`regions_prefetched` in the 60s sample. Patch-owned regions are excluded:
their tiles filter out in full too, but prefetch never places them on the DDS
disk, so `InProgress` would be a claim nothing could confirm.

That case is one instance of the general rule: a region is marked `InProgress`
once **none of its planned tiles is outstanding**, where a tile stops being
outstanding when the executor accepts it or the filter removes it as cached.
The two can happen in different calls. Prefetch submits until the request
channel is full and keeps the remainder as pending, and a cruise strip (about
1,500 tiles) is larger than the channel (1,000), so most plans are finished by
later `boundary_pending` drains, which re-filter what they carry. The coordinator
therefore keeps each plan's outstanding tiles until they settle, across drains,
rather than judging a region inside a single submission. Judging it inside one
call left every region split by the channel cut, and every region with even one
cached tile, permanently unmarked: re-planned as new on the next cycle, with each
tile still queued in the executor passing the filter and being submitted again
(#296).

### Region Tile-Set SSOT

`SceneryIndex::tiles_in_region(region)` is the single definition of "which DDS
tiles belong to this DSF region": a tile belongs to the region containing its
geographic centre (the `LOAD_CENTER` of its `.ter` file, which since #327 is
computed from the file name rather than read: see "Building the index" below). The submit, promote,
and rescue paths all consult this one method, so they cannot disagree about
whether a region is complete. Earlier code reconstructed the tile set from a
45nm radius query per path, which returns a circle overlapping neighbouring
regions rather than the region itself — the source of several promotion bugs
fixed under #176.

### Building the index

`SceneryIndex::build_from_package` lists a package's `terrain/` folder and
builds each tile from its file name with `tile_from_ter_name` (#327). An
Ortho4XP name, `<row>_<col>_<provider><zoom>[_water_overlay|_sea_overlay].ter`,
carries the texture's row, column and chunk zoom, and the sea flag; the centre
is the Web Mercator centre of the texture's 16x16 chunks, the corner of chunk
(row + 8, col + 8). Checked against all 4,980,320 `.ter` files of an installed
world, the computed centre is within 5e-6 degrees of `LOAD_CENTER` (its printed
rounding) and never in another 1 degree region, and a full index built both
ways matched tile for tile. A whole-world rebuild fell from 260 s to 17 s with
a warm page cache, and no longer depends on reading five million files cold.
Any other name is read and parsed as before, so a hand-made package still
indexes.

`BoundaryStrategy::region_disk_state` (`xearthlayer/src/prefetch/adaptive/boundary_strategy.rs`)
is the single completeness predicate built on top of that tile set, consumed
by both the fast path (`promote_completed_regions`) and the rescue path
(`evaluate_stale_regions`). It returns one of four states — `Complete`,
`Incomplete`, `NoTiles`, or `Unknown` — rather than a `bool`, because the
answer feeds a decision whose terminal outcome is permanent exclusion
(`NoCoverage`): "we cannot tell" (`Unknown` — no `DdsDiskCacheChecker` and/or
no `SceneryIndex` wired) must stay distinguishable from "we checked and it is
absent" (`Incomplete`/`NoTiles`), or a region could be retired to `NoCoverage`
on a cycle where it was never actually checked.

**How thoroughly it scans is a parameter, not a second function.** Callers pass
a `CoverageDetail`. The rescue path asks for `ExactCounts` because `covered` is
what separates an advancing region from a stuck one, and it can afford the full
scan: it only ever looks at regions already stale for `stale_region_timeout`
(120s). `promote_completed_regions` asks for `FirstMissOnly` and stops at the
first absent tile, because it discards the counts and runs on every coordinator
cycle (2s) over every `InProgress` region. The difference is not academic: when
the XEL DDS cache misses, the disjunction below falls through to
`OrthoUnionIndex::dds_tile_exists`, which tries four filename prefixes and —
because `textures/` is in `LAZY_DIRECTORIES` and is never indexed — resolves
each by `stat()`ing every installed source, so one absent tile costs roughly
4 × N_sources syscalls. Counting a cold region's whole tile set every 2s steals
CPU from the executor in exactly the backlog condition this machinery exists to
relieve.

A `FirstMissOnly` answer reports `Incomplete { coverage: TileCoverage::NotCounted }`.
The variant carries no payload, so "not counted" cannot be mistaken for the
observation `covered: 0` — a fabricated zero would read as "nothing has
arrived" and drive a strike. `TileCoverage::counts()` returns an `Option`,
forcing every reader to handle the absence explicitly.

**Coverage is a disjunction.** A tile counts as covered if it is present in
the XEL DDS disk cache (`DdsDiskCacheChecker`) **or** ships inside an
installed scenery package (`OrthoUnionIndex::dds_tile_exists`) — prefetch
deliberately never downloads tiles the disk filter (stage 3 of the four-tier
filter) already excluded as package-shipped, so promotion must recognise
that same evidence or a region covered entirely by package imagery could
never be confirmed and would ratchet to `NoCoverage` over land. The two
lookups use different coordinate systems: the XEL DDS cache is keyed by
**tile** coordinates (`tile.row, tile.col, tile.zoom`), while
`OrthoUnionIndex::dds_tile_exists` is keyed by **chunk** coordinates
(`tile.chunk_origin()`) — mixing them up silently returns "not covered" for
every tile (the #172 keying bug).

**Strikes are progress-based, and the strike count lives on the coordinator,
not in the `GeoIndex`.** `region_retry: HashMap<DsfRegion, RegionRetryState>`
(`xearthlayer/src/prefetch/adaptive/coordinator/core.rs`) tracks, per region,
`strikes: u8` and `last_covered: usize` — the covered-tile count observed at
the *previous* evaluation, deliberately a latest-observation, not a
high-water mark, because the DDS disk cache's GC daemon can evict tiles and
make coverage regress; a peak would blind progress detection until coverage
climbed back past it. A strike means "the covered count did not increase
since last time" — not "took too long" — so a region under a large backlog
that is genuinely, slowly advancing never strikes, and one that is truly
stuck (0 new tiles landing) strikes every evaluation.

`region_retry` is cleared in exactly three places, and each is a case where
the strike history stops being relevant to what happens next:
- **`Complete`** (both the fast path and the rescue path) — a promoted
  region's failure history belongs to the episode that failed, not to the
  region, so a region that recovers and is later demoted by
  `PrefetchStateObserver` should not resume the old episode's strike count.
- **`NoTiles`** retirement to `NoCoverage` — for the same reason: if the
  observer later disproves `NoCoverage` (a FUSE generation succeeds there),
  the region re-enters with a clean slate.
- **The advancing arm of `Incomplete`** (`covered > last_covered`) — strikes
  reset to 0 in place, because the region is making progress and the ladder
  should not carry over from before it started advancing.

It is deliberately **not** cleared on the stuck arm of `Incomplete` (the
strike increments instead) or when `PrefetchStateObserver` clears a
`Deferred` region's `GeoIndex` entry after an on-demand FUSE generation (see
[Demotion](#demotion-closing-the-loop-on-wrong-claims) below) — the observer
only holds a `GeoIndex` handle and structurally cannot reach
`region_retry`, and that absence of a reset is intentional: a region
demand-cleared mid-episode is still the same ongoing episode, not a fresh
one, so the 20/30/40/60s ladder keeps escalating across repeated
demand-driven clears rather than flattening back to 20s every time.

### Demotion: Closing the Loop on Wrong Claims

`Prefetched` and `NoCoverage` are claims, not guarantees. If FUSE has to
*generate* a tile on demand inside a region carrying either state, the claim
was wrong — whether from a scenery-index gap, a premature promotion, or
eviction of the tile after promotion. `PrefetchStateObserver`
(`xearthlayer/src/prefetch/state_observer.rs`) sits on the one FUSE code path
every implementation shares (`DdsRequestor::on_dds_response`, defaulted to a
no-op, overridden by `Fuse3OrthoUnionFS`) and reacts to exactly this:

```
Prefetched ──┐
             ├──▶ on-demand FUSE generation observed ──▶ demoted to *absent*
NoCoverage ──┘        (region re-enters the prefetch cycle)
```

`InProgress` regions are exempt — they are expected to miss while prefetch is
still running — and cache hits are exempt, since serving from cache is the
system working, not a divergence.

`Deferred` gets separate, milder handling (#226): an on-demand FUSE
generation there clears the region's `GeoIndex` entry immediately — demand
beats backoff, since a deferred region was never claimed *ready* in the
first place — but this does **not** count as a divergence: no
`prefetch_state_diverged` metric, no `prefetch_region_demoted` metric, and no
`120s` rate limit. It is a repair, not evidence of a wrong claim. This does
not license a longer deferral ladder: by the time FUSE asks for the tile,
the miss has already happened. See `test_fuse_miss_clears_deferral_without_counting_divergence`
in `xearthlayer/src/prefetch/state_observer.rs`.

Note also that this clear does not reset the coordinator's `region_retry`
strike count — see [Strike counts](#region-tile-set-ssot) above: the
observer only holds a `GeoIndex` handle and cannot reach `region_retry`,
and that is intentional, so the ladder keeps escalating across repeated
demand-driven clears of the same episode.

Re-entering the prefetch cycle only *repairs* two of the three causes. The
scenery index is immutable for the life of the process, so where the cause is
an index gap the next cycle re-derives the same empty tile set and re-marks
the region `NoCoverage`. The loop is bounded and safe, but for that cause the
observer flags the gap and nothing more — do not read a repeating
`state_diverged` in one region as a repair in progress.

Every divergence is counted and reported via
`MetricsClient::prefetch_state_diverged()`, but the region is only cleared
(and `MetricsClient::prefetch_region_demoted()` fired) at most once per region
per 120s window. Eviction inside the retained window can recur, so an
unbounded demote → re-prefetch → evict loop would churn on a long flight; the
rate limit exists so the diagnostic doesn't become the noise it's meant to
surface.

The daemon aggregates these and logs them on the same 60s cadence as the
memory sample. The counters among them are cumulative totals since process
start; nothing windows them, so difference successive samples to get a rate.
The `regions_*` state fields are gauges, assigned wholesale from the GeoIndex
each maintenance cycle rather than accumulated:

```
INFO Prefetch sample uptime_s=... regions_in_progress=... regions_prefetched=...
     regions_nocoverage=... regions_deferred_active=... promotions_normal=...
     promotions_rescue=... state_diverged=... regions_demoted=...
     regions_deferred=... fuse_generations=... fuse_latency_p50_ms=...
     fuse_latency_p95_ms=... fuse_latency_max_ms=... fuse_latency_count=...
     fuse_timeouts=... demand_in_flight=... prefetch_paused_secs=...
     skipped_cached=... lane_load=... lane_running=... lane_queued=...
```

The lane fields (#296) are the no-harm instrument:
- `fuse_latency_p50_ms`, `fuse_latency_p95_ms`, `fuse_latency_max_ms` and
  `fuse_latency_count` describe FUSE demand-generation latency over this sample
  only. Cache hits are excluded. Zeros mean no generation completed in the window.
- `fuse_timeouts` is per sample: requests abandoned at the FUSE timeout, including
  those that timed out while still queued for the demand lane, before any job
  started. Their wait, measured from when FUSE made the request, is inside the
  latency figures.
- `demand_in_flight` is a gauge of demand jobs running now.
- `prefetch_paused_secs` is how long the prefetch lane stood aside for demand
  during this sample.
- `skipped_cached` is cumulative since process start: tiles the cache probe found
  already cached when their prefetch job was admitted.
- `lane_load` is `PrefetchLane::load()`, 0.0 to 1.0: jobs waiting for admission
  against the lane's `tile_generation` group size. `lane_running` is the jobs the
  lane has admitted and not finished. `lane_queued` is the jobs held waiting for
  admission, by a full group or by the demand pause, which is where work sits
  while demand has the lane paused.

The TUI shows the same latency over the whole session instead of one sample, so
the line is not blank at cruise, where most samples have no generations. The
median, 95th percentile and maximum come from a fixed-size log-spaced histogram
(about 5% relative width, 1 ms to 10 minutes, longer waits in the top bucket;
the maximum and count are exact) fed by the same FUSE waits as the per-sample
window, and the timeout count is the session total, shown when non-zero. The
line's colour comes from the most recent sample that had generations, held
across empty samples, so one early stall does not keep it red all flight. The
log fields above are unchanged.

The latency figures cover generations only: a cache hit never waits on a lane,
and counting it would swamp the percentiles with near-zero waits. That leaves
the line empty on a well-prefetched flight, which reads as a broken component,
so it also shows the session cached share: FUSE requests served from the memory
or DDS disk cache, over those plus FUSE generations (`fuse_jobs_submitted`).
Requests coalesced onto an in-flight job and all prefetch traffic are excluded.
The share is a whole percent, shown as `>99%` when some request was generated
but the rounded value would read 100. It uses the line's neutral style, so it
never competes with the amber or red of the latency figures. With no FUSE
request yet the line reads `no FUSE requests yet`; with requests but no
generations it reads `no FUSE generations yet │ 100% cached`.

To compare prefetch on and off, run the same route with `prefetch.enabled = true`
and `false` and compare `fuse_latency_p95_ms`, `fuse_latency_max_ms` and
`fuse_timeouts` across the samples.

**Two of these fields differ only by suffix and are easy to misread against
each other — read this before reading a flight log:**
- `regions_deferred_active` is a **gauge**: how many regions are `Deferred`
  *right now*, assigned wholesale from the `PrefetchRegionState` event each
  maintenance cycle, exactly like `regions_in_progress`, `regions_prefetched`,
  and `regions_nocoverage` above it. It goes up and down.
- `regions_deferred` is a **counter**: how many deferrals have *occurred*
  in total since process start, incremented once per stuck-`Incomplete`
  evaluation (`MetricEvent::PrefetchRegionDeferred`), same convention as
  `promotions_normal`/`promotions_rescue`/`state_diverged`/`regions_demoted`
  below it. It only goes up.

Misreading `regions_deferred_active` as the counter (or vice versa) matters
in practice: `regions_nocoverage` non-zero over land is an acceptance
criterion for #226, and `regions_deferred`/`regions_deferred_active`
non-zero but bounded is the expected healthy signal for a region that is
slow rather than absent. Confusing a climbing counter for a stuck gauge (or
the reverse) produces a false alarm in either direction.

`fuse_generations` mirrors `fuse_jobs_submitted` (aggregated from
`MetricEvent::JobSubmitted { is_fuse: true }`) — the only default-level
source for criterion 5 ("on-demand FUSE generations fall during cruise").
Per-tile FUSE request logging is `debug!`-only (#209), so without this field
that criterion is unreadable at default log level. It is cumulative since
process start, the same convention as the other counters on this line.

`promotions_normal` counts fast-path promotions (`promote_completed_regions`,
per-cycle region maintenance); `promotions_rescue` counts stale-region
rescue promotions (`evaluate_stale_regions`, the `RegionDiskState::Complete`
arm above). A healthy flight should show `promotions_normal` dominating; a
high `promotions_rescue` ratio means the fast path is stalling and the
rescue path is carrying the work — see [Region Tile-Set
SSOT](#region-tile-set-ssot).

`state_diverged` and `regions_demoted` are expected to stay at (or near) zero
on a healthy flight; a climbing `state_diverged` with a flat `regions_demoted`
means the rate limit is suppressing repeat divergences in the same region —
worth investigating even though no region is currently stuck in a wrong
state.

### Retention Inference

The X-Plane Window is inferred, not observed directly. X-Plane's measured
window is ~3° of latitude by ~3°/cos(lat) of longitude, and it keeps scenery
it has loaded for a while after the aircraft moves on. Rather than model that
window separately, retention is derived from the prefetch box, which already
contains it:

```
Retained area = every DSF region the box touches
              + `window_buffer` regions on each side (default 1)

Regions inside retained area → RetainedRegion in GeoIndex
Regions previously retained but now outside → RetainedRegion removed
```

`PrefetchBox::update_retention_with_shape` maintains this every cycle, in
cruise only: on the ground there is no "behind" to retain, and updating it
there would evict regions that legitimately live away from the aircraft, such
as prewarm state.

When a region leaves the retained area, the coordinator also evicts:
- Its `PrefetchedRegion` entry (making it eligible for re-prefetch if the aircraft returns)

`InProgress` regions are never evicted — they represent actively running prefetch jobs.

All retention decisions are logged at `DEBUG` level for flight test analysis.

---

## SceneryWindow

In v2.0 `SceneryWindow` was the core model: it derived X-Plane's window
dimensions, ran the boundary monitors and tracked retention through an
`Uninitialized → Assumed → Ready` state machine. PR #102 moved prediction to
`PrefetchBox`, and retention moved there too (see above).

What remains (`adaptive/scenery_window.rs`) is a holder for the window
configuration and a latitude-dependent column count. Nothing outside its own
tests constructs it, so it is dead code awaiting removal.

### Sliding Prefetch Box (Cruise Phase)

The cruise phase uses a **sliding prefetch box** instead of boundary monitors. The box moves with the aircraft every telemetry tick, biased in the direction of travel. This replaced the boundary-monitor approach after flight testing revealed fundamental timing issues (issue #86).

#### Problem with Boundary Monitors

Flight testing over dense European coverage (LOWW->LPPT, heading ~260° WSW) showed that X-Plane loads tiles **3.3-3.7° ahead** of the aircraft. The boundary crossing fires when the aircraft is ~1° from the window edge, but by then X-Plane has already requested tiles 2° beyond the edge -- prefetch is too late. Result: 20-second freeze at DSF boundaries.

**Key finding:** X-Plane loads 2° of new tiles beyond its current boundary when the aircraft is ~1° from that boundary. Maintaining tiles prefetched 3° ahead always covers this:

```
Aircraft --1°--> XP edge --2°--> XP new load limit
Aircraft --------3°-------------> Prefetch box edge
                                  Covered
```

#### Speed-Proportional Box Extent

The box extent scales linearly with ground speed via `ExtentCalculator::compute_extent()`:

```
t = clamp((ground_speed_kt - 40.0) / (450.0 - 40.0), 0.0, 1.0)
extent = box_min_extent + t * (box_extent - box_min_extent)   // 3.5 + t * 3.5 by default
```

| Ground Speed | Extent (defaults) |
|-------------|--------|
| 40 kt | 3.5° |
| 140 kt (approach) | ~4.4° |
| 250 kt (climb/descent) | ~5.3° |
| 450 kt (cruise) | 7.0° |
| > 450 kt | 7.0° (clamped) |

The maximum is `prefetch.box_extent` (default 7.0°, `DEFAULT_BOX_EXTENT`), the minimum `DEFAULT_BOX_MIN_EXTENT` (3.5°). The ramp applies in the **Cruise** phase only. The **Ground** phase uses the full `box_extent` with a symmetric 0.5 bias, a square box centred on the aircraft, and the **Transition** phase (takeoff) does not prefetch at all.

**Rationale:** At approach speeds (~140 kt), a fixed full-size extent prefetched tiles 25+ minutes ahead of the aircraft, wasting resources and contributing to swap-in storms when those tiles eventually entered memory. Scaling extent to speed ensures prefetch depth stays proportional to how far ahead the aircraft will travel in the next few minutes.

The coordinator computes extent each cycle from live ground speed and passes it to `PrefetchBox` as part of a `BoxShape`. `PrefetchBox` itself remains a pure geometry calculator: extent is an input parameter, not internal state.

**Scale.** A 1°×1° DSF holds roughly 200 to 300 DDS tiles at ZL16 in mid latitudes (about 11 columns and 16 rows of tiles at 46°N, plus higher zoom tiles around airports). The full 7° box covers up to 7×7 = 49 DSFs, so one new row or column of DSFs entering the box is around 1,500 tiles, and a turn, which swings the heading bias onto the other axis, can bring in several such strips at once.

#### Box Shape and Proportional Heading Bias

The box is a rectangle in lat/lon space with **proportional heading bias**:

- **Total extent:** Speed-proportional in cruise (3.5° to 7.0° by default, see above). Covers X-Plane's observed loading area with overlap scaled to approach speed.
- **Proportional bias formula:** `forward_fraction = 0.5 + (max_bias - 0.5) * |component|`
  - At cardinal headings: primary axis gets 80/20 (at `max_bias=0.8`), perpendicular axes get 50/50
  - At diagonals: both axes share ~71/29
  - The bias slides smoothly -- no binary thresholds

The forward/behind determination uses the track heading decomposed into lat/lon:

```
lon_forward = -sin(track)   // positive = moving east
lat_forward = -cos(track)   // positive = moving north
```

**Examples:**

| Heading | Lon bias | Lat bias | Box shape |
|---------|----------|----------|-----------|
| 270° (due west) | 3° west, 1° east | 2° each side | Biased west |
| 225° (southwest) | 3° west, 1° east | 3° south, 1° north | Biased SW |
| 260° (WSW, LOWW->LPPT) | 3° west, 1° east | 3° south, 1° north | Biased WSW |

#### Per-Cycle Operation

Telemetry arrives several times a second; a prefetch cycle runs at most every
2 s (`MIN_CYCLE_INTERVAL`). Each cycle (`process_telemetry`) does one of two
things:

**If tiles are pending from an earlier cycle**, it drains them and plans
nothing new. The pending tiles are re-filtered first (anything cached in the
meantime is settled without being submitted), then submitted as a
`boundary_pending` plan.

**Otherwise it plans:**

1. Compute the box from position, track and phase (see above)
2. Enumerate the DSF regions it intersects (via `floor()` arithmetic)
3. Keep only regions `PrefetchedRegion::should_prefetch` admits: absent, or
   `Deferred` with its `retry_after` passed. `InProgress`, `Prefetched` and
   `NoCoverage` are skipped
4. Expand each region to its tiles with `SceneryIndex::tiles_in_region`; a
   region with none is marked `NoCoverage`
5. Apply the four-tier filter (memory, patch, installed package, DDS disk)
6. Submit the survivors (see Throttling)
7. Mark each region `InProgress` once none of its tiles is outstanding (see
   *Region States*: this can complete in a later cycle's drain)

Tiles are submitted in plan order. There is no distance sort and no per-cycle
cap: both were removed after the #172 flight, where a distance sort combined
with a cap starved the far side of the box.

Region maintenance (stale `InProgress` evaluation, promotion to `Prefetched`,
and eviction of regions outside the retained window, so they can be prefetched
again on return) runs at the end of every cycle, whether or not anything was
submitted.

#### Throttling

- **Backpressure:** the higher of executor load and FUSE pressure (see
  *Backpressure*). Above 0.8 the cycle is deferred and its whole plan kept as
  pending; above 0.5 half the plan is submitted and the rest kept as pending
- **Transition throttle:** after takeoff, the submitted fraction ramps from 25%
  to 100% over 30 s; the overflow is kept as pending
- **Lane capacity:** tiles are submitted until the lane returns
  `LaneError::Full` (its request channel, 1,000 requests, is full); the remainder
  is kept as pending. Prefetch never waits for space, and pending tiles are never
  dropped
- **Admission pause:** while FUSE is waiting on generation the lane admits no new
  job, so submitted tiles queue in the lane until demand clears
- **Deduplication:** `PrefetchedRegion` state in the GeoIndex keeps a region
  from being planned twice. Tiles already queued for generation are not
  filtered by prefetch; if one is submitted again, the daemon coalescer joins
  it to the job in flight (coalescing is within the prefetch lane only)

#### Design Decisions

1. **DSF-native math:** Box margins are in degrees, region enumeration uses integer DSF coordinates (`floor()`). No cos(lat) correction -- raw degrees match X-Plane's loading model.
2. **No component threshold:** Any non-zero heading component biases that axis. Only exact cardinal headings produce symmetric distribution on the perpendicular axis. Rationale: even a slight component means the aircraft will eventually need those tiles. Simplicity over cleverness.
3. **One box for every phase:** the ring-based `GroundStrategy` was deleted after #172. On the ground the same box is used, at full extent with a symmetric bias; in cruise it is speed-proportional and heading-biased. The phase detector selects the shape, and retention tracking runs in cruise only, since a stationary aircraft has no "behind" to retain.
4. **Transition phase:** The box applies immediately at full size on entering cruise. The `TransitionThrottle` ramp independently controls submission rate (25% -> 100% over 30s).

**Empirical basis:** Debug map flight testing at YBAS (2026-03-21) revealed X-Plane loads a 6x6 DSF area, not the 3x3 previously assumed. The proportional bias model was designed to match X-Plane's directional loading pattern without binary bias switching artefacts.

Retention tracking uses `PrefetchBox::update_retention_with_shape()` with box-derived bounds (see [Retention Inference](#retention-inference)).

### World Rebuild Detection (not implemented)

v2.0 specified rebuild detection: a FUSE burst covering more than half the
window and centred on the aircraft (a teleport or rendering change) would clear
the `PrefetchedRegion` and `RetainedRegion` layers and re-derive the window.
**No code implements this today.** After a teleport, the box simply moves:
regions at the new position are absent and are planned as new, and the old
regions fall out of the retained area and are evicted. What is lost is only the
immediate reset; regions marked `Prefetched` near the old position are not
cleared until they leave the retained area.

---

## BoundaryStrategy

Despite the name, `BoundaryStrategy` no longer plans anything. Since PR #102 it
holds the region lifecycle operations on the `PrefetchedRegion` layer: marking
regions `InProgress` or `NoCoverage`, promoting them to `Prefetched` against
the DDS disk (`promote_completed_regions`), reporting a region's disk state
(`region_disk_state`), and evicting regions outside the retained area
(`evict_non_retained`). The coordinator's `evaluate_stale_regions` uses
`region_disk_state` to drive stale `InProgress` regions and the `Deferred`
ladder. See [Region States](#region-states-geoindex-prefetchedregion-layer)
for the state machine.

### Algorithm (historical — superseded)

> **This block describes the boundary-monitor design and no longer matches the
> code.** `BoundaryCrossing` and its monitors were deleted in PR #102, which
> replaced them with the sliding `PrefetchBox` described above; step 4a's
> SceneTracker history is a write-only field; and step 4c's geometric fallback
> was removed in #176 — see *Region Tile-Set SSOT*, which is now the only
> definition of a region's tile set. It is kept for the reasoning about
> asymmetric load depth in step 1, which the measurements still support.

```
Input:  Vec<BoundaryCrossing> from SceneryWindow (sorted by urgency)
        XelWindow (GeoIndex PrefetchedRegion layer)
Output: PrefetchPlan { tiles: Vec<TileCoord>, estimated_time }

For each BoundaryCrossing:
    1. Determine DSF regions in the row/column

       ROW (lat crossing at lat 52, window width lon 7..11 at 50°N):
         Depth 0: [(52,7), (52,8), (52,9), (52,10)]    ← most urgent (3-4 cols wide)
         Depth 1: [(53,7), (53,8), (53,9), (53,10)]
         Depth 2: [(54,7), (54,8), (54,9), (54,10)]    ← 3 rows deep total
         = 9-12 DSF regions

       COLUMN (lon crossing at lon 11, window height lat 49..52):
         Depth 0: [(49,11), (50,11), (51,11)]           ← most urgent (3-4 rows wide)
         Depth 1: [(49,12), (50,12), (51,12)]            ← 2 cols deep total
         = 6-8 DSF regions

       Loading depth is asymmetric:
         ROW loads: 3 rows deep (73% of observed events)
         COL loads: 2 cols deep (64% of observed events)
         Width: 3-4 tiles perpendicular (matches window dimensions)

    2. Check SceneryIndex coverage for each region
       Regions with no scenery → mark NoCoverage in GeoIndex, skip

    3. Remove regions already in XEL Window (set difference)
       InProgress or Prefetched → skip
       NoCoverage → skip
       Absent → include

    4. For each remaining DSF region, get DDS tiles:
       SceneryIndex::tiles_in_region — the only source; a region the index
       knows nothing about yields no tiles and is marked NoCoverage

    5. Apply four-tier filter:
       Memory cache → Patch exclusion → Installed package disk → XEL DDS disk cache

    6. Order tiles:
       Primary: urgency rank (depth 0 before depth 1 before depth 2)
       Secondary: distance from aircraft position

    7. Mark submitted regions as InProgress in GeoIndex

    8. Append to PrefetchPlan
```

### Diagonal Flight and Turns

There is no per-axis prediction to order. On a diagonal track both axes of the
box are biased forward (about 71/29 each at 45°), so the regions ahead on both
axes are in the box at once. In a turn the bias swings with the track every
cycle, and the regions the swing brings into the box are planned as new, which
is why a turn can submit several strips' worth of tiles at once. Within a plan,
tiles are submitted in plan order: there is no urgency or distance ordering
(see [Per-Cycle Operation](#per-cycle-operation)).

### Interface

> **Stale**, like the algorithm block above: `BoundaryStrategy` does not
> implement `AdaptivePrefetchStrategy` — it exposes static functions
> (`promote_completed_regions`, `evict_non_retained`, `region_disk_state`)
> instead. The trait itself still exists (`xearthlayer/src/prefetch/adaptive/strategy.rs`);
> its current `calculate_prefetch` signature, for reference:

```rust
pub trait AdaptivePrefetchStrategy: Send + Sync {
    fn calculate_prefetch(
        &self,
        position: (f64, f64),
        track: f64,
        calibration: &PerformanceCalibration,
        already_cached: &HashSet<TileCoord>,
    ) -> PrefetchPlan;
}
```

---

## Strategy Mode

The trigger mode comes from `prefetch.mode` and nothing else:

```rust
pub enum StrategyMode {
    Aggressive,
    Opportunistic,   // default
    Disabled,        // never prefetch
}
```

**`Aggressive` and `Opportunistic` currently behave identically.**
`should_prefetch_now` returns `true` for both, and nothing else branches on the
mode. The names describe triggers from earlier designs: the position trigger
went with the boundary monitors (PR #102), and the "executor is quiet" trigger
was the circuit breaker removed in PR #96. Backpressure and the sim state gates
now apply in every mode, so in practice the setting is an on/off switch.

### Why there is no automatic selection

Version 1.1 of this design measured tile throughput during the initial scenery
load and picked a mode from it (above 30 tiles/sec aggressive, 10 to 30
opportunistic, below 10 disabled), with a rolling recalibration every 15
minutes. That machinery was built and unit tested but never wired: no
production code fed it completions, and `with_calibration` had no caller, so
the default `auto` mode always fell through to opportunistic.

It was removed in #277 rather than wired. Every later change to the prefetcher
(the sliding box, phase detection, the region state machine and its deferral
ladder, the transition throttle) was flight tested and tuned against a
prefetcher that was in fact running opportunistic. Turning selection on would
change the behaviour those parameters were chosen for, on a signal that has
never been exercised in the field. `auto` was retired with it; existing configs
that say `mode = auto` load as `opportunistic`.

`PerformanceCalibration` remains as a fixed estimate
(`PerformanceCalibration::default_opportunistic()`) that `PrefetchPlan` uses to
budget plan completion time.

---

## Integration Points

### SceneTracker Integration (removed)

The `SceneTracker` that was to supply `loaded_bounds()` and `loaded_regions()`
to a `SceneryWindow` was deleted in #204: nothing read it, and `SceneryWindow`
itself has no live constructor. The X-Plane window is a retention box around
the aircraft (`PrefetchBox::update_retention`), not an observation of FUSE
reads.

### GeoIndex Integration

The `GeoIndex` stores spatial state via type-keyed layers:

| Layer | Key | Value | Updated By |
|-------|-----|-------|------------|
| `PatchCoverage` (existing) | DSF region | patch name | OrthoUnionIndex (startup) |
| `RetainedRegion` (new) | DSF region | `()` | SceneryWindow (each cycle) |
| `PrefetchedRegion` (new) | DSF region | `RegionState` | BoundaryStrategy (on submit/complete/eviction) |

### PrefetchLane Integration

The coordinator and prewarm submit tiles through `PrefetchLane` (see
[Demand and prefetch lanes](#demand-and-prefetch-lanes)), never through
`DdsClient`, which is the FUSE demand path. The earlier `DdsClient::prefetch`,
`prefetch_with_cancellation`, `sender`, `executor_load` and `fuse_pressure` methods
were removed. Isolation, not priority, now keeps FUSE ahead of prefetch.

### Backpressure

Each cycle the coordinator takes the higher of two independent pressures and
compares it against the same thresholds:

- Load > 80%: defer entire cycle
- Load > 50%: submit reduced batch (50%)
- Load < 50%: full submission

**`PrefetchLane::load()`** is how saturated the prefetch lane's queue is: jobs
waiting for admission against its `tile_generation` group size, clamped to 1.0.
A lane busy with work it has admitted reads 0.0; it reaches 1.0 when a full
group's worth is queued behind the running one. It is not the lane's pool
utilisation, because a small lane's pools read full whenever it is productively
busy, which deferred most cycles. The deferral log still names this figure
`executor_load`.

**`PinnedTileBudget`'s pressure** is demand on the mount: memory pinned by open virtual DDS
handles, as a fraction of the ceiling that bounds it. Added in
[#246](https://github.com/samsoir/xearthlayer/issues/246).

The two are needed because they diverge exactly when it matters. At a boundary
crossing X-Plane opens many textures at once, and most of those reads are served
from cache, so they never become executor work and executor load stays low while
the mount is saturated. That collision is the peak concurrency event, and per
[#227](https://github.com/samsoir/xearthlayer/issues/227) the glibc arena grows
to cover the worst burst it ever sees and never gives the space back, so the
collision sets the memory floor for the whole session. Prefetch submitting at
full rate into it was making that peak larger than it needed to be.

Two earlier mechanisms do not cover this. `SimState::should_prefetch()` gates on
`async_scenery_load_in_progress`, which reports X-Plane's own DSF loader rather
than demand on us, and was never true during the flight that motivated the
change. Lane load misses cache-served reads as described above.

Open handles rather than a read rate, deliberately. `open` and `release` reach
the filesystem on every platform; reads do not. macOS serves virtual DDS through
the kernel page cache, because macFUSE faults when a `direct_io` file is
`mmap`ed, so a read-rate signal would be accurate on Linux and nearly blind
there. Handles also need no sampling window and no threshold of their own: the
fraction against the ceiling is already the 0 to 1 figure the thresholds above
compare against.

A deferral logs both figures and which one dominated, because calibrating this
means knowing which pressure caused a given cycle to yield:

```
Backpressure, deferring prefetch cycle, tiles stored as pending
  load=85.0% executor_load=12.0% fuse_pressure=85.0% source=fuse tiles_planned=1425
```

### TransitionThrottle

The existing `TransitionThrottle` ramps prefetch from 25% to 100% over 30 seconds after takeoff, preventing competition with X-Plane's post-takeoff scene loading. Unchanged.

### Four-Tier Filter Chain

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Prefetch Filter Chain                            │
│                                                                         │
│  1. MEMORY CACHE (async) - moka LRU query                              │
│     Skip tiles already generated (DaemonMemoryCache)                    │
│                                                                         │
│  2. PATCHED REGION EXCLUSION (GeoIndex PatchCoverage)                   │
│     Skip tiles in DSF regions owned by scenery patches                  │
│                                                                         │
│  3. INSTALLED PACKAGE DISK (OrthoUnionIndex) - filesystem probe         │
│     Skip tiles that already ship as real .dds files in a package        │
│                                                                         │
│  4. XEL DDS DISK CACHE (DdsDiskCacheChecker) - filesystem probe         │
│     Skip tiles XEL has already downloaded and cached itself             │
│                                                                         │
│  Only tiles passing ALL four filters are submitted for download         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Configuration

### Configuration Options

`docs/configuration.md` is the reference for every setting. The `[prefetch]`
keys that exist today, with their defaults:

```ini
[prefetch]
enabled = true                 ; master switch
mode = opportunistic           ; opportunistic | aggressive | disabled
web_api_port = 8086            ; X-Plane Web API, for position and SimState

; Takeoff transition: hold, then ramp submission from 25% to 100% over 30 s
takeoff_climb_ft = 1000
takeoff_timeout_secs = 90
landing_hysteresis_secs = 15
ramp_duration_secs = 30
ramp_start_fraction = 0.25

; Region lifecycle
window_buffer = 1              ; DSF regions retained beyond the box on each side
stale_region_timeout = 120     ; seconds before an InProgress region is re-evaluated

; Prefetch box
box_extent = 7.0               ; degrees per axis: ground extent, and cruise maximum
box_max_bias = 0.8             ; forward fraction on the primary axis

; Prefetch lane capacity as a fraction of the capacity FUSE demand gets
capacity_share = 0.35          ; 0.05 to 0.50

cycle_interval_ms = 2000       ; INERT: parsed but never read, see below
```

The cruise minimum extent (3.5°) and the speeds at either end of the ramp
(40 kt and 450 kt) are constants in `config/defaults.rs`, not settings.

**`cycle_interval_ms` has no effect.** It is parsed and carried into the
orchestrator configuration, but the runner paces cycles with the
`MIN_CYCLE_INTERVAL` constant (2 s) and never reads it. It is a removal
candidate, like the keys retired in #249 and #277.

### Deprecated Settings

Settings retired by earlier designs. All are in `DEPRECATED_KEYS` (`config/upgrade.rs`), so `xearthlayer config upgrade` removes them. The table records why for the v2.0 retirements; the v2.0 boundary-monitor keys (`trigger_distance`, `load_depth`, `default_window_rows`, `default_window_cols`, `max_tiles_per_cycle`, `ground_ring_radius`) were retired in turn by the sliding box:

| Setting | Replacement | Reason |
|---------|-------------|--------|
| `trigger_position` | `trigger_distance` | Position within DSF replaced by distance from window edge |
| `lead_distance` | `load_depth` | Static lead replaced by dynamic boundary prediction |
| `band_width` | (derived from window) | Derived from observed window dimensions |
| `track_stability_threshold` | (removed) | No TurnDetector — monitors handle turns naturally |
| `turn_threshold` | (removed) | No TurnDetector |
| `track_stability_duration` | (removed) | No TurnDetector |
| `strategy` | (removed) | No longer selecting between ground/cruise/flight_plan — coordinator auto-selects |
| `time_budget_margin` | (removed) | Time budget replaced by boundary urgency |
| `calibration_aggressive_threshold` | (removed, #277) | Configured a calibrator that never ran |
| `calibration_opportunistic_threshold` | (removed, #277) | Configured a calibrator that never ran |
| `calibration_sample_duration` | (removed, #277) | Configured a calibrator that never ran |

---

## Error Handling & Edge Cases

### Before the First Cycle

The box needs only position, track and ground speed, so prefetch can plan from
the first telemetry sample; there is no window to derive first. On the ground
the box is square and unbiased. During the Transition phase (takeoff) prefetch
plans nothing, and once Cruise begins the `TransitionThrottle` ramps
submission up.

### Ocean / Sparse Regions

Regions in the box with no tiles in the scenery index are marked `NoCoverage`
and skipped. The box keeps moving with the aircraft over uncovered areas, so
covered regions ahead enter it, and are planned, while the aircraft is still
over water. That makes the **no-coverage to coverage** transition the same as
any other: the coastline is prefetched before the aircraft reaches it.

### Starting in a No-Coverage Area

Nothing special: the regions around the aircraft are `NoCoverage`, and covered
regions are planned as soon as the box reaches them.

### Teleport / World Rebuild

There is no rebuild detection (see [World Rebuild Detection](#world-rebuild-detection-not-implemented)).
After a teleport the box is planned at the new position like any other, and
the old position's regions are evicted once they fall outside the retained
area.

### Aircraft Stationary or Orbiting

A stationary or orbiting aircraft keeps the box over the same regions. Once
those are `InProgress` or `Prefetched` there is nothing new to plan, and
cycles submit nothing, which is correct: the area is already covered.

### Stale Telemetry Safe Mode

When position telemetry goes stale (no update for **5 seconds**), the prefetch runner enters **safe mode**:

1. The `LoopState.telemetry_paused` flag is set to `true`
2. Tile submissions are paused — the coordinator loop skips prefetch evaluation
3. In-progress regions remain `InProgress` in GeoIndex (not evicted)

**On telemetry resume** (next position update arrives):

1. `telemetry_paused` is cleared
2. `AircraftState.on_ground` is read to determine the correct flight phase:
   - `on_ground = true` → PhaseDetector reset to `Ground`
   - `on_ground = false` → PhaseDetector reset to `Cruise`
3. The first live telemetry update immediately refines phase via real ground speed
4. `TransitionThrottle` applies normally after a Ground→Cruise transition

This avoids submitting speculative tiles based on an extrapolated stale position that may be wrong by minutes of flight time. It also handles paused sim gracefully — the sim pauses telemetry, safe mode activates, and on resume the coordinator picks up exactly where the aircraft is.

**`on_ground` propagation chain:**
```
SimState (X-Plane Web API)
  └─→ WebApiAdapter
        └─→ aircraft_position::AircraftState.on_ground
              └─→ prefetch::AircraftState.on_ground
                    └─→ PhaseDetector reset on telemetry resume
```

### Partial Tile Failure

If some tiles in a submitted region fail:
- Region stays `InProgress`
- After `stale_region_timeout` (120s), `region_disk_state` reports `Incomplete` — see
  [Region States](#region-states-geoindex-prefetchedregion-layer) above. If the covered
  count grew since the previous evaluation, strikes clear and the region reverts to
  *absent* for immediate retry. If it did not grow, that's a strike: the region is
  marked `Deferred` on the 20/30/40/60s ladder (the ceiling repeats indefinitely) and
  excluded from the target diff until `retry_after` passes — it is **never** retired to
  `NoCoverage` for this reason, however many consecutive strikes it accumulates
- Once `retry_after` passes (or immediately, on the advancing-strike path): target diff
  finds the region again, re-submits
- Four-tier filter handles individual tile dedup (only failed tiles re-submitted)

---

## Testing Strategy

### Unit Tests

| Component | Test Focus |
|-----------|-----------|
| `PrefetchBox` | Proportional bias at cardinal, diagonal and southern headings; constant total extent; region enumeration ahead and behind; `new_regions` skips tracked regions; retention covers the box and never evicts `Prefetched` regions |
| `extent` | Speed ramp endpoints, clamping and midpoint |
| `BoundaryStrategy` | `mark_in_progress` / `mark_no_coverage`; disk-verified promotion, including package-shipped tiles and region boundaries; `region_disk_state` counts; `evict_non_retained` preserves `InProgress` |
| `RegionState` | absent → InProgress → Prefetched; timeout revert; `NoTiles` → `NoCoverage`; progress-based strikes → `Deferred` on the 20/30/40/60s ladder; `Deferred` expiry via `should_prefetch` |
| `plan_executor` | Defer above 0.8 and halve above 0.5, from either executor load or FUSE pressure; transition throttle overflow; channel-full remainder kept as pending, never dropped |
| `PhaseDetector`, `TransitionThrottle` | Phase transitions; ramp start, duration and reset on landing |
| `GeoIndex layers` | RetainedRegion add/remove; PrefetchedRegion state transitions |

### Coordinator Scenarios

These run the real coordinator against test doubles for the DDS client, disk
checker and scenery index (`coordinator/test_support.rs`):

| Scenario | Validates |
|----------|-----------|
| First cruise tick, then later ticks and a long flight | A plan on the first tick; no region planned twice; new plans as the box moves |
| Cruise plan over covered terrain | Every tile of every intersecting region is planned, no cap |
| Uncovered region in the box | Marked `NoCoverage`, nothing submitted |
| Plan cut by the request channel | Remainder kept as pending and drained before any new plan |
| Region split by the channel cut, then drained | Region marked once its last tile is submitted; the box is not re-planned (#296) |
| Region partly cached | Cached and submitted tiles together complete it; not re-planned (#296) |
| Region entirely cached | Reaches `Prefetched` through disk-verified promotion (#176) |
| Deferred cycle, channel cap of one | No region marked before its tiles are submitted (#172) |
| Stuck and slow regions | Deferral ladder, strike reset on progress, never `NoCoverage` while tiles exist (#226) |
| Stationary aircraft over cached terrain | No promotions inflate the counters |
| Scenery loading, replay, paused | Prefetch skips or continues as the sim state requires |

### Flight Test Validation

Unit tests cannot show whether tiles arrive before X-Plane asks for them; only
a flight can. The procedure and acceptance criteria are in
[prefetch-flight-test-plan.md](prefetch-flight-test-plan.md). The default-level
signals are the 60 s Prefetch sample (`regions_*` gauges and `fuse_generations`,
which should fall during cruise) and the per-cycle `Prefetch cycle summary`
line, where `tiles_planned` far above `tiles_submitted` cycle after cycle means
plans are being cut by the request channel.

---

## References

- [X-Plane Scenery Loading Whitepaper v1.1](xplane-scenery-loading-whitepaper.md) — Empirical loading behaviour research
- [Flight Test Data](../../tests/flight-data/) — Compressed flight logs
- [Job Executor Design](job-executor-design.md) — Daemon architecture, job/task traits
- [GeoIndex Design](geo-index-design.md) — Geospatial reference database

---

**Document Version History**

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | January 2026 | Initial design based on flight test research |
| 1.1 | January 2026 | Added rolling recalibration, priority-based backpressure, design considerations |
| 2.0 | March 2026 | Major revision: replaced band-based cruise strategy with boundary-driven model. Three-window architecture (X-Plane / XEL / Target). Dual boundary monitors (lat/lon). Removed CruiseStrategy, BandCalculator, BoundaryPrioritizer, TurnDetector. Added SceneryWindow with window derivation, retention inference, and `Assumed` state for no-coverage starts. Two-phase commit for region tracking. GeoIndex extended with RetainedRegion and PrefetchedRegion layers. Updated configuration (deprecated 7 settings, added 6 new). Based on whitepaper v1.1 findings from 5 flights / 10.5+ hours of data. |
| 2.1 | March 2026 | **Window dimension and loading depth correction.** Empirical map-based measurement at EDDF/YPAD/WSSS revealed window is ~3° lat × ~3°/cos(lat) lon, not 9×9°. FUSE burst analysis (65 classified events with telemetry) confirmed asymmetric loading depth: ROW loads 3 deep (73%), COL loads 2 deep (64%), both 3-4 wide. Trigger distance ~1° from window edge. Updated defaults: `default_window_rows` 9→3, `default_window_cols` 9→auto, `trigger_distance` 3.0→1.0, `load_depth` 3→0 (auto: 3 rows, 2 cols). See whitepaper v1.2 for methodology. |
| 2.2 | October 2026 | Removed throughput calibration and the `auto` mode (#277). Mode is configuration only; see [Strategy Mode](#strategy-mode). |
| 2.3 | October 2026 | **Brought into line with the code.** The document still described the v2.0 boundary monitors, deleted in PR #102. Removed the monitor sections; rewrote the overview, goals, component diagram, module structure, per-cycle operation, throttling, retention, edge cases, configuration and testing strategy for the sliding box. Corrected the box extent to 3.5° to 7.0° (default `box_extent` 7.0). Recorded that world rebuild detection is not implemented, that `SceneryWindow` is dead code, and that `cycle_interval_ms` is inert. Documented that a region is marked once none of its tiles is outstanding, across drains (#296). |
| 2.4 | October 2026 | **Demand and prefetch lanes (#296).** Prefetch moved out of the shared pipeline into its own lane behind the `PrefetchLane` trait, paused while FUSE waits on generation. Added the Demand and prefetch lanes section (no-harm contract, admission pause, always claim, cache probe, sizing, the lane contract), replaced the `DdsClient` integration and backpressure inputs, and added the lane and FUSE latency fields to the Prefetch sample. Added `prefetch.capacity_share`. |
