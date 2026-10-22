//! Region selectors: a partial region code that selects a group of packages.
//!
//! Region codes are hierarchical by convention (`NA-CANADA-WEST`), so a
//! leading part of a code selects every package beneath it: `NA` selects all
//! of North America and `NA-CANADA` both halves of Canada. Matching is a
//! case-insensitive prefix match on the raw string and always expands, so an
//! exact title is one match among the rest rather than a short circuit.
//!
//! A selector is user input that never becomes a path. Only the titles it
//! resolves to reach the filesystem, and those were already validated as
//! region codes when their library index or metadata was parsed.

use crate::error::CliError;

/// Resolve `selector` against `candidates`, keeping every candidate whose
/// region code starts with it, in candidate order.
///
/// Each command passes the candidates it reads: library entries for
/// `install`, installed packages for `update`, `remove` and `info`. An empty
/// result is an error that lists the codes those candidates offer.
pub fn resolve<T>(
    selector: &str,
    candidates: Vec<T>,
    code: impl Fn(&T) -> &str,
) -> Result<Vec<T>, CliError> {
    resolve_many(&[selector.to_string()], candidates, code)
}

/// Resolve several selectors together: every candidate any of them selects,
/// once each, in candidate order. One selector that matches nothing fails
/// the whole resolution, so a typo never silently shrinks a batch.
pub fn resolve_many<T>(
    selectors: &[String],
    candidates: Vec<T>,
    code: impl Fn(&T) -> &str,
) -> Result<Vec<T>, CliError> {
    let selectors: Vec<&str> = selectors.iter().map(|s| s.trim()).collect();
    if selectors.is_empty() || selectors.iter().any(|s| s.is_empty()) {
        return Err(CliError::Packages(
            "Region code must not be empty".to_string(),
        ));
    }

    let selected_by = |c: &T, selector: &str| starts_with_ignore_case(code(c), selector);
    if let Some(unmatched) = selectors
        .iter()
        .find(|s| !candidates.iter().any(|c| selected_by(c, s)))
    {
        return Err(no_match(unmatched, candidates.iter().map(&code)));
    }
    Ok(candidates
        .into_iter()
        .filter(|c| selectors.iter().any(|s| selected_by(c, s)))
        .collect())
}

/// What a command acts on: the packages some region selectors pick, or every
/// package (`--all`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Regions(Vec<String>),
    All,
}

impl Selection {
    /// From the command line, where clap allows exactly one of the two forms.
    pub fn from_args(regions: Vec<String>, all: bool) -> Self {
        if all {
            Self::All
        } else {
            Self::Regions(regions)
        }
    }

    /// The candidates this selection picks. `--all` with no candidates is an
    /// error carrying `nothing`, so a command never runs on an empty set.
    pub fn select<T>(
        &self,
        candidates: Vec<T>,
        code: impl Fn(&T) -> &str,
        nothing: &str,
    ) -> Result<Vec<T>, CliError> {
        match self {
            Self::All if candidates.is_empty() => Err(CliError::Packages(nothing.to_string())),
            Self::All => Ok(candidates),
            Self::Regions(selectors) => resolve_many(selectors, candidates, code),
        }
    }

    /// The selection as the user typed it, for messages: `NA, EU-WEST` or
    /// `--all`.
    pub fn describe(&self) -> String {
        match self {
            Self::All => "--all".to_string(),
            Self::Regions(selectors) => selectors.join(", "),
        }
    }
}

