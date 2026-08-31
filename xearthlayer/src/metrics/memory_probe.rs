//! Process memory sampling.
//!
//! Provides a mockable abstraction over reading the current process's memory
//! footprint. The production implementation is backed by the `memory-stats`
//! crate, which supports Linux, macOS and Windows; on Unix its only dependency
//! is `libc`, already in this crate's tree.

/// A point-in-time sample of the process's memory footprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySample {
    /// Resident set size in bytes (physical memory currently held).
    pub rss_bytes: u64,
    /// Virtual memory size in bytes (address space reserved).
    pub vm_bytes: u64,
    /// OS thread count. `None` on platforms where it is not read.
    pub threads: Option<u64>,
    /// Anonymous memory swapped out to disk, in bytes. `None` on platforms
    /// where it is not read.
    ///
    /// This is the field that matters most for diagnosing issue #209: in the
    /// OOM flight, 54.7 GB of 64 GB anonymous memory was swapped out while
    /// `rss_bytes` alone read a healthy 9.3 GB. A trace that only logs
    /// `rss_bytes` cannot see a process swapping itself to death.
    pub swap_bytes: Option<u64>,
    /// Anonymous resident memory in bytes, from `/proc/self/status` `RssAnon`.
    /// `None` where not readable.
    ///
    /// `anon_bytes + swap_bytes` is the process's **committed** footprint --
    /// what the OOM killer scores. Prefer it to `vm_bytes`, which also counts
    /// address space that is mapped but never touched: thread stacks alone
    /// measured 2.21 MB per thread over a pool cycling 37..586 threads, so
    /// `vm_bytes` carries a ~1 GB sawtooth that is not memory at all (#227).
    pub anon_bytes: Option<u64>,
    /// glibc's own accounting, `None` on non-glibc platforms.
    pub allocator: Option<AllocatorSample>,
}

/// glibc allocator accounting, from `mallinfo2`.
///
/// **The fields are not interchangeable, and the obvious formula is wrong.**
/// `in_use_bytes` (`uordblks`) counts *arena* chunks only; a block served by
/// `mmap` never appears in it. Measured on glibc 2.44, an 11 MiB allocation --
/// DDS-tile sized -- moves `hblkhd` by 11538432 and `uordblks` by 1968. So
/// `(heap + mmapped) - in_use` reports every live mmapped tile as retention,
/// which is both wrong and believable.
///
/// What each field actually answers for #227:
///
/// - `heap_free_bytes` (`fordblks`) -- free space glibc holds inside arenas.
///   **This is the retention signal.** Arena memory returns to the OS only via
///   `malloc_trim` or a free heap top.
/// - `mmapped_bytes` (`hblkhd`) -- currently mmapped. glibc unmaps these on
///   free, so this tracks live large allocations, not retention.
/// - `heap_bytes` (`arena`) rising while `in_use_bytes` stays flat is arena
///   growth the program is not using.
///
/// Watch for a **transition**: glibc's mmap threshold starts at 128 KB and
/// adapts upward to 32 MB as mmapped blocks are freed. Once it passes 11 MiB,
/// DDS buffers stop coming from `mmap` and start coming from arenas -- at
/// which point they are no longer returned to the OS on free. That is
/// candidate 2 stated precisely, and it would show as `mmapped_bytes` falling
/// while `heap_bytes` and `heap_free_bytes` climb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorSample {
    /// Bytes obtained from `sbrk` (`mallinfo2.arena`).
    pub heap_bytes: u64,
    /// Bytes obtained from `mmap` for large allocations (`mallinfo2.hblkhd`).
    pub mmapped_bytes: u64,
    /// Bytes currently allocated to the program (`mallinfo2.uordblks`).
    pub in_use_bytes: u64,
    /// Free bytes held inside arenas (`mallinfo2.fordblks`) -- fragmentation.
    pub heap_free_bytes: u64,
}

