//! Prefetch plan execution with backpressure and throttle control.
//!
//! Extracted from `core.rs` to isolate the submission logic that
//! manages backpressure thresholds, transition throttle, and
//! channel-full handling.

use crate::coord::TileCoord;
use crate::executor::DdsClient;

use super::super::strategy::PrefetchPlan;
use super::super::transition_throttle::TransitionThrottle;
use super::core::{
    BACKPRESSURE_DEFER_THRESHOLD, BACKPRESSURE_REDUCED_FRACTION, BACKPRESSURE_REDUCE_THRESHOLD,
};

// ─────────────────────────────────────────────────────────────────────────────
// Result type
// ─────────────────────────────────────────────────────────────────────────────

/// Result of a plan execution attempt.
///
/// `submitted_tiles` is the authoritative set of tiles that were accepted
/// by the executor during this call. Callers that need a per-tile submission
/// signal (e.g. the coordinator deciding which regions to mark `InProgress`)
/// must use this field rather than inferring submission from `pending` —
/// tiles dropped by the `MAX_PENDING_TILES` truncation appear in neither
/// `submitted_tiles` nor `pending`, and a negative check would misclassify
/// them as submitted. See #172 Part 2.
pub(crate) struct ExecutionResult {
    /// Tiles that were successfully submitted to the executor in this call.
    pub submitted_tiles: Vec<TileCoord>,
    /// Tiles that could not be submitted (channel full / throttle overflow),
    /// retained for retry on a subsequent cycle. May be truncated to
    /// `MAX_PENDING_TILES` — tiles beyond the cap are silently dropped.
    pub pending: Vec<TileCoord>,
    /// Whether the entire cycle was deferred due to high load.
    pub deferred: bool,
}

