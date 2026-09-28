//! Region assignment helpers for geographic coordinates.
//!
//! This module provides utilities for suggesting region assignments based on
//! latitude/longitude coordinates. These are suggestions only - publishers
//! can define their own regional groupings.

/// Suggested region based on geographic coordinates.
///
/// These are the default XEarthLayer regional groupings. Publishers may
/// use different schemes (countries, states, custom regions, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuggestedRegion {
    /// Africa: lat -35 to 37, lon -18 to 52
    Africa,
    /// Antarctica: lat -90 to -60, lon -180 to 180
    Antarctica,
    /// Asia: lat 0 to 80, lon 52 to 180 or -180 to -170
    Asia,
    /// Australia/Oceania: lat -50 to 0, lon 110 to 180
    Australia,
    /// Europe: lat 35 to 72, lon -25 to 52
    Europe,
    /// North America: lat 15 to 85, lon -170 to -50
    NorthAmerica,
    /// South America: lat -60 to 15, lon -90 to -30
    SouthAmerica,
}

impl SuggestedRegion {
    /// Get the lowercase region code for folder naming.
    pub fn code(&self) -> &'static str {
        match self {
            SuggestedRegion::Africa => "afr",
            SuggestedRegion::Antarctica => "ant",
            SuggestedRegion::Asia => "asia",
            SuggestedRegion::Australia => "aus",
            SuggestedRegion::Europe => "eur",
            SuggestedRegion::NorthAmerica => "na",
            SuggestedRegion::SouthAmerica => "sa",
        }
    }

    /// Get the full region name.
    pub fn name(&self) -> &'static str {
        match self {
            SuggestedRegion::Africa => "Africa",
            SuggestedRegion::Antarctica => "Antarctica",
            SuggestedRegion::Asia => "Asia",
            SuggestedRegion::Australia => "Australia",
            SuggestedRegion::Europe => "Europe",
            SuggestedRegion::NorthAmerica => "North America",
            SuggestedRegion::SouthAmerica => "South America",
        }
    }
}

/// Result of region suggestion for a set of tiles.
#[derive(Debug, Clone)]
pub struct RegionSuggestion {
    /// The suggested region (if tiles consistently fall in one region).
    pub region: Option<SuggestedRegion>,

    /// All regions that tiles fall into.
    pub regions_found: Vec<SuggestedRegion>,

    /// Tiles that don't clearly fit any region.
    pub ambiguous_tiles: Vec<(i32, i32)>,

    /// Tiles that fall in multiple overlapping regions.
    pub overlapping_tiles: Vec<(i32, i32, Vec<SuggestedRegion>)>,
}

impl RegionSuggestion {
    /// Returns true if all tiles fit in a single region.
    pub fn is_unambiguous(&self) -> bool {
        self.region.is_some()
            && self.ambiguous_tiles.is_empty()
            && self.overlapping_tiles.is_empty()
    }
}

/// Suggest a region for a single coordinate.
///
/// Returns all regions the coordinate could belong to (may be multiple
/// due to overlapping boundaries, or empty if in ocean/uncovered area).
pub fn suggest_region(latitude: i32, longitude: i32) -> Vec<SuggestedRegion> {
    let mut regions = Vec::new();

    // Antarctica (check first, no longitude restriction)
    if (-90..-60).contains(&latitude) {
        regions.push(SuggestedRegion::Antarctica);
    }

    // Africa
    if (-35..=37).contains(&latitude) && (-18..=52).contains(&longitude) {
        regions.push(SuggestedRegion::Africa);
    }

    // Asia (wraps around dateline)
    if (0..=80).contains(&latitude)
        && ((52..=180).contains(&longitude) || (-180..=-170).contains(&longitude))
    {
        regions.push(SuggestedRegion::Asia);
    }

    // Australia/Oceania
    if (-50..=0).contains(&latitude) && (110..=180).contains(&longitude) {
        regions.push(SuggestedRegion::Australia);
    }

    // Europe
    if (35..=72).contains(&latitude) && (-25..=52).contains(&longitude) {
        regions.push(SuggestedRegion::Europe);
    }

    // North America
    if (15..=85).contains(&latitude) && (-170..=-50).contains(&longitude) {
        regions.push(SuggestedRegion::NorthAmerica);
    }

    // South America
    if (-60..=15).contains(&latitude) && (-90..=-30).contains(&longitude) {
        regions.push(SuggestedRegion::SouthAmerica);
    }

    regions
}

