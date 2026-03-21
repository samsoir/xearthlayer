//! Live debug map for prefetch observability.
//!
//! Feature-gated behind `debug-map`. Serves a browser-based Leaflet.js map
//! showing aircraft position, sliding prefetch box bounds, DSF region states
//! from GeoIndex, and pipeline metrics. Polls via JSON API at 2-second intervals.
//!
//! # Usage
//!
//! Build with `--features debug-map`, then open `http://localhost:8087` in a browser.