impl ExecutionResult {
    /// Count of tiles actually submitted. Convenience accessor for callers
    /// that only need the cardinality (status reporting, totals).
    #[inline]
    pub fn submitted_count(&self) -> usize {
        self.submitted_tiles.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

/// Execute a prefetch plan by submitting tiles to the DDS client.
///
/// Applies backpressure-aware submission based on the higher of executor
/// resource utilisation and demand on the FUSE mount (#246):
/// - Load > [`BACKPRESSURE_DEFER_THRESHOLD`]: skips this cycle (deferred)
/// - Load > [`BACKPRESSURE_REDUCE_THRESHOLD`]: submits reduced fraction
/// - Applies transition throttle (takeoff ramp-up)
/// - Stops immediately on `ChannelFull` error
///
/// # Returns
///
/// An [`ExecutionResult`] with submitted count, pending tiles, and defer flag.
pub(crate) fn execute_plan(
    plan: &PrefetchPlan,
    client: &dyn DdsClient,
    transition_throttle: &mut TransitionThrottle,
    cancellation: tokio_util::sync::CancellationToken,
) -> ExecutionResult {
    let _span = tracing::debug_span!(
        target: "profiling",
        "prefetch_execute",
        tile_count = plan.tiles.len(),
        strategy = plan.strategy,
    )
    .entered();

    // Backpressure is the higher of two independent pressures, because neither
    // sees the other's load. `executor_load` measures our own pipeline: tile
    // generation, downloads, encoding. `fuse_pressure` measures X-Plane's demand
    // on the mount, which at a boundary crossing is mostly reads served from
    // cache and so invisible to the pools. Submitting at full rate into that
    // collision is what set the memory ceiling in #227; see #246.
    let executor_load = client.executor_load();
    let fuse_pressure = client.fuse_pressure();
    let load = executor_load.max(fuse_pressure);
    if load > BACKPRESSURE_DEFER_THRESHOLD {
        // Store every planned tile as pending so they're retried when load
        // drops. No cap — the executor controls throughput via its channel
        // capacity and resource pools, not us. See #172 post-flight finding:
        // silent drops at submission boundaries create forward-starvation
        // because discarded tiles never get re-planned on subsequent cycles
        // once their regions get marked or cached by some other path.
        // Both figures, not just the maximum: calibrating this throttle means
        // knowing which pressure deferred a cycle, and they are the two
        // independent causes. A log that reported only `load` could not
        // distinguish "our pipeline is saturated" from "X-Plane is reading hard".
        tracing::info!(
            load = format!("{:.1}%", load * 100.0),
            executor_load = format!("{:.1}%", executor_load * 100.0),
            fuse_pressure = format!("{:.1}%", fuse_pressure * 100.0),
            source = if fuse_pressure > executor_load {
                "fuse"
            } else {
                "executor"
            },
            tiles_planned = plan.tiles.len(),
            "Backpressure, deferring prefetch cycle, tiles stored as pending"
        );
        return ExecutionResult {
            submitted_tiles: Vec::new(),
            pending: plan.tiles.clone(),
            deferred: true,
        };
    }

    // Determine how many tiles to submit based on executor load
    let max_tiles = if load > BACKPRESSURE_REDUCE_THRESHOLD {
        let reduced = ((plan.tiles.len() as f64) * BACKPRESSURE_REDUCED_FRACTION).ceil() as usize;
        tracing::debug!(
            load = format!("{:.1}%", load * 100.0),
            executor_load = format!("{:.1}%", executor_load * 100.0),
            fuse_pressure = format!("{:.1}%", fuse_pressure * 100.0),
            source = if fuse_pressure > executor_load {
                "fuse"
            } else {
                "executor"
            },
            full_plan = plan.tiles.len(),
            reduced_to = reduced,
            "Moderate backpressure, reducing prefetch submission"
        );
        reduced
    } else {
        plan.tiles.len()
    };

    // Apply transition throttle (takeoff ramp-up)
    let max_tiles = if transition_throttle.is_active() {
        let fraction = transition_throttle.fraction();
        if fraction == 0.0 {
            // Store every planned tile as pending so they're submitted once
            // the ramp begins. No cap (see BACKPRESSURE_DEFER_THRESHOLD branch
            // above).
            tracing::debug!(
                tiles_deferred = plan.tiles.len(),
                "Transition throttle — grace period, tiles stored as pending"
            );
            return ExecutionResult {
                submitted_tiles: Vec::new(),
                pending: plan.tiles.clone(),
                deferred: false,
            };
        }
        let throttled = ((max_tiles as f64) * fraction).ceil() as usize;
        tracing::debug!(
            fraction = format!("{:.0}%", fraction * 100.0),
            full = max_tiles,
            throttled_to = throttled,
            "Transition throttle — ramping up"
        );
        throttled
    } else {
        max_tiles
    };

    // Store tiles beyond the throttle/backpressure cutoff as pending
    let throttle_overflow: Vec<TileCoord> = if max_tiles < plan.tiles.len() {
        plan.tiles[max_tiles..].to_vec()
    } else {
        Vec::new()
    };

    let tiles_to_submit: Vec<TileCoord> = plan.tiles.iter().take(max_tiles).copied().collect();
    let mut submitted_tiles: Vec<TileCoord> = Vec::with_capacity(tiles_to_submit.len());
    let mut channel_remainder = Vec::new();
    for (idx, tile) in tiles_to_submit.iter().enumerate() {
        let request =
            crate::runtime::JobRequest::prefetch_with_cancellation(*tile, cancellation.clone());
        match client.submit(request) {
            Ok(()) => submitted_tiles.push(*tile),
            Err(crate::executor::DdsClientError::ChannelFull) => {
                channel_remainder = tiles_to_submit[idx..].to_vec();
                tracing::debug!(
                    submitted = submitted_tiles.len(),
                    channel_remaining = channel_remainder.len(),
                    "Channel full — storing remainder for next cycle"
                );
                break;
            }
            Err(crate::executor::DdsClientError::ChannelClosed) => {
                tracing::warn!("Executor channel closed — stopping prefetch");
                break;
            }
        }
    }

    // Merge channel remainder + throttle overflow into pending.
    // No cap: the executor's channel capacity and resource pools are the
    // rate governor — silent drops here would violate the "every tile
    // that intersects the prefetch window gets prefetched" invariant
    // (see cruise branch in core.rs). Pending drains via subsequent
    // cycles; memory cost is ~12 bytes per tile which is trivial even
    // at tens of thousands of entries.
    let pending = if !channel_remainder.is_empty() || !throttle_overflow.is_empty() {
        let mut combined = channel_remainder;
        combined.extend(throttle_overflow);
        tracing::debug!(
            submitted = submitted_tiles.len(),
            pending = combined.len(),
            "Storing {} tiles for subsequent cycles",
            combined.len()
        );
        combined
    } else {
        Vec::new()
    };

    if !submitted_tiles.is_empty() {
        tracing::info!(
            tiles = submitted_tiles.len(),
            strategy = plan.strategy,
            estimated_ms = plan.estimated_completion_ms,
            "Prefetch batch submitted"
        );
    }

    ExecutionResult {
        submitted_tiles,
        pending,
        deferred: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::adaptive::coordinator::test_support::{
        test_calibration, test_plan, BackpressureMockClient, CapLimitedDdsClient, HighLoadDdsClient,
    };
    use crate::prefetch::adaptive::transition_throttle::TransitionThrottle;
    use std::sync::Arc;

    fn default_throttle() -> TransitionThrottle {
        TransitionThrottle::new()
    }

    /// #246: X-Plane's demand on the mount is invisible to `executor_load`,
    /// which sees generation jobs but not reads served from cache, and to
    /// `SimState::should_prefetch`, which reports X-Plane's own DSF loader. At a
    /// boundary crossing prefetch therefore submitted at full rate into the peak
    /// concurrency event, which per #227 is what sets the memory floor.
    #[test]
    fn fuse_pressure_alone_defers_the_cycle() {
        // Executor idle, mount saturated: exactly the collision the existing
        // signals miss.
        let client = BackpressureMockClient::new(0.0).with_fuse_pressure(0.85);
        let plan = test_plan(50);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(
            result.deferred,
            "FUSE pressure above the defer threshold must defer the cycle"
        );
        assert_eq!(result.submitted_count(), 0);
        assert_eq!(
            result.pending.len(),
            50,
            "deferred tiles are stored as pending, never dropped (#172)"
        );
    }

    #[test]
    fn fuse_pressure_alone_reduces_the_submission() {
        // 0.50 is where the reference-leg spike lands: 23 open handles against
        // the 512 MB ceiling. Reduce, not defer.
        let client = BackpressureMockClient::new(0.0).with_fuse_pressure(0.55);
        let plan = test_plan(100);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(!result.deferred);
        assert!(
            result.submitted_count() < 100 && result.submitted_count() > 0,
            "moderate FUSE pressure should reduce rather than stop, got {}",
            result.submitted_count()
        );
    }

    #[test]
    fn the_higher_of_the_two_signals_wins() {
        // Neither signal may mask the other: a busy executor with a quiet mount
        // must still defer, which is the pre-#246 behaviour.
        let executor_busy = BackpressureMockClient::new(0.85).with_fuse_pressure(0.0);
        let plan = test_plan(10);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &executor_busy,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(result.deferred, "executor load must still defer on its own");
    }

    /// Acceptance criterion 4: no return of the #59 self-tripping behaviour.
    /// A quiet mount and a quiet executor must submit the whole plan.
    #[test]
    fn cruise_conditions_submit_the_full_plan() {
        // ~900 reads/min cruise pins almost nothing: X-Plane holds a handful of
        // textures open, nowhere near the ceiling.
        let client = BackpressureMockClient::new(0.1).with_fuse_pressure(0.05);
        let plan = test_plan(40);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(!result.deferred);
        assert_eq!(
            result.submitted_count(),
            40,
            "cruise prefetch throughput must be unaffected"
        );
    }

    #[test]
    fn test_defers_under_high_backpressure() {
        let client = BackpressureMockClient::new(0.85);
        let mut throttle = default_throttle();
        let plan = test_plan(10);

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_count(), 0);
        assert!(result.submitted_tiles.is_empty());
        assert!(result.deferred);
        assert_eq!(result.pending.len(), 10);
    }

    #[test]
    fn test_reduces_under_moderate_backpressure() {
        let client = BackpressureMockClient::new(0.6);
        let mut throttle = default_throttle();
        let plan = test_plan(10);

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_count(), 5);
        assert!(!result.deferred);
    }

    #[test]
    fn test_full_submission_under_low_pressure() {
        let client = BackpressureMockClient::new(0.1);
        let mut throttle = default_throttle();
        let plan = test_plan(10);

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_count(), 10);
        assert!(result.pending.is_empty());
    }

