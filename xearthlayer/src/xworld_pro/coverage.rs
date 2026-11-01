//! Which XEarthLayer overlay packages X-World Pro replaces.

use std::collections::BTreeSet;

use super::detect::{Continent, Detection};

/// The X-World Pro continent of a region code
/// (`<CONTINENT>-<REGION>-<SUBREGIONS>`, any case), or `None`.
pub fn continent_of_region(code: &str) -> Option<Continent> {
    let head = code.split('-').next().unwrap_or("");
    match head.to_ascii_uppercase().as_str() {
        "NA" | "SA" => Some(Continent::America),
        "EU" => Some(Continent::Europe),
        "AF" => Some(Continent::Africa),
        "AS" => Some(Continent::Asia),
        "OC" => Some(Continent::AustraliaOceania),
        _ => None,
    }
}

/// Where X-World Pro is active, for leaving XEarthLayer overlays out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayCoverage {
    pub continents: BTreeSet<Continent>,
    /// All five continents: every overlay is left out, mapped or not.
    pub whole_planet: bool,
}

impl OverlayCoverage {
    pub fn from_detection(d: &Detection) -> Self {
        Self {
            continents: d.active_continents(),
            whole_planet: d.whole_planet(),
        }
    }

    /// Whether the overlay package for `region` is left out.
    pub fn suppresses(&self, region: &str) -> bool {
        self.whole_planet
            || continent_of_region(region).is_some_and(|c| self.continents.contains(&c))
    }

    /// No continent active: nothing is left out.
    pub fn is_empty(&self) -> bool {
        !self.whole_planet && self.continents.is_empty()
    }

    /// "the whole planet", or the continents joined with ", ".
    pub fn describe(&self) -> String {
        if self.whole_planet {
            return "the whole planet".to_string();
        }
        self.continents
            .iter()
            .map(|c| c.label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How the consolidated `yzXEL_overlay` folder is rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayPlan {
    /// `packages.disable_overlays`: remove the folder.
    Remove,
    /// X-World Pro is active: build it without the covered regions.
    Except(OverlayCoverage),
    /// Build it from every installed overlay package.
    All,
}

/// The one rule for startup and the package commands: disabling overlays
/// wins, then a non-empty X-World Pro coverage, else everything.
pub fn overlay_plan(disable_overlays: bool, coverage: Option<&OverlayCoverage>) -> OverlayPlan {
    if disable_overlays {
        return OverlayPlan::Remove;
    }
    match coverage {
        Some(c) if !c.is_empty() => OverlayPlan::Except(c.clone()),
        _ => OverlayPlan::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_codes_map_by_their_first_segment() {
        assert_eq!(
            continent_of_region("NA-USA-MX-CENTRAL"),
            Some(Continent::America)
        );
        assert_eq!(
            continent_of_region("na-canada-greenland"),
            Some(Continent::America)
        );
        assert_eq!(continent_of_region("SA-BRAZIL"), Some(Continent::America));
        assert_eq!(continent_of_region("EU-WEST"), Some(Continent::Europe));
        assert_eq!(continent_of_region("AF-NORTH"), Some(Continent::Africa));
        assert_eq!(continent_of_region("AS-RUSSIA-EAST"), Some(Continent::Asia));
        assert_eq!(
            continent_of_region("OC-AUSTRALIA-NZ-PACIFIC"),
            Some(Continent::AustraliaOceania)
        );
        assert_eq!(continent_of_region("XX-SOMEWHERE"), None);
        assert_eq!(continent_of_region(""), None);
    }

    fn coverage(cs: &[Continent], whole: bool) -> OverlayCoverage {
        OverlayCoverage {
            continents: cs.iter().copied().collect(),
            whole_planet: whole,
        }
    }

    #[test]
    fn active_continents_suppress_their_regions_only() {
        let c = coverage(&[Continent::Europe], false);
        assert!(c.suppresses("EU-WEST"));
        assert!(!c.suppresses("NA-CANADA-WEST"));
        assert!(!c.suppresses("XX-UNKNOWN"));
    }

    #[test]
    fn the_whole_planet_suppresses_everything() {
        let c = coverage(&Continent::ALL, true);
        assert!(c.suppresses("NA-USA-MX-CENTRAL"));
        assert!(c.suppresses("XX-UNKNOWN"));
    }

    #[test]
    fn disabling_overlays_wins_over_any_coverage() {
        let c = coverage(&[Continent::Europe], false);
        assert_eq!(overlay_plan(true, Some(&c)), OverlayPlan::Remove);
        assert_eq!(overlay_plan(true, None), OverlayPlan::Remove);
    }

    #[test]
    fn no_or_empty_coverage_builds_everything() {
        assert_eq!(overlay_plan(false, None), OverlayPlan::All);
        assert_eq!(
            overlay_plan(false, Some(&OverlayCoverage::default())),
            OverlayPlan::All
        );
    }

    #[test]
    fn non_empty_coverage_leaves_regions_out() {
        let c = coverage(&[Continent::Europe], false);
        assert_eq!(overlay_plan(false, Some(&c)), OverlayPlan::Except(c));
    }

    #[test]
    fn descriptions_name_the_continents() {
        assert_eq!(
            coverage(&[Continent::America, Continent::Europe], false).describe(),
            "America, Europe"
        );
        assert_eq!(
            coverage(&Continent::ALL, true).describe(),
            "the whole planet"
        );
        assert!(coverage(&[], false).is_empty());
    }
}
