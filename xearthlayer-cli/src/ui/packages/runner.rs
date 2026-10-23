//! Runs the package manager in the terminal.
//!
//! Each turn of the loop: stop at once if the terminal is gone, act on a
//! signal, draw, wait up to 50 ms for keys, hand the queue's and the worker's
//! messages to the state, and carry out the effects that came back. The
//! terminal is restored on every way out, a panic included.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::panic::PanicHookInfo;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use xearthlayer::config::ConfigFile;
use xearthlayer::manager::{
    InstallQueue, LocalPackageStore, ManagerResult, PackageInstaller, Stages,
};

use super::app::{Action, AppState, Effect, Input, Msg};
use super::effects::{ChannelObserver, Effects, Settings};
use super::input::action_for;
use super::view;
use crate::commands::packages::{
    default_custom_scenery_dir, default_install_dir, default_temp_dir, DefaultPackageManagerService,
};
use crate::error::CliError;
use crate::signals::{terminal_gone, ActiveQueue, Signal, SignalWatch};

/// How long each turn waits for a key. Every turn draws, so this is also
/// the longest gap between frames: short enough for the spinners' 100 ms
/// frames whenever a read or a job is in flight.
const KEY_POLL: Duration = Duration::from_millis(50);

/// How often the state hears that time passed.
const TICK: Duration = Duration::from_secs(1);

/// Run the package manager until it exits, and return its one-line summary.
/// A signal that ended it gives `CliError::Interrupted`.
pub fn run_manager(config: &ConfigFile) -> Result<String, CliError> {
    let (tx, rx) = mpsc::channel();
    let active = ActiveQueue::default();
    // Every signal is handled, so the terminal is always restored. The state
    // shows its own cancelling note: a line on stderr would land mid-screen.
    let watch = SignalWatch::start_handling_all(active.clone(), |_| {})
        .map_err(|e| CliError::Packages(format!("could not install signal handlers: {e}")))?;

    let install_dir = default_install_dir(config);
    let temp_dir = default_temp_dir(config);
    let concurrent_downloads = config.packages.concurrent_downloads;
    let settings = Settings {
        install_dir: install_dir.clone(),
        library_url: config.packages.library_url.clone(),
        custom_scenery_path: default_custom_scenery_dir(config),
        disable_overlays: config.packages.disable_overlays,
        xworld_pro: config.packages.xworld_pro,
    };
    let observer_tx = tx.clone();
    let start_queue = move || {
        let installer = PackageInstaller::new(LocalPackageStore::new(&install_dir), &temp_dir)
            .with_parallel_downloads(concurrent_downloads);
        InstallQueue::start(
            installer,
            Arc::new(ChannelObserver::new(observer_tx.clone())),
        )
    };
    let mut effects = Effects::new(
        start_queue,
        Arc::new(DefaultPackageManagerService::new()),
        settings,
        active,
        tx,
    );
    let (mut state, initial) = AppState::new(config.packages.auto_install_overlays);

    let mut screen = Screen::enter()
        .map_err(|e| CliError::Packages(format!("could not open the terminal: {e}")))?;
    let result = run_loop(&mut screen, &mut state, &mut effects, &rx, &watch, initial);
    if result.is_err() {
        // Nothing can be shown any more: stop the jobs rather than wait.
        let _ = effects.apply(Effect::Abandon);
    }
    // The terminal comes back first; dropping the effects may wait for an
    // overlay rebuild or a swap.
    drop(screen);
    if let Some(note) = finishing_note(effects.rebuild_pending(), terminal_gone()) {
        eprintln!("{note}");
    }
    drop(effects);

    if let Some(signal) = watch.received() {
        if summary_after(signal, terminal_gone()) {
            eprintln!("{}", state.summary());
        }
        return Err(CliError::Interrupted(signal));
    }
    result.map_err(|e| CliError::Packages(format!("package manager terminal error: {e}")))?;
    Ok(state.summary())
}

