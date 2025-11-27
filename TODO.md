# XEarthLayer Bug/Snag List

Tracking issues discovered during development and testing.

## Performance Issues

(None currently open)

## Visual Glitches

(None currently open)

## Tech Debt

### TD1: Migrate to Async FUSE Model
- **Status**: Open
- **Priority**: Medium
- **Description**: Current implementation uses blocking thread pool for parallel tile generation.
  The FUSE library (`fuser`) supports an async model that could provide better scalability.
- **Current Implementation**: `ParallelTileGenerator` wraps tile generation with:
  - Blocking thread pool (std::thread)
  - mpsc channels for work distribution
  - Request coalescing via HashMap
- **Future Implementation**:
  - Use `fuser`'s async filesystem trait
  - Integrate with tokio runtime
  - Replace blocking channels with async equivalents
- **Benefits**:
  - Better thread utilization under high load
  - More efficient I/O waiting
  - Native async/await integration
- **Risks**:
  - Significant refactoring required
  - Need to verify compatibility with current architecture
- **Added**: 2025-11-25

## Future Enhancements

### E2: Tile Pre-fetching
- Predict which tiles will be needed based on aircraft position/heading
- Generate tiles before X-Plane requests them

### E3: Progressive Loading
- Return low-resolution placeholder immediately
- Upgrade to full resolution in background

### E4: Persistent Cache Warming
- On startup, verify cached tiles are still valid
- Pre-warm cache for known flight areas

## Completed

### P1: Slow Initial Load ✓
- **Completed**: 2025-11-25
- **Original Issue**: Initial scene load took 4-5 minutes at KOAK
- **Resolution**: Implemented parallel tile generation with request coalescing
- **Result**: Load time reduced to ~30 seconds (with cache hits)

### E1: Parallel Tile Generation ✓
- **Completed**: 2025-11-25
- **Implementation**: `ParallelTileGenerator` in `xearthlayer/src/tile/parallel.rs`
- **Features**:
  - Configurable thread pool (default: num_cpus)
  - Request coalescing for duplicate tile requests
  - Timeout handling with magenta placeholder fallback
  - Configuration via `[generation]` section in config.ini

### Graceful Shutdown ✓
- **Completed**: 2025-11-25
- **Implementation**: Signal handling with automatic FUSE unmount
- **Features**:
  - Ctrl+C and SIGTERM trigger clean shutdown
  - BackgroundSession auto-unmounts on drop
  - No stale mounts left behind

---

### V1: Scenery Glitches ✓
- **Closed**: 2025-11-26
- **Original Issue**: Visual glitches observed during first test flight
- **Resolution**: Not an XEarthLayer issue - caused by Ortho4XP scenery pack configuration for X-Plane 12. Improved with better Ortho4XP settings.

## Notes

- Test environment: San Francisco Bay Area (Ortho4XP scenery pack)
- X-Plane 12 on Linux
- First successful end-to-end test: 2025-11-25
