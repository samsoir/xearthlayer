# Prefetch V4: Streaming Prefetch (speclet)

**Status:** proposed (2026-11-26), targeting 0.5.2. Part of the
compute-pipeline plan, [#475](https://github.com/samsoir/xearthlayer/issues/475).
Not implemented.

**Version:** V4 is the fourth iteration of prefetch. It succeeds the adaptive
prefetch coordinator ([Adaptive Tile-Based Prefetch](adaptive-prefetch-design.md))
and keeps its prefetch box, flight phases and filter pipeline.

**Companion:** [GPU JPEG decode](gpu-jpeg-decode-design.md). The decoder and the
GPU encoder (#424, #425) are fastest on deep, steady batches of tiles. V4 exists
to supply that stream. Each design is half of the same goal: tiles streamed
through the GPU, end to end, with the CPU left for X-Plane.

## Why

Since #296, prefetch has had its own lane: its own executor, pools,
tile-generation group and HTTP semaphore. Its jobs are admitted only while no
demand job is in flight. The coordinator still meters work into that lane as it
did when the two shared one queue: in cycles, with its own backpressure. That
metering is now a second throttle on top of the lane's resources, and it caps the
stream structurally.

- A cycle runs at most every 2 s, and offers at most the lane channel's 256
  tiles.
- It stops when more than about 56 jobs are waiting, and halves the submission
  above about 35.
- It then waits for the next telemetry-driven cycle.

At today's network-bound 7–10 tiles/s this rarely binds. With a warm chunk cache
and a GPU pipeline able to take 100 tiles/s or more, it does. It also leaves the
pipeline idle between cycles, so the GPU sees short, uneven batches instead of a
stream.

X-Plane asks for 20 or more DSF areas at a scene load, and prefetch works 9–12
areas ahead of the aircraft. There is almost never a lone tile. The job is to
stream as much as possible, as fast as possible, through whatever devices the
machine has.

## Today's controls (V3) and their fate

| # | Control (`prefetch/adaptive/coordinator`) | V4 |
|---|---|---|
| 1 | Cycle cadence: at most every 2 s (`MIN_CYCLE_INTERVAL`), driven by telemetry | **Removed** as a throughput limit |
| 2 | Pending drain: a cycle with held-over tiles submits only those | **Removed**: the backlog replaces `pending_tiles` |
| 3 | Defer: `max(lane.load(), fuse_pressure) > 0.8` holds the whole cycle | **Removed** for `lane.load()` (see 6) |
| 4 | Reduce: above 0.5, only half of the plan is submitted | **Removed** |
| 5 | Lane channel full (256): the remainder goes to pending | **Removed**: the feeder fills to a depth |
| 6 | `fuse_pressure`: memory pinned by open virtual DDS handles (#246) | **Kept**, as a yield condition |
| 7 | Sim state: nothing while X-Plane loads scenery or replays | **Kept**, as a yield condition |
| 8 | Flight phase: nothing in Transition; a 30 s ramp after it (`TransitionThrottle`) | **Kept**, as yield conditions |
| 9 | Region lifecycle: new 1° regions planned whole (GeoIndex `PrefetchedRegion`) | **Kept**, simplified (see Backlog) |
| 10 | Executor admission gate: no prefetch admitted while demand is in flight | **Kept** (the executor's rule) |

Controls 1–5 protected a shared queue that no longer exists. Controls 6–8 and 10
protect X-Plane and memory, and remain the conditions under which prefetch
yields.

## Design

### Backlog

- **Contents.** The coordinator keeps a backlog of the 1° DSF areas in the
  prefetch box that still have uncached tiles.
- **Order.** Areas are ranked by proximity: the distance from the aircraft to
  each area's centre, nearest first. There is no time-to-reach arithmetic. The
  box is already heading-biased in cruise (80/20, `PrefetchBox`), so plain
  proximity inside it stays forward-weighted.
- **Whole areas.** Work is normalised per area. An area's tiles go into the
  stream together, in a stable order, before the next area's. Tiles from several
  areas don't interleave, so no area is fragmented. That also matches how X-Plane
  loads scenery, a DSF at a time.
- **Re-ranking** happens when the nearest area changes, not on every telemetry
  update.
- **Filtering.** The existing pipeline (memory cache, patches, installed
  packages, DDS disk) runs when an area enters the backlog. `CachedTileProbeTask`
  re-checks each tile at admission, as today.
- **Region states.** The GeoIndex states become simpler: an area is queued, then
  done. #226's rule still holds: `NoCoverage` only when the scenery index gives
  the area zero tiles, and `Deferred` stays retryable.

### Feeder

The feeder replaces the cycle as the thing that submits tiles.

- **Driven by completions.** When a prefetch job finishes, the feeder tops the
  lane up again to a target in-flight depth, the next tiles from the
  highest-ranked area. Telemetry updates the box and the ranking. They no longer
  pace submission.
- **The target depth** keeps the lane's pools busy and, on a GPU, keeps the
  batches full that [GPU JPEG decode](gpu-jpeg-decode-design.md) and the encoder
  need: 16 tiles or more at once was where decode reached its throughput. It
  starts at the lane's tile-generation group and is tuned by calibration (#475).
- **Device-aware pacing,** through the scheduler (#475 phase 4):
  - **On a GPU: throughput first.** The feeder keeps the GPU's queue full.
  - **On ISPC: X-Plane's frame rate first.** Prefetch on the CPU is paced by
    `prefetch.capacity_share` (the prefetch lane's share of the pools). XEL
    already reads X-Plane's frame period through the Web API (`SimState`), so a
    closed loop that slows CPU prefetch when frame time rises can follow, if
    flights show the need.
  - **Demand is unaffected:** its own lane, as fast as possible, with impact on
    frame time accepted.
- **Yield conditions.** The feeder submits nothing while any of these holds:
  - `fuse_pressure` is high (#246);
  - X-Plane is loading scenery or replaying;
  - the flight phase is Transition, or within its ramp, which limits the depth;
  - the executor's admission gate is closed by demand.

  These are pauses, not throughput controls.

### Stale work

- **Areas leaving the box** are dropped from the backlog.
- **Admitted jobs for areas far behind the aircraft** are cancelled through
  their jobs' cancellation tokens. A cancelled tile that was already encoding
  is still cached (#385).
- **Dropping work that hasn't been admitted** costs nothing.

### Observability

- **The `Prefetch sample` line:** backlog areas and tiles, the nearest area's
  distance, the feed rate, the in-flight depth against its target, and the
  yield reason with its share of the window.
- **The dashboard:** `PrefetchState` keeps its states (Disabled, Paused by
  demand, Holding for takeoff, Ramping, Active, Idle). Its "Deferred (load)",
  which named the removed backpressure deferral, becomes "Yielding", with the
  reason (memory pressure or scenery loading).

## How we judge it

- **First:** X-Plane's frame time during a scene load and in steady flight, prefetch on
  against off, on ISPC and on the GPU.
- **Then:**
  - FUSE demand latency (p50, p95);
  - prefetch throughput with a warm chunk cache;
  - areas ready before the aircraft reaches them.
- **The no-harm contract** of the adaptive design still applies: a demand tile
  is never slower because prefetch exists.

## Open questions

- The exact target depth for each device, from calibration (#477), and whether
  the GPU's two tiles in flight (#425) wants more.
- Whether the takeoff ramp can relax once prefetch runs on the GPU, off the
  CPU. Frame time will tell.
- How far behind the aircraft an admitted job must be before it is cancelled.

## References

- [#475](https://github.com/samsoir/xearthlayer/issues/475): the compute-pipeline
  plan (scheduler, calibration, phases).
- [GPU JPEG decode](gpu-jpeg-decode-design.md): what V4's stream feeds.
- [Adaptive Tile-Based Prefetch](adaptive-prefetch-design.md): V3, which V4
  succeeds (prefetch box, phases, filters, the no-harm contract).
- [GPU encoding](gpu-encoding-design.md): the encoder and its lanes (#421–#425).
- #296: two executor lanes. #172: the forward-starvation lessons (no caps, no
  silent drops). #226: region retirement. #246: `fuse_pressure`.
