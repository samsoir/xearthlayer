//! Merge one tile's DSF networks as X-Plane presents them: highest
//! priority first; a chain is dropped when any point lies in a zone of a
//! higher-priority DSF. Unlit subtypes are dropped too.

use super::class::RoadClass;
use super::dsf::{Chains, DsfRoads};
use super::exclusion::Exclusion;

/// One tile's lit road chains after the priority merge.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedRoads {
    pub tile: (i32, i32),
    pub chains: Chains,
    pub class: Vec<RoadClass>,
    /// Per chain `[south, west, north, east]`, offsets from the tile corner.
    pub bbox: Vec<[f32; 4]>,
    /// Chains dropped by a higher-priority exclusion zone.
    pub removed: usize,
}

impl MergedRoads {
    pub fn empty(tile: (i32, i32)) -> Self {
        Self {
            tile,
            chains: Chains::new(),
            class: vec![],
            bbox: vec![],
            removed: 0,
        }
    }

    /// Heap bytes held, for the byte-bounded tile cache.
    pub fn bytes(&self) -> usize {
        self.chains.bytes() + self.class.len() + self.bbox.len() * 16 + 64
    }

    /// Whether any chain's box meets the box (absolute degrees).
    pub fn crosses(&self, south: f64, west: f64, north: f64, east: f64) -> bool {
        let (lat0, lon0) = (f64::from(self.tile.0), f64::from(self.tile.1));
        self.bbox.iter().any(|b| {
            f64::from(b[2]) + lat0 >= south
                && f64::from(b[0]) + lat0 <= north
                && f64::from(b[3]) + lon0 >= west
                && f64::from(b[1]) + lon0 <= east
        })
    }
}

/// Fed the DSFs of one tile from highest priority to lowest.
pub struct Merger {
    tile: (i32, i32),
    zones: Vec<Exclusion>,
    out: MergedRoads,
}

impl Merger {
    pub fn new(tile: (i32, i32)) -> Self {
        Self {
            tile,
            zones: vec![],
            out: MergedRoads::empty(tile),
        }
    }

    /// Merges `layer` under every layer added before it, then adds its own
    /// zones for the layers below. Appends straight from the layer's
    /// arrays: no allocation per chain.
    pub fn add(&mut self, layer: &DsfRoads) {
        let (lat0, lon0) = (f64::from(self.tile.0), f64::from(self.tile.1));
        let c = &layer.chains;
        for i in 0..c.len() {
            let Some(class) = RoadClass::of(c.subtype[i]) else {
                continue;
            };
            let pts = c.points(i);
            let (lon, lat) = (&c.lon[pts.clone()], &c.lat[pts]);
            if !self.zones.is_empty()
                && lon.iter().zip(lat).any(|(&x, &y)| {
                    let (x, y) = (f64::from(x) + lon0, f64::from(y) + lat0);
                    self.zones.iter().any(|z| z.contains(x, y))
                })
            {
                self.out.removed += 1;
                continue;
            }
            let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for (&x, &y) in lon.iter().zip(lat) {
                b = [b[0].min(y), b[1].min(x), b[2].max(y), b[3].max(x)];
            }
            self.out.chains.push_from(c, i);
            self.out.class.push(class);
            self.out.bbox.push(b);
        }
        self.zones.extend(layer.exclusions.iter().cloned());
    }

    /// Whether a zone covers the whole tile, so no lower DSF can add roads.
    pub fn covered(&self) -> bool {
        self.zones
            .iter()
            .any(|z| z.covers_tile(self.tile.0, self.tile.1))
    }