/// `str::starts_with`, ignoring ASCII case. `get` rather than slicing, so a
/// selector longer than the code, or one that ends inside a multi-byte
/// character, is simply not a match.
fn starts_with_ignore_case(code: &str, prefix: &str) -> bool {
    code.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn no_match<'a>(selector: &str, codes: impl Iterator<Item = &'a str>) -> CliError {
    let mut codes: Vec<&str> = codes.collect();
    codes.sort_unstable();
    codes.dedup();
    let available = if codes.is_empty() {
        "none".to_string()
    } else {
        codes.join(", ")
    };
    CliError::Packages(format!(
        "No package matches '{selector}'. Available: {available}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The world rebuild's region codes, plus a legacy `EU` that coexists
    /// with its successors while the rebuild is promoted.
    const CODES: &[&str] = &[
        "NA-USA-BAHAMAS-BERMUDA",
        "NA-CANADA-WEST",
        "NA-CANADA-EAST",
        "NA-GREENLAND",
        "NA-MEXICO-CENTRAL-CARIBBEAN",
        "EU",
        "EU-WEST",
        "EU-EAST",
        "AF-NORTH",
        "AF-SOUTH",
        "OC-AUSTRALIA-NZ-PACIFIC",
        "AS-CHINA-MONGOLIA",
        "AS-INDIA-MIDDLE-EAST-HIMALAYIAN",
        "AS-ASIA-PACIFIC",
        "AS-RUSSIA-WEST",
        "AS-RUSSIA-EAST",
    ];

    fn select(selector: &str) -> Result<Vec<&'static str>, CliError> {
        resolve(selector, CODES.to_vec(), |c| c)
    }

    fn error_message(result: Result<Vec<&'static str>, CliError>) -> String {
        match result {
            Err(CliError::Packages(message)) => message,
            other => panic!("expected a packages error, got {:?}", other.ok()),
        }
    }

    #[test]
    fn a_continent_code_selects_every_package_on_it() {
        assert_eq!(
            select("NA").unwrap(),
            vec![
                "NA-USA-BAHAMAS-BERMUDA",
                "NA-CANADA-WEST",
                "NA-CANADA-EAST",
                "NA-GREENLAND",
                "NA-MEXICO-CENTRAL-CARIBBEAN",
            ]
        );
    }

    #[test]
    fn a_country_code_selects_every_part_of_it() {
        assert_eq!(
            select("NA-CANADA").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
        assert_eq!(
            select("AS-RUSSIA").unwrap(),
            vec!["AS-RUSSIA-WEST", "AS-RUSSIA-EAST"]
        );
        assert_eq!(
            select("NA-MEXICO").unwrap(),
            vec!["NA-MEXICO-CENTRAL-CARIBBEAN"]
        );
    }

    #[test]
    fn a_full_code_selects_exactly_that_package() {
        assert_eq!(select("AF-SOUTH").unwrap(), vec!["AF-SOUTH"]);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(
            select("na-canada").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
    }

    #[test]
    fn an_exact_title_does_not_stop_the_expansion() {
        assert_eq!(select("EU").unwrap(), vec!["EU", "EU-WEST", "EU-EAST"]);
    }

    #[test]
    fn a_selector_need_not_end_on_a_segment_boundary() {
        assert_eq!(
            select("NA-CAN").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
        assert_eq!(select("AF-S").unwrap(), vec!["AF-SOUTH"]);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(select("  AF-NORTH ").unwrap(), vec!["AF-NORTH"]);
    }

    #[test]
    fn no_match_is_an_error_naming_the_selector_and_the_available_codes() {
        let message = error_message(resolve("XX", vec!["NA", "EU-WEST"], |c| c));

        assert!(message.contains("'XX'"), "{message}");
        assert!(message.contains("Available: EU-WEST, NA"), "{message}");
    }

    #[test]
    fn available_codes_are_listed_once_each() {
        // A region with both an ortho and an overlay package is two candidates.
        let message = error_message(resolve("XX", vec!["NA", "NA", "EU"], |c| c));

        assert!(message.contains("Available: EU, NA"), "{message}");
    }

    #[test]
    fn no_candidates_at_all_says_so() {
        let message = error_message(resolve("NA", Vec::<&str>::new(), |c| c));

        assert!(message.contains("Available: none"), "{message}");
    }

    fn select_many(selectors: &[&str]) -> Result<Vec<&'static str>, CliError> {
        let selectors: Vec<String> = selectors.iter().map(|s| s.to_string()).collect();
        resolve_many(&selectors, CODES.to_vec(), |c| c)
    }

    #[test]
    fn several_selectors_select_everything_any_of_them_matches_in_candidate_order() {
        assert_eq!(
            select_many(&["EU-WEST", "NA-CANADA"]).unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST", "EU-WEST"]
        );
    }

    #[test]
    fn overlapping_selectors_select_each_package_once() {
        assert_eq!(
            select_many(&["NA", "NA-CANADA"]).unwrap(),
            select("NA").unwrap()
        );
    }

    #[test]
    fn one_selector_that_matches_nothing_fails_the_whole_selection() {
        let message = error_message(select_many(&["NA", "XX"]));

        assert!(message.contains("'XX'"), "{message}");
    }

    #[test]
    fn an_empty_selector_among_others_is_refused() {
        let message = error_message(select_many(&["NA", "  "]));

        assert!(message.contains("must not be empty"), "{message}");
    }

    #[test]
    fn all_selects_every_candidate() {
        assert_eq!(
            Selection::All
                .select(vec!["NA", "EU"], |c| c, "nothing")
                .unwrap(),
            vec!["NA", "EU"]
        );
    }

    #[test]
    fn all_with_no_candidates_is_an_error_with_the_given_message() {
        let result =
            Selection::All.select(Vec::<&str>::new(), |c| c, "No ortho packages installed");

        assert_eq!(error_message(result), "No ortho packages installed");
    }

    #[test]
    fn regions_select_through_their_selectors() {
        let selection = Selection::Regions(vec!["AF".to_string()]);

        assert_eq!(
            selection.select(CODES.to_vec(), |c| c, "nothing").unwrap(),
            vec!["AF-NORTH", "AF-SOUTH"]
        );
    }

    #[test]
    fn a_selection_is_built_from_the_command_line() {
        assert_eq!(Selection::from_args(vec![], true), Selection::All);
        assert_eq!(
            Selection::from_args(vec!["NA".into(), "EU".into()], false),
            Selection::Regions(vec!["NA".into(), "EU".into()])
        );
    }

    #[test]
    fn an_empty_selector_selects_nothing() {
        // Every code starts with the empty string, so without this guard
        // `install ""` would install every package in the library.
        let message = error_message(select("   "));

        assert!(message.contains("must not be empty"), "{message}");
    }
}
