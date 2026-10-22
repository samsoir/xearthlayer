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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::{Handle, Signals};
use signal_hook::SigId;
use xearthlayer::manager::Canceller;

/// Set by the SIGHUP handler itself, so it is true the moment the signal
/// lands, whatever the watch thread is busy with.
fn terminal_gone_flag() -> &'static Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// Whether SIGHUP said the terminal is gone.
pub fn terminal_gone() -> bool {
    terminal_gone_flag().load(Ordering::SeqCst)
}

/// A poisoned lock still holds a usable value, and these locks guard plain
/// data: a panic elsewhere must not silence every later signal.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// What a signal does to the running queue. `Abandon` is the stronger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

fn apply(action: Action, queue: &Canceller) {
    match action {
        Action::Cancel => queue.cancel_all(),
        Action::Abandon => queue.abandon(),
    }
}

#[derive(Default)]
struct Slot {
    canceller: Option<Canceller>,
    /// The strongest stop request so far. It outlives any one queue: a
    /// command that runs several queues in turn must not start the next one
    /// after the user asked to stop.
    requested: Option<Action>,
}

/// The queue a signal should stop, if one is running, and whether a stop
/// was asked for.
#[derive(Clone, Default)]
pub struct ActiveQueue(Arc<Mutex<Slot>>);

impl std::fmt::Debug for ActiveQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot = lock(&self.0);
        f.debug_struct("ActiveQueue")
            .field("running", &slot.canceller.is_some())
            .field("requested", &slot.requested)
            .finish()
    }
}

impl ActiveQueue {
    pub fn set(&self, canceller: Canceller) {
        lock(&self.0).canceller = Some(canceller);
    }

    /// Call as soon as the queue's work is done: a live `Canceller` keeps the
    /// queue's lock held.
    pub fn clear(&self) {
        lock(&self.0).canceller = None;
    }

    #[cfg(test)]
    pub fn current(&self) -> Option<Canceller> {
        lock(&self.0).canceller.clone()
    }

    /// Whether a stop was asked for. Check before starting a queue.
    pub fn stop_requested(&self) -> bool {
        lock(&self.0).requested.is_some()
    }

    /// Record a stop request (a later `Abandon` upgrades a recorded
    /// `Cancel`) and return what to apply to the running queue, if any.
    /// Nothing is applied here: applying can take long, and the slot must
    /// stay available meanwhile.
    pub fn request(&self, action: Action) -> Option<(Action, Canceller)> {
        let mut slot = lock(&self.0);
        let effective = slot
            .requested
            .map_or(action, |recorded| recorded.max(action));
        slot.requested = Some(effective);
        slot.canceller.clone().map(|queue| (effective, queue))
    }

    /// Apply the recorded request to the running queue. Call after submitting
    /// work: a request that came before the work was in the queue found
    /// nothing to stop. Idempotent.
    pub fn reapply(&self) {
        let (requested, queue) = {
            let slot = lock(&self.0);
            (slot.requested, slot.canceller.clone())
        };
        if let (Some(action), Some(queue)) = (requested, queue) {
            apply(action, &queue);
        }
    }
}

/// Routes the three signals to the active queue until dropped.
pub struct SignalWatch {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
    received: Arc<Mutex<Option<Signal>>>,
    hangup_flag: SigId,
}

impl SignalWatch {
    pub fn start(active: ActiveQueue, notice: impl Fn(&str) + Send + 'static) -> io::Result<Self> {
        // First, so the flag is set before the watch thread hears of SIGHUP.
        let hangup_flag = signal_hook::flag::register(SIGHUP, Arc::clone(terminal_gone_flag()))?;
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
                    lock(&first).get_or_insert(signal);
                    let Some((action, queue)) = active.request(action(signal, interrupts)) else {
                        // Nothing to protect: behave as if no handler existed.
                        let _ = signal_hook::low_level::emulate_default_handler(raw);
                        continue;
                    };
                    if signal == Signal::Interrupt {
                        interrupts += 1;
                    }
                    if action == Action::Cancel {
                        notice("Cancelling, press Ctrl+C again to quit now");
                    }
                    // Off this thread: cancelling can take long (it deletes
                    // temp trees), and the next signal must still be heard.
                    let apply_it = move || apply(action, &queue);
                    let spawned = std::thread::Builder::new()
                        .name("xel-signal-apply".into())
                        .spawn(apply_it.clone());
                    if spawned.is_err() {
                        apply_it();
                    }
                }
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
            received,
            hangup_flag,
        })
    }

    /// The first signal received, which decides the exit code: the user's
    /// intent (an interrupt) wins, and a later SIGTERM from a shutdown should
    /// not turn a 130 into a 143.
    pub fn received(&self) -> Option<Signal> {
        *lock(&self.received)
    }
}

impl Drop for SignalWatch {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        signal_hook::low_level::unregister(self.hangup_flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Condvar;
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

    /// Counts calls. `cancel_all` can be made to block until `release`.
    #[derive(Default)]
    struct Recorded {
        cancels: AtomicUsize,
        abandons: AtomicUsize,
        hold_cancel: AtomicBool,
        released: (Mutex<bool>, Condvar),
    }
    impl Recorded {
        fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
    }
    impl QueueControl for Recorded {
        fn cancel_all(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            if self.hold_cancel.load(Ordering::SeqCst) {
                let mut open = self.released.0.lock().unwrap();
                while !*open {
                    open = self.released.1.wait(open).unwrap();
                }
            }
        }
        fn abandon(&self) {
            self.abandons.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_stop_requested_before_a_queue_is_set_is_applied_by_reapply() {
        let recorded = Arc::new(Recorded::default());
        let active = ActiveQueue::default();

        assert!(!active.stop_requested());
        assert!(active.request(Action::Cancel).is_none(), "no queue yet");
        assert!(active.stop_requested());

        active.set(Canceller::new(recorded.clone()));
        assert_eq!(recorded.cancels.load(Ordering::SeqCst), 0);
        active.reapply();
        active.reapply();
        assert_eq!(
            recorded.cancels.load(Ordering::SeqCst),
            2,
            "idempotent, so repeating is safe"
        );
        assert_eq!(recorded.abandons.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_later_abandon_upgrades_a_recorded_cancel_and_never_the_reverse() {
        let recorded = Arc::new(Recorded::default());
        let active = ActiveQueue::default();
        active.set(Canceller::new(recorded.clone()));

        assert_eq!(active.request(Action::Cancel).unwrap().0, Action::Cancel);
        assert_eq!(active.request(Action::Abandon).unwrap().0, Action::Abandon);
        assert_eq!(active.request(Action::Cancel).unwrap().0, Action::Abandon);
        active.reapply();
        assert_eq!(recorded.abandons.load(Ordering::SeqCst), 1);
        assert_eq!(recorded.cancels.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_poisoned_slot_still_works() {
        let active = ActiveQueue::default();
        let poisoner = active.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.0.lock().unwrap();
            panic!("poison the slot");
        })
        .join();
        assert!(!active.stop_requested());
        active.request(Action::Cancel);
        assert!(active.stop_requested());
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
        // The first cancel is slow, as when it deletes temp trees.
        recorded.hold_cancel.store(true, Ordering::SeqCst);
        let active = ActiveQueue::default();
        active.set(Canceller::new(recorded.clone()));
        let watch = SignalWatch::start(active, |_| {}).unwrap();

        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        eventually(|| recorded.cancels.load(Ordering::SeqCst) == 1);
        // The second Ctrl+C is heard while the first cancel is still running.
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        eventually(|| recorded.abandons.load(Ordering::SeqCst) == 1);
        recorded.release();
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
