//! Memory pinned by open virtual DDS handles, against a fixed ceiling.
//!
//! A FUSE handle memoises its tile so the 12 to 23 ranged reads the kernel
//! makes for one X-Plane texture resolve it once rather than each time (#234).
//! Each memoised tile pins its whole payload, so the number of concurrently
//! open textures bounds a real quantity of memory and the total needs a cap
//! (#236).
//!
//! This type owns that accounting. It is shared rather than private to the
//! filesystem because the same number answers two questions:
//!
//! - **May this open memoise its tile?** Asked by the FUSE layer, which
//!   degrades to per-call resolution rather than exceeding the ceiling.
//! - **Is X-Plane reading heavily right now?** Asked by prefetch, which yields
//!   when it is (#246). Open textures are the cheapest honest measure of
//!   demand on the mount: `open` and `release` reach the filesystem on every
//!   platform, where reads do not. macOS serves virtual DDS through the kernel
//!   page cache, because macFUSE faults when a `direct_io` file is `mmap`ed, so
//!   a read-rate signal would be accurate on Linux and nearly blind on macOS.
//!
//! The fraction is deliberately a ratio against the ceiling rather than a rate
//! over a window. It needs no sampling interval, no smoothing, and no threshold
//! of its own: it is already the 0 to 1 figure the prefetch backpressure branch
//! compares against `BACKPRESSURE_REDUCE_THRESHOLD`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Default ceiling on bytes pinned by memoised tiles.
///
/// 512 MB is about 46 concurrently open 11.17 MB textures. Sized against
/// observed flights: a boundary crossing on the reference leg reached 23 open
/// handles, so this is roughly double the worst case seen.
///
/// If `dds_budget_exhausted` on the `Memory sample` line is ever non-zero, a
/// real scene has exceeded what this was sized against. Raise it against
/// `dds_pinned_peak_mb` from that flight rather than by guessing again.
pub const DEFAULT_PINNED_TILE_CEILING: u64 = 512 * 1024 * 1024;

/// Tracks bytes pinned by memoised tiles against a ceiling.
///
/// Cheap to share: every operation is a relaxed atomic. Relaxed is sufficient
/// because no decision here orders other memory, and both readers tolerate a
/// slightly stale value. The FUSE check is racy by construction anyway, since
/// concurrent opens can both observe headroom.
#[derive(Debug)]
pub struct PinnedTileBudget {
    pinned: AtomicU64,
    peak: AtomicU64,
    ceiling: u64,
}

impl PinnedTileBudget {
    /// Create a budget with the default ceiling.
    pub fn new() -> Self {
        Self::with_ceiling(DEFAULT_PINNED_TILE_CEILING)
    }

    /// Create a budget with an explicit ceiling, for tests and tuning.
    pub fn with_ceiling(ceiling: u64) -> Self {
        Self {
            pinned: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            ceiling,
        }
    }

    /// Whether pinning `bytes` more would exceed the ceiling.
    ///
    /// Strictly greater, matching the original check: a claim that lands
    /// exactly on the ceiling is allowed.
    pub fn would_exceed(&self, bytes: u64) -> bool {
        self.pinned_bytes().saturating_add(bytes) > self.ceiling
    }

    /// Record `bytes` as pinned, updating the peak.
    pub fn pin(&self, bytes: u64) {
        let now = self.pinned.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Release `bytes` previously pinned.
    ///
    /// Saturating: a double release must not wrap the counter into a value that
    /// would make the budget look permanently exhausted.
    pub fn release(&self, bytes: u64) {
        let _ = self
            .pinned
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(bytes))
            });
    }

    /// Bytes currently pinned.
    pub fn pinned_bytes(&self) -> u64 {
        self.pinned.load(Ordering::Relaxed)
    }

    /// Highest value `pinned_bytes` has reached.
    ///
    /// The instantaneous figure returns to zero between bursts, so the peak is
    /// what the ceiling actually has to cover.
    pub fn peak_bytes(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// The ceiling.
    pub fn ceiling_bytes(&self) -> u64 {
        self.ceiling
    }

    /// Pinned bytes as a fraction of the ceiling, clamped to 0.0 to 1.0.
    ///
    /// This is the prefetch backpressure signal. Clamped because the ceiling is
    /// advisory: concurrent opens can each see headroom and both proceed, so
    /// the pinned total can momentarily exceed it.
    pub fn fraction(&self) -> f64 {
        if self.ceiling == 0 {
            return 0.0;
        }
        (self.pinned_bytes() as f64 / self.ceiling as f64).clamp(0.0, 1.0)
    }
}

