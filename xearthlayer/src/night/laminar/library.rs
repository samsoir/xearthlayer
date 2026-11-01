//! X-Plane `library.txt`: virtual terrain names to files, global entries only.

use std::collections::{HashMap, HashSet};

/// The global (unconditional) exports of one `library.txt`.
#[derive(Debug, Default)]
pub struct LibraryIndex {
    exclude: HashMap<String, String>,
    export: HashMap<String, String>,
    summer: HashMap<String, String>,
    seasonal: HashMap<String, String>,
}

impl LibraryIndex {
    /// Index the global entries of a `library.txt`.
    pub fn parse(text: &str) -> Self {
        let mut index = Self::default();
        let mut global_regions: HashSet<String> = HashSet::from(["all".to_string()]);
        let mut defining: Option<String> = None;
        let mut in_global = true; // before any REGION line

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            match tokens.as_slice() {
                ["REGION_DEFINE", name, ..] => defining = Some((*name).to_string()),
                ["REGION_RECT", "-180", "-90", "179", "89", ..] => {
                    if let Some(name) = &defining {
                        global_regions.insert(name.clone());
                    }
                }
                ["REGION", name, ..] => in_global = global_regions.contains(*name),
                _ if !in_global => {}
                ["EXPORT_EXCLUDE", v, f, ..] => insert_first(&mut index.exclude, v, f),
                ["EXPORT", v, f, ..] => insert_first(&mut index.export, v, f),
                [kind, seasons, v, f, ..]
                    if *kind == "EXPORT_SEASON" || *kind == "EXPORT_EXCLUDE_SEASON" =>
                {
                    if seasons.split(',').any(|s| s == "sum") {
                        insert_first(&mut index.summer, v, f);
                    } else {
                        insert_first(&mut index.seasonal, v, f);
                    }
                }
                _ => {}
            }
        }
        index
    }

    /// The file a virtual path names, relative to the library's folder.
    pub fn resolve(&self, virtual_path: &str) -> Option<&str> {
        [&self.exclude, &self.export, &self.summer, &self.seasonal]
            .into_iter()
            .find_map(|m| m.get(virtual_path))
            .map(String::as_str)
    }
}

fn insert_first(map: &mut HashMap<String, String>, v: &str, f: &str) {
    map.entry(v.to_string()).or_insert_with(|| f.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "A\n1200\nLIBRARY\n\n\
        REGION_DEFINE terrain10_NAW/\n\
        REGION_BITMAP maps/region_NAW.png\n\
        REGION terrain10_NAW/\n\
        EXPORT_EXCLUDE\tlib/g10/terrain10/urban_tmp_sdry3.ter\tterrain10_NAW/regional.ter\n\
        \n\
        REGION_DEFINE all\n\
        REGION_RECT\t-180 -90 179 89\n\
        REGION all\n\
        # EXPORT lib/g10/terrain10/urban_tmp_sdry3.ter terrain12/commented.ter\n\
        EXPORT   lib/g10/terrain10/urban_tmp_sdry3.ter\tterrain12/urban_tmp_sdry3.ter\n\
        EXPORT   lib/g10/terrain10/urban_tmp_sdry3.ter\tterrain12/second.ter\n\
        EXPORT   lib/g10/terrain10/ind_tmp_sdry1.ter\tterrain12/ind_old.ter\n\
        EXPORT_EXCLUDE lib/g10/terrain10/ind_tmp_sdry1.ter\tterrain12/ind_tmp_sdry1.ter\n\
        EXPORT_SEASON fal,win\tlib/g10/terrain10/urban_cld_dry_prk.ter\tterrain12/fal/park_tmp_sdry1.ter\n\
        EXPORT_SEASON spr,sum\tlib/g10/terrain10/urban_cld_dry_prk.ter\t\tterrain12/park_tmp_sdry1.ter\n";

    #[test]
    fn resolves_a_global_export() {
        let idx = LibraryIndex::parse(SAMPLE);
        assert_eq!(
            idx.resolve("lib/g10/terrain10/urban_tmp_sdry3.ter"),
            Some("terrain12/urban_tmp_sdry3.ter")
        );
    }

    #[test]
    fn regional_blocks_and_comments_are_ignored() {
        let idx = LibraryIndex::parse(SAMPLE);
        assert_ne!(
            idx.resolve("lib/g10/terrain10/urban_tmp_sdry3.ter"),
            Some("terrain10_NAW/regional.ter")
        );
    }

    #[test]
    fn exclude_wins_over_export() {
        let idx = LibraryIndex::parse(SAMPLE);
        assert_eq!(
            idx.resolve("lib/g10/terrain10/ind_tmp_sdry1.ter"),
            Some("terrain12/ind_tmp_sdry1.ter")
        );
    }

    #[test]
    fn summer_wins_among_seasonal_exports() {
        let idx = LibraryIndex::parse(SAMPLE);
        assert_eq!(
            idx.resolve("lib/g10/terrain10/urban_cld_dry_prk.ter"),
            Some("terrain12/park_tmp_sdry1.ter")
        );
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing() {
        assert_eq!(
            LibraryIndex::parse(SAMPLE).resolve("lib/g10/terrain10/nope.ter"),
            None
        );
    }

    #[test]
    fn a_library_without_regions_is_global() {
        let idx = LibraryIndex::parse("A\n1200\nLIBRARY\nEXPORT lib/x.ter terrain12/x.ter\n");
        assert_eq!(idx.resolve("lib/x.ter"), Some("terrain12/x.ter"));
    }
}