/// Analyze a set of tile coordinates and suggest a region.
///
/// This examines all tiles and determines if they consistently fall within
/// a single region. If tiles span multiple regions or fall in ambiguous
/// areas, this is reported.
pub fn analyze_tiles(tiles: &[(i32, i32)]) -> RegionSuggestion {
    use std::collections::HashSet;

    let mut all_regions: HashSet<SuggestedRegion> = HashSet::new();
    let mut ambiguous_tiles = Vec::new();
    let mut overlapping_tiles = Vec::new();

    for &(lat, lon) in tiles {
        let regions = suggest_region(lat, lon);

        match regions.len() {
            0 => ambiguous_tiles.push((lat, lon)),
            1 => {
                all_regions.insert(regions[0]);
            }
            _ => {
                for &r in &regions {
                    all_regions.insert(r);
                }
                overlapping_tiles.push((lat, lon, regions));
            }
        }
    }

    let regions_found: Vec<_> = all_regions.into_iter().collect();

    // Suggest a single region only if all tiles fall in the same one
    let region =
        if regions_found.len() == 1 && ambiguous_tiles.is_empty() && overlapping_tiles.is_empty() {
            Some(regions_found[0])
        } else {
            None
        };

    RegionSuggestion {
        region,
        regions_found,
        ambiguous_tiles,
        overlapping_tiles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggested_region_code() {
        assert_eq!(SuggestedRegion::NorthAmerica.code(), "na");
        assert_eq!(SuggestedRegion::Europe.code(), "eur");
        assert_eq!(SuggestedRegion::Asia.code(), "asia");
    }

    #[test]
    fn test_suggested_region_name() {
        assert_eq!(SuggestedRegion::NorthAmerica.name(), "North America");
        assert_eq!(SuggestedRegion::Europe.name(), "Europe");
    }

    #[test]
    fn test_suggest_region_california() {
        // Los Angeles area
        let regions = suggest_region(34, -118);
        assert_eq!(regions, vec![SuggestedRegion::NorthAmerica]);
    }

    #[test]
    fn test_suggest_region_europe() {
        // London area
        let regions = suggest_region(51, 0);
        assert_eq!(regions, vec![SuggestedRegion::Europe]);
    }

    #[test]
    fn test_suggest_region_antarctica() {
        let regions = suggest_region(-75, 0);
        assert_eq!(regions, vec![SuggestedRegion::Antarctica]);
    }

    #[test]
    fn test_suggest_region_australia() {
        // Sydney area
        let regions = suggest_region(-34, 151);
        assert_eq!(regions, vec![SuggestedRegion::Australia]);
    }

    #[test]
    fn test_suggest_region_asia() {
        // Tokyo area
        let regions = suggest_region(35, 139);
        // Tokyo is in Asia range (lat 0-80, lon 52-180)
        assert!(regions.contains(&SuggestedRegion::Asia));
    }

    #[test]
    fn test_suggest_region_overlap_europe_africa() {
        // Morocco/Spain border area - overlaps Europe and Africa
        let regions = suggest_region(36, -5);
        assert!(regions.contains(&SuggestedRegion::Europe));
        assert!(regions.contains(&SuggestedRegion::Africa));
    }

    #[test]
    fn test_suggest_region_ocean() {
        // Middle of Pacific Ocean - no region
        let regions = suggest_region(0, -150);
        assert!(regions.is_empty());
    }

    #[test]
    fn test_analyze_tiles_single_region() {
        let tiles = vec![(34, -118), (35, -119), (36, -120)];
        let suggestion = analyze_tiles(&tiles);

        assert!(suggestion.is_unambiguous());
        assert_eq!(suggestion.region, Some(SuggestedRegion::NorthAmerica));
        assert!(suggestion.ambiguous_tiles.is_empty());
        assert!(suggestion.overlapping_tiles.is_empty());
    }

    #[test]
    fn test_analyze_tiles_multiple_regions() {
        // Mix of NA and Europe tiles
        let tiles = vec![(34, -118), (51, 0)];
        let suggestion = analyze_tiles(&tiles);

        assert!(!suggestion.is_unambiguous());
        assert_eq!(suggestion.region, None);
        assert!(suggestion
            .regions_found
            .contains(&SuggestedRegion::NorthAmerica));
        assert!(suggestion.regions_found.contains(&SuggestedRegion::Europe));
    }

    #[test]
    fn test_analyze_tiles_with_ambiguous() {
        // One valid NA tile, one in ocean
        let tiles = vec![(34, -118), (0, -150)];
        let suggestion = analyze_tiles(&tiles);

        assert!(!suggestion.is_unambiguous());
        assert_eq!(suggestion.ambiguous_tiles, vec![(0, -150)]);
    }

    #[test]
    fn test_analyze_tiles_empty() {
        let tiles: Vec<(i32, i32)> = vec![];
        let suggestion = analyze_tiles(&tiles);

        assert_eq!(suggestion.region, None);
        assert!(suggestion.regions_found.is_empty());
    }

    #[test]
    fn test_region_suggestion_is_unambiguous() {
        let unambiguous = RegionSuggestion {
            region: Some(SuggestedRegion::NorthAmerica),
            regions_found: vec![SuggestedRegion::NorthAmerica],
            ambiguous_tiles: vec![],
            overlapping_tiles: vec![],
        };
        assert!(unambiguous.is_unambiguous());

        let ambiguous = RegionSuggestion {
            region: None,
            regions_found: vec![SuggestedRegion::NorthAmerica, SuggestedRegion::Europe],
            ambiguous_tiles: vec![],
            overlapping_tiles: vec![],
        };
        assert!(!ambiguous.is_unambiguous());
    }
}
