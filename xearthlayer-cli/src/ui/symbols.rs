//! Which symbols the Status pane draws: emoji, or plain marks for terminals
//! that cannot show them.
//!
//! No terminal reports its font, so `auto` decides from what can be checked:
//! the terminal type, the locale, whether the session is remote, whether a
//! colour font covers the emoji (Linux, via fontconfig, which nearly every
//! Linux terminal falls back through), and finally how far the terminal
//! moves the cursor for one emoji. Each check that can only say "no" runs
//! before the ones that cost something, and any doubt picks plain marks:
//! a plain mark is always legible, a missing emoji is a box.

use std::io::{self, Write};
use std::process::Command;

/// The four emoji the Status pane draws.
pub const ENGINE: &str = "🌐";
pub const NIGHT_LIT: &str = "🌙";
pub const NIGHT_DARK: &str = "🌑";
pub const XWORLD_PRO: &str = "🌍";
const ALL_EMOJI: [&str; 4] = [ENGINE, NIGHT_LIT, NIGHT_DARK, XWORLD_PRO];

/// What `tui.symbols` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolPreference {
    #[default]
    Auto,
    Emoji,
    Plain,
}

impl SymbolPreference {
    /// From the `tui.symbols` value, which the config loader has validated.
    pub fn from_config(value: &str) -> Self {
        match value {
            "emoji" => Self::Emoji,
            "plain" => Self::Plain,
            _ => Self::Auto,
        }
    }
}

/// The symbols the Status pane draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolSet {
    Emoji,
    #[default]
    Plain,
}

/// What `auto` reads about the terminal. The two checks that cost something
/// (a process, a terminal round trip) take `&mut self` so a double can count
/// them, and are only asked when nothing cheaper has decided.
pub trait TerminalEnvironment {
    /// An environment variable, `None` when unset or empty.
    fn var(&self, name: &str) -> Option<String>;
    fn is_macos(&self) -> bool;
    /// Whether a colour font covers every Status pane emoji, `None` when
    /// it cannot be found out.
    fn has_colour_emoji_font(&mut self) -> Option<bool>;
    /// Columns the terminal moves the cursor for one emoji, `None` when it
    /// does not answer.
    fn emoji_width(&mut self) -> Option<u16>;
}

/// The symbol set to draw, and why, for the log.
pub fn resolve(
    preference: SymbolPreference,
    env: &mut impl TerminalEnvironment,
) -> (SymbolSet, &'static str) {
    match preference {
        SymbolPreference::Emoji => return (SymbolSet::Emoji, "tui.symbols = emoji"),
        SymbolPreference::Plain => return (SymbolSet::Plain, "tui.symbols = plain"),
        SymbolPreference::Auto => {}
    }
    let plain = |reason| (SymbolSet::Plain, reason);

    if matches!(env.var("TERM").as_deref(), Some("linux" | "dumb")) {
        return plain("the terminal type draws no emoji");
    }
    // The variable the C library would consult first is the one in force.
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| env.var(name));
    if locale.is_some_and(|l| !is_utf8_locale(&l)) {
        return plain("the locale is not UTF-8");
    }
    // The fonts that matter are on the machine the user types at.
    if env.var("SSH_CONNECTION").is_some() || env.var("SSH_TTY").is_some() {
        return plain("an SSH session, whose terminal fonts cannot be checked");
    }
    if !env.is_macos() && env.has_colour_emoji_font() != Some(true) {
        return plain("no colour emoji font is installed");
    }
    if env.emoji_width() != Some(2) {
        return plain("the terminal does not draw emoji two columns wide");
    }
    (SymbolSet::Emoji, "the terminal can draw emoji")
}

fn is_utf8_locale(locale: &str) -> bool {
    let locale = locale.to_ascii_lowercase();
    locale.contains("utf-8") || locale.contains("utf8")
}

/// The real terminal and system.
pub struct SystemTerminal;

