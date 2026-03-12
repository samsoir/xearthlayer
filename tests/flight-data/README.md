# Flight Test Data

Compressed debug-level flight logs used for analysing X-Plane's scenery loading
behaviour. These logs informed the findings in
[`docs/dev/xplane-scenery-loading-whitepaper.md`](../../docs/dev/xplane-scenery-loading-whitepaper.md).

## Flights

| # | File | Route | Duration | FUSE Requests | Notes |
|---|------|-------|----------|---------------|-------|
| 1 | `flight1-eddh-eddf.7z` | EDDH → EDDF | ~1:00 | — | Initial baseline |
| 2 | `flight2-eddh-ekch.7z` | EDDH → EKCH | ~0:45 | — | Short over-water segment |
| 3 | `flight3-eddh-lfmn.7z` | EDDH → LFMN | ~2:00 | — | Long overland, multiple DSF crossings |
| 4 | `flight4-kjfk-egll.7z` | KJFK → EGLL | ~2:00 | — | Transatlantic (sparse scenery) |
| 5 | `flight5-lfll-diagonal-orbit.7z.*` | LFLL diagonal orbit | 2:27 | 1,156,911 | Heading changes, DSF row/column loading (split archive) |

## Format

- Compressed with 7z (LZMA2)
- Decompress: `7z x <file>.7z` (for split archives: `7z x <file>.7z.001`)
- Log format: tracing structured output (debug level)

## Usage

These logs are reference data for scenery loading research. Key analysis
patterns:

```bash
# Count FUSE DDS read requests
grep "fuse_read.*\.dds" <logfile> | wc -l

# Extract cache miss bursts
grep "cache_miss" <logfile> | awk '{print $1}' | cut -d: -f1-2 | uniq -c | sort -rn

# Identify DSF boundary crossings
grep "dsf_crossing\|boundary" <logfile>
```
