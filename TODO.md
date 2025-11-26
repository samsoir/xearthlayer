# XEarthLayer Bug/Snag List

Tracking issues discovered during development and testing.

## Performance Issues

### P1: Slow Initial Load (4-5 minutes to load KOAK)
- **Status**: Open
- **Severity**: High
- **Description**: Initial scene load takes 4-5 minutes at Oakland International Airport (KOAK)
- **Observed**: 2025-11-25, first X-Plane integration test
- **Likely Causes**:
  - Sequential tile generation (one at a time)
  - No pre-fetching of nearby tiles
  - Each tile requires ~2 seconds (download + encode)
- **Potential Fixes**:
  - Parallel tile generation for multiple requests
  - Background pre-fetching based on camera position
  - Progressive loading (low-res first, upgrade later)
  - Persistent disk cache to avoid regeneration on restart

## Visual Glitches

### V1: Scenery Glitches (TBD)
- **Status**: Open, needs investigation
- **Severity**: Unknown
- **Description**: Visual glitches observed during first test flight
- **Observed**: 2025-11-25, first X-Plane integration test
- **Details**: To be documented after further investigation
- **Potential Causes**:
  - Tile coordinate misalignment
  - Mipmap generation issues
  - DDS format compatibility
  - Tile boundary seams

## Future Enhancements

### E1: Parallel Tile Generation
- Allow multiple tiles to be generated concurrently
- Currently FUSE operations are serialized

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

(Move items here when fixed)

---

## Notes

- Test environment: San Francisco Bay Area (Ortho4XP scenery pack)
- X-Plane 12 on Linux
- First successful end-to-end test: 2025-11-25
