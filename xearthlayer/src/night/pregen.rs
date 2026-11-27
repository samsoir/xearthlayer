//! The queue of lit pre-generations (#433).
//!
//! When the probe commits a footprint lit, the footprint is queued here and
//! generated in the background, so the read X-Plane makes later is a memory
//! hit. In the flight of 2026-11-26 every footprint the probe answered lit
//! was read (lit probes equalled generations in every scene load), so the
//! work is not wasted; it moves to `stat` time, overlapping X-Plane's DSF
//! loading.
//!
//! Rules:
//!
//! - At most [`MAX_QUEUED_PREGENERATIONS`] wait; one past them is dropped,
//!   and the read of its footprint generates as it would have without
//!   pre-generation.
//! - As many run at once as there are workers, one fewer than the
//!   generation permits ([`pregen_workers`]): each takes the next footprint
//!   once it is done with its own, and one permit is always left to reads.
//! - Once closed (shutdown) nothing queued runs, and nothing is queued.
//!
//! The queue is the decision alone; the lit source runs the workers.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::layout::{tail_len, FIRST_VISIBLE_ZL16};
use super::payloads::LIT_TAIL_CACHE_BYTES;
use super::stats::LitStats;
use crate::fuse::DdsFilename;

/// Most pre-generations waiting at once: about half as many as the tail
/// memory holds ZL16 tails (1,151 in [`LIT_TAIL_CACHE_BYTES`]). A
/// pre-generation's tail waits in memory until X-Plane reads it; a queue
/// longer than that memory would evict the tails it pre-generated first
/// before they are read, and those reads would generate them again. The
/// other half is for the tails X-Plane has read and comes back to. A
/// capped zoom's tail is larger (2048 px first visible level: 4 times), so
/// a queue of those holds proportionally fewer before evicting; the bound
/// is set for ZL16, the zoom of every installed night package. A scene load
/// in the flight of 2026-11-26 generated 300 to 1,200 lit textures: past
/// the bound, a footprint is dropped and generated when read, as before
/// pre-generation. Each entry is a texture name, a few dozen bytes.
pub(crate) const MAX_QUEUED_PREGENERATIONS: usize = 512;

const _: () = assert!(
    2 * MAX_QUEUED_PREGENERATIONS as u64 * tail_len(FIRST_VISIBLE_ZL16) <= LIT_TAIL_CACHE_BYTES
);

/// Pre-generation workers for `permits` generation permits: one fewer, at
/// least one (there are at least 2 permits). A pre-generation admitted
/// holds its permit to the end of its generation; with a worker per permit,
/// a read arriving during a backlog could find every permit so held and wait
/// out a pre-generation. One fewer always leaves a permit no pre-generation
/// holds.
pub(crate) fn pregen_workers(permits: usize) -> usize {
    permits.saturating_sub(1).max(1)
}

/// What queueing one footprint did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Queued {
    /// Queued; `start_worker` when a worker must be started for it (fewer
    /// are running than allowed).
    Yes { start_worker: bool },
    /// The queue is full: dropped.
    Dropped,
    /// Closed: nothing is queued any more.
    Closed,
}

#[derive(Debug, Default)]
struct State {
    names: VecDeque<DdsFilename>,
    /// Workers running: each has taken a footprint and will ask for the
    /// next before it stops.
    running: usize,
    closed: bool,
}

/// Footprints waiting for pre-generation, and the workers draining them.
#[derive(Debug)]
pub(crate) struct PregenQueue {
    bound: usize,
    workers: usize,
    state: Mutex<State>,
}

