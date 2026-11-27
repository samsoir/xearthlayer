//! Generated lit payloads in memory, as two parts per footprint (#432).
//!
//! A payload is the first visible level (the head, 512 KiB at ZL16) and the
//! levels below it (the tail, 170.7 KiB). X-Plane reads the small levels
//! first, level by level with tens of seconds between reads, and most
//! textures never read their first visible level: kept whole, 256 MiB held
//! 383 ZL16 payloads against about 1,000 textures read at one location, and
//! a scene load evicted the small levels X-Plane came back for. Kept apart,
//! with a budget each, the tails of the whole working set fit.
//!
//! Rules:
//!
//! - The tail is kept on every load that may be kept (a generation, or a
//!   read from the lit disk tier); the head only when a read reaches it. A
//!   load started by a read of the small levels keeps the tail alone; a
//!   later read of the first visible level loads again (from the disk tier,
//!   or by generation for a payload kept in memory only) and keeps its head.
//! - Each part kept is a copy of its own: a slice of the loaded buffer
//!   would keep the whole buffer alive while either part lives, and the
//!   weigher, which counts length, would under-count it.
//! - Concurrent loads of one footprint are one load, whichever parts their
//!   reads want: a read that misses joins the load in flight, and keeps the
//!   parts it wants from it.
//! - A load has the most urgent [`GenerationClass`] of the callers sharing
//!   it (#433): a caller that joins raises it, so a read joining a
//!   pre-generation's load waits for a permit as a read.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use moka::policy::EvictionPolicy;

use super::admission::{GenerationClass, LoadPriority};
use super::layout::{LitLayout, LitParts, PayloadPart};
use super::stats::{LitStats, PayloadLookup, PayloadOccupancy};

const MIB: u64 = 1024 * 1024;

/// Memory for the tails: 1,151 ZL16 tails (174,776 bytes each), more than
/// the 1,006 ZL16 textures of the KCCR flight of 2026-11-14 (167.7 MiB).
pub(crate) const LIT_TAIL_CACHE_BYTES: u64 = 192 * MIB;

/// Memory for the heads: 128 ZL16 first visible levels (524,288 bytes
/// each); that flight read 44.
pub(crate) const LIT_HEAD_CACHE_BYTES: u64 = 64 * MIB;

/// Memory for generated visible levels, both parts: as before the split.
pub(crate) const LIT_PAYLOAD_CACHE_BYTES: u64 = LIT_TAIL_CACHE_BYTES + LIT_HEAD_CACHE_BYTES;

const _: () = assert!(LIT_PAYLOAD_CACHE_BYTES == 256 * MIB);

/// A footprint: `(row, col, zoom)`.
pub(crate) type FootprintKey = (u32, u32, u8);

/// One part of the payloads of every footprint.
type PartCache = moka::sync::Cache<FootprintKey, Bytes>;

/// A payload served but not kept in memory, or `None` (black): shared with
/// every read joining the same load, and kept by none.
#[derive(Debug)]
pub(crate) struct Uncached(pub Option<Bytes>);

/// The parts of a payload one read wants: those its range overlaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Wanted([bool; 2]);

impl Wanted {
    /// The parts `[offset, offset + len)` overlaps.
    pub(crate) fn reached(layout: &LitLayout, offset: u64, len: u32) -> Self {
        Self(PayloadPart::ALL.map(|part| layout.reaches(part, offset, len)))
    }

    /// Both parts: the whole payload.
    #[cfg(test)]
    pub(crate) fn whole() -> Self {
        Self([true; 2])
    }

    pub(crate) fn contains(self, part: PayloadPart) -> bool {
        self.0[part as usize]
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == [false; 2]
    }

    fn parts(self) -> impl Iterator<Item = PayloadPart> {
        PayloadPart::ALL
            .into_iter()
            .filter(move |&p| self.contains(p))
    }
}

/// What one load of a footprint gave every read sharing it.
#[derive(Debug, Clone)]
struct Loaded {
    /// The whole payload, or `None` (black).
    payload: Option<Bytes>,
    /// Whether its parts may be kept in memory.
    keep: bool,
}

impl From<Result<Bytes, Uncached>> for Loaded {
    fn from(loaded: Result<Bytes, Uncached>) -> Self {
        match loaded {
            Ok(payload) => Self {
                payload: Some(payload),
                keep: true,
            },
            Err(Uncached(payload)) => Self {
                payload,
                keep: false,
            },
        }
    }
}