    #[test]
    fn test_channel_full_stores_remainder() {
        let client = CapLimitedDdsClient::new(5);
        let mut throttle = default_throttle();
        let plan = test_plan(10);

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_count(), 5);
        assert_eq!(result.pending.len(), 5);
    }

    #[test]
    fn test_pending_retains_entire_plan_on_full_defer() {
        // Regression guard for #172 post-flight finding: pending must retain
        // EVERY planned tile when the entire plan is deferred. No cap, no
        // silent drops. The executor's natural backpressure is the only
        // rate governor; the coordinator must not discard work at the
        // submission boundary.
        let client = Arc::new(HighLoadDdsClient);
        let mut throttle = default_throttle();

        let tiles: Vec<TileCoord> = (0..5000)
            .map(|i| TileCoord {
                row: 5000 + i,
                col: 8000,
                zoom: 14,
            })
            .collect();
        let cal = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles.clone(), &cal, "boundary", 0, 5000);

        let result = execute_plan(
            &plan,
            client.as_ref(),
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_count(), 0);
        assert!(result.deferred);
        assert_eq!(
            result.pending.len(),
            5000,
            "Pending must retain every planned tile — no cap"
        );
        assert_eq!(result.pending, tiles, "Pending must contain the full plan");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Part 2 (#172): ExecutionResult.submitted_tiles authoritative-set tests
    //
    // `submitted_tiles` is the authoritative record of what the executor
    // accepted. Callers deciding per-region completion must rely on this
    // positive signal, not on "not in pending".
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_submitted_tiles_contains_exactly_accepted_submissions() {
        // 5 distinct tiles, channel cap of 3 → first 3 submit, last 2 go
        // to pending as channel_remainder. `submitted_tiles` must contain
        // exactly the first 3, in submission order.
        let tiles: Vec<TileCoord> = (0..5)
            .map(|i| TileCoord {
                row: 100 + i,
                col: 200,
                zoom: 14,
            })
            .collect();
        let cal = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles.clone(), &cal, "test", 0, 5);
        let client = CapLimitedDdsClient::new(3);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(result.submitted_tiles.len(), 3, "First 3 tiles submit");
        assert_eq!(result.pending.len(), 2, "Last 2 tiles become pending");

        // Submitted tiles must be exactly the first three, in order
        assert_eq!(result.submitted_tiles, tiles[..3]);
        // Pending must be the remainder
        assert_eq!(result.pending, tiles[3..]);
        // Submitted and pending must be disjoint
        for t in &result.submitted_tiles {
            assert!(
                !result.pending.contains(t),
                "Submitted tile {:?} must not appear in pending",
                t
            );
        }
    }

    #[test]
    fn test_submitted_tiles_empty_when_throttle_cuts_all() {
        // A BackpressureMockClient at high pressure causes defer — all
        // tiles go to pending, none submitted. `submitted_tiles` must be
        // empty.
        let plan = test_plan(10);
        let client = BackpressureMockClient::new(0.9);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            &client,
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(result.submitted_tiles.is_empty());
        assert!(result.deferred);
        assert_eq!(result.pending.len(), 10);
    }

    #[test]
    fn test_no_tile_dropped_from_large_deferred_plan() {
        // Regression guard for #172 post-flight finding: the old
        // `MAX_PENDING_TILES` cap silently dropped tiles beyond the cap.
        // That's been removed — every tile in a deferred plan must
        // appear in `pending`, no matter how large.
        //
        // This test uses a plan far larger than the old cap (2000) to
        // prove the cap is gone.
        let tile_count = 10_000;
        let tiles: Vec<TileCoord> = (0..tile_count as u32)
            .map(|i| TileCoord {
                row: 10_000 + i,
                col: 20_000,
                zoom: 14,
            })
            .collect();
        let cal = test_calibration();
        let plan = PrefetchPlan::with_tiles(tiles.clone(), &cal, "test", 0, tile_count);
        let client = Arc::new(HighLoadDdsClient);
        let mut throttle = default_throttle();

        let result = execute_plan(
            &plan,
            client.as_ref(),
            &mut throttle,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(result.submitted_tiles.is_empty(), "High load defers all");
        assert_eq!(
            result.pending.len(),
            tile_count,
            "Pending must retain every tile — no cap, no silent drops"
        );
        assert_eq!(
            result.pending, tiles,
            "Pending must contain every planned tile in order"
        );

        // The last few tiles of the plan (which the old cap would have
        // dropped) must all be in pending.
        for t in tiles.iter().rev().take(10) {
            assert!(
                result.pending.contains(t),
                "Tile past old cap boundary must still be in pending: {:?}",
                t
            );
        }
    }
}
