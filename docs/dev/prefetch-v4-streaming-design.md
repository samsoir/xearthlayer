# Prefetch V4: Streaming Prefetch (speclet)

**Status:** proposed (2026-11-26), targeting 0.5.2. Part of the
compute-pipeline plan, [#475](https://github.com/samsoir/xearthlayer/issues/475).
Not implemented; tracked in [#488](https://github.com/samsoir/xearthlayer/issues/488).

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
| 11 | Stale telemetry: nothing submitted after 5 s without telemetry (`TELEMETRY_STALE_THRESHOLD`, `coordinator/constants.rs`; `runner.rs`) | **Changed**: the ranking freezes and the feeder keeps going; a long dropout yields (see Telemetry dropouts) |

Controls 1–5 protected a shared queue that no longer exists. Controls 6–8 and 10
protect X-Plane and memory, and remain the conditions under which prefetch
yields. Control 11 protected the ranking, not X-Plane, so V4 keeps the ranking
safe without stopping the stream.

Control 2 binds in flight. On a contributor's EDDM to LBSF flight, held-over
tiles stayed pending for 25 minutes (8,859 tiles down to 0). While any were
pending, every cycle submitted only those.

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
  - the executor's admission gate is closed by demand;
  - telemetry has been stale for longer than the dropout limit (see Telemetry
    dropouts).

  These are pauses, not throughput controls. The line's health (see Failed
  tiles) lowers the depth rather than stopping the feeder, and provider pushback
  pauses only the requests to that provider.

### Telemetry dropouts

The V3 coordinator stops submitting after 5 s without telemetry. On the EDDM to
LBSF flight, telemetry dropped out 22 times, for 33.5 s at the median. That
pause costs prefetch time, although the backlog it would feed was ranked from a
position only seconds old.

V4 separates the ranking from the stream:

- **Short dropout: freeze the ranking.** Once telemetry is stale
  (`TELEMETRY_STALE_THRESHOLD`, 5 s), the feeder stops changing the backlog.
  It adds no areas, drops none and re-ranks nothing, and cancels no stale work.
  It keeps feeding the areas already queued, in the order they had at the last
  known position.
- **Long dropout: yield.** After a dropout limit (60 s, to be tuned from
  flights), the feeder stops submitting until telemetry returns. A minute
  without a position at cruise speed is several kilometres, and a frozen ranking
  is no longer a good guess.
- **When telemetry returns,** the coordinator updates the box and re-ranks once,
  as for a change of nearest area, then resumes.
- **The other yields don't change.** If X-Plane stopped answering because it is
  loading scenery, the sim-state yield already pauses the feeder.

### Failed tiles and network health

A prefetch job whose chunks fail or time out still completes. Its tile is served
to no one and isn't cached (#180, #388).

The EDDM to LBSF flight's `--debug` log (12:25 to 13:25Z) shows that "a failed
chunk" covers three different things, and V4 treats each on its own:

| Failure | What it means | On that flight |
|---|---|---|
| **Timeout** (and other transport errors) | The line is slow or losing packets. A retry nearly always works | 13,865 timeouts: 13,488 first attempts, and all but 57 of those chunks downloaded in the end |
| **HTTP 503** (and 429) | The provider is pushing back | 478 chunks lost, nearly all in 13:10 to 13:13Z, right after a prefetch batch of 1,006 tiles, while the Apple token churned (22 refreshes in two minutes) |
| **HTTP 404** | The provider has no imagery there (#301). Never retried | 2,530 chunks on 76 tiles. Some are sea tiles in `zzXEL_eu-west_ortho`, built from GO2 imagery; others are on land in Turkey and at ZL18 around an airport. Re-requested 4,442 times in the hour |

The packages are built from one provider's imagery, and the user may stream from
another. So 404s are permanent in places, and they are no sign of network health.

V4's rules:

- **The feeder refills on every completion,** whatever the outcome. A failed job
  frees its place in the lane like any other.
- **A tile incomplete through timeouts or 503s goes back to the end of its area's
  backlog,** with an attempt count. An area isn't done until every tile is cached
  or has run out of attempts. An area with tiles out of attempts becomes
  `Deferred` under #226's rules (retryable, never `NoCoverage` for indexed tiles),
  and its tiles are tried again when it next ranks.
- **A tile incomplete only through 404s is dropped for the session.** Asking again
  gets the same answer. It doesn't count against its area, which can be done
  without it. Whether such a tile should be cached, with its missing chunks
  filled, is a question for the cache rules (#180, #388), not for V4.
- **The line's health sets the depth.** The signal is the slow tail of chunk
  latency (a high percentile over a recent window) against chunks downloaded per
  second, from the download orchestrator. It is not a failure rate:
  - Timeouts on that flight tracked how much was asked of a slow line. From
    12:47Z its throughput fell from 100,000–200,000 chunks a minute to
    20,000–45,000, even in quiet minutes. Before then, minutes of heavy demand
    (12:46, 12:51) caused 3 and 28 timeouts; after it, the same kind of minute
    caused 500 to 1,300.
  - When the slow tail grows while throughput doesn't, the feeder lowers the
    target depth. It restores the depth step by step as the tail shrinks. More
    requests in flight on a saturated line only lengthen every request's wait.
  - The contributor's cold loads ran at 3 tiles/s one evening and 13 to 14 the
    next, on the same line. The depth has to follow what the line delivers, not
    a fixed number.
- **Provider pushback backs off the provider.** A burst of 503s or 429s from a
  provider pauses the feeder's new requests to it for a short, growing interval,
  whatever the line's health. A token refresh that coincides is the provider's
  business (its own refresh lock), not the feeder's.

The same latency tail serves demand: #491 proposes that X-Plane's chunks are
retried, or sent a second time, while their tile can still make its deadline.

### Stale work

- **Areas leaving the box** are dropped from the backlog.
- **Admitted jobs for areas far behind the aircraft** are cancelled through
  their jobs' cancellation tokens. A cancelled tile that was already encoding
  is still cached (#385).
- **Dropping work that hasn't been admitted** costs nothing.

### Observability

- **The `Prefetch sample` line:**
  - backlog areas and tiles, and the nearest area's distance;
  - the feed rate, and the in-flight depth against its target;
  - the yield reason, with its share of the window;
  - telemetry dropouts, with their frozen and yielding time;
  - tiles returned to the backlog, tiles dropped for 404s, and areas deferred;
  - the chunk latency tail and chunks per second behind any lowered depth, and
    provider back-offs with their cause (503 or 429).
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
- The telemetry dropout limit (60 s proposed), and whether X-Plane's Web API
  drops out mainly during scene loads, when the sim-state yield already applies.
- How many attempts a tile gets before its area is deferred.
- The latency percentile and window for the line's health, and how far it
  lowers the depth. The EDDM to LBSF log shows the shape on a lossy 1 Gbit cable
  line; a rule that also holds on 5 Gbit fibre needs flights on both.
- Whether XEL's own connections can congest a home line (up to 1,024 per lane on
  that flight, before `network.connections`), which the depth control would then
  have to catch as well. Asked on PR #480.
- The back-off interval for provider pushback, and whether 404s seen for one
  provider should be remembered across sessions.

## References

- [#475](https://github.com/samsoir/xearthlayer/issues/475): the compute-pipeline
  plan (scheduler, calibration, phases).
- [GPU JPEG decode](gpu-jpeg-decode-design.md): what V4's stream feeds.
- [Adaptive Tile-Based Prefetch](adaptive-prefetch-design.md): V3, which V4
  succeeds (prefetch box, phases, filters, the no-harm contract).
- [GPU encoding](gpu-encoding-design.md): the encoder and its lanes (#421–#425).
- #491: X-Plane's chunks retried within their tile's deadline, from the same
  latency tail.
- #296: two executor lanes. #172: the forward-starvation lessons (no caps, no
  silent drops). #226: region retirement. #246: `fuse_pressure`.
