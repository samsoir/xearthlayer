//! Lit generation permits admitted by priority (#433).
//!
//! In a scene load the generation permits (`lit_generation_permits`) are the
//! queue lit reads wait in: in the flight of 2026-11-26 a read waiting for
//! generation took up to 0.4 to 1.1 s (p95, max), mostly waiting for a
//! permit (p95 223 to 582 ms, up to 21 waiting), while a generation itself
//! took about 20 ms (median). FIFO, a read of the nearest textures waited
//! behind reads of the small levels of far ones, and pre-generations
//! (`pregen`) would have queued ahead of reads.
//!
//! Rules:
//!
//! - The number of permits is the same as before: only the order changes,
//!   so the night share of the blocking pool is unchanged.
//! - Waiters are admitted by [`GenerationClass`], then first come first
//!   served within a class: reads that reach the first visible level, then
//!   reads of the small levels only, then pre-generations. A pre-generation
//!   therefore takes a permit only when no read is waiting.
//! - A load several reads share has the most urgent class of any of them:
//!   a read that joins a load raises its waiter ([`LoadPriority::raise`]),
//!   which then queues as the class it was raised to, behind the waiters of
//!   that class already queued.
//! - A waiter whose future is dropped leaves the queue; one dropped after
//!   it was admitted but before it saw its permit gives the permit back. No
//!   permit is ever lost.
//! - Once closed (shutdown), pre-generations waiting are refused and new
//!   ones are refused at once; reads are admitted as before.
//!
//! [`Admission`] is the decision alone, pure; [`GenerationPermits`] puts it
//! behind an async permit.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::oneshot;

/// Who waits for a generation permit, in the order they are admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GenerationClass {
    /// A read that reaches the first visible level: the textures nearest
    /// the aircraft.
    HeadRead,
    /// A read of the small levels only.
    TailRead,
    /// A background generation queued when a probe answered lit.
    Pregeneration,
}

impl GenerationClass {
    /// Every class, in admission order (also the statistics' index).
    #[cfg(test)]
    pub const ALL: [Self; 3] = [Self::HeadRead, Self::TailRead, Self::Pregeneration];

    /// Its name in the debug lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::HeadRead => "head_read",
            Self::TailRead => "tail_read",
            Self::Pregeneration => "pregeneration",
        }
    }

    /// The class of a read: whether it reaches the first visible level.
    pub fn of_read(reaches_head: bool) -> Self {
        if reaches_head {
            Self::HeadRead
        } else {
            Self::TailRead
        }
    }
}

/// One waiter in an [`Admission`].
pub(crate) type Ticket = u64;

/// The admission decision: which waiter takes the next free permit.
///
/// Waiters are kept ordered by `(class, order)`: the first one is admitted
/// next. `order` is the moment a waiter queued as its class, from one
/// counter, so first come first served within a class.
#[derive(Debug)]
pub(crate) struct Admission {
    free: usize,
    next: u64,
    queue: BTreeMap<(GenerationClass, u64), Ticket>,
    places: HashMap<Ticket, (GenerationClass, u64)>,
    closed: bool,
}

