//! Setup Step 6: SimHeaven X-World Pro (#213). The text comes from pure
//! functions of the detection; the prompt is a thin layer over them.

use std::path::Path;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm};
use xearthlayer::xworld_pro::{
    detect, manual_path, order_violation, setup_findings, Detection, Finding, OrderViolation,
    OverlayCoverage,
};

use crate::error::CliError;

pub(super) const NONE_FOUND: &str = "No SimHeaven X-World Pro detected.";

pub(super) const DECLINED_NOTE: &str = "X-World Pro keeps its normal priority. XEarthLayer's \
overlays may then draw roads and buildings over X-World Pro's; you can change this later \
with 'xearthlayer config set packages.xworld_pro true'.";

/// Shown whatever the answer: it applies to anyone running X-World Pro with ortho.
pub(super) const VRAM_NOTE: &str = "XEarthLayer and X-World Pro both use a lot of video \
memory. On graphics cards with less than 24 GB, lower X-Plane's rendering settings if your \
frame rate drops. X-World Pro with all layers costs about 5 to 10 fps on its own, even \
without ortho.";

/// What a "no" prints: the decline note, then the video memory note.
pub(super) fn declined_lines() -> Vec<String> {
    vec![
        DECLINED_NOTE.to_string(),
        String::new(),
        VRAM_NOTE.to_string(),
    ]
}

/// "SimHeaven X-World Pro detected: ..." or `None` when nothing is installed.
pub(super) fn detected_line(d: &Detection) -> Option<String> {
    if !d.is_present() {
        return None;
    }
    let coverage = OverlayCoverage::from_detection(d);
    let what = if coverage.whole_planet {
        "the whole planet".to_string()
    } else if coverage.is_empty() {
        "installed, but no continent is enabled".to_string()
    } else {
        coverage.describe()
    };
    Some(format!("SimHeaven X-World Pro detected: {what}"))
}

/// The three messages, with the install's current problems under the first.
pub(super) fn guidance(
    d: &Detection,
    findings: &[Finding],
    order: Option<&OrderViolation>,
    manual: Option<&Path>,
) -> Vec<String> {
    let mut out = vec![
        "1. Install X-World Pro as its manual describes:".to_string(),
        "   - in scenery_packs.ini, X-World Pro above the libraries, and XEarthLayer's \
         entries (zzXEL_..., yzXEL_...) below every X-World Pro layer (the library is not checked)"
            .to_string(),
        "   - the ortho library: 'library - ortho.txt' in place of library.txt".to_string(),
        "   - for any layer with two versions, the _O4XP-Mesh one enabled and the \
         _XP12-Mesh one disabled (Europe's bridges in X-World Pro 1.1)"
            .to_string(),
        "   - after an X-Plane update, run the library's install script (install_XWP_Linux_Mac.sh) again in the X-World Pro library folder inside Custom Scenery".to_string(),
    ];
    if let Some(manual) = manual {
        out.push(format!("   Manual: {}", manual.display()));
    }
    if order.is_some() || !findings.is_empty() {
        out.push("   Found in your installation now:".to_string());
        if let Some(v) = order {
            out.push(format!("   - {}", v.describe()));
        }
        out.extend(findings.iter().map(|f| format!("   - {}", f.message())));
    }
    out.push(String::new());
    out.push(format!("2. {VRAM_NOTE}"));
    out.push(String::new());
    let coverage = OverlayCoverage::from_detection(d);
    let where_ = if coverage.whole_planet {
        "all of them".to_string()
    } else if coverage.is_empty() {
        "none yet, as no continent is enabled".to_string()
    } else {
        coverage.describe()
    };
    out.push(format!(
        "3. On every start, XEarthLayer checks which X-World Pro continents are active and \
         turns its own overlays off there (now: {where_}). It also checks the scenery order \
         and offers to stop so you can fix it."
    ));
    out
}

/// Step 6. Returns the value for `packages.xworld_pro`.
pub(super) fn step_xworld(
    theme: &ColorfulTheme,
    custom_scenery: Option<&Path>,
) -> Result<bool, CliError> {
    let Some(custom_scenery) = custom_scenery else {
        println!("No Custom Scenery folder configured; skipping.");
        return Ok(false);
    };
    let d = detect(custom_scenery);
    let Some(line) = detected_line(&d) else {
        println!("{NONE_FOUND}");
        return Ok(false);
    };
    println!("{}", style(line).green());
    println!();
    let yes = Confirm::with_theme(theme)
        .with_prompt("Give X-World Pro priority over XEarthLayer overlays?")
        .default(true)
        .interact()
        .map_err(|e| CliError::Config(format!("X-World Pro selection failed: {}", e)))?;
    println!();
    if yes {
        let findings = setup_findings(&d);
        let order = order_violation(&d);
        for line in guidance(&d, &findings, order.as_ref(), manual_path(&d).as_deref()) {
            println!("{line}");
        }
    } else {
        for line in declined_lines() {
            println!("{line}");
        }
    }
    Ok(yes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::xworld_pro::{Continent, Detection, PackEntry, XwpLayer};

    fn with(continents: &[Continent]) -> Detection {
        let layers = continents
            .iter()
            .enumerate()
            .map(|(i, c)| XwpLayer {
                entry: PackEntry {
                    line: i + 1,
                    enabled: true,
                    path: format!("Custom Scenery/x{i}/"),
                },
                continent: *c,
                layer: "09-scenery".into(),
                dir: std::env::temp_dir(), // exists, so resolved
                resolved: true,
            })
            .collect();
        Detection {
            layers,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_detected_has_no_line() {
        assert!(detected_line(&Detection::default()).is_none());
    }

    #[test]
    fn the_detected_line_names_continents_or_the_whole_planet() {
        let l = detected_line(&with(&[Continent::Europe, Continent::America])).unwrap();
        assert!(l.contains("America, Europe"), "{l}");
        let l = detected_line(&with(&Continent::ALL)).unwrap();
        assert!(l.contains("whole planet"), "{l}");
    }

    #[test]
    fn guidance_has_the_three_messages() {
        let text = guidance(&with(&[Continent::Europe]), &[], None, None).join("\n");
        assert!(text.contains("manual"), "{text}");
        assert!(text.contains("24 GB"), "{text}");
        assert!(text.contains("5 to 10 fps"), "{text}");
        assert!(
            text.contains("overlays") && text.contains("Europe"),
            "{text}"
        );
    }

    #[test]
    fn guidance_lists_current_findings_and_the_manual_path() {
        let finding = Finding::BrokenEntry {
            name: "simHeaven_X-WORLD-Pro_Europe-09-scenery".into(),
            line: 4,
        };
        let manual = std::path::Path::new("/x/simHeaven_X-WORLD-Pro_Manual_v1.1.pdf");
        let text = guidance(&with(&[Continent::Europe]), &[finding], None, Some(manual)).join("\n");
        assert!(text.contains("line 4"), "{text}");
        assert!(
            text.contains("simHeaven_X-WORLD-Pro_Manual_v1.1.pdf"),
            "{text}"
        );
    }

    #[test]
    fn declining_still_gives_the_vram_note() {
        let text = declined_lines().join("\n");
        assert!(text.contains(DECLINED_NOTE), "{text}");
        assert!(text.contains("24 GB"), "{text}");
        assert!(text.contains("5 to 10 fps"), "{text}");
    }

    #[test]
    fn whole_planet_guidance_says_all_overlays() {
        let text = guidance(&with(&Continent::ALL), &[], None, None).join("\n");
        assert!(text.contains("all of them"), "{text}");
    }
}