impl AllocatorSample {
    /// Reads glibc's accounting. `None` where `mallinfo2` is unavailable.
    ///
    /// `mallinfo2` takes every arena's lock, so it is not free -- at the 60s
    /// sample cadence that is immaterial, but do not call it on a hot path.
    pub fn read() -> Option<Self> {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            // SAFETY: mallinfo2 takes no arguments and returns a plain struct
            // of integers. It is thread-safe; glibc locks the arenas itself.
            let mi = unsafe { libc::mallinfo2() };
            Some(Self {
                heap_bytes: mi.arena as u64,
                mmapped_bytes: mi.hblkhd as u64,
                in_use_bytes: mi.uordblks as u64,
                heap_free_bytes: mi.fordblks as u64,
            })
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            None
        }
    }

    /// Bytes glibc holds from the OS inside arenas without handing them to the
    /// program -- the retention signal for #227.
    ///
    /// This is `fordblks` alone. It deliberately excludes `hblkhd`: mmapped
    /// blocks are unmapped on free, so live ones are not retention.
    pub fn arena_retained_bytes(&self) -> u64 {
        self.heap_free_bytes
    }

    /// Total bytes obtained from the OS, by either route.
    pub fn obtained_bytes(&self) -> u64 {
        self.heap_bytes + self.mmapped_bytes
    }
}

/// Reads the current process's memory footprint.
///
/// Implemented as a trait so the metrics daemon can be tested without reading
/// real process memory.
pub trait MemoryProbe: Send + Sync {
    /// Returns the current sample, or `None` if the platform cannot supply one.
    fn sample(&self) -> Option<MemorySample>;
}

/// Production probe backed by the `memory-stats` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessMemoryProbe;

/// Serializes the first call to `memory_stats()` to work around a race in memory-stats 1.2.0.
///
/// memory-stats uses an atomic CAS to guard init on Linux. A concurrent loser thread may read
/// the uninitialized state before the winner has written it, causing PAGE_SIZE to remain 0 and
/// both physical_mem and virtual_mem to come out as 0. Calling once here ensures the static
/// init completes before any concurrent use in the test suite or daemon.
static MEMORY_STATS_INIT: std::sync::Once = std::sync::Once::new();

impl ProcessMemoryProbe {
    /// Creates a new probe.
    pub fn new() -> Self {
        Self
    }

    /// Reads OS thread count and swapped-out anonymous memory in one pass
    /// over `/proc/self/status`.
    ///
    /// Linux-only and best-effort: `memory-stats` does not expose either
    /// value, and hand-rolling mach FFI for macOS is not worth the risk on a
    /// blocking CI platform. Thread count is tracked because tokio's blocking
    /// pool (default 512 threads) is implicated in issue #209; swap is
    /// tracked because it is the field that actually caught #209 — see
    /// `MemorySample::swap_bytes`.
    ///
    /// Both fields are read from the same file read so the file is only
    /// opened once per sample.
    #[cfg(target_os = "linux")]
    fn linux_status_fields() -> (Option<u64>, Option<u64>, Option<u64>) {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return (None, None, None);
        };

        let mut threads = None;
        let mut swap_bytes = None;
        let mut anon_bytes = None;

        for line in status.lines() {
            if let Some(value) = line.strip_prefix("Threads:") {
                threads = value.trim().parse().ok();
            } else if let Some(value) = line.strip_prefix("VmSwap:") {
                // Format is like "      0 kB" - strip the "kB" suffix, then
                // the value is already in kB per the /proc/self/status contract.
                swap_bytes = value
                    .trim()
                    .strip_suffix("kB")
                    .and_then(|kb| kb.trim().parse::<u64>().ok())
                    .map(|kb| kb * 1024);
            } else if let Some(value) = line.strip_prefix("RssAnon:") {
                anon_bytes = value
                    .trim()
                    .strip_suffix("kB")
                    .and_then(|kb| kb.trim().parse::<u64>().ok())
                    .map(|kb| kb * 1024);
            }
        }

        (threads, swap_bytes, anon_bytes)
    }

    #[cfg(not(target_os = "linux"))]
    fn linux_status_fields() -> (Option<u64>, Option<u64>, Option<u64>) {
        (None, None, None)
    }
}

