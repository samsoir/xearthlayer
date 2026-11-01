//! The prefetch lane: where prefetch work runs (#296).
//!
//! The contract is written to be met over a network, so prefetch can later be
//! offloaded to another machine. The result of a submission is a complete
//! tile appearing in the primary's DDS disk cache; a receipt reports only an
//! outcome, never the tile's bytes. Pausing is not part of the trait: a lane
//! is built with the demand state and stands aside on its own.

use tokio::sync::{mpsc, oneshot};

use crate::coord::TileCoord;
use crate::executor::{JobCounts, JobSubmitter};
use crate::runtime::{DdsResponse, JobRequest};

/// A destination for prefetch work.
pub trait PrefetchLane: Send + Sync {
    /// Fire-and-forget.
    fn submit(&self, tile: TileCoord) -> Result<(), LaneError>;
    /// As `submit`, with a receipt that resolves to the outcome.
    fn submit_with_receipt(&self, tile: TileCoord) -> Result<LaneReceipt, LaneError>;
    /// How saturated the lane's queue is, 0.0..=1.0: work waiting to start
    /// against what the lane runs at once, so 1.0 means a full round is
    /// queued behind the running one. Read by coordinator back-pressure.
    fn load(&self) -> f64;
}

/// Why a lane refused a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneError {
    /// The lane cannot take more work now; retry later.
    Full,
    /// The lane has shut down.
    Closed,
}

/// What became of a submitted tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneOutcome {
    Generated,
    AlreadyCached,
    Failed,
    /// The lane dropped the request without an answer.
    Dropped,
}

/// Resolves to the outcome of one submission. Carries no tile bytes.
pub struct LaneReceipt {
    rx: oneshot::Receiver<LaneOutcome>,
}

impl LaneReceipt {
    pub fn from_outcome_channel(rx: oneshot::Receiver<LaneOutcome>) -> Self {
        Self { rx }
    }

    pub async fn outcome(self) -> LaneOutcome {
        self.rx.await.unwrap_or(LaneOutcome::Dropped)
    }
}

/// A lane backed by this process's executor daemon.
pub struct LocalPrefetchLane {
    tx: mpsc::Sender<JobRequest>,
    counts: JobCounts,
    group: usize,
    job_submitter: JobSubmitter,
}

impl LocalPrefetchLane {
    /// `group` is the lane executor's tile-generation group size: how many
    /// jobs it admits at once.
    pub fn new(
        tx: mpsc::Sender<JobRequest>,
        counts: JobCounts,
        group: usize,
        job_submitter: JobSubmitter,
    ) -> Self {
        Self {
            tx,
            counts,
            group: group.max(1),
            job_submitter,
        }
    }

    /// Local-only: cache GC housekeeping runs on this lane's executor. When
    /// prefetch is offloaded, GC must move back to a local executor.
    pub fn job_submitter(&self) -> JobSubmitter {
        self.job_submitter.clone()
    }

    /// Whether the lane's daemon is still taking requests.
    pub fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }

    fn send(&self, request: JobRequest) -> Result<(), LaneError> {
        self.tx.try_send(request).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => LaneError::Full,
            mpsc::error::TrySendError::Closed(_) => LaneError::Closed,
        })
    }
}

fn outcome_of(response: Result<DdsResponse, oneshot::error::RecvError>) -> LaneOutcome {
    match response {
        Ok(r) if r.cache_hit => LaneOutcome::AlreadyCached,
        Ok(r) if r.is_success() => LaneOutcome::Generated,
        Ok(_) => LaneOutcome::Failed,
        Err(_) => LaneOutcome::Dropped,
    }
}

impl PrefetchLane for LocalPrefetchLane {
    fn submit(&self, tile: TileCoord) -> Result<(), LaneError> {
        self.send(JobRequest::prefetch(tile))
    }