impl Admission {
    /// `permits` free, nobody waiting.
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            free: permits,
            next: 0,
            queue: BTreeMap::new(),
            places: HashMap::new(),
            closed: false,
        }
    }

    fn order(&mut self) -> u64 {
        let order = self.next;
        self.next += 1;
        order
    }

    /// Queues a waiter of `class`; `None` when refused (a pre-generation
    /// once closed).
    pub(crate) fn enqueue(&mut self, class: GenerationClass) -> Option<Ticket> {
        if self.closed && class == GenerationClass::Pregeneration {
            return None;
        }
        let order = self.order();
        let ticket = order;
        self.queue.insert((class, order), ticket);
        self.places.insert(ticket, (class, order));
        Some(ticket)
    }

    /// Admits waiters while permits are free, first in order first, each
    /// with the class it was admitted as. After it, either no permit is free
    /// or nobody waits.
    pub(crate) fn admit(&mut self) -> Vec<(Ticket, GenerationClass)> {
        let mut admitted = Vec::new();
        while self.free > 0 {
            let Some(((class, _), ticket)) = self.queue.pop_first() else {
                break;
            };
            self.places.remove(&ticket);
            self.free -= 1;
            admitted.push((ticket, class));
        }
        admitted
    }

    /// A permit given back.
    pub(crate) fn release(&mut self) {
        self.free += 1;
    }

    /// Takes `ticket` out of the queue: whether it was waiting.
    pub(crate) fn cancel(&mut self, ticket: Ticket) -> bool {
        match self.places.remove(&ticket) {
            Some(place) => {
                self.queue.remove(&place);
                true
            }
            None => false,
        }
    }

    /// Moves a waiting `ticket` to `class` when that is more urgent than its
    /// own, behind the waiters of `class` already queued: whether it moved.
    pub(crate) fn raise(&mut self, ticket: Ticket, class: GenerationClass) -> bool {
        match self.places.get(&ticket) {
            Some(&(own, _)) if class < own => {
                self.cancel(ticket);
                let order = self.order();
                self.queue.insert((class, order), ticket);
                self.places.insert(ticket, (class, order));
                true
            }
            _ => false,
        }
    }

    /// Refuses every pre-generation waiting, and every later one: the
    /// tickets refused.
    pub(crate) fn close(&mut self) -> Vec<Ticket> {
        self.closed = true;
        let refused: Vec<_> = self
            .queue
            .iter()
            .filter(|((class, _), _)| *class == GenerationClass::Pregeneration)
            .map(|(_, &ticket)| ticket)
            .collect();
        for &ticket in &refused {
            self.cancel(ticket);
        }
        refused
    }

    /// Waiters of `class` now.
    #[cfg(test)]
    pub(crate) fn waiting(&self, class: GenerationClass) -> usize {
        self.queue.keys().filter(|(c, _)| *c == class).count()
    }

    /// Permits free now.
    #[cfg(test)]
    pub(crate) fn free(&self) -> usize {
        self.free
    }
}

/// The admission and the channel each waiter's answer goes through: its
/// class when admitted, `None` when refused.
struct State {
    admission: Admission,
    answers: HashMap<Ticket, oneshot::Sender<Option<GenerationClass>>>,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    /// Admits every waiter that can be, and tells each. A waiter that can
    /// no longer hear (gone without leaving the queue, which `Waiter`'s
    /// drop rules out) gives its permit straight back.
    fn dispatch(&mut self) {
        loop {
            let admitted = self.admission.admit();
            if admitted.is_empty() {
                return;
            }
            for (ticket, class) in admitted {
                let told = self
                    .answers
                    .remove(&ticket)
                    .is_some_and(|tx| tx.send(Some(class)).is_ok());
                if !told {
                    self.admission.release();
                }
            }
        }
    }
}

/// The lit generation permits, admitted by [`GenerationClass`].
pub(crate) struct GenerationPermits {
    state: Shared,
}