impl MemoryProbe for ProcessMemoryProbe {
    fn sample(&self) -> Option<MemorySample> {
        // Serialize the first call to memory_stats to ensure upstream initialization completes
        // before concurrent use (see MEMORY_STATS_INIT doc comment).
        MEMORY_STATS_INIT.call_once(|| {
            let _ = memory_stats::memory_stats();
        });

        let stats = memory_stats::memory_stats()?;
        let (threads, swap_bytes, anon_bytes) = Self::linux_status_fields();
        Some(MemorySample {
            rss_bytes: stats.physical_mem as u64,
            vm_bytes: stats.virtual_mem as u64,
            threads,
            swap_bytes,
            anon_bytes,
            allocator: AllocatorSample::read(),
        })
    }
}

/// Asks glibc to return free arena memory to the OS, and logs what moved.
///
/// A pure measurement, called once at shutdown: the answer is how much of the
/// footprint was glibc holding free memory rather than the process holding
/// objects. Freed arena memory is normally released only when the heap top is
/// free, so a large drop here is direct evidence for candidate 2 of #227, and
/// no drop points at genuine retention.
///
/// Not for periodic use -- `malloc_trim` walks every arena taking locks.
pub fn log_malloc_trim_at_shutdown() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const MB: u64 = 1024 * 1024;
        let probe = ProcessMemoryProbe;
        let (Some(before), Some(before_alloc)) = (probe.sample(), AllocatorSample::read()) else {
            return;
        };

        // SAFETY: malloc_trim takes a pad in bytes and is thread-safe; glibc
        // locks the arenas itself.
        let released = unsafe { libc::malloc_trim(0) };

        let (Some(after), Some(after_alloc)) = (probe.sample(), AllocatorSample::read()) else {
            return;
        };

        let anon_before = before.anon_bytes.unwrap_or(before.rss_bytes);
        let anon_after = after.anon_bytes.unwrap_or(after.rss_bytes);

        tracing::info!(
            trim_released_any = released == 1,
            anon_before_mb = anon_before / MB,
            anon_after_mb = anon_after / MB,
            anon_freed_mb = anon_before.saturating_sub(anon_after) / MB,
            heap_free_before_mb = before_alloc.arena_retained_bytes() / MB,
            heap_free_after_mb = after_alloc.arena_retained_bytes() / MB,
            heap_before_mb = before_alloc.heap_bytes / MB,
            heap_after_mb = after_alloc.heap_bytes / MB,
            "malloc_trim at shutdown"
        );
    }
}

/// Logs glibc allocator overrides when any are set.
///
/// These change how freed memory is returned to the OS, so a trace gathered
/// with them set is not comparable to one gathered without. Recording them
/// prevents silently comparing incomparable runs.
pub fn log_allocator_environment() {
    let overrides: Vec<String> = [
        "MALLOC_ARENA_MAX",
        "MALLOC_MMAP_THRESHOLD_",
        "MALLOC_TRIM_THRESHOLD_",
    ]
    .iter()
    .filter_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| format!("{}={}", key, value))
    })
    .collect();

    if !overrides.is_empty() {
        tracing::info!(
            allocator_env = %overrides.join(" "),
            "Allocator environment overrides active"
        );
    }
}

/// Test double returning fixed values.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticMemoryProbe {
    sample: Option<MemorySample>,
}

#[cfg(test)]
impl StaticMemoryProbe {
    /// Creates a probe returning the given byte counts, 42 threads, and zero swap.
    pub(crate) fn new(rss_bytes: u64, vm_bytes: u64) -> Self {
        Self {
            sample: Some(MemorySample {
                rss_bytes,
                vm_bytes,
                threads: Some(42),
                swap_bytes: Some(0),
                anon_bytes: Some(rss_bytes),
                allocator: None,
            }),
        }
    }

    /// Overrides the configured allocator accounting.
    ///
    /// No-op if the probe was built with [`Self::unavailable`].
    pub(crate) fn with_allocator(mut self, allocator: AllocatorSample) -> Self {
        if let Some(sample) = self.sample.as_mut() {
            sample.allocator = Some(allocator);
        }
        self
    }

    /// Overrides the configured anonymous-resident byte count.
    ///
    /// No-op if the probe was built with [`Self::unavailable`].
    pub(crate) fn with_anon_bytes(mut self, anon_bytes: u64) -> Self {
        if let Some(sample) = self.sample.as_mut() {
            sample.anon_bytes = Some(anon_bytes);
        }
        self
    }