/// One load in flight; whichever caller runs it, the others wait for its
/// value. Should the caller running it be dropped, a waiting one runs its
/// own load in its place (`OnceCell`'s rule), with the same priority.
struct Load {
    value: tokio::sync::OnceCell<Loaded>,
    priority: Arc<LoadPriority>,
}

type LoadSlot = Arc<Load>;

/// What [`PayloadMemory::serve`] found for one read.
#[derive(Debug, Clone)]
pub(crate) struct Served {
    /// The parts the read wants, or none (black).
    pub parts: LitParts,
    /// How each wanted part was found (`None` for a part not wanted).
    pub lookups: [Option<PayloadLookup>; 2],
}

impl Served {
    /// How the read as a whole was served: from memory when every part it
    /// wants was there, a load when it ran one, else joined.
    pub(crate) fn lookup(&self) -> PayloadLookup {
        let mut lookups = self.lookups.iter().flatten();
        if lookups.clone().all(|&l| l == PayloadLookup::Hit) {
            PayloadLookup::Hit
        } else if lookups.any(|&l| l == PayloadLookup::Loaded) {
            PayloadLookup::Loaded
        } else {
            PayloadLookup::Joined
        }
    }
}

/// The lookup of a wanted part: a hit when it was in memory when asked for,
/// else a load when this read ran it, else a join.
fn part_lookup(was_cached: bool, ran: bool) -> PayloadLookup {
    match (was_cached, ran) {
        (true, _) => PayloadLookup::Hit,
        (false, true) => PayloadLookup::Loaded,
        (false, false) => PayloadLookup::Joined,
    }
}

/// Generated visible levels by footprint, head and tail apart, each within
/// its own budget, least recently read evicted first.
///
/// Evicting is safe: a payload regenerates to the same bytes (R13), or to
/// none (black) once one of its recipes has been given up on, and never
/// changes the size. A payload is only built from a complete set of recipes.
pub(crate) struct PayloadMemory {
    head: PartCache,
    tail: PartCache,
    /// Loads in flight. A load keeps its parts before it leaves this map,
    /// and a read checks the parts again under the map's lock before
    /// starting one, so a read never loads what a finished load kept.
    loads: DashMap<FootprintKey, LoadSlot>,
}

impl PayloadMemory {
    /// Memory holding up to `head_bytes` of heads and `tail_bytes` of
    /// tails, counting evictions into `stats`.
    pub(crate) fn new(head_bytes: u64, tail_bytes: u64, stats: Arc<LitStats>) -> Self {
        Self {
            head: part_cache(PayloadPart::Head, head_bytes, Arc::clone(&stats)),
            tail: part_cache(PayloadPart::Tail, tail_bytes, stats),
            loads: DashMap::new(),
        }
    }

    fn cache(&self, part: PayloadPart) -> &PartCache {
        match part {
            PayloadPart::Head => &self.head,
            PayloadPart::Tail => &self.tail,
        }
    }

    /// The parts of `wanted` in memory now, each read (which moves it up
    /// the eviction order).
    fn cached(&self, key: FootprintKey, wanted: Wanted) -> LitParts {
        let mut parts = LitParts::default();
        for part in wanted.parts() {
            if let Some(bytes) = self.cache(part).get(&key) {
                parts.set(part, bytes);
            }
        }
        parts
    }

    /// Whether every part of `wanted` is in `parts`.
    fn has_all(parts: &LitParts, wanted: Wanted) -> bool {
        wanted.parts().all(|part| parts.get(part).is_some())
    }

    /// Keeps `part` of `payload` as a copy of its own, unless it is in
    /// memory already. A payload of another length than the layout's is
    /// never kept (loads give none: the disk tier checks the length, and
    /// generation follows the layout); it reads black.
    fn keep(&self, key: FootprintKey, layout: &LitLayout, payload: &Bytes, part: PayloadPart) {
        let cache = self.cache(part);
        if payload.len() as u64 == layout.payload_len() && !cache.contains_key(&key) {
            cache.insert(key, Bytes::copy_from_slice(&layout.part_of(payload, part)));
        }
    }