impl GenerationPermits {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                admission: Admission::new(permits),
                answers: HashMap::new(),
            })),
        }
    }

    /// A permit for the load `priority` stands for, admitted as its class
    /// (raised meanwhile by reads joining the load); `None` when refused (a
    /// pre-generation after [`Self::close`]). Dropping the future leaves
    /// the queue, or gives back a permit admitted but not yet seen.
    pub(crate) async fn acquire(&self, priority: &LoadPriority) -> Option<GenerationPermit> {
        let (tx, rx) = oneshot::channel();
        let ticket = {
            // Priority before state, here and in `LoadPriority::raise`.
            let mut p = priority.lock();
            let mut s = lock(&self.state);
            let ticket = s.admission.enqueue(p.class)?;
            s.answers.insert(ticket, tx);
            p.waiting = Some((Arc::clone(&self.state), ticket));
            s.dispatch();
            ticket
        };
        let mut waiter = Waiter {
            state: &self.state,
            priority,
            ticket,
            rx,
            answered: false,
        };
        let answer = (&mut waiter.rx).await;
        waiter.answered = true;
        match answer {
            Ok(Some(class)) => Some(GenerationPermit {
                state: Arc::clone(&self.state),
                class,
            }),
            _ => None,
        }
    }

    /// Refuses pre-generations from now on, those waiting included.
    ///
    /// A read that joins a pre-generation's load after it was refused, in
    /// the moment between `close` and the load leaving the in-flight map,
    /// shares that load's answer: black, for that one read. A read that
    /// joined before `close` raised the waiter to its own class, and is not
    /// refused. This is acceptable because it happens only at unmount, to
    /// at most one read per footprint whose pre-generation was waiting:
    /// the size is unchanged (R7), the black payload is kept nowhere, and
    /// X-Plane's next read of the level (if the session goes on that long)
    /// loads it again. Retrying the load for such a read would add a path
    /// that only shutdown takes, to save a texture nobody will look at.
    pub(crate) fn close(&self) {
        let mut s = lock(&self.state);
        for ticket in s.admission.close() {
            if let Some(tx) = s.answers.remove(&ticket) {
                let _ = tx.send(None);
            }
        }
    }

    /// Waiters of `class` now.
    #[cfg(test)]
    pub(crate) fn waiting(&self, class: GenerationClass) -> usize {
        lock(&self.state).admission.waiting(class)
    }

    /// Permits free now.
    #[cfg(test)]
    pub(crate) fn free(&self) -> usize {
        lock(&self.state).admission.free()
    }

    /// Every permit, held by the caller (tests that need generations to
    /// wait).
    #[cfg(test)]
    pub(crate) async fn hold_all(&self) -> Vec<GenerationPermit> {
        let priority = LoadPriority::new(GenerationClass::HeadRead);
        let mut held = Vec::new();
        while self.free() > 0 {
            held.push(
                self.acquire(&priority)
                    .await
                    .expect("a read is never refused"),
            );
        }
        held
    }
}

/// A waiter between queueing and seeing its answer.
struct Waiter<'a> {
    state: &'a Shared,
    priority: &'a LoadPriority,
    ticket: Ticket,
    rx: oneshot::Receiver<Option<GenerationClass>>,
    answered: bool,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.priority.clear(self.ticket);
        if self.answered {
            return;
        }
        // Answers are sent under the state's lock: what the channel holds
        // here is final.
        let mut s = lock(self.state);
        match self.rx.try_recv() {
            Ok(Some(_)) => {
                s.admission.release();
                s.dispatch();
            }
            Ok(None) => {}
            Err(_) => {
                s.admission.cancel(self.ticket);
                s.answers.remove(&self.ticket);
            }
        }
    }
}

/// One generation permit; given back, to the next waiter in order, when
/// dropped.
pub(crate) struct GenerationPermit {
    state: Shared,
    class: GenerationClass,
}

impl GenerationPermit {
    /// The class it was admitted as.
    pub(crate) fn class(&self) -> GenerationClass {
        self.class
    }
}

impl Drop for GenerationPermit {
    fn drop(&mut self) {
        let mut s = lock(&self.state);
        s.admission.release();
        s.dispatch();
    }
}

/// The class of one load, and its waiter while it waits for a permit: the
/// most urgent class of the reads sharing the load (or a pre-generation's,
/// until a read joins it).
pub(crate) struct LoadPriority(Mutex<PriorityState>);

struct PriorityState {
    class: GenerationClass,
    waiting: Option<(Shared, Ticket)>,
}

