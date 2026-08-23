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
