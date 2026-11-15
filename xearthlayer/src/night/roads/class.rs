//! Road classes for lighting, from DSF road subtypes (`.net` ROAD_TYPE / 100).
//! Shared by Laminar's roads.net, roads_EU.net and X-World Pro's nets.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RoadClass {
    Motorway,
    Ramp,
    Primary,
    Secondary,
    Local,
    Minor,
    LitFootway,
}

/// Subtypes known to carry no road light: 0 (Laminar's unnamed type),
/// 74 (XWP `#Pedestrian`), 151-154 (rail), 220 (power lines).
const KNOWN_UNLIT: [u8; 7] = [0, 74, 151, 152, 153, 154, 220];

impl RoadClass {
    pub const COUNT: usize = 7;

    /// Every class, in [`RoadClass::index`] order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Motorway,
        Self::Ramp,
        Self::Primary,
        Self::Secondary,
        Self::Local,
        Self::Minor,
        Self::LitFootway,
    ];

    pub fn of(subtype: u8) -> Option<Self> {
        match subtype {
            100..=119 => Some(Self::Motorway),
            120..=129 => Some(Self::Ramp),
            10..=29 => Some(Self::Primary),
            30..=49 => Some(Self::Secondary),
            50..=69 => Some(Self::Local),
            70..=73 | 80..=82 => Some(Self::Minor),
            75 => Some(Self::LitFootway),
            _ => None,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether roads of this class glow. In flight (KCCR, 2026-11-14) the
    /// sim's own residential streets read as scattered building light, not
    /// lines of lamps, and the Laminar pattern already carries that light;
    /// glowing local streets, minor roads and footways covered about four
    /// times the texels X-Plane's 3D scene lights beside them. They keep
    /// their class and model parameters for a later lamp option.
    pub fn is_lit(self) -> bool {
        matches!(
            self,
            Self::Motorway | Self::Ramp | Self::Primary | Self::Secondary
        )
    }
}

/// Whether a subtype is drawn: its class glows ([`RoadClass::is_lit`]).
/// An unknown subtype (neither classified nor known unlit) is logged once
/// per run.
pub fn is_lit_subtype(subtype: u8) -> bool {
    if let Some(class) = RoadClass::of(subtype) {
        return class.is_lit();
    }
    if !KNOWN_UNLIT.contains(&subtype) {
        static SEEN: OnceLock<Mutex<HashSet<u8>>> = OnceLock::new();
        let first = SEEN
            .get_or_init(Default::default)
            .lock()
            .map(|mut s| s.insert(subtype))
            .unwrap_or(false);
        if first {
            tracing::debug!(subtype, "Night roads: unknown road subtype, drawn unlit");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laminar_and_xwp_subtypes_map_to_classes() {
        assert_eq!(RoadClass::of(100), Some(RoadClass::Motorway));
        assert_eq!(RoadClass::of(110), Some(RoadClass::Motorway));
        assert_eq!(RoadClass::of(120), Some(RoadClass::Ramp));
        assert_eq!(RoadClass::of(10), Some(RoadClass::Primary));
        assert_eq!(RoadClass::of(28), Some(RoadClass::Primary));
        assert_eq!(RoadClass::of(30), Some(RoadClass::Secondary));
        assert_eq!(RoadClass::of(50), Some(RoadClass::Local));
        assert_eq!(RoadClass::of(68), Some(RoadClass::Local));
        assert_eq!(RoadClass::of(70), Some(RoadClass::Minor));
        assert_eq!(RoadClass::of(72), Some(RoadClass::Minor));
        assert_eq!(RoadClass::of(80), Some(RoadClass::Minor));
        assert_eq!(RoadClass::of(82), Some(RoadClass::Minor));
        assert_eq!(RoadClass::of(75), Some(RoadClass::LitFootway));
    }

    #[test]
    fn only_motorways_ramps_and_main_roads_are_drawn() {
        for s in [100u8, 119, 120, 129, 10, 29, 30, 49] {
            assert!(is_lit_subtype(s), "subtype {s}");
        }
        // Classified, so never logged as unknown, but drawn unlit.
        for s in [50u8, 69, 70, 73, 80, 82, 75] {
            assert!(RoadClass::of(s).is_some(), "subtype {s}");
            assert!(!is_lit_subtype(s), "subtype {s}");
        }
    }

    #[test]
    fn pedestrian_rail_power_and_zero_are_unlit() {
        for s in [0u8, 74, 151, 152, 153, 154, 220] {
            assert_eq!(RoadClass::of(s), None, "subtype {s}");
            assert!(!is_lit_subtype(s));
        }
    }

    #[test]
    fn unknown_subtypes_are_unlit() {
        for s in [1u8, 9, 76, 83, 99, 130, 200, 255] {
            assert_eq!(RoadClass::of(s), None, "subtype {s}");
            assert!(!is_lit_subtype(s), "subtype {s}");
            assert!(!is_lit_subtype(s), "subtype {s}, logged once");
        }
    }

    #[test]
    fn every_range_holds_from_its_lower_to_its_upper_bound() {
        use RoadClass::*;
        let ranges: [(std::ops::RangeInclusive<u8>, RoadClass); 8] = [
            (100..=119, Motorway),
            (120..=129, Ramp),
            (10..=29, Primary),
            (30..=49, Secondary),
            (50..=69, Local),
            (70..=73, Minor),
            (80..=82, Minor),
            (75..=75, LitFootway),
        ];
        for (range, class) in &ranges {
            for s in [*range.start(), *range.end()] {
                assert_eq!(RoadClass::of(s), Some(*class), "subtype {s}");
                assert_eq!(is_lit_subtype(s), class.is_lit(), "subtype {s}");
            }
        }
        // Every subtype is in at most one range, and outside them none.
        for s in 0..=255u8 {
            let holding: Vec<_> = ranges.iter().filter(|(r, _)| r.contains(&s)).collect();
            assert!(holding.len() <= 1, "subtype {s}");
            assert_eq!(
                RoadClass::of(s),
                holding.first().map(|(_, c)| *c),
                "subtype {s}"
            );
        }
    }

    #[test]
    fn all_lists_the_classes_in_index_order() {
        assert_eq!(RoadClass::ALL.len(), RoadClass::COUNT);
        for (i, class) in RoadClass::ALL.iter().enumerate() {
            assert_eq!(class.index(), i, "{class:?}");
        }
    }

    #[test]
    fn the_default_model_is_indexed_by_class() {
        use crate::night::roads::RoadModel;
        // The table of `docs/dev/night-lighting-design.md` (Road classes):
        // half width, sigma and intensity per class.
        let table = [
            (RoadClass::Motorway, 8.0, 10.0, 1.00),
            (RoadClass::Ramp, 4.0, 8.0, 0.90),
            (RoadClass::Primary, 6.0, 9.0, 0.95),
            (RoadClass::Secondary, 5.0, 8.0, 0.425),
            (RoadClass::Local, 3.0, 7.0, 0.60),
            (RoadClass::Minor, 2.0, 6.0, 0.45),
            (RoadClass::LitFootway, 1.5, 5.0, 0.30),
        ];
        for (class, half_width_m, sigma_m, intensity) in table {
            let p = RoadModel::DEFAULT.classes[class.index()];
            assert_eq!(
                (p.half_width_m, p.sigma_m, p.intensity),
                (half_width_m, sigma_m, intensity),
                "{class:?}"
            );
        }
    }
}
