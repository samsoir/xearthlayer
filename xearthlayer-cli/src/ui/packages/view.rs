//! Draws the package manager's state.
//!
//! ```text
//!  XEarthLayer Packages   [1 Local]  2 Library  3 Progress (2)
//!  XEarthLayer is running (pid N): ...        (only while run is active)
//! ┌ list ───────────────┐┌ Details ────────────┐
//! │                     ││                     │
//! └─────────────────────┘└─────────────────────┘
//!  status line
//!  keys of the tab
//! ```
//!
//! [`draw`] reads the state and the time and nothing else, so one state and
//! one `now` always draw the same frame.

use std::time::Instant;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    Frame,
};

use super::app::{AppState, Tab};
use super::widgets::{chrome, help, library, local, progress};

/// Smallest terminal the manager draws in.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 12;

/// Draw the whole manager: the tabs, then any dialog over them.
pub fn draw(frame: &mut Frame, state: &AppState, now: Instant) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        too_small(frame, area);
        return;
    }

    let banner = u16::from(state.run_active().is_some());
    let [title, banner_area, body, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    chrome::title_bar(frame, title, state);
    if let Some(pid) = state.run_active() {
        chrome::run_banner(frame, banner_area, pid);
    }
    match state.tab() {
        Tab::Local => {
            let (list, details) = chrome::split_body(body, 60);
            local::draw(frame, list, details, state, now);
        }
        Tab::Library => {
            let (list, details) = chrome::split_body(body, 60);
            library::draw(frame, list, details, state, now);
        }
        Tab::Progress => {
            let (list, details) = chrome::split_body(body, 60);
            progress::draw(frame, list, details, state, now);
        }
    }
    chrome::status_line(frame, status, state);
    chrome::footer(frame, footer, state.tab());

    if let Some(modal) = state.modal() {
        help::draw(frame, modal);
    }
}