fn run_loop<S, F>(
    screen: &mut Screen,
    state: &mut AppState,
    effects: &mut Effects<S, F>,
    rx: &Receiver<Msg>,
    watch: &SignalWatch,
    initial: Vec<Effect>,
) -> io::Result<()>
where
    S: Stages,
    F: Fn() -> ManagerResult<InstallQueue<S>>,
{
    let mut todo: VecDeque<Effect> = initial.into();
    // Signals acted on so far.
    let mut heard: u64 = 0;
    let mut last_tick = Instant::now();
    loop {
        while let Some(effect) = todo.pop_front() {
            if effects.apply(effect).is_break() {
                return Ok(());
            }
        }
        if terminal_gone() {
            let _ = effects.apply(Effect::Abandon);
            return Ok(());
        }
        if let Some((nth, signal)) = watch.latest().filter(|(nth, _)| *nth > heard) {
            heard = nth;
            todo.extend(on_signal(state, signal, nth, Instant::now()));
            continue;
        }

        screen.draw(state)?;

        if event::poll(KEY_POLL)? {
            loop {
                if let Event::Key(key) = event::read()? {
                    if let Some(action) = action_for(key, state.tab(), state.modal()) {
                        todo.extend(state.update(Input::Action(action), Instant::now()));
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        for msg in rx.try_iter() {
            effects.observe(&msg);
            todo.extend(state.update(Input::Msg(msg), Instant::now()));
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            todo.extend(state.update(Input::Msg(Msg::Tick), last_tick));
        }
    }
}

/// What the `nth` signal from outside does, counting from 1. In raw mode
/// Ctrl+C is a key, so an interrupt was sent by something else. The first
/// one quits as a confirmed `q` does, cancelling any jobs first; any signal
/// after it, and SIGTERM or SIGHUP at any time, leave at once.
fn on_signal(state: &mut AppState, signal: Signal, nth: u64, now: Instant) -> Vec<Effect> {
    match signal {
        Signal::Interrupt if nth == 1 => {
            let mut effects = state.update(Input::Action(Action::Quit), now);
            effects.extend(state.update(Input::Action(Action::Confirm), now));
            effects
        }
        Signal::Interrupt | Signal::Terminate | Signal::Hangup => {
            vec![Effect::Abandon, Effect::Exit]
        }
    }
}

/// What is said while dropping the effects finishes an overlay rebuild: the
/// shell prompt does not come back meanwhile. Nothing once SIGHUP has taken
/// the terminal.
fn finishing_note(rebuild_pending: bool, terminal_gone: bool) -> Option<&'static str> {
    (rebuild_pending && !terminal_gone).then_some("Finishing the overlay folder…")
}

/// Whether the summary is printed, to stderr, after `signal` ended the
/// manager: after any signal but SIGHUP, which took the terminal with it.
fn summary_after(signal: Signal, terminal_gone: bool) -> bool {
    signal != Signal::Hangup && !terminal_gone
}

// ============================================================================
// The terminal
// ============================================================================

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Raw mode and the alternate screen, left on drop and on a panic.
struct Screen {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    /// The hook in place before ours, put back on drop.
    previous_hook: Arc<PanicHook>,
}

impl Screen {
    fn enter() -> io::Result<Self> {
        let previous_hook: Arc<PanicHook> = Arc::new(std::panic::take_hook());
        let chained = Arc::clone(&previous_hook);
        let runner = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            // Only a panic on this thread ends the manager. The queue catches
            // a panicking stage and carries on, and so does the manager.
            if std::thread::current().id() == runner {
                // First, so the panic message lands on the normal screen.
                restore_terminal();
            }
            chained(info);
        }));
        // From here on, an early return restores through drop.
        let mut screen = Self {
            terminal: None,
            previous_hook,
        };
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        screen.terminal = Some(terminal);
        Ok(screen)
    }

    fn draw(&mut self, state: &AppState) -> io::Result<()> {
        if let Some(terminal) = &mut self.terminal {
            let now = Instant::now();
            terminal.draw(|frame| view::draw(frame, state, now))?;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore_terminal();
        // The hooks cannot be changed while panicking; ours stays, and only
        // restores a terminal that is already restored.
        if !std::thread::panicking() {
            let previous = Arc::clone(&self.previous_hook);
            let _ = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Leave raw mode and the alternate screen. Errors are ignored: after SIGHUP
/// there is no terminal to write to.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use semver::Version;
    use xearthlayer::manager::{JobId, JobKind, JobSummary};
    use xearthlayer_package::PackageType;

    use super::*;

    fn with_a_job() -> AppState {
        let (mut state, _) = AppState::new(true);
        let summary = JobSummary {
            kind: JobKind::Install,
            region: "EU-WEST".to_string(),
            package_type: PackageType::Ortho,
            version: Some(Version::new(1, 0, 0)),
        };
        state.update(Input::Msg(Msg::Queued(JobId(1), summary)), Instant::now());
        state
    }

    #[test]
    fn an_interrupt_with_jobs_running_cancels_them_before_exiting() {
        let mut state = with_a_job();

        let effects = on_signal(&mut state, Signal::Interrupt, 1, Instant::now());

        assert_eq!(effects, [Effect::CancelAll]);
    }

    #[test]
    fn an_interrupt_with_nothing_running_exits() {
        let (mut state, _) = AppState::new(true);

        let effects = on_signal(&mut state, Signal::Interrupt, 1, Instant::now());

        assert_eq!(effects, [Effect::Exit]);
    }

    #[test]
    fn a_signal_after_the_first_abandons_and_exits() {
        let mut state = with_a_job();
        let now = Instant::now();

        assert_eq!(
            on_signal(&mut state, Signal::Interrupt, 1, now),
            [Effect::CancelAll]
        );
        assert_eq!(
            on_signal(&mut state, Signal::Terminate, 2, now),
            [Effect::Abandon, Effect::Exit]
        );

        let mut state = with_a_job();
        on_signal(&mut state, Signal::Interrupt, 1, now);
        assert_eq!(
            on_signal(&mut state, Signal::Interrupt, 2, now),
            [Effect::Abandon, Effect::Exit],
            "a second interrupt quits now"
        );
    }

    #[test]
    fn the_summary_follows_every_signal_but_a_hangup() {
        assert!(summary_after(Signal::Interrupt, false));
        assert!(summary_after(Signal::Terminate, false));
        assert!(!summary_after(Signal::Hangup, false));
        assert!(
            !summary_after(Signal::Interrupt, true),
            "a later hangup took the terminal"
        );
    }

    #[test]
    fn a_pending_rebuild_is_announced_unless_the_terminal_is_gone() {
        assert_eq!(
            finishing_note(true, false),
            Some("Finishing the overlay folder…")
        );
        assert_eq!(finishing_note(false, false), None);
        assert_eq!(finishing_note(true, true), None);
    }

    #[test]
    fn terminate_and_hangup_abandon_and_exit_at_once() {
        for signal in [Signal::Terminate, Signal::Hangup] {
            let mut state = with_a_job();

            let effects = on_signal(&mut state, signal, 1, Instant::now());

            assert_eq!(effects, [Effect::Abandon, Effect::Exit], "{signal}");
        }
    }
}
