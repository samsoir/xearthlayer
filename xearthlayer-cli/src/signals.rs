//! What SIGINT, SIGTERM and SIGHUP do to a running package operation.
//!
//! The first Ctrl+C cancels: every job stops at its next check, a swap in
//! progress finishes, and temp directories are cleaned up. A second Ctrl+C,
//! SIGTERM (shutdown, `kill`, a service manager) or SIGHUP (terminal closed)
//! abandons: the same, but nothing waits for cleanup, because there may be
//! no time for it. The next queue start sweeps what is left. After SIGHUP,
//! nothing more is written to the terminal: it is gone, and `println!`
//! panics on the write error.
//!
//! With no queue running there is nothing to protect, and the signal gets
//! its default action, exactly as before this module existed.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::{Handle, Signals};
use xearthlayer::manager::Canceller;

static TERMINAL_GONE: AtomicBool = AtomicBool::new(false);

/// Whether SIGHUP said the terminal is gone.
pub fn terminal_gone() -> bool {
    TERMINAL_GONE.load(Ordering::SeqCst)
}

/// The signals that stop a package operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Interrupt,
    Terminate,
    Hangup,
}

impl Signal {
    pub fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            SIGINT => Some(Self::Interrupt),
            SIGTERM => Some(Self::Terminate),
            SIGHUP => Some(Self::Hangup),
            _ => None,
        }
    }

    pub fn raw(self) -> i32 {
        match self {
            Self::Interrupt => SIGINT,
            Self::Terminate => SIGTERM,
            Self::Hangup => SIGHUP,
        }
    }

    /// 128 plus the signal number, as a shell reports it.
    pub fn exit_code(self) -> u8 {
        128 + self.raw() as u8
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
        })
    }
}

/// What a signal does to the running queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Cancel,
    Abandon,
}

/// Only a first Ctrl+C waits for cleanup.
pub fn action(signal: Signal, interrupts_before: u32) -> Action {
    match (signal, interrupts_before) {
        (Signal::Interrupt, 0) => Action::Cancel,
        _ => Action::Abandon,
    }
}

/// The queue a signal should stop, if one is running.
#[derive(Clone, Default)]
pub struct ActiveQueue(Arc<Mutex<Option<Canceller>>>);

impl std::fmt::Debug for ActiveQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveQueue")
            .field("running", &self.current().is_some())
            .finish()
    }
}

impl ActiveQueue {
    pub fn set(&self, canceller: Canceller) {
        *self.0.lock().unwrap() = Some(canceller);
    }

    /// Call as soon as the queue's work is done: a live `Canceller` keeps the
    /// queue's lock held.
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }

    pub fn current(&self) -> Option<Canceller> {
        self.0.lock().unwrap().clone()
    }
}

/// Routes the three signals to the active queue until dropped.
pub struct SignalWatch {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
    received: Arc<Mutex<Option<Signal>>>,
}

impl SignalWatch {
    pub fn start(active: ActiveQueue, notice: impl Fn(&str) + Send + 'static) -> io::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let handle = signals.handle();
        let received = Arc::new(Mutex::new(None));
        let first = Arc::clone(&received);
        let thread = std::thread::Builder::new()
            .name("xel-signals".into())
            .spawn(move || {
                let mut interrupts = 0;
                for raw in signals.forever() {
                    let Some(signal) = Signal::from_raw(raw) else {
                        continue;
                    };
                    first.lock().unwrap().get_or_insert(signal);
                    if signal == Signal::Hangup {
                        TERMINAL_GONE.store(true, Ordering::SeqCst);
                    }
                    let Some(queue) = active.current() else {
                        // Nothing to protect: behave as if no handler existed.
                        let _ = signal_hook::low_level::emulate_default_handler(raw);
                        continue;
                    };
                    match action(signal, interrupts) {
                        Action::Cancel => {
                            notice("Cancelling, press Ctrl+C again to quit now");
                            queue.cancel_all();
                        }
                        Action::Abandon => queue.abandon(),
                    }
                    if signal == Signal::Interrupt {
                        interrupts += 1;
                    }
                }
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
            received,
        })
    }

    /// The first signal received, which decides the exit code: the user's
    /// intent (an interrupt) wins, and a later SIGTERM from a shutdown should
    /// not turn a 130 into a 143.
    pub fn received(&self) -> Option<Signal> {
        *self.received.lock().unwrap()
    }
}

impl Drop for SignalWatch {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use xearthlayer::manager::{Canceller, QueueControl};

    #[test]
    fn the_first_interrupt_cancels_and_everything_after_abandons() {
        assert_eq!(action(Signal::Interrupt, 0), Action::Cancel);
        assert_eq!(action(Signal::Interrupt, 1), Action::Abandon);
        assert_eq!(action(Signal::Terminate, 0), Action::Abandon);
        assert_eq!(action(Signal::Hangup, 0), Action::Abandon);
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(Signal::Interrupt.exit_code(), 130);
        assert_eq!(Signal::Terminate.exit_code(), 143);
        assert_eq!(Signal::Hangup.exit_code(), 129);
    }

    #[test]
    fn raw_numbers_map_to_the_three_signals_and_nothing_else() {
        for signal in [Signal::Interrupt, Signal::Terminate, Signal::Hangup] {
            assert_eq!(Signal::from_raw(signal.raw()), Some(signal));
        }
        assert_eq!(Signal::from_raw(signal_hook::consts::SIGUSR1), None);
    }

    #[derive(Default)]
    struct Recorded {
        cancels: AtomicUsize,
        abandons: AtomicUsize,
    }
    impl QueueControl for Recorded {
        fn cancel_all(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
        }
        fn abandon(&self) {
            self.abandons.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn eventually(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "signal not delivered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// One test, because a raised signal reaches every watch in the process.
    /// The queue is always set before raising: with no queue, the watch
    /// falls back to the default action, which would end the test binary.
    #[test]
    fn real_signals_reach_the_active_queue() {
        let recorded = Arc::new(Recorded::default());
        let active = ActiveQueue::default();
        active.set(Canceller::new(recorded.clone()));
        let watch = SignalWatch::start(active, |_| {}).unwrap();

        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        eventually(|| recorded.cancels.load(Ordering::SeqCst) == 1);
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        eventually(|| recorded.abandons.load(Ordering::SeqCst) == 1);
        signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
        eventually(|| recorded.abandons.load(Ordering::SeqCst) == 2);
        assert!(!terminal_gone());
        signal_hook::low_level::raise(signal_hook::consts::SIGHUP).unwrap();
        eventually(|| recorded.abandons.load(Ordering::SeqCst) == 3);

        assert!(terminal_gone());
        assert_eq!(
            watch.received(),
            Some(Signal::Interrupt),
            "the first signal decides the exit code"
        );
    }
}
