//! Road classes for lighting, from DSF road subtypes (`.net` ROAD_TYPE / 100).
//! Shared by Laminar's roads.net, roads_EU.net and X-World Pro's nets.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

/// Whether a subtype is drawn. An unknown subtype (neither lit nor known
/// unlit) is logged once per run.
pub fn is_lit_subtype(subtype: u8) -> bool {
    if RoadClass::of(subtype).is_some() {
        return true;
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
        }
    }
}
