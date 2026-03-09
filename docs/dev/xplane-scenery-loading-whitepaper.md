# X-Plane 12 Scenery Loading Behavior

**A Technical White Paper Based on Empirical Research**

**Authors**: XEarthLayer Project
**Date**: January 2026
**Version**: 1.1

---

## Abstract

This document presents empirical findings on how X-Plane 12 loads orthophoto scenery during flight. Through systematic flight testing with instrumented logging, we captured over 4 million scenery tile requests across five test flights totaling 10.5+ hours of flight time. The research reveals predictable patterns in X-Plane's scenery loading behavior that are not documented in the X-Plane SDK or developer resources.

These findings are valuable to developers building scenery streaming systems, caching solutions, or performance optimization tools for X-Plane.

---

## Table of Contents

1. [Introduction](#introduction)
2. [Methodology](#methodology)
3. [Key Findings](#key-findings)
4. [Detailed Analysis](#detailed-analysis)
5. [Conclusions](#conclusions)
6. [Appendix: Raw Data](#appendix-raw-data)

---

## Introduction

### Background

X-Plane 12 uses a tile-based scenery system organized around DSF (Distribution Scenery Format) tiles, where each DSF tile covers 1° latitude × 1° longitude. Within each DSF tile, orthophoto textures are stored as DDS files at various zoom levels (typically ZL12-ZL18).

The X-Plane SDK currently provides no documentation on:
- When the simulator decides to load scenery ahead of the aircraft
- How much scenery is loaded in each loading event
- The spatial pattern of tile loading (individual tiles vs. bands)
- How loading behavior changes with aircraft speed or heading

Understanding these behaviors is essential for:
- Scenery streaming systems (loading tiles on-demand from remote servers)
- Prefetch/caching systems (pre-loading tiles before X-Plane requests them)
- Performance optimization (reducing scenery loading stutters)

### Research Goals

1. Determine the **trigger position** within a DSF tile that initiates scenery loading
2. Measure the **lead distance** (how far ahead X-Plane loads)
3. Identify **loading patterns** (individual tiles vs. complete bands)
4. Analyze behavior during **diagonal flight** (NE/SE/SW/NW headings)
5. Test whether **aircraft speed** affects loading timing
6. Measure **turn adaptation** timing after heading changes
7. Observe loading behavior **over oceans** vs. land

---

## Methodology

### Test Environment

| Component | Specification |
|-----------|---------------|
| Simulator | X-Plane 12.4 |
| System | Linux, AMD Ryzen 9 9950X3D, 94GB RAM, NVMe storage |
| Network | 5 Gbps symmetrical fiber |
| Instrumentation | XEarthLayer with debug logging enabled |

### Instrumented Logging

All scenery requests from X-Plane were captured via a FUSE virtual filesystem that logged:
- Timestamp of each DDS texture request
- Tile coordinates (row, col, zoom level)
- DSF tile identifier
- Cache hit/miss status
- Request latency

Aircraft position was logged every 20 seconds via UDP telemetry:
- Latitude, longitude, altitude
- Heading, ground speed
- DSF tile containing aircraft

### Test Flights

| Flight | Route | Heading | Duration | DDS Requests | Purpose |
|--------|-------|---------|----------|--------------|---------|
| 1 | EDDH → EDDF | ~198° (S) | 2:00:00 | 562,612 | Longitudinal band loading |
| 2 | EDDH → EKCH | ~55° (NE) | 0:35:00 | ~400,000 | Diagonal loading pattern |
| 3 | EDDH → LFMN | ~171° (S) | 2:21:37 | 1,224,600 | Turn adaptation, long-haul |
| 4 | KJFK → EGLL | ~65° (NE) | 2:26:56 | 813,206 | High-speed cruise, ocean |
| 5 | LFLL diagonal orbit | 317°/47°/139° | 2:27:00 | 1,156,911 | Heading changes, DSF row/column loading |

**Total**: 10.5+ hours of flight time, 4+ million DDS requests analyzed.

---

## Key Findings

### Summary Table

| Behavior | Finding | Confidence |
|----------|---------|------------|
| **Trigger Position** | ~0.6° into current DSF tile (heading direction) | High |
| **Lead Distance** | 1-2° ahead (primary), up to 3° (fringe) | High |
| **Loading Unit** | DSF-aligned strips: rows (heading N/S) or columns (heading E/W) | High |
| **Strip Depth** | 3 DSF tiles deep along the heading axis (primary load) | High |
| **Strip Width** | 4-6 DSF tiles perpendicular to travel | High |
| **Diagonal Loading** | Separate row and column jobs fired by independent boundary monitors | High |
| **Direction Priority** | Longitude (E/W) boundary crossing fires column load slightly before latitude fires row load | Medium |
| **Speed Independence** | Trigger is position-based only — speed, heading, and track are not used | High |
| **Turn Behavior** | No heading detection; new loads fire 20-40s after turn as position reaches new boundaries | High |
| **Post-Turn Radius Fill** | Tiles missed during turn loaded over 10-20 minutes | High |
| **Ocean Behavior** | Same request rate, but 97% cache hits | High |
| **Burst Size** | 400-1,200 cache misses per loading event (20,000-35,000 total FUSE requests including cache hits) | High |

---

## Detailed Analysis

### 1. Trigger Position

X-Plane initiates scenery loading when the aircraft reaches approximately **0.6° into the current DSF tile** in the direction of travel.

**Evidence (Flight 1)**:
```
Time     DSF Tile    Entry Position    Loading Triggered
──────────────────────────────────────────────────────────
32:33    +52+010     (0.96°, 0.41°)    35 seconds later (at 0.89°)
78:33    +51+010     (0.97°, 0.10°)    2 seconds later (at 0.60°)
81:13    +51+009     (0.60°, 0.99°)    Immediate (already at threshold)
```

**Interpretation**: The trigger is position-based, not time-based. If the aircraft enters a DSF tile already past the 0.6° threshold, loading begins immediately. If entry is at the tile edge (0.0°), loading waits until 0.6° is reached.

### 2. Lead Distance

X-Plane loads scenery **1-2° ahead** of the aircraft as a primary load, with fringe loading extending up to 3°. Earlier observations suggesting a consistent 2-3° lead appear to have included secondary/fringe loading in the measurement.

**Evidence (Flight 1)**:
```
Aircraft Position: 52.7°N, heading south
Loading observed:
  - Latitude 50° to 52° (2.7° ahead)
  - Longitude 8° to 11° (4° wide band)
```

**Evidence (Flight 3)**:
```
Aircraft Position: 48.5°N, heading south toward Nice
Loading observed:
  - Latitude 45° to 48° (3.5° ahead)
```

**Evidence (Flight 5 — Refined measurement)**:
```
Burst at 20:06 — Aircraft at +46+004, heading NW (317°):
  Primary load: +47 row (1° ahead) — 869 tiles (86% of burst)
  Secondary:    +48 row (2° ahead) — 51 tiles (5%)
  Fringe:       +45 row (1° behind) — 8 tiles (1%)

Burst at 22:08 — Aircraft at +47+015, heading SE (140°):
  Primary load: +46 row (1° ahead) — 1,129 tiles (83% of burst)
  Secondary:    +45 row (2° ahead) — 59 tiles (4%)
```

**Revised interpretation**: The primary loading event targets the DSF row/column **1° ahead** of the aircraft. Secondary loading at 2° ahead and minor fringe loading at 1° behind account for the remaining 15-20%. The earlier 2-3° measurement was likely capturing the full extent including fringe, not the primary target.

### 3. DSF Strip Loading Pattern

X-Plane loads **DSF-aligned strips** of tiles — complete rows or columns of DSF tiles, not individual tiles or arbitrary shapes.

**Key distinction**: The loading unit is a **DSF row** (when heading predominantly N/S) or a **DSF column** (when heading predominantly E/W). The strips are 3 DSF tiles deep along the heading axis and 4-6 DSF tiles wide perpendicular to it.

**Observed Pattern (Southbound Flight)**:
```
┌─────────────────────────────────────────────┐
│  Loading burst contains:                    │
│  - 3 complete latitude rows (50°, 51°, 52°) │
│  - Each row spans 4° longitude (8° to 11°)  │
│  - Total: ~7,000-11,000 tiles per burst     │
└─────────────────────────────────────────────┘
```

**Evidence (Flight 5 — DSF strip grids)**:

Heading NW (317°) — X-Plane loads a **latitude row** strip:
```
Burst at 20:06 — Aircraft at +46, lon 4.6
 LAT\LON      3      4      5      6
 ────────────────────────────────────
   +48       16     12     11     12      fringe
   +47      184    228    216    241  ◄── PRIMARY ROW (4 columns wide)
   +46       46     24      3      ·  ◄── aircraft row
   +45        6      2      ·      ·      fringe
```

Heading NE (47°) — X-Plane loads a **latitude row** with column spillover:
```
Burst at 20:42-44 — Aircraft at +49, lon 3.6
 LAT\LON      1      2      3      4      5      6
 ──────────────────────────────────────────────────
   +50       18    230    309    326    280     18  ◄── PRIMARY ROW
   +49        ·      ·     32     81    275     18  ◄── aircraft row
   +48        ·      ·      1      6    217     17      column spill (lon 5)
   +47        ·      ·      ·      ·    244     18      column spill (lon 5)
```

Heading SE (140°) — X-Plane loads a **latitude row** strip:
```
Burst at 22:08-10 — Aircraft at +47, lon 15.6
 LAT\LON     12     13     14     15     16     17
 ──────────────────────────────────────────────────
   +48        ·      ·      ·      ·      4     55      fringe behind
   +47        ·      ·      1     11     73     22  ◄── aircraft row
   +46       17    181    248    231    220    232  ◄── PRIMARY ROW (6 cols wide)
   +45        1     12     12     11     11     12      fringe
```

This confirms X-Plane's scenery system organizes loading by DSF tile rows/columns rather than proximity-based algorithms. The 3-deep primary strip is consistent across all headings.

### 4. Diagonal Flight Loading

When flying diagonal headings, X-Plane does **not** load "diagonal bands." It fires **separate row and column loading jobs** triggered independently by approaching latitude and longitude boundaries respectively. On a diagonal track, both types of boundary crossing occur frequently, so the loads may overlap in time — but they are distinct operations.

**Evidence (Flight 2 - EDDH to EKCH, heading 55° NE)**:
```
Entry to +54+011:
  - EAST band (+12 longitude) loaded after 17.0 seconds  ← LON boundary crossing
  - NORTH band (+55 latitude) loaded after 46.0 seconds   ← LAT boundary crossing

The 29-second gap confirms these are separate jobs, not one diagonal load.
```

**Evidence (Flight 5 - Heading 47° NE, two crossings 3 minutes apart)**:

At 20:42-44, the aircraft crossed both a LON boundary (lon 004→005) and a LAT boundary (lat +48→+49). The two loads arrived in distinct minutes:

```
20:42 — COLUMN load at lon 005 (LON boundary crossing):
              3      4      5      6
   +49       10      1    275     18
   +48        ·      ·    217     17
   +47        ·      ·    244     18     ← column spans 3 lat rows

20:43 — ROW load at lat +50 (LAT boundary crossing):
              1      2      3      4
   +50       18    230     36    326     ← row spans 4 lon columns
   +49        ·      ·     12     31
   +48        ·      ·      1      3
```

The column load arrived first (20:42), the row load one minute later (20:43). These are clearly two separate loading jobs triggered by different boundary crossings.

**Model**: X-Plane runs two independent boundary monitors based purely on **aircraft position relative to the currently loaded area** — not heading, ground speed, or any other flight vector:

1. **Latitude monitor**: When the aircraft position approaches a latitude boundary of the loaded area → fire **row load** (tiles along the longitude axis)
2. **Longitude monitor**: When the aircraft position approaches a longitude boundary of the loaded area → fire **column load** (tiles along the latitude axis)

X-Plane's loading algorithm is deliberately naive — it does not use heading, ground speed, or track to anticipate future scenery needs. It simply checks whether the aircraft's current position is close enough to the edge of the loaded area to trigger the next load. This means:
- On a diagonal heading, both monitors fire within a short time window as the aircraft approaches both boundaries
- On a cardinal heading (due N/S or due E/W), only one monitor fires frequently
- X-Plane does not pre-compute or reorient the loaded area based on flight direction

This simplicity is actually beneficial for prefetch systems like XEL: since X-Plane's loading is purely reactive and position-based, a prefetch system that *does* use heading and speed to anticipate boundary crossings can stay reliably ahead of X-Plane's requests.

### 5. Speed Independence

Aircraft ground speed does **NOT** affect the trigger position. Whether flying at 150 knots or 560 knots, X-Plane triggers loading at the same ~0.6° position within the DSF tile.

**Evidence (Flight 4 - 550+ kt cruise)**:
```
DSF Tile    Entry Position    Ground Speed
+42-071     0.00°, 0.03°     556 kt
+43-069     0.01°, 0.09°     555 kt
+44-065     0.18°, 0.01°     554 kt
+45-062     0.31°, 0.96°     555 kt
+46-058     0.23°, 0.01°     560 kt

Trigger position remained consistent at ~0.6° despite high speed.
```

**Interpretation**: The trigger is purely position-based. X-Plane does not use heading, ground speed, or track in its scenery loading algorithm — only the aircraft's current position relative to the edge of the loaded area. At higher speeds, the aircraft simply reaches the trigger position faster, giving less time for background loading systems to prepare. This naive approach means a prefetch system that uses heading and speed to anticipate boundary crossings can reliably stay ahead of X-Plane's reactive loading.

### 6. Turn Adaptation

X-Plane does not detect or respond to heading changes directly. After a turn, the aircraft's new track moves it toward different boundaries of the loaded area, which naturally triggers different row/column loads within **20-40 seconds**. The apparent "adaptation" is simply the position-based boundary monitors firing for the new boundaries. **Radius fill** of tiles missed during the turn continues for **10-20 minutes**.

**Evidence (Flight 3 - Turn during approach to Nice)**:
```
Time        Heading    Event
20:52:03    149.7°     Pre-turn heading
20:52:23    104.6°     Turn initiated (45° change)
20:52:43    88.8°      Turn complete
~20:53:10   -          Loading pattern shifted to east bands
```

**Evidence (Flight 5 - Post-turn radius fill)**:

Three ~90° turns were observed. After each, loading bursts contained a significant fraction of tiles **behind** the new heading — tiles that were *lateral* to the old heading and had never been loaded:

```
Burst        Hdg   Time since turn   Behind%   Explanation
──────────────────────────────────────────────────────────
20:06        317°  ~6 min             1%       Old-direction tiles still cached
20:17-19     317°  ~17 min           23%       Lateral tiles from old heading being filled
20:30-31      46°  ~2 min             2%       Too soon after turn for gap fill
20:42-44      47°  ~14 min           24%       Lateral tiles from NW leg now behind NE heading
21:35-38     139°  ~13 min           16%       Lateral tiles from NE leg now behind SE heading
22:08-10     140°  ~46 min            4%       Radius fully caught up
```

**Interpretation**: X-Plane maintains a scenery radius in all directions. After a heading change:
1. **Forward adaptation** (20-40 seconds): The loading direction shifts to match the new heading
2. **Radius fill** (10-20 minutes): Tiles that were outside the loaded area during the turn are gradually filled in — these appear as "behind" tiles relative to the new heading but were actually *lateral* to the old heading
3. **Steady state** (>30 minutes): Behind-loading drops to ~4%, representing only minor fringe coverage at the edge of the scenery radius

### 7. Ocean Behavior

Over featureless ocean, X-Plane maintains the **same request rate** but achieves very high cache hit rates.

**Evidence (Flight 4 - Atlantic crossing)**:
```
Phase              Request Rate    Cache Hit Rate
JFK departure      3,883/min       ~70%
Mid-Atlantic       3,808/min       97%
Newfoundland       1,262/min       ~85%
```

**Interpretation**: X-Plane continues requesting tiles over ocean (likely water textures), but the tiles are simpler and highly cached, data is reduced due to the water masks. This creates an opportunity for prefetch systems to prepare upcoming coastal tiles.

### 8. Scenery Window Dimensions (Empirically Measured)

**Methodology**: X-Plane 12's in-sim map displays the loaded scenery area as a grid. By identifying reference airfields at the edges of the displayed loaded area, we measured the actual geographic extent of the scenery window at three airports spanning different latitudes.

**NOTE**: The grid squares visible on X-Plane's map are NOT 1°×1° DSF tiles. They are approximately 0.5° each (e.g., 8 squares × 0.5° ≈ 4° longitude). This was a significant correction from earlier assumptions.

#### Measurements

| Airport | Lat | Window Lat | Window Lon | AC Position |
|---------|-----|-----------|-----------|-------------|
| EDDF (Frankfurt) | 50.0°N | ~3.0° (49.0–52.0) | ~4.1° (6.9–11.0) | Offset west (westbound ops) |
| YPAD (Adelaide) | 34.9°S | ~3.0° (33.0–36.0) | ~3.9° (137.1–141.0) | Offset SW (SW runway ops) |
| WSSS (Singapore) | 1.35°N | ~2.94° (0.02–2.96) | ~3.96° (102.0–105.9) | Near center |

**Reference airfields used**:

*EDDF*: EDVW (51.97°N, near N edge), EDRO (49.03°N, on S edge), EDPH (49.27°N/11.01°E, on E edge), EDLE (51.40°N/6.94°E, near W edge)

*YPAD*: YWHA (-33.06°, near N edge), YVOB (-35.96°, on S edge), YPNN (-35.25°/140.94°, near E edge), YTKL (~-35.90°/~137.27°, near W edge)

*WSSS*: WIDL (2.96°N/105.76°E, near N/E corner), island coastline (0.02°N, S edge), WIBS (1.37°N/102.14°E, near W edge)

#### Key Findings

1. **Latitude span is constant at ~3°** regardless of position on the globe.

2. **Longitude span scales with latitude** to maintain roughly constant physical distance:
   - At equator (1°N): ~4.0° lon × cos(1°) ≈ 4.0° physical
   - At 35°S: ~3.9° lon × cos(35°) ≈ 3.2° physical
   - At 50°N: ~4.1° lon × cos(50°) ≈ 2.6° physical
   - Formula: lon_span ≈ 3° / cos(latitude), targeting a roughly square physical area (~330km × 330km)

3. **Window positioning is directional** — biased toward the active runway/departure heading:
   - EDDF (westbound ops): 1.7° to west edge, 2.4° to east edge
   - YPAD (SW ops): 1.05° to south, 1.43° to west (biased south-west)
   - WSSS (near center): ~2.0° in all directions

4. **Boundary buffer is ~1–1.5°** from the nearest edge, not 3–4° as previously assumed. At jet cruise speeds (~450kt), this gives approximately 8–12 minutes of prefetch time before a boundary crossing.

#### Correction from FUSE-based Analysis

Our earlier FUSE data analysis (sections below) suggested a larger window (6 DSF tiles deep, 4-6 wide). The FUSE analysis measured **request activity** (tiles X-Plane reads from disk), which includes fringe reads and re-reads beyond the visible loaded area. The X-Plane map measurement captures the **actually loaded and rendered** scenery area, which is the relevant metric for prefetch targeting.

The FUSE burst analysis remains valid for understanding **loading event** patterns (trigger positions, burst sizes, row vs column loads), but the overall window dimensions should use the map-based measurements.

### 8a. FUSE-Based Loading Event Depth (Supplementary)

FUSE cache miss burst analysis (LFLL diagonal orbit flight, 65 classified loading events with telemetry) reveals **asymmetric loading depth** between latitude and longitude crossings:

**ROW loads (latitude boundary crossings, 51 events):**
- **Depth: 3 rows** (73%), 2 rows (25%), 1 row (2%)
- **Width: 3-4 columns** (avg 3.5)
- Total: ~9-12 DSF regions per ROW event

**COLUMN loads (longitude boundary crossings, 14 events):**
- **Depth: 2 columns** (64%), 1 column (29%), 3 columns (7%)
- **Width: 3-4 rows** (avg 3.4)
- Total: ~6-8 DSF regions per COL event

**Trigger distance**: Approximately **1°** from the window edge (based on telemetry-correlated burst analysis at 45-52°N). ROW triggers averaged 0.87° from the DSF boundary, COL triggers averaged 0.97°, consistent with a ~1° degree-based trigger model.

**The depth asymmetry** likely reflects the window shape: the window is ~3° tall (latitude) but ~4-5° wide (longitude at these latitudes). X-Plane loads proportionally more rows (3 of 3 = full window depth) than columns (2 of 4-5 = partial window width) per crossing event.

**Implication for prefetch**: ROW crossings (latitude) are the larger events (~9-12 regions) and should be prioritised. Use axis-specific load depths: 3 rows for latitude crossings, 2 columns for longitude crossings. The ~1° trigger distance means the aircraft is always close to an edge — prefetch must complete quickly.

### 9. Initial Load Area

When a flight begins, X-Plane loads the full scenery window (~3° lat × ~4° lon at mid-latitudes) around the aircraft position. The window may be directionally biased based on the active runway orientation.

**Evidence (Map-based, March 2026)**:
```
EDDF (50°N): 49.0–52.0°N, 6.9–11.0°E (~3° × 4.1°, biased west)
YPAD (35°S): 33.0–36.0°S, 137.1–141.0°E (~3° × 3.9°, biased SW)
WSSS (1°N):  0.02–2.96°N, 102.0–105.9°E (~3° × 4°, centered)
```

**Earlier FUSE-based observations** suggested a larger initial load (~12° × 12°), but this likely captured the sum of multiple progressive loading events during simulator startup rather than a single loading area. The map-based measurement provides the accurate window extent.

This initial load takes **30 seconds to 5 minutes** depending on cache state, connection speed, and the number of scenery tiles in the region.

---

## Conclusions

### X-Plane 12 Scenery Loading Model

Based on our empirical research, X-Plane 12's scenery loading follows this model:

```
X-PLANE SCENERY LOADING ALGORITHM (Inferred)
┌───────────────────────────────────────────────────────────────┐
│ 1. INITIAL LOAD                                               │
│    - Load scenery window around starting position             │
│    - Window: ~3° latitude × ~3°/cos(lat) longitude            │
│    - Directionally biased toward active runway heading        │
│    - Complete before flight begins                            │
│                                                               │
│ 2. SCENERY WINDOW (maintained area)                           │
│    - ~3° latitude (constant worldwide)                        │
│    - ~3°/cos(lat) longitude (constant physical distance)      │
│    - ~330km × 330km physical area                             │
│    - Slides as aircraft advances; directionally biased        │
│    - Aircraft typically 1–1.5° from nearest edge              │
│                                                               │
│ 3. BOUNDARY-TRIGGERED LOADING (position-only, no vectors)     │
│    - Two independent boundary monitors (latitude + longitude) │
│    - Based ONLY on aircraft position vs. loaded area edge     │
│    - Does NOT use heading, ground speed, or track prediction  │
│    - Latitude monitor: aircraft near lat edge → ROW load      │
│      → 3 rows deep × 3-4 cols wide (73% of observed events)  │
│    - Longitude monitor: aircraft near lon edge → COLUMN load  │
│      → 2 cols deep × 3-4 rows wide (64% of observed events)  │
│    - Trigger distance: ~1° from window edge                   │
│                                                               │
│ 4. DIAGONAL HANDLING                                          │
│    - Both monitors fire independently on diagonal headings    │
│    - Produces overlapping row + column loads within minutes   │
│    - Not a single diagonal load — two separate jobs           │
│                                                               │
│ 5. TURN BEHAVIOR                                              │
│    - No explicit turn detection — purely a position effect    │
│    - New track approaches different boundaries → new loads    │
│    - Apparent "adaptation" takes 20-40 seconds (time to reach │
│      trigger threshold on new boundary)                       │
│    - Radius fill of lateral gaps continues 10-20 minutes      │
└───────────────────────────────────────────────────────────────┘
```

### Implications for Developers

1. **Prefetch Systems**: Predict which DSF boundary (latitude or longitude) the aircraft will cross next, and pre-load the corresponding row or column. With the aircraft typically only 1–1.5° from the nearest edge, prefetch must trigger early and complete quickly. Trigger at 0.3–0.5° into the DSF tile to complete before X-Plane's 0.6° threshold.

2. **Caching Systems**: Cache complete DSF rows or columns, not individual tiles. X-Plane will request entire rows on latitude crossings and entire columns on longitude crossings.

3. **Performance Optimization**: The small scenery window (~3° × 4°) means the aircraft is always close to an edge. Prefetch systems have approximately 8–12 minutes at jet cruise speed before a boundary crossing — less in diagonal flight where both axes may trigger in quick succession.

4. **Diagonal Flight**: Monitor both latitude and longitude boundaries independently. On diagonal headings, both crossings occur frequently and may overlap — prefetch must handle concurrent row and column loads.

5. **Speed Considerations**: Faster aircraft reduce the time window but don't change the trigger position.

6. **Post-Turn Handling**: After heading changes, expect 10-20 minutes of lateral gap filling as X-Plane populates tiles that were outside the loaded area during the turn.

7. **Latitude-Dependent Longitude Width**: The scenery window longitude span increases at higher latitudes (4.1° at 50°N vs 4.0° at equator). Prefetch systems should compute column width dynamically using the aircraft's latitude.

---

## Appendix: Raw Data

### Flight Test Logs

| Flight | Log File | Size |
|--------|----------|------|
| 1 | `xearthlayer-eddh-eddf.log` | 433 MB |
| 2 | `xearthlayer-eddh-ekch.log` | 307 MB |
| 3 | `xearthlayer-eddh-lfmn.log` | 1.4 GB |
| 4 | `xearthlayer-kjfk-egll-not-completed.log` | 861 MB |
| 5 | `xearthlayer-lfll-diagonal-orbit.log` | 3.1 GB |

### Analysis Tools

Log analysis performed using `scripts/analyze_flight_logs.py` which extracts:
- Position updates from APT telemetry
- DDS request timestamps and coordinates
- Burst detection (loading events)
- Cache hit/miss statistics

### References

- X-Plane 12 SDK Documentation (limited scenery system documentation)
- DSF Specification: https://developer.x-plane.com/article/dsf-specification/
- XEarthLayer Project: https://github.com/[project-url]

---

**Document Version History**

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | January 2026 | Initial release based on 4 test flights |
| 1.1 | March 2026 | Added Flight 5 (LFLL diagonal orbit). Major model revision: X-Plane uses position-only boundary detection (no heading/speed/track). Separate row/column jobs triggered by independent lat/lon boundary monitors. Updated lead distance (1-2° primary, not 2-3°). Added 3-deep strip pattern, post-turn radius fill, scenery window depth (2 behind + 3 ahead). Turn "adaptation" reframed as position effect. Updated burst size to distinguish cache misses from total FUSE requests. |
| 1.2 | March 2026 | **Major window dimension correction.** Empirical measurement via X-Plane in-sim map at three airports (EDDF 50°N, YPAD 35°S, WSSS 1°N) revealed scenery window is ~3° lat × ~3°/cos(lat) lon (~330km × 330km), NOT 9×9° or 6×8°. Map grid squares are ~0.5°, not 1° DSF tiles. Window directionally biased toward departure heading. Corrected initial load area from 12×12° to actual window dimensions. Updated loading event depth from "3 deep" to "1-2 deep". Previous FUSE-based analysis retained as supplementary data. |
