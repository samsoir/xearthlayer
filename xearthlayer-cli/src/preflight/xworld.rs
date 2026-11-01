//! SimHeaven X-World Pro checks on every `run` (#213). Detection contributes
//! what X-Plane will load; the order check can stop startup; the setup check
//! only warns. XEarthLayer never edits scenery_packs.ini or X-World Pro.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};
use xearthlayer::xworld_pro::{
    detect, order_violation, setup_findings, Detection, OrderViolation, OverlayCoverage,
    SCENERY_PACKS_FILE,
};

use super::names;

const ENABLE_HINT: &str = "SimHeaven X-World Pro detected. Run 'xearthlayer setup', or \
'xearthlayer config set packages.xworld_pro true', to give it priority over XEarthLayer overlays";

const ORDER_HINT: &str = "Move XEarthLayer's entries (zzXEL_..., yzXEL_...) below every \
X-World Pro layer in scenery_packs.ini, as the 'Scenery_packs.ini' section of the \
X-World Pro manual describes, then start XEarthLayer again";

fn xworld_on(ctx: &BootstrapContext) -> bool {
    ctx.command() == "run" && ctx.config().is_some_and(|c| c.packages.xworld_pro)
}

/// Contributes the X-World Pro detection when the setting is on, and hints
/// when it is off but X-World Pro is installed.
pub struct XWorldProDetect;

impl Preflight<BootstrapContext> for XWorldProDetect {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_DETECT.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(config) = ctx.config() else {
            return Status::unsatisfied("configuration has not been loaded");
        };
        let Some(custom_scenery) = ctx.custom_scenery_path() else {
            return Status::unsatisfied("Custom Scenery path has not been resolved");
        };
        if !config.packages.xworld_pro {
            return if detect(custom_scenery).is_present() {
                Status::Warning(ENABLE_HINT.to_string())
            } else {
                Status::Satisfied
            };
        }
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run").remediable();
        };
        match coverage_report(d, config.packages.disable_overlays) {
            CoverageReport::Info(message) => {
                tracing::info!("{message}");
                Status::Satisfied
            }
            CoverageReport::Warn(message) => Status::Warning(message),
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let custom_scenery = ctx.custom_scenery_path().cloned().ok_or_else(|| {
            PreflightError::new(
                names::XWORLD_PRO_DETECT,
                "Custom Scenery path has not been resolved",
            )
        })?;
        ctx.set_xworld_pro(detect(&custom_scenery));
        Ok(Remedy::default())
    }
}

/// The detection outcome as a message: a report of what is active (logged)
/// or a misconfiguration the user should see (printed as a warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageReport {
    Info(String),
    Warn(String),
}

/// Describe what X-World Pro detection found. Active continents are a report
/// the dashboard's Status pane also shows; an unreadable file, support with
/// nothing installed, or no enabled continent are worth the user's attention.
pub fn coverage_report(
    d: &xearthlayer::xworld_pro::Detection,
    disable_overlays: bool,
) -> CoverageReport {
    let mut report = if let Some(e) = &d.read_error {
        CoverageReport::Warn(format!(
            "X-World Pro: could not read {e}; XEarthLayer overlays are unchanged"
        ))
    } else if !d.is_present() {
        CoverageReport::Warn(
            "packages.xworld_pro is on, but no X-World Pro was found in scenery_packs.ini; \
             XEarthLayer overlays are unchanged"
                .to_string(),
        )
    } else {
        let coverage = OverlayCoverage::from_detection(d);
        if coverage.is_empty() {
            CoverageReport::Warn(
                "X-World Pro is installed but no continent is enabled; XEarthLayer overlays \
                 are unchanged"
                    .to_string(),
            )
        } else if coverage.whole_planet {
            CoverageReport::Info(
                "X-World Pro: the whole planet is active; all XEarthLayer overlays are off"
                    .to_string(),
            )
        } else {
            CoverageReport::Info(format!(
                "X-World Pro: {} active; XEarthLayer overlays are off there",
                coverage.describe()
            ))
        }
    };
    if disable_overlays {
        let (CoverageReport::Info(message) | CoverageReport::Warn(message)) = &mut report;
        message.push_str(" (packages.disable_overlays turns every XEarthLayer overlay off anyway)");
    }
    report
}