impl TerminalEnvironment for SystemTerminal {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn is_macos(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn has_colour_emoji_font(&mut self) -> Option<bool> {
        let output = Command::new("fc-list")
            .arg(fontconfig_pattern())
            .arg("family")
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        // fc-list exits 0 when nothing matches: only the output says.
        Some(!output.stdout.iter().all(u8::is_ascii_whitespace))
    }

    fn emoji_width(&mut self) -> Option<u16> {
        use crossterm::{cursor, execute, style::Print};
        let mut stdout = io::stdout();
        execute!(stdout, cursor::MoveTo(0, 0), Print(ENGINE)).ok()?;
        stdout.flush().ok()?;
        cursor::position().ok().map(|(column, _)| column)
    }
}

/// A fontconfig pattern matching colour fonts that cover every emoji.
fn fontconfig_pattern() -> String {
    let codepoints: Vec<String> = ALL_EMOJI
        .iter()
        .flat_map(|e| e.chars())
        .map(|c| format!("{:x}", c as u32))
        .collect();
    format!(":color=true:charset={}", codepoints.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A terminal described by its variables and check answers, counting
    /// the costly checks.
    struct FakeTerminal {
        vars: HashMap<&'static str, &'static str>,
        macos: bool,
        font: Option<bool>,
        width: Option<u16>,
        font_checks: usize,
        width_checks: usize,
    }

    impl FakeTerminal {
        /// A local Linux kitty session that can draw emoji.
        fn capable_linux() -> Self {
            Self {
                vars: HashMap::from([("TERM", "xterm-kitty"), ("LANG", "en_GB.UTF-8")]),
                macos: false,
                font: Some(true),
                width: Some(2),
                font_checks: 0,
                width_checks: 0,
            }
        }

        fn with_var(mut self, name: &'static str, value: &'static str) -> Self {
            self.vars.insert(name, value);
            self
        }
    }

    impl TerminalEnvironment for FakeTerminal {
        fn var(&self, name: &str) -> Option<String> {
            self.vars
                .get(name)
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
        }
        fn is_macos(&self) -> bool {
            self.macos
        }
        fn has_colour_emoji_font(&mut self) -> Option<bool> {
            self.font_checks += 1;
            self.font
        }
        fn emoji_width(&mut self) -> Option<u16> {
            self.width_checks += 1;
            self.width
        }
    }

    fn auto(env: &mut FakeTerminal) -> SymbolSet {
        resolve(SymbolPreference::Auto, env).0
    }

    #[test]
    fn the_setting_reads_its_three_values() {
        assert_eq!(
            SymbolPreference::from_config("auto"),
            SymbolPreference::Auto
        );
        assert_eq!(
            SymbolPreference::from_config("emoji"),
            SymbolPreference::Emoji
        );
        assert_eq!(
            SymbolPreference::from_config("plain"),
            SymbolPreference::Plain
        );
    }

    #[test]
    fn an_explicit_setting_wins_without_checking_anything() {
        for (preference, expected) in [
            (SymbolPreference::Emoji, SymbolSet::Emoji),
            (SymbolPreference::Plain, SymbolSet::Plain),
        ] {
            let mut env = FakeTerminal::capable_linux().with_var("TERM", "linux");
            env.font = Some(false);
            assert_eq!(resolve(preference, &mut env).0, expected);
            assert_eq!((env.font_checks, env.width_checks), (0, 0));
        }
    }

    #[test]
    fn a_capable_local_linux_terminal_gets_emoji() {
        let mut env = FakeTerminal::capable_linux();
        assert_eq!(auto(&mut env), SymbolSet::Emoji);
        assert_eq!((env.font_checks, env.width_checks), (1, 1));
    }

    #[test]
    fn the_text_console_and_dumb_terminals_get_plain_marks() {
        for term in ["linux", "dumb"] {
            let mut env = FakeTerminal::capable_linux().with_var("TERM", term);
            assert_eq!(auto(&mut env), SymbolSet::Plain, "{term}");
            assert_eq!((env.font_checks, env.width_checks), (0, 0));
        }
    }

    #[test]
    fn a_locale_that_is_not_utf8_gets_plain_marks() {
        let mut env = FakeTerminal::capable_linux().with_var("LANG", "C");
        assert_eq!(auto(&mut env), SymbolSet::Plain);
    }

    #[test]
    fn the_most_specific_locale_variable_decides() {
        let mut env = FakeTerminal::capable_linux()
            .with_var("LANG", "C")
            .with_var("LC_CTYPE", "en_US.utf8");
        assert_eq!(auto(&mut env), SymbolSet::Emoji, "LC_CTYPE beats LANG");

        let mut env = FakeTerminal::capable_linux().with_var("LC_ALL", "POSIX");
        assert_eq!(auto(&mut env), SymbolSet::Plain, "LC_ALL beats both");
    }

    #[test]
    fn an_unset_locale_says_nothing_either_way() {
        let mut env = FakeTerminal::capable_linux();
        env.vars.remove("LANG");
        assert_eq!(auto(&mut env), SymbolSet::Emoji);
    }

    #[test]
    fn an_ssh_session_gets_plain_marks_even_on_macos() {
        for var in ["SSH_CONNECTION", "SSH_TTY"] {
            let mut env = FakeTerminal::capable_linux().with_var(var, "x");
            env.macos = true;
            assert_eq!(auto(&mut env), SymbolSet::Plain, "{var}");
            assert_eq!((env.font_checks, env.width_checks), (0, 0));
        }
    }

    #[test]
    fn linux_without_a_colour_emoji_font_gets_plain_marks() {
        for font in [Some(false), None] {
            let mut env = FakeTerminal::capable_linux();
            env.font = font;
            assert_eq!(auto(&mut env), SymbolSet::Plain, "{font:?}");
            assert_eq!(env.width_checks, 0, "no probe once the font rules it out");
        }
    }

    #[test]
    fn macos_skips_the_font_check_because_apple_color_emoji_ships_with_it() {
        let mut env = FakeTerminal::capable_linux();
        env.macos = true;
        env.font = None;
        assert_eq!(auto(&mut env), SymbolSet::Emoji);
        assert_eq!(env.font_checks, 0);
    }

    #[test]
    fn a_terminal_that_moves_other_than_two_columns_gets_plain_marks() {
        for width in [Some(1), Some(3), None] {
            let mut env = FakeTerminal::capable_linux();
            env.width = width;
            assert_eq!(auto(&mut env), SymbolSet::Plain, "{width:?}");
        }
    }

    #[test]
    fn the_fontconfig_pattern_names_every_emoji_and_asks_for_colour() {
        assert_eq!(
            fontconfig_pattern(),
            ":color=true:charset=1f310 1f319 1f311 1f30d"
        );
    }
}