    pub fn finish(mut self) -> MergedRoads {
        let o = &mut self.out;
        o.chains.lon.shrink_to_fit();
        o.chains.lat.shrink_to_fit();
        o.chains.start.shrink_to_fit();
        o.chains.subtype.shrink_to_fit();
        o.class.shrink_to_fit();
        o.bbox.shrink_to_fit();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(chains: &[(u8, &[(f32, f32)])], zones: &[&str]) -> DsfRoads {
        let mut c = Chains::new();
        for (s, pts) in chains {
            c.push(*s, pts);
        }
        DsfRoads {
            tile: (46, 6),
            overlay: true,
            net_def: Some("x.net".into()),
            exclusions: zones.iter().map(|z| Exclusion::parse(z).unwrap()).collect(),
            chains: c,
        }
    }

    fn covered_after(z: &[&str]) -> bool {
        let mut m = Merger::new((46, 6));
        m.add(&layer(&[], z));
        m.covered()
    }

    #[test]
    fn a_higher_zone_removes_lower_chains_with_any_point_inside() {
        let mut m = Merger::new((46, 6));
        // High: a zone on the south-west quarter.
        m.add(&layer(
            &[(10, &[(0.1, 0.1), (0.2, 0.2)])],
            &["6/46/6.5/46.5"],
        ));
        // Low: one chain crossing into the zone, one wholly outside.
        m.add(&layer(
            &[
                (50, &[(0.4, 0.4), (0.9, 0.9)]),
                (50, &[(0.7, 0.7), (0.8, 0.8)]),
            ],
            &[],
        ));
        let r = m.finish();
        assert_eq!(
            r.chains.len(),
            2,
            "high chain + the low chain wholly outside"
        );
        assert_eq!(r.removed, 1);
    }

    #[test]
    fn a_layer_never_excludes_its_own_or_higher_chains() {
        let mut m = Merger::new((46, 6));
        m.add(&layer(&[(10, &[(0.1, 0.1), (0.2, 0.2)])], &[]));
        m.add(&layer(&[(50, &[(0.3, 0.3), (0.4, 0.4)])], &["6/46/7/47"]));
        let r = m.finish();
        assert_eq!(r.chains.len(), 2);
        assert_eq!(r.removed, 0);
    }

    #[test]
    fn only_a_whole_tile_rectangle_covers() {
        assert!(covered_after(&["6/46/7/47"]));
        assert!(!covered_after(&["6/46/6.9/47"]));
        assert!(!covered_after(&["6/46/7/47;6,46,7,46,7,47,6,47"]));
    }

    #[test]
    fn unlit_subtypes_are_not_merged_and_classes_and_boxes_follow_chains() {
        let mut m = Merger::new((46, 6));
        m.add(&layer(
            &[
                (74, &[(0.1, 0.1), (0.2, 0.2)]),
                (100, &[(0.3, 0.5), (0.6, 0.4)]),
            ],
            &[],
        ));
        let r = m.finish();
        assert_eq!(r.chains.len(), 1);
        assert_eq!(r.class, vec![RoadClass::Motorway]);
        assert_eq!(r.bbox[0], [0.4, 0.3, 0.5, 0.6]);
        assert!(r.crosses(46.45, 6.35, 46.46, 6.36));
        assert!(!r.crosses(46.8, 6.8, 46.9, 6.9));
    }

    #[test]
    fn merged_points_and_offsets_stay_aligned_across_layers() {
        let mut m = Merger::new((46, 6));
        m.add(&layer(
            &[(10, &[(0.1, 0.2), (0.3, 0.4)])],
            &["6/46/6.15/46.5"],
        ));
        m.add(&layer(
            &[
                (50, &[(0.12, 0.3), (0.13, 0.31)]),
                (50, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]),
            ],
            &[],
        ));
        let r = m.finish();
        let mut expect = Chains::new();
        expect.push(10, &[(0.1, 0.2), (0.3, 0.4)]);
        expect.push(50, &[(0.5, 0.6), (0.7, 0.8), (0.9, 0.95)]);
        assert_eq!(r.chains, expect);
        assert_eq!(r.removed, 1);
        assert!(r.bytes() >= r.chains.bytes());
    }

    #[test]
    fn an_empty_merge_crosses_nothing() {
        let r = MergedRoads::empty((46, 6));
        assert!(!r.crosses(46.0, 6.0, 47.0, 7.0));
        assert_eq!(r.chains.len(), 0);
    }
}