impl LoadPriority {
    pub(crate) fn new(class: GenerationClass) -> Self {
        Self(Mutex::new(PriorityState {
            class,
            waiting: None,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, PriorityState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The load's class now.
    #[cfg(test)]
    pub(crate) fn class(&self) -> GenerationClass {
        self.lock().class
    }

    /// A read of `class` shares the load: the load takes it when it is more
    /// urgent, its waiter too if it waits for a permit.
    pub(crate) fn raise(&self, class: GenerationClass) {
        let mut p = self.lock();
        if class < p.class {
            p.class = class;
            if let Some((state, ticket)) = &p.waiting {
                lock(state).admission.raise(*ticket, class);
            }
        }
    }

    fn clear(&self, ticket: Ticket) {
        let mut p = self.lock();
        if p.waiting.as_ref().is_some_and(|(_, t)| *t == ticket) {
            p.waiting = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GenerationClass::{HeadRead, Pregeneration, TailRead};

    /// The classes of `admission`'s waiters admitted when `n` permits come
    /// back, in order.
    fn admitted_after(admission: &mut Admission, n: usize) -> Vec<(Ticket, GenerationClass)> {
        for _ in 0..n {
            admission.release();
        }
        admission.admit()
    }

    #[test]
    fn reads_are_admitted_before_pregenerations_and_head_reads_first() {
        let mut a = Admission::new(0);
        let pregen = a.enqueue(Pregeneration).unwrap();
        let tail = a.enqueue(TailRead).unwrap();
        let head = a.enqueue(HeadRead).unwrap();
        assert_eq!(
            admitted_after(&mut a, 3),
            [(head, HeadRead), (tail, TailRead), (pregen, Pregeneration)]
        );
    }

    #[test]
    fn a_class_is_first_come_first_served() {
        let mut a = Admission::new(0);
        let first = a.enqueue(TailRead).unwrap();
        let second = a.enqueue(TailRead).unwrap();
        assert_eq!(admitted_after(&mut a, 1), [(first, TailRead)]);
        assert_eq!(admitted_after(&mut a, 1), [(second, TailRead)]);
    }

    #[test]
    fn no_more_are_admitted_than_permits_are_free() {
        let mut a = Admission::new(2);
        let tickets: Vec<_> = (0..3).map(|_| a.enqueue(HeadRead).unwrap()).collect();
        assert_eq!(a.admit().len(), 2);
        assert_eq!((a.free(), a.waiting(HeadRead)), (0, 1));
        assert!(a.admit().is_empty(), "nothing free");
        assert_eq!(admitted_after(&mut a, 1), [(tickets[2], HeadRead)]);
    }

    #[test]
    fn a_pregeneration_is_admitted_only_when_no_read_waits() {
        let mut a = Admission::new(0);
        let pregen = a.enqueue(Pregeneration).unwrap();
        a.enqueue(TailRead).unwrap();
        assert_eq!(admitted_after(&mut a, 1)[0].1, TailRead);
        let read = a.enqueue(TailRead).unwrap();
        assert_eq!(admitted_after(&mut a, 1), [(read, TailRead)], "came later");
        assert_eq!(admitted_after(&mut a, 1), [(pregen, Pregeneration)]);
    }

    #[test]
    fn a_raised_waiter_queues_behind_its_new_class() {
        let mut a = Admission::new(0);
        let head = a.enqueue(HeadRead).unwrap();
        let pregen = a.enqueue(Pregeneration).unwrap();
        let tail = a.enqueue(TailRead).unwrap();
        assert!(a.raise(pregen, HeadRead));
        assert!(!a.raise(pregen, TailRead), "never lowered");
        assert!(!a.raise(head, HeadRead), "already that class");
        assert_eq!(
            admitted_after(&mut a, 3),
            [(head, HeadRead), (pregen, HeadRead), (tail, TailRead)]
        );
        assert!(!a.raise(pregen, HeadRead), "no longer waiting");
    }

    #[test]
    fn a_cancelled_waiter_leaves_the_queue() {
        let mut a = Admission::new(0);
        let gone = a.enqueue(HeadRead).unwrap();
        let next = a.enqueue(TailRead).unwrap();
        assert!(a.cancel(gone));
        assert!(!a.cancel(gone), "once");
        assert_eq!(admitted_after(&mut a, 1), [(next, TailRead)]);
    }

    #[test]
    fn closing_refuses_pregenerations_only() {
        let mut a = Admission::new(0);
        let pregen = a.enqueue(Pregeneration).unwrap();
        let read = a.enqueue(TailRead).unwrap();
        assert_eq!(a.close(), [pregen]);
        assert_eq!(a.enqueue(Pregeneration), None);
        let head = a.enqueue(HeadRead).unwrap();
        assert_eq!(
            admitted_after(&mut a, 3),
            [(head, HeadRead), (read, TailRead)]
        );
        assert_eq!(a.free(), 1, "the refused one took none");
    }

    // The async permit.

    async fn poll_once<F: std::future::Future + Unpin>(f: &mut F) -> std::task::Poll<F::Output> {
        futures::poll!(f)
    }

    #[tokio::test]
    async fn every_permit_held_and_a_pregeneration_queued_the_next_permit_goes_to_a_read() {
        let permits = GenerationPermits::new(2);
        let held = permits.hold_all().await;
        let (pregen, read) = (
            LoadPriority::new(Pregeneration),
            LoadPriority::new(TailRead),
        );
        let mut pregen_wait = Box::pin(permits.acquire(&pregen));
        assert!(poll_once(&mut pregen_wait).await.is_pending());
        let mut read_wait = Box::pin(permits.acquire(&read));
        assert!(poll_once(&mut read_wait).await.is_pending());
        let mut held = held.into_iter();
        drop(held.next());
        let permit = read_wait.await.unwrap();
        assert_eq!(permit.class(), TailRead);
        assert!(
            poll_once(&mut pregen_wait).await.is_pending(),
            "still waits"
        );
        drop(held.next());
        assert_eq!(pregen_wait.await.unwrap().class(), Pregeneration);
    }

    #[tokio::test]
    async fn a_dropped_waiter_frees_its_place() {
        let permits = GenerationPermits::new(1);
        let held = permits.hold_all().await;
        let (gone, next) = (LoadPriority::new(HeadRead), LoadPriority::new(TailRead));
        {
            let mut wait = Box::pin(permits.acquire(&gone));
            assert!(poll_once(&mut wait).await.is_pending());
            assert_eq!(permits.waiting(HeadRead), 1);
        }
        assert_eq!(permits.waiting(HeadRead), 0, "left the queue");
        let mut wait = Box::pin(permits.acquire(&next));
        assert!(poll_once(&mut wait).await.is_pending());
        drop(held);
        assert_eq!(wait.await.unwrap().class(), TailRead);
    }

    #[tokio::test]
    async fn a_waiter_dropped_after_it_was_admitted_gives_its_permit_back() {
        let permits = GenerationPermits::new(1);
        let held = permits.hold_all().await;
        let priority = LoadPriority::new(HeadRead);
        let mut wait = Box::pin(permits.acquire(&priority));
        assert!(poll_once(&mut wait).await.is_pending());
        drop(held); // admitted, not yet polled
        assert_eq!(permits.free(), 0);
        drop(wait);
        assert_eq!(permits.free(), 1, "no permit lost");
        assert_eq!(permits.hold_all().await.len(), 1);
    }

    #[tokio::test]
    async fn a_read_joining_a_waiting_pregeneration_raises_it_ahead_of_later_reads() {
        let permits = GenerationPermits::new(1);
        let held = permits.hold_all().await;
        let (pregen, later) = (
            LoadPriority::new(Pregeneration),
            LoadPriority::new(TailRead),
        );
        let mut pregen_wait = Box::pin(permits.acquire(&pregen));
        assert!(poll_once(&mut pregen_wait).await.is_pending());
        pregen.raise(HeadRead);
        assert_eq!(pregen.class(), HeadRead);
        let mut later_wait = Box::pin(permits.acquire(&later));
        assert!(poll_once(&mut later_wait).await.is_pending());
        drop(held);
        let permit = pregen_wait.await.unwrap();
        assert_eq!(permit.class(), HeadRead);
        assert!(poll_once(&mut later_wait).await.is_pending());
    }

    #[tokio::test]
    async fn a_closed_permit_set_refuses_pregenerations_and_admits_reads() {
        let permits = GenerationPermits::new(1);
        let held = permits.hold_all().await;
        let pregen = LoadPriority::new(Pregeneration);
        let mut waiting = Box::pin(permits.acquire(&pregen));
        assert!(poll_once(&mut waiting).await.is_pending());
        permits.close();
        assert!(waiting.await.is_none(), "refused while waiting");
        assert!(permits.acquire(&pregen).await.is_none(), "and from now on");
        drop(held);
        let read = LoadPriority::new(TailRead);
        assert!(permits.acquire(&read).await.is_some());
        assert_eq!(permits.free(), 1, "every permit came back");
    }
}