    fn submit_with_receipt(&self, tile: TileCoord) -> Result<LaneReceipt, LaneError> {
        let (request, response_rx) = JobRequest::prewarm(tile);
        self.send(request)?;
        let (outcome_tx, outcome_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = outcome_tx.send(outcome_of(response_rx.await));
        });
        Ok(LaneReceipt::from_outcome_channel(outcome_rx))
    }

    /// Jobs waiting for admission against the group size. Not the pools'
    /// utilisation: a small lane's pools read full whenever it is
    /// productively busy, which deferred most prefetch cycles.
    fn load(&self) -> f64 {
        (self.counts.waiting() as f64 / self.group as f64).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::JobExecutor;
    use crate::runtime::RequestOrigin;
    use bytes::Bytes;
    use std::time::Duration;

    fn tile() -> TileCoord {
        TileCoord {
            row: 100,
            col: 200,
            zoom: 12,
        }
    }

    const GROUP: usize = 10;

    fn lane_counting(tx: mpsc::Sender<JobRequest>, counts: JobCounts) -> LocalPrefetchLane {
        LocalPrefetchLane::new(tx, counts, GROUP, JobExecutor::new(Default::default()).1)
    }

    fn lane_with(tx: mpsc::Sender<JobRequest>) -> LocalPrefetchLane {
        lane_counting(tx, JobCounts::default())
    }

    #[tokio::test]
    async fn submit_sends_a_responseless_prefetch_request() {
        let (tx, mut rx) = mpsc::channel(4);
        let lane = lane_with(tx);
        lane.submit(tile()).unwrap();
        let req = rx.recv().await.unwrap();
        assert_eq!(req.origin, RequestOrigin::Prefetch);
        assert!(req.response_tx.is_none());
    }

    #[tokio::test]
    async fn full_channel_reports_full() {
        let (tx, _rx) = mpsc::channel(1);
        let lane = lane_with(tx);
        lane.submit(tile()).unwrap();
        assert!(matches!(lane.submit(tile()), Err(LaneError::Full)));
    }

    #[tokio::test]
    async fn closed_channel_reports_closed() {
        let (tx, rx) = mpsc::channel(1);
        let lane = lane_with(tx);
        drop(rx);
        assert!(!lane.is_connected());
        assert!(matches!(lane.submit(tile()), Err(LaneError::Closed)));
    }

    #[tokio::test]
    async fn receipt_maps_responses_to_outcomes() {
        for (response, expected) in [
            (
                DdsResponse::cache_hit(Bytes::from_static(b"x"), Duration::ZERO),
                LaneOutcome::AlreadyCached,
            ),
            (
                DdsResponse::new(Bytes::from_static(b"x"), false, Duration::ZERO, true),
                LaneOutcome::Generated,
            ),
            (
                DdsResponse::new(Bytes::new(), false, Duration::ZERO, false),
                LaneOutcome::Failed,
            ),
        ] {
            let (tx, mut rx) = mpsc::channel(1);
            let receipt = lane_with(tx).submit_with_receipt(tile()).unwrap();
            let req = rx.recv().await.unwrap();
            assert_eq!(req.origin, RequestOrigin::Prewarm);
            req.response_tx.unwrap().send(response).unwrap();
            assert_eq!(receipt.outcome().await, expected);
        }
    }

    #[tokio::test]
    async fn dropped_request_reports_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let receipt = lane_with(tx).submit_with_receipt(tile()).unwrap();
        drop(rx.recv().await.unwrap());
        assert_eq!(receipt.outcome().await, LaneOutcome::Dropped);
    }

    #[tokio::test]
    async fn idle_lane_reports_no_load() {
        let (tx, _rx) = mpsc::channel(1);
        assert_eq!(lane_with(tx).load(), 0.0);
    }

    /// A lane busy with work it has admitted is doing its job, not
    /// saturated: reading that as load deferred most prefetch cycles.
    #[tokio::test]
    async fn lane_with_only_running_jobs_reports_no_load() {
        let (tx, _rx) = mpsc::channel(1);
        let counts = JobCounts::default();
        counts.store(GROUP, 0);
        assert_eq!(lane_counting(tx, counts).load(), 0.0);
    }

    #[tokio::test]
    async fn load_is_jobs_waiting_against_the_group() {
        let (tx, _rx) = mpsc::channel(1);
        let counts = JobCounts::default();
        let lane = lane_counting(tx, counts.clone());
        counts.store(GROUP, GROUP / 2);
        assert_eq!(lane.load(), 0.5);
        counts.store(GROUP, GROUP);
        assert_eq!(lane.load(), 1.0, "a group's worth waiting is saturated");
        counts.store(GROUP, 3 * GROUP);
        assert_eq!(lane.load(), 1.0, "clamped");
    }
}