    /// Overrides the configured swap byte count.
    ///
    /// No-op if the probe was built with [`Self::unavailable`].
    pub(crate) fn with_swap_bytes(mut self, swap_bytes: u64) -> Self {
        if let Some(sample) = self.sample.as_mut() {
            sample.swap_bytes = Some(swap_bytes);
        }
        self
    }

    /// Creates a probe that always fails to sample.
    pub(crate) fn unavailable() -> Self {
        Self { sample: None }
    }
}

#[cfg(test)]
impl MemoryProbe for StaticMemoryProbe {
    fn sample(&self) -> Option<MemorySample> {
        self.sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of #227's next flight: glibc's own accounting must be
    /// readable, and the retention gap must be derivable from it.
    #[test]
    fn test_allocator_sample_reads_glibc_accounting() {
        let Some(a) = AllocatorSample::read() else {
            panic!("mallinfo2 must be available on a glibc Linux build");
        };
        // A running test process has allocated *something* from somewhere.
        assert!(
            a.heap_bytes + a.mmapped_bytes > 0,
            "allocator reports no memory obtained from the OS"
        );
        assert!(a.in_use_bytes > 0, "allocator reports nothing in use");
        // glibc's own invariant: an arena is its in-use plus its free space.
        // If this breaks, the struct is being misread (usually a layout or
        // signedness mistake), and every other figure is untrustworthy.
        assert!(
            a.in_use_bytes + a.heap_free_bytes >= a.heap_bytes,
            "arena {} exceeds in_use {} + free {}",
            a.heap_bytes,
            a.in_use_bytes,
            a.heap_free_bytes
        );
    }

    /// Retention is the gap between what glibc took and what we hold.
    #[test]
    fn test_arena_retention_excludes_live_mmapped_blocks() {
        let a = AllocatorSample {
            heap_bytes: 900,
            mmapped_bytes: 100_000,
            in_use_bytes: 250,
            heap_free_bytes: 650,
        };
        // Not 100_650: a live mmapped block is in use, not retained. The naive
        // `obtained - in_use` would report 100_650 here.
        assert_eq!(a.arena_retained_bytes(), 650);
        assert_eq!(a.obtained_bytes(), 100_900);
    }

    /// An explicit user setting must win over our pin, because `mallopt`
    /// silently overrides what glibc already read from the environment.
    /// Measured on glibc 2.44 with 64 threads: `MALLOC_ARENA_MAX=64` alone
    /// gives a 23 MB arena, but `MALLOC_ARENA_MAX=64` plus `mallopt(4)` gives
    /// 1 MB — the user's setting vanishes with no diagnostic.
    #[test]
    fn an_explicit_env_setting_defers_our_pin() {
        assert!(user_pinned(Some("8"), None, TUNABLE_ARENA_MAX));
    }

    /// `GLIBC_TUNABLES` is the second way to set the same parameter, and
    /// `mallopt` overrides it just as silently.
    #[test]
    fn a_tunables_setting_defers_our_pin() {
        assert!(user_pinned(
            None,
            Some("glibc.malloc.arena_max=8"),
            TUNABLE_ARENA_MAX
        ));
    }

    /// A tunables string that names some *other* parameter must not stop us
    /// pinning this one. A naive `is_some()` on the variable would.
    #[test]
    fn unrelated_tunables_do_not_defer_our_pin() {
        assert!(!user_pinned(
            None,
            Some("glibc.malloc.tcache_max=64:glibc.malloc.arena_test=2"),
            TUNABLE_ARENA_MAX
        ));
    }

    /// With nothing set, we pin.
    #[test]
    fn an_empty_environment_lets_us_pin() {
        assert!(!user_pinned(None, None, TUNABLE_ARENA_MAX));
    }

    /// The cap must be well below glibc's default of `8 x ncores`, or pinning
    /// it achieves nothing. Four is the value flown on 2026-08-25.
    #[test]
    fn arena_max_is_meaningfully_below_the_glibc_default() {
        let glibc_default = 8 * std::thread::available_parallelism().map_or(4, |p| p.get());
        assert!(ARENA_MAX >= 2, "at least two arenas, or contention bites");
        assert!(
            (ARENA_MAX as usize) < glibc_default,
            "ARENA_MAX {} must be below glibc's default {}",
            ARENA_MAX,
            glibc_default
        );
    }

    /// The whole point of pinning the threshold: after `configure_allocator`,
    /// DDS-sized allocations must keep going through `mmap` however many
    /// allocate/free cycles precede them.
    ///
    /// Without it, glibc's threshold adapts past 11 MiB after **one** cycle and
    /// tiles start coming from arenas, where freeing them returns nothing to
    /// the OS. Measured on glibc 2.44: holding 44 MiB gives hblkhd 44.4 MB on
    /// the first round, then 0.4 MB with arena 45.6 MB on every round after.
    ///
    /// Asserted as a lower bound while holding the memory. `mallinfo2` is
    /// process-global and cargo runs tests in parallel, so a delta here is not
    /// this test's to measure -- see `test_allocator_sample_reports_live_allocations`.
    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn test_configure_allocator_keeps_large_blocks_off_the_arena() {
        const MIB: usize = 1024 * 1024;

        // Drive the adaptation the way the tile pipeline does. Without the pin
        // this alone moves subsequent 11 MiB blocks into the arena.
        for _ in 0..4 {
            let churn = vec![3u8; 11 * MIB];
            std::hint::black_box(&churn);
            drop(churn);
        }

        // Environment override wins by design, so this returns false under an
        // allocator experiment. Either way the threshold ends up pinned.
        let _ = configure_allocator();

        let tiles: Vec<Vec<u8>> = (0..12).map(|_| vec![9u8; 11 * MIB]).collect();
        let held = std::hint::black_box(&tiles).len() * 11 * MIB;
        let a = AllocatorSample::read().expect("glibc");

        assert!(
            a.mmapped_bytes as usize >= held * 3 / 4,
            "holding {held} bytes of DDS-sized buffers after four churn cycles, \
             glibc reports only {} mmapped (arena {}). The threshold adapted, \
             which is exactly what configure_allocator must prevent.",
            a.mmapped_bytes,
            a.heap_bytes
        );
        drop(tiles);
    }

    /// The probe must report real figures, not a constant or a stale read.
    ///
    /// Asserted as a **lower bound while holding** the memory, never as a
    /// delta. `mallinfo2` is process-global and cargo runs tests in parallel
    /// threads, so a before/after difference is not the test's to measure --
    /// a sibling test freeing its buffers in between makes `obtained_bytes`
    /// fall across an allocation. That version failed 8 runs in 12.
    ///
    /// Other tests can only ever *add* to these figures, so a floor holds.
    #[test]
    fn test_allocator_sample_reports_live_allocations() {
        const MIB: usize = 1024 * 1024;
        // DDS-sized blocks, which glibc serves by mmap: an 11 MiB allocation
        // moves hblkhd by 11538432 and uordblks by 1968 on glibc 2.44.
        let tiles: Vec<Vec<u8>> = (0..12).map(|_| vec![7u8; 11 * MIB]).collect();
        let held = std::hint::black_box(&tiles).len() * 11 * MIB;

        // Assert on the total, never on which route glibc chose. Its mmap
        // threshold adapts upward after a single alloc/free of this size, so
        // an identical allocation lands in `hblkhd` early in a process and in
        // `arena` later. A test that named the route failed 25 runs in 25.
        let a = AllocatorSample::read().expect("glibc");
        assert!(
            a.obtained_bytes() as usize >= held,
            "obtained {} is below the {held} bytes held live (heap {}, mmapped {})",
            a.obtained_bytes(),
            a.heap_bytes,
            a.mmapped_bytes
        );
        drop(tiles);
    }

    /// `anon_bytes` is what makes committed memory readable without arithmetic.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_probe_reports_anonymous_resident_memory() {
        let sample = ProcessMemoryProbe.sample().expect("probe must work here");
        let anon = sample.anon_bytes.expect("linux always exposes RssAnon");
        assert!(anon > 0, "process reports no anonymous resident memory");
        assert!(
            anon <= sample.rss_bytes,
            "anonymous {} cannot exceed total resident {}",
            anon,
            sample.rss_bytes
        );
    }