/// What the user chose when the order is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderChoice {
    /// Stop so the user can fix `scenery_packs.ini`.
    Stop,
    /// The user chose to continue: remembered until the file changes.
    Continue,
    /// Nobody could be asked (no terminal): continue, warn, remember nothing.
    ContinueUnasked,
}

/// How an order problem is put to the user. Injected so a test never asks.
pub trait ConfirmOrder: Send + Sync {
    fn decide(&self, violation: &OrderViolation, scenery_packs: &Path) -> OrderChoice;
}

/// Continue as if the user chose to: tests.
#[cfg(test)]
pub struct ContinueAnyway;

#[cfg(test)]
impl ConfirmOrder for ContinueAnyway {
    fn decide(&self, _: &OrderViolation, _: &Path) -> OrderChoice {
        OrderChoice::Continue
    }
}

/// Whether there is someone to ask (#326). The prompt is drawn on stderr and
/// the answer read from stdin, so both must be the terminal: asking on stdin
/// alone drew the menu into a redirected file, or explained nothing when stdout
/// was the one redirected.
fn can_ask(stdin_is_terminal: bool, stderr_is_terminal: bool) -> bool {
    stdin_is_terminal && stderr_is_terminal
}

/// The prompt, all on one stream: what is wrong, how to fix it, what each key does.
fn order_prompt(violation: &OrderViolation, scenery_packs: &Path) -> String {
    format!(
        "\nX-World Pro must load before XEarthLayer, but {} has them the other way round:\n\
         \n  line {:>4}  {}\n  line {:>4}  {}\n\
         \nTo fix it, move XEarthLayer's entries (zzXEL_..., yzXEL_...) below every \
         X-World Pro line,\nthen start XEarthLayer again.\n\
         \n  s  stop now, so you can fix the file\
         \n  c  continue anyway (not asked again until scenery_packs.ini changes)\n",
        scenery_packs.display(),
        violation.xel.line,
        violation.xel.name(),
        violation.xwp.line,
        violation.xwp.name(),
    )
}

/// The choice a key makes, if any. Ctrl+C and Escape stop.
fn choice_for_key(key: &console::Key) -> Option<OrderChoice> {
    use console::Key;
    match key {
        Key::Char('s' | 'S') | Key::Escape | Key::CtrlC => Some(OrderChoice::Stop),
        Key::Char('c' | 'C') => Some(OrderChoice::Continue),
        _ => None,
    }
}

/// Ask, when there is a terminal to ask in.
pub struct AskUserOrder;

impl ConfirmOrder for AskUserOrder {
    fn decide(&self, violation: &OrderViolation, scenery_packs: &Path) -> OrderChoice {
        use std::io::IsTerminal;
        // `cfg!(test)` backs up injection: a test must never block on input,
        // and the streams are terminals when the suite runs from a shell.
        if cfg!(test)
            || !can_ask(
                std::io::stdin().is_terminal(),
                std::io::stderr().is_terminal(),
            )
        {
            return OrderChoice::ContinueUnasked;
        }
        let term = console::Term::stderr();
        let _ = term.write_str(&order_prompt(violation, scenery_packs));
        loop {
            let _ = term.write_str("\nPress s or c: ");
            match term.read_key() {
                Ok(key) => {
                    if let Some(choice) = choice_for_key(&key) {
                        let _ = term.write_line(if choice == OrderChoice::Stop {
                            "s"
                        } else {
                            "c"
                        });
                        return choice;
                    }
                }
                // A terminal that cannot be read cannot answer: never block.
                Err(_) => return OrderChoice::ContinueUnasked,
            }
        }
    }
}

