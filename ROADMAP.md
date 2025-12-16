# XEarthLayer Roadmap

This document outlines the strategic direction and planned features for XEarthLayer.

## Vision

XEarthLayer aims to be a high-performance, reliable virtual filesystem for serving orthophoto scenery to X-Plane. Built in Rust for stability and efficiency, it provides a robust alternative to Python-based solutions.

## Current State (v0.2.x)

- Async pipeline architecture with multi-threaded FUSE
- Request coalescing and HTTP concurrency limiting
- Two-tier caching (memory + disk)
- TUI dashboard for real-time monitoring
- Linux distribution packages (.deb, .rpm, AUR)
- Multiple imagery providers (Bing, Google, USGS, ArcGIS, Apple Maps, MapBox)

## Planned Features

### Near Term

#### ~~Additional Tile Sources~~ ✓ Completed
- ~~**USGS** - High-quality imagery for United States~~
- ~~**ArcGIS** - World imagery service~~
- ~~**Apple Maps** - High Quality closed source mapping provider~~
- ~~**MapBox** - Commercial mapping service built on top of openstreetmap~~

#### Linux GTK GUI
- User interface for XEarthLayer for linux desktop environments built using the gtk5 framework

#### Performance Improvements
- Tile pre-fetching based on aircraft position/heading (E2)
- Progressive loading with low-res placeholders (E3)
- Persistent cache warming on startup (E4)

#### Diagnostics
- Diagnostic magenta chunks with encoded error info (E5)
- Enhanced error reporting and metrics export
- Optional end-user crash reporting
- End-user bug reporting

#### Tech Debt
- Refactor CLI validation to Specification Pattern (move business rules from CLI to library modules)

### Medium Term

#### Platform Support
- macOS support (Apple Silicon)
- linux arm64 support

#### User Experience
- MacOS GUI 
- Scenery pack management interface
- Flight route cache pre-warming

### Long Term

#### Advanced Features
- Multiple simultaneous tile sources with fallback
- Tile source quality/freshness comparison
- Integration with flight planning tools

## Non-Goals

The following are explicitly out of scope:

- **Just-in-time scenery compilation** - CPU overhead is prohibitive (~5 min/tile). XEarthLayer serves pre-built scenery.
- **Scenery building** - Use Ortho4XP or similar tools. XEarthLayer is a serving layer, not a build tool.
- **Post-processing/color correction** - This belongs in the build pipeline, not runtime serving.

## Contributing

If you'd like to contribute to a roadmap item, please open an issue to discuss the approach first.

---

*Last updated: 2025-12-16*