    #[test]
    fn process_probe_reports_nonzero_memory() {
        let probe = ProcessMemoryProbe::new();
        let sample = probe
            .sample()
            .expect("probe must work on supported platforms");
        assert!(sample.rss_bytes > 0, "rss_bytes should be positive");
        assert!(sample.vm_bytes > 0, "vm_bytes should be positive");
    }

    #[test]
    fn static_probe_returns_configured_values() {
        let probe = StaticMemoryProbe::new(1024, 2048);
        let sample = probe.sample().unwrap();
        assert_eq!(sample.rss_bytes, 1024);
        assert_eq!(sample.vm_bytes, 2048);
        assert_eq!(sample.swap_bytes, Some(0));
    }

    #[test]
    fn static_probe_swap_bytes_can_be_overridden() {
        let probe = StaticMemoryProbe::new(1024, 2048).with_swap_bytes(9_999);
        let sample = probe.sample().unwrap();
        assert_eq!(sample.swap_bytes, Some(9_999));
    }

    #[test]
    fn unavailable_probe_returns_none() {
        assert!(StaticMemoryProbe::unavailable().sample().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_probe_reports_thread_count() {
        let sample = ProcessMemoryProbe::new().sample().unwrap();
        assert!(
            sample.threads.unwrap_or(0) >= 1,
            "linux should report >= 1 thread"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_probe_reports_swap_bytes() {
        let sample = ProcessMemoryProbe::new().sample().unwrap();
        assert!(
            sample.swap_bytes.is_some(),
            "linux should always expose VmSwap, even if the value is 0"
        );
    }
}

// =============================================================================
// Allocator configuration
// =============================================================================

/// Cap above which glibc must serve an allocation with `mmap` rather than from
/// an arena: 1 MiB.
///
/// A DDS tile is 11.17 MiB. glibc's mmap threshold *starts* at 128 KiB and
/// adapts **upward** — as far as 32 MiB — each time an mmapped block is freed.
/// After a single tile allocate/free cycle it has already passed 11 MiB, and
/// from then on tiles come from arenas instead. Arena memory is returned to the
/// OS only when the heap top is free or `malloc_trim` runs, so every arena that
/// ever served a tile keeps that space for the life of the process.
///
/// Measured on glibc 2.44, 64 threads allocating and freeing 11 MiB buffers:
///
/// | configuration | anonymous resident retained |
/// |---|---|
/// | default | 191.5 MB |
/// | `MALLOC_ARENA_MAX=2` | 70.4 MB |
/// | threshold pinned to 1 MiB | **4.6 MB** |
///
/// The cost is one `mmap`/`munmap` pair per tile, measured at **+0.16 ms** —
/// about 20 seconds of CPU across an 11-hour flight. See issue #227.
const MMAP_THRESHOLD_BYTES: usize = 1024 * 1024;

/// Maximum number of glibc malloc arenas: 4.
///
/// glibc gives each thread an arena to avoid lock contention, defaulting to
/// `8 x ncores` — 256 on a 32-core host. A thread keeps its arena, each arena
/// grows to cover the worst burst *it* ever saw, and glibc returns memory only
/// by trimming the **top** of a heap, so space stranded below a live
/// allocation stays committed. Total arena is therefore the sum of every
/// arena's personal high-water mark, and with 256 available a burst can always
/// recruit fresh ones — which is why it never converges.
///
/// Measured over an 11-hour flight on the default ceiling: the arena reached
/// 6,091 MB while holding 1,002 MB of live data, growing in 18 discrete steps
/// that were still occurring at hour 10.75. Capping the count removes the
/// supply: once every arena has seen its worst burst the sum saturates.
///
/// On the same route with this cap, the arena reached 3,916 MB within six
/// minutes and then held to the byte — 151 consecutive samples, 26,360
/// encodes, zero drift. That figure is peak concurrent demand (3,093 MB
/// measured) plus about 27% fragmentation headroom: the arena is now sized by
/// what the workload needs at once rather than by its history.
///
/// Four rather than two keeps a contention margin. The largest and most
/// frequent allocations already bypass arenas entirely via
/// [`MMAP_THRESHOLD_BYTES`], so what remains is one dominant size class of
/// ~256 KiB chunk buffers — close to the best case for a low arena count.
/// See issue #227.
const ARENA_MAX: i32 = 4;

/// The `GLIBC_TUNABLES` key that sets the arena ceiling.
const TUNABLE_ARENA_MAX: &str = "glibc.malloc.arena_max";

/// The `GLIBC_TUNABLES` key that sets the mmap threshold.
const TUNABLE_MMAP_THRESHOLD: &str = "glibc.malloc.mmap_threshold";

/// Whether the user has already pinned this parameter themselves.
///
/// glibc reads both `MALLOC_*` variables and `GLIBC_TUNABLES` before `main`,
/// but a later `mallopt` silently overrides either. Measured on glibc 2.44:
/// `MALLOC_ARENA_MAX=64` alone yields a 23 MB arena; the same variable with a
/// subsequent `mallopt(M_ARENA_MAX, 4)` yields 1 MB. Without this check a user
/// running an allocator experiment would have their setting discarded with no
/// diagnostic.
fn user_pinned(env_value: Option<&str>, tunables: Option<&str>, tunable_key: &str) -> bool {
    env_value.is_some() || tunables.is_some_and(|t| t.contains(tunable_key))
}

/// Which allocator parameters [`configure_allocator`] pinned.
///
/// A parameter is `false` when the user set it themselves, when `mallopt`
/// rejected it, or when the target is not linux-gnu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorConfig {
    /// glibc's mmap threshold was pinned to [`MMAP_THRESHOLD_BYTES`].
    pub mmap_threshold_pinned: bool,
    /// glibc's arena ceiling was pinned to [`ARENA_MAX`].
    pub arena_max_pinned: bool,
}

/// Pins glibc's mmap threshold so large allocations are never served from an
/// arena, and are therefore returned to the OS when freed.
///
/// Returns `true` if the threshold was applied. Call once, early, before the
/// process allocates in earnest.
///
/// Setting this parameter through `mallopt` also **disables glibc's dynamic
/// adjustment** of the threshold, which is the point: a lower starting value
/// alone would adapt straight back up.
///
/// An explicit `MALLOC_MMAP_THRESHOLD_` in the environment wins — glibc has
/// already applied it at startup, and overriding it here would silently break
/// anyone measuring an allocator experiment.
pub fn configure_allocator() -> AllocatorConfig {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let tunables = std::env::var("GLIBC_TUNABLES").ok();
        let mmap_env = std::env::var("MALLOC_MMAP_THRESHOLD_").ok();
        let arena_env = std::env::var("MALLOC_ARENA_MAX").ok();

        AllocatorConfig {
            mmap_threshold_pinned: pin(
                libc::M_MMAP_THRESHOLD,
                MMAP_THRESHOLD_BYTES as i32,
                "mmap threshold",
                user_pinned(
                    mmap_env.as_deref(),
                    tunables.as_deref(),
                    TUNABLE_MMAP_THRESHOLD,
                ),
            ),
            arena_max_pinned: pin(
                libc::M_ARENA_MAX,
                ARENA_MAX,
                "arena ceiling",
                user_pinned(arena_env.as_deref(), tunables.as_deref(), TUNABLE_ARENA_MAX),
            ),
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        AllocatorConfig {
            mmap_threshold_pinned: false,
            arena_max_pinned: false,
        }
    }
}

/// Apply one `mallopt` parameter unless the user already set it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn pin(param: libc::c_int, value: i32, label: &str, deferred: bool) -> bool {
    if deferred {
        tracing::info!(
            parameter = label,
            "Set in the environment; leaving glibc's value alone"
        );
        return false;
    }

    // SAFETY: mallopt takes two ints and is thread-safe; glibc locks
    // internally. Both parameters used here are documented.
    let applied = unsafe { libc::mallopt(param, value) };
    if applied == 1 {
        tracing::debug!(parameter = label, value, "Pinned glibc allocator parameter");
        true
    } else {
        tracing::warn!(
            parameter = label,
            value,
            "mallopt was rejected; allocator retention is not bounded"
        );
        false
    }
}
