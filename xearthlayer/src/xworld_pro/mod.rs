//! SimHeaven X-World Pro: what is installed and enabled, read from X-Plane's
//! `scenery_packs.ini` and the Custom Scenery folders (copied or symlinked
//! alike). XEarthLayer only reads; it never writes `scenery_packs.ini`,
//! X-World Pro's files or X-Plane's `Resources`.

mod coverage;
mod detect;
mod findings;
mod packs;

pub use coverage::{continent_of_region, overlay_plan, OverlayCoverage, OverlayPlan};
pub use detect::{detect, Continent, Detection, XwpLayer, XwpLibrary, LAYER_PREFIX, LIBRARY_NAME};
pub use findings::{manual_path, order_violation, setup_findings, Finding, OrderViolation};
pub use packs::{parse_scenery_packs, PackEntry, SCENERY_PACKS_FILE};