    /// The parts of `key`'s payload a caller of `class` wanting `wanted`
    /// is served from: from memory when they are all there, else from the
    /// footprint's load (`load`, run unless another caller's load is in
    /// flight, which this one joins, raising it to `class`). The load keeps
    /// the tail and, when this caller wants it, the head; a caller joining
    /// it keeps the head it wants itself. `load` is handed the load's
    /// priority, for its generation permit.
    pub(crate) async fn serve<F, Fut>(
        &self,
        key: FootprintKey,
        layout: &LitLayout,
        wanted: Wanted,
        class: GenerationClass,
        load: F,
    ) -> Served
    where
        F: FnOnce(Arc<LoadPriority>) -> Fut,
        Fut: Future<Output = Result<Bytes, Uncached>>,
    {
        let cached = self.cached(key, wanted);
        let was_cached = PayloadPart::ALL.map(|p| cached.get(p).is_some());
        let lookups = |ran| {
            PayloadPart::ALL.map(|p| {
                wanted
                    .contains(p)
                    .then(|| part_lookup(was_cached[p as usize], ran))
            })
        };
        if Self::has_all(&cached, wanted) {
            return Served {
                parts: cached,
                lookups: lookups(false),
            };
        }
        let slot = match self.loads.entry(key) {
            dashmap::Entry::Occupied(e) => {
                e.get().priority.raise(class);
                Arc::clone(e.get())
            }
            dashmap::Entry::Vacant(e) => {
                // A load that finished since kept its parts before leaving
                // the map: what it kept is seen here.
                let now = self.cached(key, wanted);
                if Self::has_all(&now, wanted) {
                    return Served {
                        parts: now,
                        lookups: lookups(false),
                    };
                }
                let load = Load {
                    value: tokio::sync::OnceCell::new(),
                    priority: Arc::new(LoadPriority::new(class)),
                };
                Arc::clone(e.insert(Arc::new(load)).value())
            }
        };
        let ran = AtomicBool::new(false);
        let loaded = slot
            .value
            .get_or_init(|| async {
                ran.store(true, Ordering::Relaxed);
                let loaded = Loaded::from(load(Arc::clone(&slot.priority)).await);
                // No await from here to the slot leaving the map: the parts
                // are kept before any later read can miss the slot.
                if let (true, Some(payload)) = (loaded.keep, &loaded.payload) {
                    self.keep(key, layout, payload, PayloadPart::Tail);
                    if wanted.contains(PayloadPart::Head) {
                        self.keep(key, layout, payload, PayloadPart::Head);
                    }
                }
                self.loads.remove_if(&key, |_, s| Arc::ptr_eq(s, &slot));
                loaded
            })
            .await
            .clone();
        let ran = ran.load(Ordering::Relaxed);
        if let (true, Some(payload), false) = (loaded.keep, &loaded.payload, ran) {
            for part in wanted.parts() {
                self.keep(key, layout, payload, part);
            }
        }
        let parts = loaded
            .payload
            .as_ref()
            .map_or_else(LitParts::default, |p| layout.parts_of(p));
        Served {
            parts,
            lookups: lookups(ran),
        }
    }

    /// Entries and bytes of each part now.
    pub(crate) fn occupancy(&self) -> PayloadOccupancy {
        PayloadOccupancy {
            head_entries: self.head.entry_count(),
            head_bytes: self.head.weighted_size(),
            tail_entries: self.tail.entry_count(),
            tail_bytes: self.tail.weighted_size(),
        }
    }

    /// Whether `part` of `key` is in memory (records no read).
    #[cfg(test)]
    pub(crate) fn contains(&self, part: PayloadPart, key: FootprintKey) -> bool {
        self.cache(part).contains_key(&key)
    }

    /// `part` of `key`, if in memory.
    #[cfg(test)]
    pub(crate) fn get(&self, part: PayloadPart, key: FootprintKey) -> Option<Bytes> {
        self.cache(part).get(&key)
    }

    /// Entries of both parts, once pending work has run.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> u64 {
        self.run_pending_tasks();
        self.head.entry_count() + self.tail.entry_count()
    }

    /// Applies pending inserts and evictions.
    #[cfg(test)]
    pub(crate) fn run_pending_tasks(&self) {
        self.head.run_pending_tasks();
        self.tail.run_pending_tasks();
    }

    /// Loads in flight.
    #[cfg(test)]
    pub(crate) fn loads_in_flight(&self) -> usize {
        self.loads.len()
    }
}

