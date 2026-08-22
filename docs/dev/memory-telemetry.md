# Memory Telemetry

**Status**: Implemented
**Added**: v0.4.7
**Issue**: [#209](https://github.com/samsoir/xearthlayer/issues/209)

## Purpose

XEarthLayer emits one structured memory sample per minute into the normal log
file for the entire life of the process. The sample is always on, needs no
flags, no environment variables, and no profiler, and it costs one read of
`/proc/self/smaps` plus one `tracing::info!` every 60 seconds.

It exists because of issue #209. A 12-hour flight ended with the kernel
OOM-killing the process at 64 GB of anonymous memory — 9.3 GB resident plus
54.7 GB swapped — against a configured 4 GB memory cache. There was no
application error and no panic: the last line in the log is the instant of the
kill. The profiler the project already had — heaptrack — writes its output on
exit, so a `SIGKILL` leaves nothing behind. The trace is deliberately coarse,
but it is already on disk when the kernel fires.

Three candidate causes for #209 are still live, and the sample line is designed
so that a single unattended flight discriminates between them:

1. **Unbounded fire-and-forget cache-write backlog.** `BuildAndCacheDdsTask`
   issues two ungated `tokio::spawn` calls per completed tile — one memory cache
   write, one DDS disk cache write — each holding its own clone of the encoded
   tile (~11.2 MB for BC1 with a full mipmap chain). The disk write reaches
   storage through `tokio::fs`, which queues on the tokio blocking pool. Neither
   spawn is gated by the executor's `max_concurrent_jobs` or by any
   `ResourcePool` permit, so if the blocking pool saturates the queue and its
   buffers grow without limit. The two spawns are independently observable as
   `disk_writes_active` and `mem_cache_writes_active` respectively — a backlog
   in either one is this candidate.
2. **glibc arena retention.** An 11.2 MB buffer sits just below glibc's adaptive
   `mmap` threshold ceiling of 32 MB. Once the threshold has adapted upward past
   the buffer size, those allocations are served from per-thread arenas rather
   than by `mmap`, and freeing them returns the memory to the arena free list,
   not to the OS.
3. **Chunk LRU index growth.** The chunk disk cache index held roughly
   12 million entries during the OOM flight.

## The sample line

```
2026-08-06 06:20:33Z  INFO Memory sample uptime_s=60 rss_mb=873 vm_mb=1459 swap_mb=0 threads=71 tiles_done=0 encodes_active=0 chunks_ok=0 chunks_failed=0 mem_cache_mb=0 dds_disk_mb=158268 chunk_disk_mb=55301 gc_evicted_mb=0 chunk_index_entries=3803083 disk_writes_active=0 mem_cache_writes_active=0
```

(Captured before #233. The example above shows 16 fields; a current line
carries **27** — the six `fuse_*` read fields, the four `dds_handles_*` /
`dds_pinned_*` fields, and `dds_budget_exhausted`, all described below. If you
are editing this paragraph, count the fields in `log_memory_sample` rather than
adjusting the number by hand: it has gone stale twice that way.)

Fields are listed below in emission order. Every `_mb` value is truncating integer
division by 1 MiB (1 048 576 bytes), so a value of `0` means "under one
mebibyte", not necessarily "nothing".

| Field | Source | Meaning |
|-------|--------|---------|
| `uptime_s` | `AggregatedState::uptime()` | Whole seconds since the metrics daemon started, which is effectively service start. Use it as the x-axis. |
| `rss_mb` | `MemoryProbe` → sum of `Rss:` in `/proc/self/smaps` | Physical pages currently held. **Excludes anything the kernel has swapped out.** |
| `vm_mb` | `MemoryProbe` → sum of `Size:` in `/proc/self/smaps` | Total mapped address space. Counts anonymous mappings whether resident or paged out. |
| `swap_mb` | `MemoryProbe` → `/proc/self/status` `VmSwap:` | Anonymous memory swapped out to disk. **This is the field that actually caught #209**: at the moment of the kill, `rss_mb` alone read a healthy 9.3 GB while `swap_mb` would have read 54.7 GB. `0` means **not readable on this platform**, not "nothing swapped" — same convention as `threads`. Linux-only; read in the same `/proc/self/status` pass as `threads`, so it costs no extra file open. |
| `threads` | `/proc/self/status` `Threads:` | OS thread count. `0` means **not readable on this platform**, not "no threads" — see below. |
| `tiles_done` | `state.encodes_completed` | Cumulative DDS encodes completed. The load counter: divide by `uptime_s` for tiles/second. |
| `encodes_active` | `state.encodes_active` | Encodes in flight right now. Each one holds a 4096×4096 RGBA source image (64 MiB) plus its output buffer. |
| `chunks_ok` | `state.chunks_downloaded` | Cumulative successful chunk downloads. |
| `chunks_failed` | `state.chunks_failed` | Cumulative failed chunk downloads. A rising count means tiles are being served but not persisted (see issue #180). |
| `mem_cache_mb` | `state.memory_cache_size_bytes` | Current moka memory cache size. Compare against `cache.memory_size`. |
| `dds_disk_mb` | `state.dds_disk_cache_size_bytes` | Current DDS disk tier size, from that tier's LRU index. |
| `chunk_disk_mb` | `state.chunk_disk_cache_size_bytes` | Current chunk disk tier size, from that tier's LRU index. |
| `gc_evicted_mb` | `state.disk_bytes_evicted` | Cumulative bytes freed by the disk GC daemons, **summed across both disk tiers**. |
| `chunk_index_entries` | `state.chunk_index_entries` | Live entries in the **chunk** tier's `LruIndex`. The DDS tier does not report this. Refreshed on every chunk-tier set/delete and once at startup (`DiskCacheProvider::report_size_to_metrics`), **but the GC batch task (`tasks/cache_gc_batch.rs`) removes entries from the index directly and does not call it**, so this gauge can lag behind the true index size during heavy GC — a burst of evictions may not be reflected until the next unrelated set/delete nudges a report. See the bytes-per-entry estimate below for translating a raw count into an approximate memory footprint. |
| `disk_writes_active` | `state.disk_writes_active` | Fire-and-forget disk cache writes currently in flight, counting both chunk writes from `DownloadChunksTask` and DDS tile writes from `BuildAndCacheDdsTask`. |
| `mem_cache_writes_active` | `state.mem_cache_writes_active` | Fire-and-forget **memory**-cache writes currently in flight — the `cache.put()` spawn in `BuildAndCacheDdsTask`, mirroring `disk_writes_active` but for the moka tier. Added specifically because this spawn previously emitted no start/complete pair at all (only `mem_cache_mb`, which only moves after the write completes), which meant a backlog concentrated there was invisible and would misread as candidate 2 (allocator retention). See the decision table below. |
| `fuse_file_reads` | `state.fuse_file_reads` | FUSE `read()` calls answered from a **real file on disk** — DSF, `.ter`, patch DDS. Not tile requests: the kernel caps every FUSE read at `max_pages * PAGE_SIZE` (1 MiB on Linux), so one X-Plane whole-file read arrives as many calls. |
| `fuse_file_read_mb` | `state.fuse_file_read_bytes` | Bytes those calls returned to the kernel — what X-Plane actually consumed. |
| `fuse_file_alloc_mb` | `state.fuse_file_alloc_bytes` | Bytes the handler read from disk to produce them. **Should track `fuse_file_read_mb` almost exactly**; a growing gap means the handler is moving more than it delivers. Before #233 the handler read the whole file per call, so this ran up to 238x ahead on the largest ortho DSF. |
| `fuse_dds_reads` | `state.fuse_dds_reads` | FUSE `read()` calls answered from a **generated DDS tile**. Because virtual DDS files are opened `FOPEN_DIRECT_IO` (#65) the kernel serves nothing from cache, so this is a complete census of X-Plane's texture reads, not a sample — which makes it the tiles-*served* denominator #227 otherwise lacks. |
| `fuse_dds_read_mb` | `state.fuse_dds_read_bytes` | Bytes those calls returned to the kernel. |
| `fuse_dds_alloc_mb` | `state.fuse_dds_alloc_bytes` | Bytes of whole tiles materialised to produce them. Since #234 the tile is charged to the read that produced it and nothing to the reads that slice it, so this should now track `fuse_dds_read_mb` closely. It ran 12-23x ahead before #234, when every ranged call re-entered the executor and cloned the whole tile. |
| `dds_handles_open` | `state.fuse_handles_open` | Virtual DDS files X-Plane currently has open (gauge). Each one may pin a whole tile so later reads can slice it (#234). Nothing measured this before, so `MAX_PINNED_TILE_BYTES` is currently a guess — this is the number that should replace it. |
| `dds_pinned_mb` | `state.fuse_handles_pinned_bytes` | Tile bytes pinned by those handles (gauge). Bounded by `MAX_PINNED_TILE_BYTES` (512 MiB); on reaching it, `open()` stops memoising and reads fall back to resolving per call. |
| `dds_handles_peak` | `state.fuse_handles_peak_open` | Highest concurrent open count this session. **Read this, not `dds_handles_open`, when sizing the cap.** The current gauges are sampled on open, on tile production and on release, so a 60-second reader almost always catches them just after a release: the first KDEN run reported `dds_pinned_mb=0` throughout while 31 files were open. |
| `dds_pinned_peak_mb` | `state.fuse_handles_peak_pinned_bytes` | Highest pinned total this session. Compare against the `dds_pinned_cap_mb` logged at mount: if it approaches the cap, `open()` is close to dropping memoisation. Two scene loads (KDEN, KSLC) peaked at 10-21 MiB against 512, roughly 24x headroom. |
| `dds_budget_exhausted` | `state.fuse_handle_budget_exhausted` | Opens refused a memoising handle because the pinned-tile budget was full (counter, #236). **Any non-zero value means the #234 fix stopped applying for those opens** and their reads resolved the tile once per call. Zero while `fuse_dds_alloc_mb` climbs means a different fault — the two are otherwise indistinguishable in a log. |

### Counters are cumulative; gauges are current-state

Every counter on this line — `tiles_done`, `chunks_ok`, `chunks_failed`,
`gc_evicted_mb`, `fuse_*_reads`, and the `Prefetch sample` counters
(`regions_deferred`, `deferrals_cleared`, `promotions_*`) — is **cumulative
since process start**. Nothing windows them: there is no per-interval reset, so
a raw value always climbs and a single line cannot tell you what happened in the
last minute. **Difference successive samples to get a rate**, or divide by
`uptime_s` for a session average.

Gauges — `encodes_active`, `disk_writes_active`, `mem_cache_writes_active`,
`dds_handles_open`, `dds_pinned_mb`, `chunk_index_entries`, the
`regions_*` state counts, and everything from `MemoryProbe` — are current-state
and are read directly. Peak fields (`dds_handles_peak`, `dds_pinned_peak_mb`)
are high-water marks over the session, not current values.

This distinction matters when judging an acceptance criterion. "`regions_deferred`
must not climb without bound" is about the *slope* across samples; the raw number
climbing is not a fault. (`AggregatedState::reset()` used to exist and implied
otherwise; it had no production caller and was removed in #229.)

### Sizing the DDS handle budget

`dds_handles_open` and `dds_pinned_mb` measure different things and diverge by
more than an order of magnitude. Handles are counted at `open()`; bytes are
counted when a tile is **materialised**, which happens on first read. X-Plane
reads each texture in about two calls and releases promptly, so most open
handles hold nothing: 31 concurrent opens corresponded to roughly **two**
resident tiles.

**Size the cap against `dds_pinned_peak_mb`, never against `dds_handles_peak`.**
Multiplying the open count by the tile size overstates the requirement by
around 16x — that error is what put the original "1.5x headroom" estimate in
#236, when the real figure was about 24x.

The cap is soft: admission tests `pinned + expected` without reserving, so
concurrent opens can pass together and overshoot. With the measured headroom
this is immaterial; if `dds_budget_exhausted` ever moves, revisit it.

### Reading the two amplification ratios

`*_alloc_mb / *_read_mb` is the read amplification for each path. 1.0 means the
handler moved exactly what X-Plane consumed; anything above it is waste, and the
allocations concerned are 11-17 MB — the same size class as the DDS buffers in
candidate A of #227, and on the same side of glibc's 32 MB adaptive mmap
threshold. Both ratios are workload-independent: they depend on file size and
the kernel's read chunk, not on flight length or throughput, so a single sample
line is enough to read them.

Since #234 a virtual DDS tile is produced once per *open* rather than once per
ranged read, so `fuse_dds_reads` still counts every read call while
`fuse_dds_alloc_mb` advances only on the read that produced a tile. Dividing
`fuse_dds_alloc_mb` by 11.17 MB therefore gives the number of textures X-Plane
actually opened -- the tiles-*served* figure #227 needs, and a different number
from `fuse_dds_reads`.

### Estimating `chunk_index_entries` memory footprint

`chunk_index_entries` is a raw count, not a byte figure, so it is easy to
misjudge whether 12 million entries means roughly 1 GB or roughly 10 GB. The
index is `DashMap<String, CacheEntryMetadata>` (`cache/lru_index.rs`), and
per entry, on a 64-bit build, roughly:

- `CacheEntryMetadata` value: `size_bytes: u64` (8 bytes) + `last_accessed:
  Instant` (16 bytes) ≈ **24 bytes**.
- `String` key struct (ptr/len/cap): **24 bytes**, plus a separate heap
  allocation for the key text itself. Chunk keys look like
  `"chunk:15:12754:5279:8:12"` (~25 bytes); with allocator rounding, call it
  **~32–48 bytes**.
- `DashMap`/`hashbrown` bucket overhead (control bytes, load-factor slack):
  a few bytes per entry, **negligible** at this scale.

Summing to roughly **80–100 bytes per entry**, 12 million entries is
approximately **1–1.2 GB** — order-of-magnitude closer to 1 GB than 10 GB.
**This is an order-of-magnitude estimate derived from the type definitions,
not a measurement** (no allocator introspection or heap profiler was run to
confirm it); treat it as a sanity check on `chunk_index_entries`, not a
precise accounting figure.

### `threads=0` means unreadable, not zero

Thread count is read from `/proc/self/status`, which only exists on Linux.
`memory-stats` does not expose a thread count, and hand-rolling mach FFI for
macOS was judged not worth the risk on a CI-blocking platform, so
`ProcessMemoryProbe::linux_status_fields()` returns `(None, None)` everywhere
except Linux and the emit line renders `None` as `0` for both `threads` and
`swap_mb`. A macOS trace will show `threads=0` and `swap_mb=0` on every line;
that tells you nothing about the process.

### `vm_mb` on macOS is not comparable to the Linux figure

On macOS, `vm_bytes` comes from mach `task_info`'s virtual size, which
includes the entire shared address space (shared libraries, framework
mappings, and other machinery the Linux `Size:` figure does not count the same
way). It routinely reads in the hundreds of gigabytes for an otherwise
ordinary process and is **not a meaningful growth signal** on that platform —
do not compare a macOS `vm_mb` trend against a Linux one, and do not read a
large absolute macOS `vm_mb` as evidence of anything by itself.

### `rss_mb` and `vm_mb` are not interchangeable — and neither is enough alone

`rss_mb` counts resident pages only. On a machine that is swapping — which is
exactly the machine that is about to be OOM-killed — a growing process can show
a **flat or falling** `rss_mb` while the kernel quietly moves its pages to swap.
In the #209 flight only 9.3 GB of the 64 GB was resident at the moment of the
kill; `rss_mb` alone would have read as healthy right up to the `SIGKILL`.

`vm_mb` counts the whole mapping regardless of residency, so it keeps rising.
The gap between the two is lazily-reserved-but-never-touched address space plus
anything paged out.

`swap_mb` closes this gap directly: it is anonymous memory the kernel has
actually swapped out, read straight from `VmSwap:` in the same
`/proc/self/status` pass as `threads`. **Watch `rss_mb + swap_mb`, not
`rss_mb` alone** — that sum is what actually tracks the process's total
anonymous footprint, and it is what would have shown the #209 flight climbing
toward 64 GB instead of sitting at a reassuring 9.3 GB. A widening
`vm_mb - (rss_mb + swap_mb)` gap on top of that is lazily-reserved address
space that was never touched at all.

### The allocator environment line

`log_allocator_environment()` runs from `CliRunner::log_startup`, whose only
caller is the `run` command (`xearthlayer-cli/src/commands/run.rs`) — it does
**not** run for every subcommand, only when actually starting the service. It
emits a line only when at least one of `MALLOC_ARENA_MAX`,
`MALLOC_MMAP_THRESHOLD_` or `MALLOC_TRIM_THRESHOLD_` is set:

```
Allocator environment overrides active allocator_env=MALLOC_MMAP_THRESHOLD_=1048576 MALLOC_ARENA_MAX=2
```

These variables change whether freed memory goes back to the OS, so a trace
gathered with them set is not comparable to one gathered without. **The absence
of this line is itself information**: it means the run used stock glibc
behaviour. Always check for it before comparing two traces.

## Reading a trace

Plot `rss_mb`, `swap_mb`, `vm_mb`, `disk_writes_active`,
`mem_cache_writes_active` and `chunk_index_entries` against `uptime_s`. Treat
`rss_mb + swap_mb` as the real growth signal (see above), and read its shape
against the other four — **both** active-write gauges, not just the disk one
— to discriminate the three candidates.

| `rss_mb + swap_mb` | `disk_writes_active` | `mem_cache_writes_active` | `chunk_index_entries` | Reading |
|---------------------|-----------------------|-----------------------------|-------------------------|---------|
| climbing | climbing | any | any | **Candidate 1** — the fire-and-forget **DDS-disk** cache-write backlog in `tasks/build_and_cache_dds.rs`. Each queued write pins ~11.2 MB. |
| climbing | any | climbing | any | **Candidate 1** — the fire-and-forget **memory**-cache-write backlog in the same task. Each queued write pins its own ~11.2 MB clone, separately from the disk-write backlog above. |
| climbing | flat | flat | climbing | **Candidate 3** — chunk LRU index growth. Cross-check that `chunk_disk_mb` is at its configured ceiling and `gc_evicted_mb` is rising: an index that grows while the tier size is pinned means entries are accumulating faster than GC removes them. Remember `chunk_index_entries` can lag during heavy GC (see field table), so confirm with `gc_evicted_mb` rather than reading a momentarily-flat count as proof nothing changed. |
| climbing | flat | flat | flat | **Candidate 2** — allocator retention. Nothing in the process is holding more logical data, but the resident footprint grows anyway. Confirm by re-running with `MALLOC_MMAP_THRESHOLD_=1048576`; if growth stops, glibc was hoarding freed buffers in per-thread arenas. **This conclusion requires `mem_cache_writes_active` to be flat, not just `disk_writes_active`.** Before `mem_cache_writes_active` existed, this exact disk-flat/index-flat shape was reachable with a backlog concentrated entirely in the uncounted memory-cache spawn — rss climbing while both older gauges sat flat — and would have been misdiagnosed as candidate 2 when it was actually candidate 1. |

Supporting reads:

- **`disk_writes_active` and `mem_cache_writes_active` are both lower bounds.**
  Each counter is incremented by the first statement *inside* its spawned
  write task, so a write that has been spawned but not yet polled is
  invisible to either gauge. If the tokio worker threads are themselves
  starved, the real backlog is larger than the numbers printed — for both
  tiers.
- **`encodes_active` × 64 MiB is the floor of what encoding costs.** Each
  in-flight encode holds a 4096×4096 RGBA source image (64 MiB) plus its output
  buffer. The number should track the CPU resource pool capacity; if it runs
  materially above that, CPU admission control is not bounding the pipeline and
  the peak is unbounded with it.
- **`threads`** climbing toward 512 points at the tokio blocking pool (default
  512 threads) filling up, which is one of the mechanisms behind candidate 1
  (the DDS-disk write path queues onto it via `tokio::fs`).
- **`tiles_done` per hour** is the load normaliser. Two traces at wildly
  different tile rates are not telling you about memory, they are telling you
  about workload.
- **`chunks_failed`** rising alongside `tiles_done` means tiles are being served
  to X-Plane but never persisted, so the same tiles will be regenerated
  repeatedly. That inflates apparent load without inflating cache size.

## Comparing two runs — read this first

**Matching load between two runs is not sufficient to make them comparable.**

The worked example is the 2026-08-05 retest of issue #209. It was run with
`MALLOC_MMAP_THRESHOLD_=1048576 MALLOC_ARENA_MAX=2` and held at 5.3 GB, against
an OOM kill at 64 GB. It also matched the OOM flight to within 1% on tiles
generated per hour. It looked like a clean confirmation of candidate 2. It was
not:

| | OOM flight | 2026-08-05 retest |
|---|---|---|
| Peak anonymous memory | 64 GB (killed) | 5.3 GB |
| Tiles generated per hour | baseline | within 1% of baseline |
| Allocator overrides | none | `MALLOC_MMAP_THRESHOLD_=1048576`, `MALLOC_ARENA_MAX=2` |
| Disk tier occupancy (DDS / chunks) | 89% / 85% | empty throughout |
| GC evictions over the run | ~24 million files | zero |
| Chunk index entries | ~12 million | small and growing from zero |

Two major variables moved between the runs, not one. The retest changed the
allocator *and* removed all cache pressure. A run that never evicts anything
never exercises the GC path, never grows the LRU index to steady state, and
never sustains the disk write queue depth that a full cache produces. The
experiment therefore discriminated nothing: the 5.3 GB ceiling is equally
consistent with "the allocator fix worked" and with "an empty cache produces a
fundamentally different workload".

Before comparing two traces, check that all of the following are in the same
regime:

1. **`dds_disk_mb` and `chunk_disk_mb`** — are both runs at their configured
   ceilings, or is one starting from empty? A cold cache is a different program.
2. **`gc_evicted_mb`** — is GC actually running in both? Zero evictions means
   the eviction path was never exercised.
3. **`chunk_index_entries`** — is the index at steady state in both, or growing
   from zero in one?
4. **The allocator environment line** — present in both, absent in both, or
   present in only one?
5. **`tiles_done / uptime_s`** — comparable tile rates.

Only then is a difference in `rss_mb + swap_mb` attributable to the change
under test. Change one variable per flight. To test candidate 2 properly, the
allocator override must be flown against a cache that is already full and
evicting, **and** with `mem_cache_writes_active` confirmed flat throughout —
otherwise a memory-cache-write backlog (candidate 1) is still on the table and
the retest proves nothing about the allocator.

## Submitting a trace

The trace is plain text in the normal log file, `~/.xearthlayer/xearthlayer.log`
by default (configurable via `logging.file`; see
[Configuration](../configuration.md)). Attach the whole file to the GitHub
issue — the memory samples are hard to interpret without the surrounding startup
lines: the version banner, the resolved cache sizes, and the allocator
environment line if one was emitted.

> **The log file is truncated on every start.** `init_logging_full` clears the
> file before opening it, so relaunching XEarthLayer destroys the previous
> flight's trace. Copy it aside *before* the next launch:
>
> ```bash
> cp ~/.xearthlayer/xearthlayer.log ~/flight-$(date +%Y%m%d-%H%M).log
> ```

To extract just the samples for plotting:

```bash
grep "Memory sample" ~/.xearthlayer/xearthlayer.log
```

A 12-hour flight produces 720 sample lines.

## Architecture

```
metrics/memory_probe.rs                    metrics/daemon.rs
───────────────────────                    ─────────────────
MemoryProbe (trait)          injected      MetricsDaemon
  fn sample() -> Option<     ───────────▶    with_memory_probe(rx, probe)
      MemorySample>                          run() → select! { … }
                                               MEMORY_SAMPLE_INTERVAL tick
ProcessMemoryProbe                             → log_memory_sample()
  memory-stats + /proc                              │
StaticMemoryProbe (test)                            ▼
                                             tracing::info!("Memory sample", …)
```

### `MemoryProbe`

```rust
pub trait MemoryProbe: Send + Sync {
    fn sample(&self) -> Option<MemorySample>;
}
```

`MemorySample` carries `rss_bytes`, `vm_bytes`, `threads: Option<u64>` and
`swap_bytes: Option<u64>`. Returning `Option` rather than a `Result` is
deliberate: a platform that cannot supply a reading is not an error condition,
it just means no sample line (or, for the two `Option` fields, no value for
just that field).

`ProcessMemoryProbe` is the production implementation. It delegates to the
`memory-stats` crate for the byte counts (Linux, macOS and Windows; on Unix its
only dependency is `libc`, already in the tree) and reads thread count and
swap bytes itself from `/proc/self/status` on Linux only, via
`ProcessMemoryProbe::linux_status_fields()` — a single read of the file that
extracts both the `Threads:` and `VmSwap:` lines in one pass, so adding
`swap_bytes` did not add a second file open per sample.

### The `memory-stats` initialisation workaround

`ProcessMemoryProbe::sample()` funnels the first call through a
`std::sync::Once`:

```rust
static MEMORY_STATS_INIT: std::sync::Once = std::sync::Once::new();
```

This works around a race in memory-stats 1.2.0. Its Linux path guards
initialisation with an atomic compare-exchange on `SMAPS_CHECKED`. A thread that
loses that CAS can read the still-default `SMAPS_EXIST` (`false`) before the
winner has stored the real value, fall through to the `/proc/self/statm`
fallback, and multiply the page counts by a `PAGE_SIZE` that is still `0` —
producing a silent, non-erroring reading of `physical_mem = 0` and
`virtual_mem = 0`. Serialising the first call ensures upstream initialisation
has completed before any concurrent use. Without it, the first sample of a run
can be a plausible-looking `rss_mb=0 vm_mb=0`.

As defense in depth for exactly this failure mode, `log_memory_sample` in
`metrics/daemon.rs` also treats a returned sample with `rss_bytes == 0` as
equivalent to `None` (same warn-once path, no line emitted), rather than
trusting the `Once` alone. A `rss_mb=0` line in a trace would otherwise read
as "process using no memory" instead of "reading failed" — the same class of
silently-misleading zero this `Once` exists to prevent.

### Injection and sampling cadence

`MetricsDaemon::new` wires in `ProcessMemoryProbe`; `with_memory_probe` takes an
`Arc<dyn MemoryProbe>` so tests can substitute `StaticMemoryProbe` and assert on
the emitted event without reading real process memory. The daemon owns a second
`tokio::time::interval` alongside the 100 ms time-series sampler:

```rust
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
```

Sixty seconds is fixed and **deliberately not configurable**. Traces are pooled
across users and machines, and a configurable cadence would be one more variable
to reconcile before two traces could be compared — precisely the failure mode
described above. Both intervals use `MissedTickBehavior::Skip` so a stalled
daemon does not emit a burst of catch-up samples.

If the probe returns `None`, **or returns `Some` with `rss_bytes == 0`**, the
daemon logs one warning (`"Memory probe unavailable; memory samples
disabled"`), latches `memory_probe_failed`, and emits no memory sample for
that tick — the `Prefetch sample` line sharing the same 60s interval still
fires unconditionally (`log_prefetch_sample` has no dependency on the memory
probe); see `docs/dev/adaptive-prefetch-design.md` for its field format. The
warning is not repeated, but the probe is retried every subsequent tick — a
later tick returning a valid reading resumes normal sampling without
re-latching anything.

Because `MetricsSystem::new` is constructed unconditionally in
`XEarthLayerService::start`, memory sampling is active for every run, including
TUI mode.

### Adding a platform implementation

To add thread count, swap, or any other field for a new platform:

1. Add a `#[cfg(target_os = "…")]` branch to
   `ProcessMemoryProbe::linux_status_fields` (or a differently-named
   platform-specific reader, if the new platform's data doesn't come from a
   single `/proc`-style file the way Linux's does). The existing
   `#[cfg(not(target_os = "linux"))]` fallback returns `(None, None)`, so
   nothing breaks if you add a platform and forget a branch — the fields just
   render as `0`.
2. Extend `MemorySample` if the platform can supply something the others cannot.
   Any new field must be `Option`-typed with an emit-time default, so a trace
   from one platform stays diffable against a trace from another.
3. Add the field to the `tracing::info!` call in `log_memory_sample` **at the
   end** of the field list, once the format has shipped in a release. Existing
   field positions are what makes traces from different builds comparable by
   eye. (`swap_mb` and `mem_cache_writes_active` were inserted mid-line rather
   than appended — grouped next to `vm_mb` and `disk_writes_active`
   respectively, for readability — because this happened before v0.4.7 first
   shipped, so no released trace format existed yet to stay compatible with.
   Once a format has shipped, prefer appending.)
4. Update the field table in this document and the assertion list in
   `memory_sample_line_carries_every_field_from_its_correct_source`, which seeds
   every field to a distinct value specifically so a swapped or dropped source
   cannot pass.

## Relationship to heaptrack

The two tools answer different questions and neither replaces the other.

[Memory Profiling](memory-profiling.md) with heaptrack gives allocation-level
attribution: which call site allocated what, ranked by contribution to peak
heap, with full backtraces. That is what you want when you already know a
workload reproduces the growth and you need to know *which line of code* is
responsible. But heaptrack writes its summary on exit — a crash or `kill -9`
loses data — so it cannot capture an OOM kill, which is by definition a
`SIGKILL`. It also adds a 2-3× slowdown on allocation-heavy code, which makes it
impractical for a 12-hour flight.

Memory telemetry gives no attribution at all. It tells you the footprint grew,
roughly how fast, and which of a small number of subsystem gauges moved with it.
Its advantages are that it costs nothing, runs unattended for the whole flight,
and is durably on disk one minute at a time, so it survives the kill that
heaptrack cannot.

| | Memory telemetry | heaptrack |
|---|---|---|
| Granularity | Process-level gauges | Per-call-site allocations |
| Overhead | Negligible | 2-3× on allocation-heavy paths |
| Survives `kill -9` | Yes | No |
| Practical duration | Unbounded | Minutes |
| Usable by other users | Yes — always on, just attach the log | No — requires a profiler build and a local run |

Use the trace for long unattended flights and for data reported by other users.
Use heaptrack for local attribution once the trace has narrowed the search to a
reproducible workload.

## References

- [Memory Profiling](memory-profiling.md) — heaptrack guide
- [Cache Service Design](cache-service-design.md) — `CacheLayer`, GC daemons, the tiers behind `mem_cache_mb` / `dds_disk_mb` / `chunk_disk_mb`
- [Job Executor Design](job-executor-design.md) — resource pools and `max_concurrent_jobs`, which the fire-and-forget cache writes bypass
- [Configuration](../configuration.md) — `logging.file`, `cache.memory_size`, `cache.disk_size`
- Issue [#209](https://github.com/samsoir/xearthlayer/issues/209) — the OOM kill this telemetry was added to diagnose
- Issue [#180](https://github.com/samsoir/xearthlayer/issues/180) — why `chunks_failed` suppresses cache writes