impl Default for PinnedTileBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_budget_is_empty_and_unpressured() {
        let budget = PinnedTileBudget::new();

        assert_eq!(budget.pinned_bytes(), 0);
        assert_eq!(budget.peak_bytes(), 0);
        assert_eq!(budget.fraction(), 0.0);
        assert_eq!(budget.ceiling_bytes(), DEFAULT_PINNED_TILE_CEILING);
    }

    #[test]
    fn pin_and_release_track_the_current_total() {
        let budget = PinnedTileBudget::with_ceiling(100);

        budget.pin(30);
        budget.pin(20);
        assert_eq!(budget.pinned_bytes(), 50);

        budget.release(30);
        assert_eq!(budget.pinned_bytes(), 20);
    }

    #[test]
    fn peak_records_the_high_water_mark_not_the_current_value() {
        // The gauge returns to zero between bursts, so only the peak says what
        // the ceiling had to cover.
        let budget = PinnedTileBudget::with_ceiling(100);

        budget.pin(80);
        budget.release(80);

        assert_eq!(budget.pinned_bytes(), 0);
        assert_eq!(budget.peak_bytes(), 80);
    }

    #[test]
    fn would_exceed_allows_a_claim_that_lands_exactly_on_the_ceiling() {
        // Preserves the original `pinned + expected > ceiling` comparison. An
        // off-by-one here would refuse memoisation one tile early.
        let budget = PinnedTileBudget::with_ceiling(100);
        budget.pin(60);

        assert!(!budget.would_exceed(40), "60 + 40 == 100 must be allowed");
        assert!(budget.would_exceed(41), "60 + 41 > 100 must be refused");
    }

    #[test]
    fn would_exceed_saturates_rather_than_wrapping() {
        let budget = PinnedTileBudget::with_ceiling(100);
        budget.pin(10);

        assert!(budget.would_exceed(u64::MAX));
    }

    #[test]
    fn release_of_more_than_is_pinned_cannot_wrap() {
        // An unbalanced release must not underflow: a wrapped counter would
        // read as near u64::MAX and make the budget look permanently full,
        // which would silently disable memoisation for the rest of the mount.
        let budget = PinnedTileBudget::with_ceiling(100);
        budget.pin(10);

        budget.release(50);

        assert_eq!(budget.pinned_bytes(), 0);
        assert!(!budget.would_exceed(100));
    }

    #[test]
    fn fraction_is_the_ratio_against_the_ceiling() {
        let budget = PinnedTileBudget::with_ceiling(1000);

        budget.pin(500);
        assert_eq!(budget.fraction(), 0.5);

        budget.pin(300);
        assert_eq!(budget.fraction(), 0.8);
    }

    #[test]
    fn fraction_is_clamped_when_the_advisory_ceiling_is_overshot() {
        // Concurrent opens can each observe headroom and both proceed, so the
        // total can pass the ceiling. The signal must stay in range.
        let budget = PinnedTileBudget::with_ceiling(100);

        budget.pin(250);

        assert_eq!(budget.fraction(), 1.0);
    }

    #[test]
    fn a_zero_ceiling_reports_no_pressure_rather_than_dividing_by_zero() {
        let budget = PinnedTileBudget::with_ceiling(0);
        budget.pin(10);

        assert_eq!(budget.fraction(), 0.0);
    }

    #[test]
    fn the_reference_leg_spike_lands_on_the_reduce_threshold() {
        // Calibration check, from the trace on #246: a boundary crossing that
        // reached 23 open handles is the collision this signal exists to catch.
        // At the default ceiling that is ~0.50, which trips the prefetch REDUCE
        // branch and stays below DEFER. If the ceiling changes, this is the
        // test that says what it did to the throttle.
        const TILE_BYTES: u64 = 11_170_000;
        let budget = PinnedTileBudget::new();

        for _ in 0..23 {
            budget.pin(TILE_BYTES);
        }

        let fraction = budget.fraction();
        assert!(
            (0.45..0.55).contains(&fraction),
            "23 open handles should read about half the ceiling, got {fraction}"
        );
    }
}