/// The memory of one part, holding up to `max_bytes` of it, counting its
/// evictions into `stats`.
///
/// LRU, not moka's default TinyLFU: once the cache is full, admission by
/// popularity would reject a part just loaded (asked for once) in favour of
/// parts read more often, and X-Plane's next read of its levels would load
/// or generate it again.
fn part_cache(part: PayloadPart, max_bytes: u64, stats: Arc<LitStats>) -> PartCache {
    moka::sync::Cache::builder()
        .eviction_policy(EvictionPolicy::lru())
        .max_capacity(max_bytes)
        .weigher(|_, v: &Bytes| v.len().try_into().unwrap_or(u32::MAX))
        .eviction_listener(move |_, _, cause| stats.record_payload_removal(part, cause))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use GenerationClass::{HeadRead, Pregeneration, TailRead};

    /// A ZL18 layout: a 256 px first visible level (32,768 bytes) and a
    /// 10,936-byte tail.
    fn layout() -> LitLayout {
        LitLayout::new(18, 4)
    }

    fn key(row: u32) -> FootprintKey {
        (row, 7, 18)
    }

    /// A payload whose bytes differ from one another.
    fn payload(l: &LitLayout) -> Bytes {
        (0..l.payload_len())
            .map(|i| (i.wrapping_mul(0x9E37_79B9) >> 13) as u8)
            .collect::<Vec<u8>>()
            .into()
    }

    fn memory(heads: u64, tails: u64) -> PayloadMemory {
        let l = layout();
        PayloadMemory::new(
            heads * l.part_len(PayloadPart::Head),
            tails * l.part_len(PayloadPart::Tail),
            Arc::default(),
        )
    }

    fn tail_only() -> Wanted {
        Wanted::reached(
            &layout(),
            layout().part_range(PayloadPart::Tail).start,
            4096,
        )
    }

    fn head_only() -> Wanted {
        Wanted::reached(&layout(), layout().payload_start(), 4096)
    }

    /// Serves `wanted` of `row`, counting loads into `loads`.
    async fn serve(m: &PayloadMemory, row: u32, wanted: Wanted, loads: &AtomicUsize) -> Served {
        let l = layout();
        let class = GenerationClass::of_read(wanted.contains(PayloadPart::Head));
        m.serve(key(row), &l, wanted, class, |_| async {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(payload(&l))
        })
        .await
    }

    #[test]
    fn a_read_wants_the_parts_its_range_overlaps() {
        let l = layout();
        let boundary = l.part_range(PayloadPart::Tail).start;
        assert_eq!(Wanted::reached(&l, 0, 128), Wanted::default());
        assert_eq!(Wanted::reached(&l, boundary, 8), Wanted([false, true]));
        assert_eq!(Wanted::reached(&l, boundary - 8, 8), Wanted([true, false]));
        assert_eq!(Wanted::reached(&l, boundary - 8, 16), Wanted::whole());
    }

    #[test]
    fn a_read_is_a_hit_only_when_every_part_it_wants_was_in_memory() {
        let served = |lookups| Served {
            parts: LitParts::default(),
            lookups,
        };
        use PayloadLookup::*;
        assert_eq!(served([None, Some(Hit)]).lookup(), Hit);
        assert_eq!(served([Some(Hit), Some(Hit)]).lookup(), Hit);
        assert_eq!(served([Some(Loaded), Some(Hit)]).lookup(), Loaded);
        assert_eq!(served([Some(Joined), Some(Hit)]).lookup(), Joined);
        assert_eq!(served([Some(Joined), None]).lookup(), Joined);
        assert_eq!(part_lookup(true, true), Hit);
        assert_eq!(part_lookup(false, true), Loaded);
        assert_eq!(part_lookup(false, false), Joined);
    }

    #[tokio::test]
    async fn a_load_for_the_small_levels_keeps_only_the_tail() {
        let (m, loads) = (memory(4, 4), AtomicUsize::new(0));
        let served = serve(&m, 0, tail_only(), &loads).await;
        assert_eq!(served.lookup(), PayloadLookup::Loaded);
        assert!(m.contains(PayloadPart::Tail, key(0)));
        assert!(
            !m.contains(PayloadPart::Head, key(0)),
            "the head is dropped"
        );
        assert_eq!(m.loads_in_flight(), 0, "the load left the map");
        let again = serve(&m, 0, tail_only(), &loads).await;
        assert_eq!(again.lookup(), PayloadLookup::Hit);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_head_read_after_a_tail_only_load_loads_again_and_keeps_its_head() {
        let (m, loads) = (memory(4, 4), AtomicUsize::new(0));
        serve(&m, 0, tail_only(), &loads).await;
        let served = serve(&m, 0, Wanted::whole(), &loads).await;
        assert_eq!(
            served.lookups,
            [Some(PayloadLookup::Loaded), Some(PayloadLookup::Hit)]
        );
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(m.contains(PayloadPart::Head, key(0)));
        let l = layout();
        let whole = payload(&l);
        assert_eq!(
            l.read_lit(0, u32::MAX, &served.parts),
            l.read_lit(0, u32::MAX, &l.parts_of(&whole))
        );
        assert_eq!(
            serve(&m, 0, head_only(), &loads).await.lookup(),
            PayloadLookup::Hit
        );
    }

    #[tokio::test]
    async fn the_tails_of_as_many_footprints_as_their_budget_holds_stay_in_memory() {
        // A budget of N tails and one head: N + 1 footprints read through
        // their small levels evict only the least recently read tail.
        const N: u32 = 6;
        let (m, loads) = (memory(1, u64::from(N)), AtomicUsize::new(0));
        for row in 0..=N {
            serve(&m, row, tail_only(), &loads).await;
            m.run_pending_tasks();
        }
        assert_eq!(loads.load(Ordering::SeqCst), N as usize + 1);
        for row in 1..=N {
            assert_eq!(
                serve(&m, row, tail_only(), &loads).await.lookup(),
                PayloadLookup::Hit,
                "{row}"
            );
        }
        assert_eq!(loads.load(Ordering::SeqCst), N as usize + 1, "no load");
        assert!(
            !m.contains(PayloadPart::Tail, key(0)),
            "the first made room"
        );
    }

    #[tokio::test]
    async fn a_full_part_memory_keeps_a_new_part_and_drops_the_least_recently_read() {
        // moka's default TinyLFU admits a new entry into a full cache only
        // when it has been asked for more often than the entry it would
        // evict: a payload just generated has been asked for once, so it lost
        // to payloads read more than once and was generated again on the
        // next read (#387).
        const HELD: u32 = 4;
        let (m, loads) = (memory(1, u64::from(HELD)), AtomicUsize::new(0));
        for row in 0..HELD {
            serve(&m, row, tail_only(), &loads).await;
        }
        m.run_pending_tasks(); // full: moka counts reads from here
        for _ in 0..2 {
            for row in 0..HELD {
                serve(&m, row, tail_only(), &loads).await;
            }
        }
        m.run_pending_tasks();
        serve(&m, HELD, tail_only(), &loads).await;
        m.run_pending_tasks();
        assert!(
            m.contains(PayloadPart::Tail, key(HELD)),
            "the new tail is kept"
        );
        assert!(
            !m.contains(PayloadPart::Tail, key(0)),
            "the least recently read tail makes room"
        );
        assert_eq!(m.entry_count(), u64::from(HELD));
    }

    #[tokio::test]
    async fn each_part_kept_is_an_allocation_of_its_own() {
        // Evicting the head frees its memory, and the weigher counts what
        // each part holds: neither part points into the loaded buffer, nor
        // into the other.
        let m = memory(4, 4);
        let l = layout();
        let whole = payload(&l);
        let given = whole.clone();
        m.serve(key(0), &l, Wanted::whole(), HeadRead, |_| async {
            Ok(given)
        })
        .await;
        let (head, tail) = (
            m.get(PayloadPart::Head, key(0)).unwrap(),
            m.get(PayloadPart::Tail, key(0)).unwrap(),
        );
        let loaded = whole.as_ptr_range();
        for (part, bytes) in [(PayloadPart::Head, &head), (PayloadPart::Tail, &tail)] {
            assert_eq!(bytes.len() as u64, l.part_len(part));
            let range = bytes.as_ptr_range();
            assert!(
                range.end <= loaded.start || range.start >= loaded.end,
                "{part:?}"
            );
        }
        let (h, t) = (head.as_ptr_range(), tail.as_ptr_range());
        assert!(h.end <= t.start || t.end <= h.start, "apart");
        assert!(
            whole.is_unique(),
            "memory keeps nothing of the loaded buffer"
        );
    }

    #[tokio::test]
    async fn a_payload_not_to_keep_is_served_and_kept_nowhere() {
        let m = memory(4, 4);
        let l = layout();
        for given in [Some(payload(&l)), None] {
            let lit = given.is_some();
            let served = m
                .serve(key(0), &l, Wanted::whole(), HeadRead, |_| async {
                    Err(Uncached(given))
                })
                .await;
            assert_eq!(served.parts.is_empty(), !lit);
            assert_eq!(m.entry_count(), 0);
            assert_eq!(m.loads_in_flight(), 0);
        }
    }

    #[tokio::test]
    async fn a_head_read_joining_a_tail_reads_load_keeps_the_head_from_it() {
        let (m, loads) = (memory(4, 4), AtomicUsize::new(0));
        let l = layout();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let tail_read = m.serve(key(0), &l, tail_only(), TailRead, |_| async {
            loads.fetch_add(1, Ordering::SeqCst);
            released.await.unwrap();
            Ok(payload(&l))
        });
        let head_read = async {
            // The tail read's load is in flight by now (join! polls in order).
            assert_eq!(m.loads_in_flight(), 1);
            let served = serve(&m, 0, head_only(), &loads).await;
            assert_eq!(served.lookup(), PayloadLookup::Joined);
            served
        };
        let ((tail, head), ()) =
            tokio::join!(async { tokio::join!(tail_read, head_read) }, async {
                tokio::task::yield_now().await;
                release.send(()).unwrap();
            });
        assert_eq!(tail.lookup(), PayloadLookup::Loaded);
        assert!(head.parts.get(PayloadPart::Head).is_some());
        assert_eq!(loads.load(Ordering::SeqCst), 1, "one load");
        assert!(m.contains(PayloadPart::Head, key(0)), "kept by the joiner");
        assert!(m.contains(PayloadPart::Tail, key(0)));
    }

    #[tokio::test]
    async fn a_read_joining_a_load_raises_its_priority_to_the_reads_class() {
        let m = memory(4, 4);
        let l = layout();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (seen_tx, seen) = tokio::sync::oneshot::channel::<Arc<LoadPriority>>();
        let whole = payload(&l);
        let pregen = m.serve(
            key(0),
            &l,
            tail_only(),
            Pregeneration,
            |priority| async move {
                seen_tx.send(priority).ok().unwrap();
                released.await.unwrap();
                Ok(whole)
            },
        );
        let joined = async {
            let priority = seen.await.unwrap();
            assert_eq!(priority.class(), Pregeneration);
            let loads = AtomicUsize::new(0);
            let read = serve(&m, 0, head_only(), &loads);
            futures::pin_mut!(read);
            assert!(futures::poll!(&mut read).is_pending(), "joined");
            assert_eq!(priority.class(), HeadRead);
            release.send(()).unwrap();
            let served = read.await;
            assert_eq!(loads.load(Ordering::SeqCst), 0, "no load of its own");
            served
        };
        let (pregen, read) = tokio::join!(pregen, joined);
        assert_eq!(pregen.lookup(), PayloadLookup::Loaded);
        assert_eq!(read.lookup(), PayloadLookup::Joined);
    }

    #[tokio::test]
    async fn a_dropped_load_leaves_the_next_read_to_load() {
        let (m, loads) = (memory(4, 4), AtomicUsize::new(0));
        let l = layout();
        {
            let pending = m.serve(key(0), &l, tail_only(), TailRead, |_| {
                std::future::pending()
            });
            futures::pin_mut!(pending);
            assert!(futures::poll!(pending).is_pending());
        }
        assert_eq!(m.loads_in_flight(), 1, "its slot stays, empty");
        let served = serve(&m, 0, tail_only(), &loads).await;
        assert_eq!(served.lookup(), PayloadLookup::Loaded);
        assert_eq!(m.loads_in_flight(), 0);
    }

    #[tokio::test]
    async fn evictions_are_counted_per_part() {
        let stats = Arc::new(LitStats::default());
        let l = layout();
        let m = PayloadMemory::new(
            l.part_len(PayloadPart::Head),
            2 * l.part_len(PayloadPart::Tail),
            stats.clone(),
        );
        let loads = AtomicUsize::new(0);
        for row in 0..3 {
            serve(&m, row, Wanted::whole(), &loads).await;
            m.run_pending_tasks();
        }
        let snap = stats.snapshot();
        assert_eq!(snap.payload_evictions(PayloadPart::Head), 2);
        assert_eq!(snap.payload_evictions(PayloadPart::Tail), 1);
        let o = m.occupancy();
        assert_eq!((o.head_entries, o.tail_entries), (1, 2));
        assert_eq!(o.head_bytes, l.part_len(PayloadPart::Head));
        assert_eq!(o.tail_bytes, 2 * l.part_len(PayloadPart::Tail));
    }
}
