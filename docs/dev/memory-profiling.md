# Memory Profiling with Heaptrack

This guide covers how to profile XEarthLayer's memory usage using [heaptrack](https://github.com/KDE/heaptrack), a Linux heap memory profiler that tracks allocations, identifies peak consumers, and detects leaks.

## Installation

```bash
# Debian/Ubuntu
sudo apt install heaptrack heaptrack-gui

# Fedora
sudo dnf install heaptrack

# Arch Linux
sudo pacman -S heaptrack

# From source (any Linux)
git clone https://github.com/KDE/heaptrack.git
cd heaptrack && mkdir build && cd build
cmake .. && make -j$(nproc) && sudo make install
```

## Running a Profile

Build a release binary first (debug builds are too slow for realistic workloads):

```bash
cargo build --release
```

Run XEarthLayer under heaptrack:

```bash
heaptrack ./target/release/xearthlayer run
```

Stop cleanly with `Ctrl+C` when done. Heaptrack writes its summary on exit — a crash or `kill -9` loses data.

The output file is named `heaptrack.xearthlayer.<pid>.zst` in the current directory.

**Important:** Run heaptrack directly against the binary, not via `cargo run`. Using `cargo run` profiles the cargo/rustup process instead of xearthlayer.

## Analyzing Results

### CLI Analysis

```bash
heaptrack_print heaptrack.xearthlayer.<pid>.zst
```

Key sections in the output:

- **PEAK MEMORY CONSUMERS** — call sites ranked by contribution to peak heap. This is the most useful section for optimization work.
- **Summary** (at the end) — peak heap, peak RSS, total leaked, runtime, allocation rate.

### GUI Analysis

```bash
heaptrack_gui heaptrack.xearthlayer.<pid>.zst
```

The GUI provides:
- Allocation flamegraphs
- Timeline of heap growth
- Per-call-site allocation counts and sizes
- Leak detection with full backtraces

## What to Look For

### Peak Memory Consumers

The `PEAK MEMORY CONSUMERS` section shows which code paths are alive at the moment of peak heap usage. Example output:

```
PEAK MEMORY CONSUMERS
2.47G peak memory consumed over 20028 calls from
moka::future::base_cache::BaseCache::do_insert_with_hash...
```

This tells you the moka memory cache contributed 2.47 GB to the peak heap. Compare against your configured `cache.memory_size` to verify the cache is behaving as expected.

### Interpreting Results

- **Peak heap** — maximum live heap allocations at any point. This is what you're optimizing.
- **Peak RSS** — includes heap + stack + mmap + heaptrack overhead. Always higher than peak heap.
- **Temporary allocations** — allocations freed within the same call frame. High counts indicate churn but don't affect peak memory.
- **Leaked memory** — allocations never freed. Small amounts (< 10 MB) are normal from static initialization and thread-local storage.

### A/B Comparisons

To compare before/after an optimization:

1. Run heaptrack on the baseline binary
2. Apply the optimization, rebuild
3. Run heaptrack on the optimized binary with the same workload
4. Compare the `PEAK MEMORY CONSUMERS` section — the optimized call site should disappear or shrink

Ensure both runs use the same:
- Config file (especially `cache.memory_size`)
- Approximate workload (similar flight duration, scenery area)
- Build flags (`--features`, release mode)

## Configuration Impact

The `cache.memory_size` setting directly controls the largest memory consumer. When profiling encoding pipeline optimizations, use a small cache (e.g., 2 GB) so pipeline allocations are proportionally visible. With a 28 GB cache, pipeline improvements are dwarfed by the cache footprint.

## Overhead

Heaptrack adds ~2-3x slowdown on allocation-heavy code. Tile generation will be slower than normal, but the allocation patterns and peak consumers are representative of production behavior.

## Tips

- **Reproducible workloads:** Use `xearthlayer run --airport ICAO` for pre-warm, then fly a consistent route
- **Short sessions:** 3-5 minutes of active tile generation is sufficient for peak analysis
- **Backend selection:** Set `texture.compressor = gpu` in config when testing the GPU path; use `ispc` (default) for ISPC-only profiling
- **Debug logging:** Add `RUST_LOG=info,xearthlayer=debug` to see encode pipeline tracing alongside the profile