/// The only thing drawn below the minimum size.
fn too_small(frame: &mut Frame, area: Rect) {
    let text =
        format!("Terminal too small for the package manager\n(needs {MIN_WIDTH}x{MIN_HEIGHT})");
    chrome::centered_text(frame, area, &text, Style::default().fg(Color::Yellow));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use chrono::DateTime;
    use ratatui::{backend::TestBackend, buffer::Buffer, style::Color, Terminal};
    use semver::Version;
    use xearthlayer::manager::{JobId, JobKind, JobSummary};
    use xearthlayer_package::{LibraryEntry, PackageType};
    use PackageType::{Ortho, Overlay};

    use super::*;
    use crate::commands::packages::actions::{LibraryRow, LibraryStatus};
    use crate::ui::packages::app::{Action, Input, JobMsg, LocalRow, Msg, Tab};

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    fn local(region: &str, t: PackageType, size: u64, update: Option<&str>) -> LocalRow {
        LocalRow {
            region: region.to_string(),
            package_type: t,
            version: v("1.0.0"),
            size_bytes: size,
            path: PathBuf::from(format!("/packages/{region}")),
            published_at: DateTime::from_timestamp(0, 0).unwrap(),
            update: update.map(v),
        }
    }

    fn lib(title: &str, t: PackageType, version: &str, status: LibraryStatus) -> LibraryRow {
        LibraryRow {
            entry: LibraryEntry::new("sum", t, title, v(version), "https://example.com/m"),
            status,
        }
    }

    fn fake_library() -> Vec<LibraryRow> {
        vec![
            lib("AF-NORTH", Ortho, "1.0.0", LibraryStatus::NotInstalled),
            lib("AF-NORTH", Overlay, "1.0.0", LibraryStatus::NotInstalled),
            lib(
                "EU-WEST",
                Ortho,
                "2.2.0",
                LibraryStatus::Update {
                    installed: v("2.1.0"),
                },
            ),
            lib("EU-WEST", Overlay, "1.0.0", LibraryStatus::UpToDate),
            lib("SA-SOUTH", Ortho, "1.0.0", LibraryStatus::NotInstalled),
        ]
    }

    /// Local: EU-WEST ortho (update to 1.1.0, 100 MB), NA-EAST overlay.
    /// Library as above. `run` not active.
    fn loaded() -> AppState {
        let (mut state, _) = AppState::new(true);
        let now = Instant::now();
        let rows = vec![
            local("EU-WEST", Ortho, 100 * 1024 * 1024, Some("1.1.0")),
            local("NA-EAST", Overlay, 20 * 1024 * 1024, None),
        ];
        state.update(Input::Msg(Msg::Installed(Ok(rows))), now);
        state.update(Input::Msg(Msg::Library(Ok(fake_library()))), now);
        state.update(Input::Msg(Msg::RunActive(None)), now);
        state
    }

    fn act(state: &mut AppState, action: Action) {
        state.update(Input::Action(action), Instant::now());
    }

    fn msg(state: &mut AppState, msg: Msg) {
        state.update(Input::Msg(msg), Instant::now());
    }

    fn queue(state: &mut AppState, id: u64, region: &str, t: PackageType) -> JobId {
        let id = JobId(id);
        let summary = JobSummary {
            kind: JobKind::Install,
            region: region.to_string(),
            package_type: t,
            version: Some(v("2.1.0")),
        };
        msg(state, Msg::Queued(id, summary));
        id
    }

    fn job(state: &mut AppState, id: JobId, m: JobMsg) {
        msg(state, Msg::Job(id, m));
    }

    fn render(state: &AppState, width: u16, height: u16) -> Buffer {
        render_at(state, width, height, Instant::now())
    }

    fn render_at(state: &AppState, width: u16, height: u16, now: Instant) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state, now)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// One line per terminal row, trailing blanks trimmed.
    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The text with line breaks and runs of spaces folded, for sentences
    /// that wrap.
    fn flat(buffer: &Buffer) -> String {
        buffer_text(buffer)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The colour of the first cell of `needle` on the row that contains it.
    fn fg_of(buffer: &Buffer, needle: &str) -> Color {
        for y in 0..buffer.area.height {
            let row: Vec<&str> = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            let line = row.concat();
            if let Some(at) = line.find(needle) {
                let column = line[..at].chars().count() as u16;
                return buffer[(column, y)].fg;
            }
        }
        panic!("{needle:?} not on screen:\n{}", buffer_text(buffer));
    }

    fn go(state: &mut AppState, tab: Tab) {
        act(state, Action::GoTab(tab));
    }

    // ------------------------------------------------------------------------
    // Chrome
    // ------------------------------------------------------------------------

    #[test]
    fn the_title_bar_brackets_the_active_tab() {
        let mut state = loaded();
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("XEarthLayer Packages"), "{text}");
        assert!(text.contains("[1 Local]  2 Library  3 Progress"), "{text}");
        go(&mut state, Tab::Library);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("1 Local  [2 Library]  3 Progress"), "{text}");
    }

    #[test]
    fn the_progress_count_shows_only_while_jobs_are_unfinished() {
        let mut state = loaded();
        assert!(!buffer_text(&render(&state, 100, 30)).contains("Progress ("));
        let a = queue(&mut state, 1, "EU-WEST", Ortho);
        queue(&mut state, 2, "EU-WEST", Overlay);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("3 Progress (2)"), "{text}");
        job(&mut state, a, JobMsg::Installed);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("3 Progress (1)"), "{text}");
    }

    #[test]
    fn the_banner_shows_while_run_is_active() {
        let mut state = loaded();
        assert!(!buffer_text(&render(&state, 100, 30)).contains("is running"));
        msg(&mut state, Msg::RunActive(Some(4242)));
        let buffer = render(&state, 100, 30);
        let text = buffer_text(&buffer);
        assert!(
            text.contains(
                "XEarthLayer is running (pid 4242): installs allowed, updates and deletes disabled"
            ),
            "{text}"
        );
        assert_eq!(fg_of(&buffer, "XEarthLayer is running"), Color::Yellow);
    }

    #[test]
    fn a_narrow_terminal_shortens_the_banner_instead_of_cutting_it() {
        let mut state = loaded();
        msg(&mut state, Msg::RunActive(Some(1234567)));
        let text = buffer_text(&render(&state, 80, 24));
        let banner = text.lines().nth(1).unwrap();
        assert!(banner.contains("(pid 1234567)"), "{banner}");
        assert!(banner.contains("updates and deletes disabled"), "{banner}");
        assert!(!banner.contains("installs allowed"), "{banner}");
    }

    #[test]
    fn each_tab_shows_its_footer_keys() {
        let mut state = loaded();
        let local = buffer_text(&render(&state, 100, 30));
        for keys in [
            "Space mark",
            "u update",
            "d delete",
            "r refresh",
            "? help",
            "q quit",
        ] {
            assert!(local.contains(keys), "{keys}\n{local}");
        }
        go(&mut state, Tab::Library);
        let library = buffer_text(&render(&state, 100, 30));
        for keys in [
            "Space mark",
            "a all",
            "t type",
            "i install",
            "r refresh",
            "q quit",
        ] {
            assert!(library.contains(keys), "{keys}\n{library}");
        }
        go(&mut state, Tab::Progress);
        let progress = buffer_text(&render(&state, 100, 30));
        for keys in [
            "c cancel",
            "C cancel all",
            "x clear done",
            "? help",
            "q quit",
        ] {
            assert!(progress.contains(keys), "{keys}\n{progress}");
        }
    }

    #[test]
    fn the_status_line_sits_above_the_footer() {
        let mut state = loaded();
        msg(
            &mut state,
            Msg::Status("3 jobs queued: see Progress".into()),
        );
        let text = buffer_text(&render(&state, 100, 30));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[28].contains("3 jobs queued: see Progress"), "{text}");
        assert!(lines[29].contains("q quit"), "{text}");
    }

    #[test]
    fn a_terminal_below_60_by_12_shows_only_the_too_small_message() {
        let state = loaded();
        let buffer = render(&state, 50, 10);
        assert!(
            flat(&buffer).contains("Terminal too small for the package manager (needs 60x12)"),
            "{}",
            buffer_text(&buffer)
        );
        assert!(!buffer_text(&buffer).contains("XEarthLayer Packages"));
        // Exactly the minimum draws the manager.
        let text = buffer_text(&render(&state, 60, 12));
        assert!(text.contains("XEarthLayer Packages"), "{text}");
        let text = buffer_text(&render(&state, 60, 11));
        assert!(text.contains("Terminal too small"), "{text}");
    }

    // ------------------------------------------------------------------------
    // Local
    // ------------------------------------------------------------------------

    #[test]
    fn local_rows_show_their_columns_and_the_update_mark() {
        let state = loaded();
        let text = buffer_text(&render(&state, 100, 30));
        let row = text.lines().find(|l| l.contains("EU-WEST")).unwrap();
        assert!(row.contains("[ ] EU-WEST"), "{row}");
        assert!(row.contains("ortho"), "{row}");
        assert!(row.contains("1.0.0"), "{row}");
        assert!(row.contains("100 MB"), "{row}");
        assert!(row.contains('↑'), "{row}");
        let row = text.lines().find(|l| l.contains("NA-EAST")).unwrap();
        assert!(row.contains("overlay") && !row.contains('↑'), "{row}");
    }

    #[test]
    fn marked_local_rows_show_x() {
        let mut state = loaded();
        act(&mut state, Action::Mark);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("[x] EU-WEST"), "{text}");
        assert!(text.contains("[ ] NA-EAST"), "{text}");
    }

    #[test]
    fn local_details_follow_the_selection() {
        let mut state = loaded();
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Type: ortho"), "{text}");
        assert!(text.contains("Version: 1.0.0 (1.1.0 available)"), "{text}");
        assert!(text.contains("Size: 100 MB"), "{text}");
        assert!(text.contains("Location: /packages/EU-WEST"), "{text}");
        assert!(text.contains("Published: 1970-01-01"), "{text}");
        act(&mut state, Action::Down);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Type: overlay"), "{text}");
        assert!(text.contains("Version: 1.0.0"), "{text}");
        assert!(!text.contains("available"), "{text}");
    }

    #[test]
    fn an_empty_local_list_says_so_without_failing() {
        let (mut state, _) = AppState::new(true);
        msg(&mut state, Msg::Installed(Ok(Vec::new())));
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Nothing installed"), "{text}");
    }

    // ------------------------------------------------------------------------
    // Library
    // ------------------------------------------------------------------------

    #[test]
    fn library_rows_show_marks_versions_and_status() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        act(&mut state, Action::Mark);
        let text = buffer_text(&render(&state, 100, 30));
        let row = text
            .lines()
            .find(|l| l.contains("AF-NORTH") && l.contains("ortho"))
            .unwrap();
        assert!(row.contains("[x] AF-NORTH"), "{row}");
        let row = text
            .lines()
            .find(|l| l.contains("EU-WEST") && l.contains("ortho"))
            .unwrap();
        assert!(row.contains("2.2.0"), "{row}");
        assert!(row.contains("update 2.1.0 → 2.2.0"), "{row}");
        let row = text
            .lines()
            .find(|l| l.contains("EU-WEST") && l.contains("overlay"))
            .unwrap();
        assert!(row.contains("installed"), "{row}");
    }

    #[test]
    fn a_narrow_terminal_shortens_the_update_status() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        let text = buffer_text(&render(&state, 80, 24));
        let row = text
            .lines()
            .find(|l| l.contains("EU-WEST") && l.contains("ortho"))
            .unwrap();
        assert!(row.contains("update ↑"), "{row}");
    }

    #[test]
    fn library_details_show_versions_and_the_overlay() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Library version: 1.0.0"), "{text}");
        assert!(text.contains("Installed version: not installed"), "{text}");
        assert!(text.contains("Overlay follows: yes"), "{text}");
        act(&mut state, Action::End); // SA-SOUTH ortho, no overlay offered
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Overlay follows: no"), "{text}");
        act(&mut state, Action::Up);
        act(&mut state, Action::Up); // EU-WEST ortho, update
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Library version: 2.2.0"), "{text}");
        assert!(text.contains("Installed version: 2.1.0"), "{text}");
    }

    #[test]
    fn the_type_filter_is_named_in_the_title() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        act(&mut state, Action::CycleFilter);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Library: ortho (3)"), "{text}");
        assert!(!text.contains("AF-NORTH overlay"), "{text}");
    }

    #[test]
    fn library_states_other_than_loaded_are_centred_text() {
        let (mut state, _) = AppState::new(true);
        go(&mut state, Tab::Library);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Loading the library"), "{text}");

        msg(&mut state, Msg::Library(Err("connection refused".into())));
        let text = buffer_text(&render(&state, 100, 30));
        assert!(
            text.contains("Could not load the library: connection refused"),
            "{text}"
        );
        assert!(text.contains("Press r to retry"), "{text}");

        msg(&mut state, Msg::LibraryNotConfigured);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("No package library is configured"), "{text}");
        assert!(text.contains("packages.library_url"), "{text}");
    }

    #[test]
    fn a_library_selection_past_the_rows_does_not_panic() {
        // The selection is not clamped while the library is away.
        let mut state = loaded();
        go(&mut state, Tab::Library);
        act(&mut state, Action::End);
        msg(&mut state, Msg::Library(Err("gone".into())));
        render(&state, 100, 30);
        // Back, with fewer rows than the stale selection.
        msg(
            &mut state,
            Msg::Library(Ok(vec![lib(
                "ONLY",
                Ortho,
                "1.0.0",
                LibraryStatus::NotInstalled,
            )])),
        );
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("ONLY"), "{text}");
        // A loaded but empty library.
        msg(&mut state, Msg::Library(Ok(Vec::new())));
        render(&state, 100, 30);
    }

    // ------------------------------------------------------------------------
    // Progress
    // ------------------------------------------------------------------------

    fn progress_state() -> AppState {
        let mut state = loaded();
        go(&mut state, Tab::Progress);
        state
    }

    #[test]
    fn an_empty_progress_tab_points_at_the_library() {
        let state = progress_state();
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("No jobs yet"), "{text}");
    }

    #[test]
    fn a_downloading_row_has_an_arrow_a_gauge_and_bytes() {
        let mut state = progress_state();
        let id = queue(&mut state, 1, "NA-CANADA", Ortho);
        job(
            &mut state,
            id,
            JobMsg::Downloading {
                done: 512 * 1024 * 1024,
                total: Some(1024 * 1024 * 1024),
            },
        );
        let text = buffer_text(&render(&state, 100, 30));
        let row = text
            .lines()
            .find(|l| l.contains("NA-CANADA ortho v2.1.0"))
            .unwrap();
        assert!(row.contains('↓'), "{row}");
        assert!(row.contains("█████░░░░░"), "{row}");
        assert!(row.contains("512 MB / 1 GB"), "{row}");
    }

    #[test]
    fn a_downloaded_row_is_light_blue_and_a_done_row_is_green() {
        let mut state = progress_state();
        let a = queue(&mut state, 1, "AAA", Ortho);
        let b = queue(&mut state, 2, "BBB", Ortho);
        let c = queue(&mut state, 3, "CCC", Ortho);
        job(&mut state, a, JobMsg::Downloaded);
        job(&mut state, b, JobMsg::Installed);
        job(&mut state, c, JobMsg::Failed("disk full".into()));
        // The selection is on the first row; the colours are on the others.
        act(&mut state, Action::Down);
        act(&mut state, Action::Down);
        let buffer = render(&state, 100, 30);
        let text = buffer_text(&buffer);
        assert!(text.contains("downloaded, waiting to install"), "{text}");
        assert!(text.contains('✓') && text.contains('✗'), "{text}");
        assert_eq!(fg_of(&buffer, "AAA ortho"), Color::LightBlue);
        assert_eq!(fg_of(&buffer, "✓"), Color::Green);
        assert_eq!(fg_of(&buffer, "✗"), Color::Red);
    }

    #[test]
    fn a_working_row_shows_the_spinner_frame_for_now() {
        let mut state = progress_state();
        let id = queue(&mut state, 1, "NA-CANADA", Ortho);
        job(&mut state, id, JobMsg::Stage("Extracting"));
        let start = state.started();
        let at = |ms: u64| {
            buffer_text(&render_at(
                &state,
                100,
                30,
                start + Duration::from_millis(ms),
            ))
        };
        let first = at(0);
        let row = first
            .lines()
            .find(|l| l.contains("NA-CANADA ortho"))
            .unwrap();
        assert!(row.contains('⠋') && row.contains("extracting"), "{row}");
        assert!(at(350).contains('⠸'));
        assert!(at(1000).contains('⠋'), "the frames wrap");
        assert_eq!(at(350), at(399), "a frame lasts 100 ms");
    }

    #[test]
    fn cancelled_and_queued_rows_have_their_marks() {
        let mut state = progress_state();
        let a = queue(&mut state, 1, "AAA", Ortho);
        queue(&mut state, 2, "BBB", Ortho);
        job(&mut state, a, JobMsg::Cancelled);
        let text = buffer_text(&render(&state, 100, 30));
        let row = text.lines().find(|l| l.contains("AAA ortho")).unwrap();
        assert!(
            row.contains("- AAA ortho") && row.contains("cancelled"),
            "{row}"
        );
        let row = text.lines().find(|l| l.contains("BBB ortho")).unwrap();
        assert!(row.contains("queued"), "{row}");
    }

    #[test]
    fn a_waiting_row_gives_both_sizes() {
        let mut state = progress_state();
        let id = queue(&mut state, 1, "AAA", Ortho);
        let gb = 1024 * 1024 * 1024;
        job(
            &mut state,
            id,
            JobMsg::Waiting {
                needed: 3 * gb,
                available: 2 * gb,
            },
        );
        // Wide enough for the row to carry the whole sentence.
        let wide = flat(&render(&state, 160, 30));
        assert!(
            wide.contains("waiting for disk space (needs 3 GB, 2 GB free)"),
            "{wide}"
        );
        // Narrower, the row is short and the details pane has the numbers.
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("waiting for disk space"), "{text}");
        assert!(text.contains("Needs: 3 GB"), "{text}");
        assert!(text.contains("Free: 2 GB"), "{text}");
    }

    #[test]
    fn the_details_pane_shows_the_full_error() {
        let mut state = progress_state();
        let id = queue(&mut state, 1, "AAA", Ortho);
        job(
            &mut state,
            id,
            JobMsg::Failed("checksum mismatch in part 3".into()),
        );
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("State: failed"), "{text}");
        assert!(
            text.contains("Error: checksum mismatch in part 3"),
            "{text}"
        );
    }

    #[test]
    fn download_details_show_the_percentage() {
        let mut state = progress_state();
        let id = queue(&mut state, 1, "AAA", Ortho);
        job(
            &mut state,
            id,
            JobMsg::Downloading {
                done: 1024 * 1024,
                total: Some(4 * 1024 * 1024),
            },
        );
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("State: downloading"), "{text}");
        assert!(text.contains("1 MB of 4 MB (25%)"), "{text}");
    }

    // ------------------------------------------------------------------------
    // Modals
    // ------------------------------------------------------------------------

    #[test]
    fn help_lists_every_key_and_how_it_works() {
        let mut state = loaded();
        act(&mut state, Action::Help);
        let text = flat(&render(&state, 100, 30));
        for keys in [
            "1 2 3",
            "Tab",
            "Shift+Tab",
            "Up Down j k PgUp PgDn Home End",
            "r refresh",
            "? help",
            "q, Esc, Ctrl+C quit",
            "Space mark",
            "a mark all",
            "u update",
            "d delete",
            "t type filter",
            "i install",
            "c cancel",
            "C cancel all",
            "x clear finished",
            "How it works",
        ] {
            assert!(text.contains(keys), "{keys}\n{text}");
        }
        for line in [
            "Installs and removals queue in the background",
            "Queuing never switches tabs",
            "While xearthlayer run is active",
            "Overlays follow their ortho",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn help_fits_an_80_by_24_terminal() {
        let mut state = loaded();
        act(&mut state, Action::Help);
        let text = buffer_text(&render(&state, 80, 24));
        assert!(text.contains("How it works"), "{text}");
        assert!(text.contains("Overlays follow their ortho"), "{text}");
        assert!(text.contains("? or Esc closes"), "{text}");
    }

    #[test]
    fn the_quit_prompt_counts_the_running_jobs() {
        let mut state = loaded();
        queue(&mut state, 1, "AAA", Ortho);
        queue(&mut state, 2, "BBB", Ortho);
        act(&mut state, Action::Quit);
        let text = flat(&render(&state, 100, 30));
        assert!(
            text.contains("2 jobs running. Quit and cancel them?"),
            "{text}"
        );
        assert!(text.contains("y/q quit"), "{text}");
        assert!(text.contains("n or Esc cancel"), "{text}");
    }

    #[test]
    fn the_delete_prompt_shows_the_total_size() {
        let mut state = loaded();
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::Delete);
        let text = flat(&render(&state, 100, 30));
        assert!(text.contains("Delete 2 packages (120 MB)?"), "{text}");
        assert!(text.contains("EU-WEST ortho"), "{text}");
        assert!(text.contains("NA-EAST overlay"), "{text}");
    }

    #[test]
    fn the_install_prompt_lists_overlays_and_what_was_left_out() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        // AF-NORTH ortho (+ overlay) and EU-WEST overlay (current).
        act(&mut state, Action::Mark);
        act(&mut state, Action::Down);
        act(&mut state, Action::Down);
        act(&mut state, Action::Down);
        act(&mut state, Action::Mark);
        act(&mut state, Action::Install);
        let text = flat(&render(&state, 100, 30));
        assert!(text.contains("Install 2 packages?"), "{text}");
        assert!(text.contains("AF-NORTH ortho v1.0.0"), "{text}");
        assert!(text.contains("AF-NORTH overlay v1.0.0"), "{text}");
        assert!(
            text.contains("Left out, already up to date: EU-WEST overlay"),
            "{text}"
        );
    }

    #[test]
    fn the_cancel_all_prompt_is_drawn() {
        let mut state = progress_state();
        queue(&mut state, 1, "AAA", Ortho);
        act(&mut state, Action::CancelAll);
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("Cancel every unfinished job?"), "{text}");
    }

    // ------------------------------------------------------------------------
    // 80x24 and long lists
    // ------------------------------------------------------------------------

    #[test]
    fn every_tab_draws_at_80_by_24() {
        let mut state = loaded();
        let id = queue(&mut state, 1, "NA-CANADA-GREENLAND", Ortho);
        job(
            &mut state,
            id,
            JobMsg::Downloading {
                done: 1,
                total: Some(100),
            },
        );
        for tab in [Tab::Local, Tab::Library, Tab::Progress] {
            go(&mut state, tab);
            let text = buffer_text(&render(&state, 80, 24));
            assert!(text.contains("XEarthLayer Packages"), "{text}");
            assert!(text.contains("q quit"), "{text}");
        }
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_in_view() {
        let (mut state, _) = AppState::new(true);
        let rows: Vec<LocalRow> = (0..60)
            .map(|i| local(&format!("REGION-{i:02}"), Ortho, 1024, None))
            .collect();
        msg(&mut state, Msg::Installed(Ok(rows)));
        let text = buffer_text(&render(&state, 100, 20));
        assert!(
            text.contains("REGION-00") && !text.contains("REGION-59"),
            "{text}"
        );
        act(&mut state, Action::End);
        let text = buffer_text(&render(&state, 100, 20));
        assert!(
            text.contains("REGION-59") && !text.contains("REGION-00"),
            "{text}"
        );
    }

    // ------------------------------------------------------------------------
    // Review fixes
    // ------------------------------------------------------------------------

    #[test]
    fn the_update_mark_is_visible_at_80_by_24_and_60_by_12() {
        let mut state = loaded();
        // A long region name, the case that pushed the mark out.
        let rows = vec![
            local(
                "NA-CANADA-GREENLAND",
                Ortho,
                100 * 1024 * 1024,
                Some("1.1.0"),
            ),
            local("EU-WEST", Overlay, 20 * 1024 * 1024, None),
        ];
        msg(&mut state, Msg::Installed(Ok(rows)));
        for (w, h) in [(80, 24), (60, 12), (100, 30)] {
            let text = buffer_text(&render(&state, w, h));
            let row = text.lines().find(|l| l.contains("NA-CAN")).unwrap();
            assert!(row.contains('\u{2191}'), "{w}x{h}: {row}");
            assert!(row.contains("100 MB"), "{w}x{h}: {row}");
        }
    }

    #[test]
    fn local_footer_lists_mark_all() {
        let state = loaded();
        let text = buffer_text(&render(&state, 100, 30));
        assert!(text.contains("a all"), "{text}");
    }

    fn many_library_rows(n: usize) -> Vec<LibraryRow> {
        (0..n)
            .map(|i| {
                lib(
                    &format!("REGION-{i:02}"),
                    Ortho,
                    "1.0.0",
                    LibraryStatus::NotInstalled,
                )
            })
            .collect()
    }

    #[test]
    fn a_long_install_confirmation_keeps_its_question_and_hint() {
        for (w, h) in [(80, 24), (60, 12)] {
            let (mut state, _) = AppState::new(false);
            msg(&mut state, Msg::Library(Ok(many_library_rows(40))));
            go(&mut state, Tab::Library);
            act(&mut state, Action::MarkAll);
            act(&mut state, Action::Install);
            let text = buffer_text(&render(&state, w, h));
            assert!(text.contains("Install 40 packages?"), "{w}x{h}\n{text}");
            assert!(text.contains("... and "), "{w}x{h}\n{text}");
            assert!(text.contains("more"), "{w}x{h}\n{text}");
            assert!(text.contains("y confirm"), "{w}x{h}\n{text}");
            assert!(text.contains("n or Esc cancel"), "{w}x{h}\n{text}");
        }
    }

    #[test]
    fn a_long_delete_confirmation_keeps_its_question_and_hint() {
        for (w, h) in [(80, 24), (60, 12)] {
            let (mut state, _) = AppState::new(false);
            let rows: Vec<LocalRow> = (0..40)
                .map(|i| local(&format!("REGION-{i:02}"), Ortho, 1024 * 1024, None))
                .collect();
            msg(&mut state, Msg::Installed(Ok(rows)));
            act(&mut state, Action::MarkAll);
            act(&mut state, Action::Delete);
            let text = buffer_text(&render(&state, w, h));
            assert!(
                text.contains("Delete 40 packages (40 MB)?"),
                "{w}x{h}\n{text}"
            );
            assert!(text.contains("more"), "{w}x{h}\n{text}");
            assert!(text.contains("y confirm"), "{w}x{h}\n{text}");
            assert!(text.contains("n or Esc cancel"), "{w}x{h}\n{text}");
        }
    }

    #[test]
    fn a_short_confirmation_lists_every_target_with_no_more_line() {
        let mut state = loaded();
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::Delete);
        let text = buffer_text(&render(&state, 80, 24));
        assert!(
            text.contains("EU-WEST ortho") && text.contains("NA-EAST overlay"),
            "{text}"
        );
        assert!(!text.contains("more"), "{text}");
    }

    #[test]
    fn a_long_left_out_note_is_cut_but_the_hint_stays() {
        let (mut state, _) = AppState::new(false);
        let mut rows = many_library_rows(2);
        rows.extend((0..30).map(|i| {
            lib(
                &format!("CURRENT-REGION-{i:02}"),
                Overlay,
                "1.0.0",
                LibraryStatus::UpToDate,
            )
        }));
        msg(&mut state, Msg::Library(Ok(rows)));
        go(&mut state, Tab::Library);
        act(&mut state, Action::MarkAll);
        act(&mut state, Action::Install);
        for (w, h) in [(80, 24), (60, 12)] {
            let text = buffer_text(&render(&state, w, h));
            assert!(
                text.contains("Left out, already up to date"),
                "{w}x{h}\n{text}"
            );
            assert!(text.contains("y confirm"), "{w}x{h}\n{text}");
            assert!(text.contains("n or Esc cancel"), "{w}x{h}\n{text}");
        }
    }

    #[test]
    fn a_long_library_error_wraps_instead_of_being_cut() {
        let (mut state, _) = AppState::new(true);
        go(&mut state, Tab::Library);
        let error = "connection refused while fetching https://example.com/library/index.txt \
                     after three attempts with the final one timing out waiting for the server";
        msg(&mut state, Msg::Library(Err(error.into())));
        let text = flat(&render(&state, 80, 24));
        assert!(text.contains(error), "{text}");
        assert!(text.contains("Press r to retry"), "{text}");
    }

    #[test]
    fn help_says_which_rows_u_d_and_i_act_on() {
        let mut state = loaded();
        act(&mut state, Action::Help);
        let text = flat(&render(&state, 80, 24));
        assert!(
            text.contains("u, d and i act on the marked rows, else the selected row"),
            "{text}"
        );
        assert!(
            text.contains("Space mark a mark all u update d delete"),
            "{text}"
        );
        assert!(
            text.contains("Space mark a mark all t type filter i install"),
            "{text}"
        );
        assert!(text.contains("? or Esc closes"), "{text}");
    }

    // ------------------------------------------------------------------------
    // Reads in progress, marks and the continent
    // ------------------------------------------------------------------------

    /// The lines of the details pane, from its title down, as a 100x30
    /// render draws them.
    fn detail_rows(state: &AppState) -> Vec<String> {
        let text = buffer_text(&render(state, 100, 30));
        let start = 60;
        text.lines()
            .skip_while(|line| !line.contains("Details"))
            .map(|line| line.chars().skip(start).collect::<String>())
            .collect()
    }

    #[test]
    fn details_name_the_continent_before_the_type() {
        let mut state = loaded();
        go(&mut state, Tab::Library); // AF-NORTH ortho
        let rows = detail_rows(&state);
        assert!(rows[1].contains("AF-NORTH"), "{rows:#?}");
        assert!(rows[3].contains("Continent: Africa"), "{rows:#?}");
        assert!(rows[4].contains("Type: ortho"), "{rows:#?}");

        go(&mut state, Tab::Local); // EU-WEST ortho
        let rows = detail_rows(&state);
        assert!(rows[3].contains("Continent: Europe"), "{rows:#?}");
        assert!(rows[4].contains("Type: ortho"), "{rows:#?}");
    }

    #[test]
    fn details_of_an_unknown_prefix_name_no_continent() {
        let (mut state, _) = AppState::new(true);
        msg(
            &mut state,
            Msg::Installed(Ok(vec![local("XX-NOWHERE", Ortho, 1024, None)])),
        );
        let rows = detail_rows(&state);
        assert!(rows[1].contains("XX-NOWHERE"), "{rows:#?}");
        assert!(rows[3].contains("Type: ortho"), "{rows:#?}");
        assert!(!rows.concat().contains("Continent"), "{rows:#?}");
    }

    /// The non-blank cells of the list pane's row that contains `needle`.
    fn row_cells(buffer: &Buffer, needle: &str) -> Vec<(Color, Color)> {
        for y in 0..buffer.area.height {
            let line: String = (1..60).map(|x| buffer[(x, y)].symbol()).collect();
            if line.contains(needle) {
                return (1..59)
                    .map(|x| &buffer[(x, y)])
                    .filter(|cell| cell.symbol() != " ")
                    .map(|cell| (cell.fg, cell.bg))
                    .collect();
            }
        }
        panic!("{needle:?} not on screen:\n{}", buffer_text(buffer));
    }

    #[test]
    fn a_marked_row_is_cyan_and_keeps_the_cursor_background() {
        let mut state = loaded();
        act(&mut state, Action::Mark); // EU-WEST, under the cursor
        let buffer = render(&state, 100, 30);
        let marked = row_cells(&buffer, "EU-WEST");
        assert!(
            marked
                .iter()
                .all(|&(fg, bg)| fg == Color::Cyan && bg == Color::DarkGray),
            "{marked:?}"
        );
        let unmarked = row_cells(&buffer, "NA-EAST");
        assert!(
            unmarked.iter().all(|&(fg, _)| fg != Color::Cyan),
            "{unmarked:?}"
        );

        // Marked away from the cursor: cyan alone.
        act(&mut state, Action::Down);
        let buffer = render(&state, 100, 30);
        let marked = row_cells(&buffer, "EU-WEST");
        assert!(
            marked
                .iter()
                .all(|&(fg, bg)| fg == Color::Cyan && bg != Color::DarkGray),
            "{marked:?}"
        );
    }

    #[test]
    fn a_marked_library_row_is_cyan_status_included() {
        let mut state = loaded();
        go(&mut state, Tab::Library);
        act(&mut state, Action::Down);
        act(&mut state, Action::Down); // EU-WEST ortho, an update
        act(&mut state, Action::Mark);
        act(&mut state, Action::Down);
        let buffer = render(&state, 100, 30);
        let marked = row_cells(&buffer, "update 2.1.0");
        assert!(
            marked.iter().all(|&(fg, _)| fg == Color::Cyan),
            "{marked:?}"
        );
        // EU-WEST overlay, under the cursor: its status keeps its green.
        let unmarked = row_cells(&buffer, "installed");
        assert!(
            unmarked.iter().all(|&(fg, _)| fg != Color::Cyan),
            "{unmarked:?}"
        );
        assert_eq!(fg_of(&buffer, "installed"), Color::Green);
    }

    #[test]
    fn the_first_load_shows_a_spinner_and_what_is_read() {
        let (mut state, _) = AppState::new(true);
        let start = state.started();
        for (w, h) in [(80, 24), (60, 12)] {
            let text = buffer_text(&render_at(&state, w, h, start));
            assert!(
                text.contains("⠋ Reading the installed packages…"),
                "{w}x{h}:\n{text}"
            );
            assert!(!text.contains("Nothing installed"), "{w}x{h}:\n{text}");
        }
        let later = buffer_text(&render_at(
            &state,
            80,
            24,
            start + Duration::from_millis(350),
        ));
        assert!(later.contains("⠸ Reading"), "{later}");

        msg(&mut state, Msg::Installed(Ok(Vec::new())));
        let text = buffer_text(&render_at(&state, 80, 24, start));
        assert!(!text.contains("Reading"), "{text}");
        assert!(text.contains("Nothing installed"), "{text}");
    }

    /// The loaded state with a job just finished, so both lists reload.
    fn reloading() -> AppState {
        let mut state = loaded();
        let id = queue(&mut state, 1, "NA-CANADA", Ortho);
        job(&mut state, id, JobMsg::Installed);
        state
    }

    #[test]
    fn a_reload_keeps_the_list_and_spins_in_the_title() {
        let mut state = reloading();
        let start = state.started();
        let text = buffer_text(&render_at(&state, 80, 24, start));
        assert!(text.contains("Installed (2) ⠋ reading"), "{text}");
        assert!(text.contains("EU-WEST"), "{text}");

        msg(
            &mut state,
            Msg::Installed(Ok(vec![local("EU-WEST", Ortho, 1, None)])),
        );
        let text = buffer_text(&render_at(&state, 80, 24, start));
        assert!(text.contains("Installed (1)"), "{text}");
        assert!(!text.contains("reading"), "{text}");
    }

    #[test]
    fn a_library_reload_while_loaded_spins_in_the_title() {
        let mut state = reloading();
        go(&mut state, Tab::Library);
        let start = state.started();
        let text = buffer_text(&render_at(&state, 80, 24, start));
        assert!(text.contains("Library: all (5) ⠋ reading"), "{text}");

        msg(&mut state, Msg::Library(Ok(fake_library())));
        let text = buffer_text(&render_at(&state, 80, 24, start));
        assert!(text.contains("Library: all (5)"), "{text}");
        assert!(!text.contains("reading"), "{text}");
    }
}
