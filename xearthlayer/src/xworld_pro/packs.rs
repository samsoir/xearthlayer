//! X-Plane's `scenery_packs.ini`: the load order of scenery packs.

/// The file in Custom Scenery that lists the scenery packs in load order.
pub const SCENERY_PACKS_FILE: &str = "scenery_packs.ini";

/// One `SCENERY_PACK` or `SCENERY_PACK_DISABLED` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    /// 1-based line number in the file.
    pub line: usize,
    /// `SCENERY_PACK` (true) or `SCENERY_PACK_DISABLED` (false).
    pub enabled: bool,
    /// The path as written: relative to the X-Plane root, or absolute.
    pub path: String,
}

impl PackEntry {
    /// The pack's folder name: the last path segment, either separator.
    pub fn name(&self) -> &str {
        self.path
            .split(['/', '\\'])
            .rfind(|s| !s.is_empty())
            .unwrap_or("")
    }
}

/// Every pack line, in file order. Header and unknown lines are skipped.
pub fn parse_scenery_packs(text: &str) -> Vec<PackEntry> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let line = raw.trim_end();
            let (enabled, rest) = if let Some(r) = line.strip_prefix("SCENERY_PACK_DISABLED ") {
                (false, r)
            } else {
                let r = line.strip_prefix("SCENERY_PACK ")?;
                (true, r)
            };
            Some(PackEntry {
                line: i + 1,
                enabled,
                path: rest.trim().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "I\n1000 Version\nSCENERY\n\n\
SCENERY_PACK Custom Scenery/KSFO/\n\
SCENERY_PACK *GLOBAL_AIRPORTS*\n\
SCENERY_PACK_DISABLED Custom Scenery/simHeaven_X-WORLD-Pro_Europe-10-forests/\n\
SCENERY_PACK /mnt/scenery/zOrtho_+49+008/\n\
SCENERY_PACK E:\\X-Plane_Scenery\\Airport\\_EDQH Herzogenaurach/\n";

    #[test]
    fn entries_keep_order_line_numbers_and_state() {
        let e = parse_scenery_packs(INI);
        assert_eq!(e.len(), 5);
        assert_eq!((e[0].line, e[0].enabled, e[0].name()), (5, true, "KSFO"));
        assert_eq!(e[1].name(), "*GLOBAL_AIRPORTS*");
        assert_eq!((e[2].line, e[2].enabled), (7, false));
        assert_eq!(e[2].name(), "simHeaven_X-WORLD-Pro_Europe-10-forests");
        assert_eq!(e[3].name(), "zOrtho_+49+008");
        assert_eq!(e[4].name(), "_EDQH Herzogenaurach");
    }

    #[test]
    fn header_and_unknown_lines_are_ignored() {
        assert!(parse_scenery_packs("I\n1000 Version\nSCENERY\nNOTHING here\n").is_empty());
    }
}