/// The fingerprint a remembered "continue" is tied to: the file's contents, so
/// X-Plane rewriting it unchanged keeps the answer and any edit asks again.
fn fingerprint(scenery_packs: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(scenery_packs).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// XEarthLayer's entries must load below every enabled X-World Pro layer.
pub struct XWorldProOrder {
    confirm: Box<dyn ConfirmOrder>,
    /// Where a "continue" is remembered, with the fingerprint it applies to.
    accepted: PathBuf,
}

impl XWorldProOrder {
    pub fn new(confirm: Box<dyn ConfirmOrder>, accepted: PathBuf) -> Self {
        Self { confirm, accepted }
    }

    fn remembered(&self, scenery_packs: &Path) -> bool {
        match (
            std::fs::read_to_string(&self.accepted),
            fingerprint(scenery_packs),
        ) {
            (Ok(stored), Some(current)) => stored.trim() == current,
            _ => false,
        }
    }
}

fn scenery_packs_of(d: &Detection) -> PathBuf {
    d.custom_scenery.join(SCENERY_PACKS_FILE)
}

impl Preflight<BootstrapContext> for XWorldProOrder {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_ORDER.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        xworld_on(ctx)
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run");
        };
        match order_violation(d) {
            None => Status::Satisfied,
            Some(v) if ctx.xworld_order_accepted() || self.remembered(&scenery_packs_of(d)) => {
                Status::Warning(format!(
                    "X-World Pro scenery order: {}; continuing. {ORDER_HINT}",
                    v.describe()
                ))
            }
            Some(v) => Status::unsatisfied(format!("X-World Pro scenery order: {}", v.describe()))
                .remediable()
                .with_hint(ORDER_HINT),
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let Some(d) = ctx.xworld_pro() else {
            return Ok(Remedy::default());
        };
        let scenery_packs = scenery_packs_of(d);
        let Some(violation) = order_violation(d) else {
            return Ok(Remedy::default());
        };
        match self.confirm.decide(&violation, &scenery_packs) {
            OrderChoice::Stop => {}
            OrderChoice::Continue => {
                if let Some(fp) = fingerprint(&scenery_packs) {
                    // Remembering is a convenience: failing to only means
                    // being asked again next time.
                    let _ = xearthlayer::cache::integrity::write_atomic(&self.accepted, |w| {
                        std::io::Write::write_all(w, fp.as_bytes())
                    });
                }
                ctx.accept_xworld_order();
            }
            OrderChoice::ContinueUnasked => ctx.accept_xworld_order(),
        }
        Ok(Remedy::default())
    }
}

/// Warnings for the ortho library, mesh variants, install links and broken entries.
pub struct XWorldProSetup;