impl PregenQueue {
    /// At most `bound` waiting, drained by at most `workers` at once.
    pub(crate) fn new(bound: usize, workers: usize) -> Self {
        Self {
            bound,
            workers,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `name` (see [`Queued`]), counting it into `stats` (queued,
    /// or dropped) under the queue's lock, as every change of the queue is,
    /// so its depth there is exact. A worker asked to start counts as
    /// running from here, so concurrent callers never start too many.
    pub(crate) fn push(&self, name: DdsFilename, stats: &LitStats) -> Queued {
        let mut s = self.lock();
        if s.closed {
            return Queued::Closed;
        }
        if s.names.len() >= self.bound {
            stats.record_pregen_dropped();
            return Queued::Dropped;
        }
        s.names.push_back(name);
        stats.record_pregen_queued();
        let start_worker = s.running < self.workers;
        if start_worker {
            s.running += 1;
        }
        Queued::Yes { start_worker }
    }

    /// The next footprint for a running worker, first queued first; `None`
    /// when there is none or the queue is closed, and the worker stops
    /// (counted out under the same lock `push` reads, so a footprint queued
    /// meanwhile starts a worker of its own).
    /// A footprint taken is counted started into `stats`.
    pub(crate) fn next(&self, stats: &LitStats) -> Option<DdsFilename> {
        let mut s = self.lock();
        let next = if s.closed { None } else { s.names.pop_front() };
        match next {
            Some(_) => stats.record_pregen_started(),
            None => s.running = s.running.saturating_sub(1),
        }
        next
    }

    /// Closes the queue: the footprints still waiting never run, and are
    /// counted cancelled into `stats`.
    pub(crate) fn close(&self, stats: &LitStats) {
        let mut s = self.lock();
        s.closed = true;
        stats.record_pregen_cancelled(s.names.len());
        s.names.clear();
    }

    /// The same queue, holding at most `bound`.
    #[cfg(test)]
    pub(crate) fn with_bound(self, bound: usize) -> Self {
        Self { bound, ..self }
    }

    /// Footprints waiting now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().names.len()
    }

    /// Workers running now.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        self.lock().running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;

    fn name(col: u32) -> DdsFilename {
        DdsFilename {
            row: 25312,
            col,
            zoom: 16,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    #[test]
    fn pregenerations_leave_a_permit_to_reads() {
        assert_eq!(pregen_workers(2), 1);
        assert_eq!(pregen_workers(3), 2);
        assert_eq!(pregen_workers(4), 3);
        assert_eq!(pregen_workers(1), 1, "never none");
    }

    #[test]
    fn a_worker_is_started_only_while_fewer_than_allowed_run() {
        let stats = LitStats::default();
        let q = PregenQueue::new(8, 2);
        assert_eq!(q.push(name(0), &stats), Queued::Yes { start_worker: true });
        assert_eq!(q.push(name(16), &stats), Queued::Yes { start_worker: true });
        assert_eq!(
            q.push(name(32), &stats),
            Queued::Yes {
                start_worker: false
            }
        );
        assert_eq!(q.running(), 2);
    }

    #[test]
    fn workers_take_footprints_in_the_order_they_were_queued_and_stop_when_none_is_left() {
        let stats = LitStats::default();
        let q = PregenQueue::new(8, 1);
        q.push(name(0), &stats);
        q.push(name(16), &stats);
        assert_eq!(q.next(&stats).map(|n| n.col), Some(0));
        assert_eq!(q.next(&stats).map(|n| n.col), Some(16));
        assert_eq!(q.running(), 1, "still running");
        assert_eq!(q.next(&stats), None);
        assert_eq!(q.running(), 0, "stopped");
        assert_eq!(
            q.push(name(32), &stats),
            Queued::Yes { start_worker: true },
            "the next footprint starts a worker"
        );
    }

    #[test]
    fn past_the_bound_a_footprint_is_dropped() {
        let stats = LitStats::default();
        let q = PregenQueue::new(2, 1);
        q.push(name(0), &stats);
        q.push(name(16), &stats);
        assert_eq!(q.push(name(32), &stats), Queued::Dropped);
        assert_eq!(q.len(), 2);
        let snap = stats.snapshot();
        assert_eq!((snap.pregen_queued, snap.pregen_dropped), (2, 1));
        q.next(&stats);
        assert!(
            matches!(q.push(name(48), &stats), Queued::Yes { .. }),
            "room again"
        );
    }

    #[test]
    fn a_closed_queue_runs_nothing_queued_and_queues_nothing() {
        let stats = LitStats::default();
        let q = PregenQueue::new(8, 1);
        q.push(name(0), &stats);
        q.push(name(16), &stats);
        q.close(&stats);
        assert_eq!(stats.snapshot().pregen_cancelled, 2);
        assert_eq!(q.next(&stats), None);
        assert_eq!(q.running(), 0);
        assert_eq!(q.push(name(32), &stats), Queued::Closed);
        assert_eq!(q.len(), 0);
    }
}
