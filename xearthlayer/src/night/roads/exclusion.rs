//! `sim/exclude_net` zones: a west/south/east/north box, or (X-Plane 12)
//! that box followed by `;lon1,lat1,lon2,lat2,...`. A lower-priority feature
//! with ANY point inside a zone is removed.

#[derive(Debug, Clone, PartialEq)]
pub struct Exclusion {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    /// `(lon, lat)` vertices, at least three, when the polygon form is used.
    pub polygon: Option<Vec<(f64, f64)>>,
}

impl Exclusion {
    pub fn parse(value: &str) -> Option<Self> {
        let (rect, poly) = match value.split_once(';') {
            Some((r, p)) => (r, Some(p)),
            None => (value, None),
        };
        let v: Vec<f64> = rect
            .split('/')
            .map(|s| s.trim().parse().ok())
            .collect::<Option<_>>()?;
        let [west, south, east, north] = v[..] else {
            return None;
        };
        if !(west < east && south < north) {
            return None;
        }
        let polygon = match poly {
            None => None,
            Some(p) => {
                let n: Vec<f64> = p
                    .split(',')
                    .map(|s| s.trim().parse().ok())
                    .collect::<Option<_>>()?;
                if n.len() < 6 || !n.len().is_multiple_of(2) {
                    return None;
                }
                Some(n.chunks(2).map(|c| (c[0], c[1])).collect())
            }
        };
        Some(Self {
            west,
            south,
            east,
            north,
            polygon,
        })
    }

    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        if lon < self.west || lon > self.east || lat < self.south || lat > self.north {
            return false;
        }
        match &self.polygon {
            None => true,
            Some(p) => point_in_polygon(p, lon, lat),
        }
    }

    /// Whether this rectangle (never a polygon) covers the whole 1 degree tile.
    pub fn covers_tile(&self, lat: i32, lon: i32) -> bool {
        let (s, w) = (f64::from(lat), f64::from(lon));
        self.polygon.is_none()
            && self.west <= w
            && self.south <= s
            && self.east >= w + 1.0
            && self.north >= s + 1.0
    }
}

/// Even-odd ray casting.
fn point_in_polygon(p: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let mut j = p.len() - 1;
    for i in 0..p.len() {
        let (xi, yi) = p[i];
        let (xj, yj) = p[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_box_in_west_south_east_north_order() {
        let e = Exclusion::parse("6.000000000/46.000000000/7.000000000/47.000000000").unwrap();
        assert_eq!((e.west, e.south, e.east, e.north), (6.0, 46.0, 7.0, 47.0));
        assert!(e.polygon.is_none());
        assert!(e.contains(6.5, 46.5));
        assert!(!e.contains(7.5, 46.5));
        assert!(e.covers_tile(46, 6));
    }

    #[test]
    fn parses_the_xp12_polygon_form_and_tests_inside_it() {
        // A triangle in the lower-left half of the box.
        let e = Exclusion::parse("6/46/7/47;6,46,7,46,6,47").unwrap();
        assert_eq!(e.polygon.as_ref().unwrap().len(), 3);
        assert!(e.contains(6.2, 46.2));
        assert!(!e.contains(6.9, 46.9));
        assert!(!e.covers_tile(46, 6), "a polygon never counts as covering");
    }

    #[test]
    fn a_partial_box_does_not_cover_the_tile() {
        let e = Exclusion::parse("6.2/46/7/47").unwrap();
        assert!(!e.covers_tile(46, 6));
    }

    #[test]
    fn malformed_values_are_rejected() {
        for v in ["", "6/46/7", "a/b/c/d", "6/46/7/47;6,46,7", "7/46/6/47"] {
            assert!(Exclusion::parse(v).is_none(), "{v:?}");
        }
    }
}