impl Preflight<BootstrapContext> for XWorldProSetup {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_SETUP.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        xworld_on(ctx)
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run");
        };
        let findings = setup_findings(d);
        if findings.is_empty() {
            return Status::Satisfied;
        }
        Status::Warning(
            findings
                .iter()
                .map(|f| f.message())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFile;
    use xearthlayer::xworld_pro::SCENERY_PACKS_FILE;

    const EU09: &str = "simHeaven_X-WORLD-Pro_Europe-09-scenery";

    /// A run context whose Custom Scenery holds `folders` and `ini`.
    fn ctx(xworld_pro: bool, folders: &[&str], ini: &str) -> (tempfile::TempDir, BootstrapContext) {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("Custom Scenery");
        std::fs::create_dir_all(&cs).unwrap();
        for f in folders {
            std::fs::create_dir_all(cs.join(f)).unwrap();
        }
        std::fs::write(cs.join(SCENERY_PACKS_FILE), ini).unwrap();
        let mut config = ConfigFile::default();
        config.packages.xworld_pro = xworld_pro;
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_config(config);
        ctx.set_custom_scenery_path(cs);
        (dir, ctx)
    }

    fn pack(name: &str) -> String {
        format!("SCENERY_PACK Custom Scenery/{name}/\n")
    }

    struct Stop;
    impl ConfirmOrder for Stop {
        fn decide(&self, _: &OrderViolation, _: &Path) -> OrderChoice {
            OrderChoice::Stop
        }
    }

    /// Answers like a terminal-less run.
    struct NoTerminal;
    impl ConfirmOrder for NoTerminal {
        fn decide(&self, _: &OrderViolation, _: &Path) -> OrderChoice {
            OrderChoice::ContinueUnasked
        }
    }

    /// An order check remembering its answer in `dir`.
    fn order_check(confirm: Box<dyn ConfirmOrder>, dir: &tempfile::TempDir) -> XWorldProOrder {
        XWorldProOrder::new(confirm, dir.path().join("state").join("accepted"))
    }

    fn bad_order() -> String {
        pack("zzXEL_ortho") + &pack(EU09)
    }

    #[test]
    fn detect_only_hints_when_the_setting_is_off() {
        let (_d, c) = ctx(false, &[EU09], &pack(EU09));
        match XWorldProDetect.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("xearthlayer setup"), "{m}"),
            other => panic!("{other:?}"),
        }
        let (_d, c) = ctx(false, &[], "");
        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
    }

    #[test]
    fn detect_contributes_and_then_reports_the_continents() {
        let (_d, mut c) = ctx(true, &[EU09], &pack(EU09));
        assert!(matches!(
            XWorldProDetect.inspect(&c),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        XWorldProDetect.remediate(&mut c).unwrap();
        // Active continents are a report, not a problem: it goes to the log
        // and the dashboard's Status pane, not to the terminal.
        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
        match coverage_report(c.xworld_pro().unwrap(), false) {
            CoverageReport::Info(m) => assert!(m.contains("Europe"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_whole_planet_is_logged_not_printed() {
        let folders: Vec<String> = ["Africa", "America", "Asia", "Australia-Oceania", "Europe"]
            .iter()
            .map(|c| format!("simHeaven_X-WORLD-Pro_{c}-09-scenery"))
            .collect();
        let names: Vec<&str> = folders.iter().map(String::as_str).collect();
        let ini: String = names.iter().map(|n| pack(n)).collect();
        let (_d, mut c) = ctx(true, &names, &ini);
        XWorldProDetect.remediate(&mut c).unwrap();

        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
        assert_eq!(
            coverage_report(c.xworld_pro().unwrap(), true),
            CoverageReport::Info(
                "X-World Pro: the whole planet is active; all XEarthLayer overlays are off \
                 (packages.disable_overlays turns every XEarthLayer overlay off anyway)"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_setting_with_nothing_installed_still_warns_on_the_terminal() {
        let (_d, mut c) = ctx(true, &[], "");
        XWorldProDetect.remediate(&mut c).unwrap();
        match XWorldProDetect.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("no X-World Pro was found"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn detect_names_a_missing_custom_scenery_path() {
        let mut c = BootstrapContext::new("run", None);
        c.set_config(ConfigFile::default());
        match XWorldProDetect.inspect(&c) {
            Status::Unsatisfied { reason, .. } => {
                assert!(reason.contains("Custom Scenery"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn order_and_setup_apply_only_with_the_setting_on_and_for_run() {
        let (_d, c) = ctx(false, &[], "");
        assert!(!order_check(Box::new(ContinueAnyway), &_d).applies(&c));
        assert!(!XWorldProSetup.applies(&c));
        let (_d, c) = ctx(true, &[], "");
        assert!(order_check(Box::new(ContinueAnyway), &_d).applies(&c));
        let mut s = BootstrapContext::new("setup", None);
        s.set_config(c.config().unwrap().clone());
        assert!(!XWorldProSetup.applies(&s));
    }

    fn detected(
        xworld_pro: bool,
        folders: &[&str],
        ini: &str,
    ) -> (tempfile::TempDir, BootstrapContext) {
        let (d, mut c) = ctx(xworld_pro, folders, ini);
        XWorldProDetect.remediate(&mut c).unwrap();
        (d, c)
    }

    #[test]
    fn a_good_order_is_satisfied() {
        let (_d, c) = detected(true, &[EU09], &(pack(EU09) + &pack("zzXEL_ortho")));
        assert_eq!(
            order_check(Box::new(Stop), &_d).inspect(&c),
            Status::Satisfied
        );
    }

    #[test]
    fn a_bad_order_continues_as_a_warning_when_the_user_says_so() {
        let (_d, mut c) = detected(true, &[EU09], &bad_order());
        let check = order_check(Box::new(ContinueAnyway), &_d);
        assert!(matches!(
            check.inspect(&c),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        check.remediate(&mut c).unwrap();
        match check.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("zzXEL_ortho") && m.contains(EU09), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_order_stays_unsatisfied_when_the_user_stops() {
        let (_d, mut c) = detected(true, &[EU09], &bad_order());
        let check = order_check(Box::new(Stop), &_d);
        check.remediate(&mut c).unwrap();
        match check.inspect(&c) {
            Status::Unsatisfied { hint: Some(h), .. } => {
                assert!(h.contains("scenery_packs.ini"), "{h}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setup_findings_are_one_warning() {
        let (_d, c) = detected(true, &[], &pack(EU09)); // folder missing
        match XWorldProSetup.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("does not exist"), "{m}"),
            other => panic!("{other:?}"),
        }
        let (_d, c) = detected(true, &[EU09], &pack(EU09));
        assert_eq!(XWorldProSetup.inspect(&c), Status::Satisfied);
    }

    /// A later start against the same folder, with `ini` as the file now reads.
    fn detected_in(dir: &tempfile::TempDir, xworld_pro: bool, ini: &str) -> BootstrapContext {
        let cs = dir.path().join("Custom Scenery");
        std::fs::write(cs.join(SCENERY_PACKS_FILE), ini).unwrap();
        let mut config = ConfigFile::default();
        config.packages.xworld_pro = xworld_pro;
        let mut c = BootstrapContext::new("run", None);
        c.set_config(config);
        c.set_custom_scenery_path(cs);
        XWorldProDetect.remediate(&mut c).unwrap();
        c
    }

    fn a_violation(c: &BootstrapContext) -> OrderViolation {
        order_violation(c.xworld_pro().unwrap()).unwrap()
    }

    #[test]
    fn the_interactive_confirmer_never_blocks_under_test() {
        let (_d, c) = detected(true, &[EU09], &bad_order());
        assert_eq!(
            AskUserOrder.decide(&a_violation(&c), Path::new("scenery_packs.ini")),
            OrderChoice::ContinueUnasked
        );
    }

    #[test]
    fn it_asks_only_when_the_prompt_can_be_seen_and_answered() {
        // #326: the menu goes to stderr and the key comes from stdin.
        assert!(can_ask(true, true));
        assert!(
            !can_ask(true, false),
            "stderr redirected: the menu would be invisible"
        );
        assert!(!can_ask(false, true), "stdin redirected: nobody can answer");
        assert!(!can_ask(false, false));
    }

    #[test]
    fn the_prompt_names_both_lines_the_file_the_fix_and_each_key() {
        let (_d, c) = detected(true, &[EU09], &bad_order());
        let text = order_prompt(
            &a_violation(&c),
            Path::new("/xp/Custom Scenery/scenery_packs.ini"),
        );
        for wanted in [
            "/xp/Custom Scenery/scenery_packs.ini",
            "line    1  zzXEL_ortho",
            &format!("line    2  {EU09}"),
            "move XEarthLayer's entries",
            "s  stop now",
            "c  continue anyway",
        ] {
            assert!(text.contains(wanted), "missing {wanted:?} in:\n{text}");
        }
    }

    #[test]
    fn keys_choose_and_anything_else_is_ignored() {
        use console::Key;
        assert_eq!(choice_for_key(&Key::Char('s')), Some(OrderChoice::Stop));
        assert_eq!(choice_for_key(&Key::Char('S')), Some(OrderChoice::Stop));
        assert_eq!(choice_for_key(&Key::Escape), Some(OrderChoice::Stop));
        assert_eq!(choice_for_key(&Key::CtrlC), Some(OrderChoice::Stop));
        assert_eq!(choice_for_key(&Key::Char('c')), Some(OrderChoice::Continue));
        assert_eq!(choice_for_key(&Key::Enter), None, "no silent default");
        assert_eq!(choice_for_key(&Key::Char('y')), None);
    }

    #[test]
    fn a_continue_is_remembered_until_scenery_packs_changes() {
        let (d, mut c) = detected(true, &[EU09], &bad_order());
        order_check(Box::new(ContinueAnyway), &d)
            .remediate(&mut c)
            .unwrap();

        // The next start: a fresh context, and a confirmer that would stop.
        let next = detected_in(&d, true, &bad_order());
        let check = order_check(Box::new(Stop), &d);
        assert!(
            matches!(check.inspect(&next), Status::Warning(_)),
            "the remembered answer stands while the file is unchanged"
        );

        // Any edit to the file asks again.
        let edited = bad_order() + &pack("Some_Other_Scenery");
        let changed = detected_in(&d, true, &edited);
        assert!(matches!(
            check.inspect(&changed),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
    }

    #[test]
    fn continuing_without_a_terminal_warns_but_remembers_nothing() {
        let (d, mut c) = detected(true, &[EU09], &bad_order());
        let check = order_check(Box::new(NoTerminal), &d);
        check.remediate(&mut c).unwrap();
        match check.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("zzXEL_ortho") && m.contains("Move"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!d.path().join("state").join("accepted").exists());
    }
}
