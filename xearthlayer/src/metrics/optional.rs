//! OptionalMetrics — extension trait for `Option<MetricsClient>`.

use super::client::MetricsClient;
use crate::cache::DiskTier;

/// Extension trait for optional metrics client usage.
///
/// This allows components to work with `Option<MetricsClient>` without
/// verbose match statements.
pub trait OptionalMetrics {
    fn download_started(&self);
    fn download_completed(&self, bytes: u64, duration_us: u64);
    fn download_failed(&self);
    fn download_retried(&self);
    fn chunk_disk_cache_hit(&self, bytes: u64);
    fn chunk_disk_cache_miss(&self);
    fn dds_disk_cache_hit(&self, bytes: u64, is_fuse: bool);
    fn dds_disk_cache_miss(&self, is_fuse: bool);
    fn disk_write_started(&self);
    fn disk_write_completed(&self, bytes: u64, tier: DiskTier);
    fn disk_cache_initial_size(&self, bytes: u64);
    fn disk_cache_size(&self, bytes: u64);
    fn dds_disk_cache_size(&self, bytes: u64);
    fn memory_cache_hit(&self, is_fuse: bool);
    fn memory_cache_miss(&self, is_fuse: bool);
    fn memory_cache_size(&self, bytes: u64);
    fn mem_cache_write_started(&self);
    fn mem_cache_write_completed(&self);
    fn job_submitted(&self, is_fuse: bool);
    fn job_started(&self);
    fn job_completed(&self, success: bool, duration_us: u64, is_fuse: bool);
    fn job_coalesced(&self);
    fn job_timed_out(&self, duration_us: u64);
    fn encode_started(&self);
    fn encode_completed(&self, bytes: u64, duration_us: u64);
    fn encode_failed(&self);
    fn assembly_completed(&self, duration_us: u64);
    fn fuse_request_started(&self);
    fn fuse_request_completed(&self);
}

/// One encode in `encodes_active`, from [`start`](Self::start) until it is
/// [`completed`](Self::completed) or dropped.
///
/// Dropping it uncompleted sends `EncodeFailed`, so every way out of an
/// encode (an encoder error, a panic, a task that returns early or is
/// dropped) ends it exactly once, and only a completed encode counts toward
/// `encodes_completed` and `bytes_encoded` (#385).
#[must_use = "dropping the guard ends the encode as failed"]
pub struct EncodeGuard {
    metrics: Option<MetricsClient>,
}

impl EncodeGuard {
    /// Sends `EncodeStarted`.
    pub fn start(metrics: &Option<MetricsClient>) -> Self {
        metrics.encode_started();
        Self {
            metrics: metrics.clone(),
        }
    }

    /// Sends `EncodeCompleted` instead of the `EncodeFailed` a drop sends.
    pub fn completed(mut self, bytes: u64, duration_us: u64) {
        self.metrics.take().encode_completed(bytes, duration_us);
    }
}

impl Drop for EncodeGuard {
    fn drop(&mut self) {
        self.metrics.take().encode_failed();
    }
}

impl OptionalMetrics for Option<MetricsClient> {
    #[inline]
    fn download_started(&self) {
        if let Some(client) = self {
            client.download_started();
        }
    }

    #[inline]
    fn download_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.download_completed(bytes, duration_us);
        }
    }

    #[inline]
    fn download_failed(&self) {
        if let Some(client) = self {
            client.download_failed();
        }
    }

    #[inline]
    fn download_retried(&self) {
        if let Some(client) = self {
            client.download_retried();
        }
    }

    #[inline]
    fn chunk_disk_cache_hit(&self, bytes: u64) {
        if let Some(client) = self {
            client.chunk_disk_cache_hit(bytes);
        }
    }

    #[inline]
    fn chunk_disk_cache_miss(&self) {
        if let Some(client) = self {
            client.chunk_disk_cache_miss();
        }
    }

    #[inline]
    fn dds_disk_cache_hit(&self, bytes: u64, is_fuse: bool) {
        if let Some(client) = self {
            client.dds_disk_cache_hit(bytes, is_fuse);
        }
    }

    #[inline]
    fn dds_disk_cache_miss(&self, is_fuse: bool) {
        if let Some(client) = self {
            client.dds_disk_cache_miss(is_fuse);
        }
    }

    #[inline]
    fn disk_write_started(&self) {
        if let Some(client) = self {
            client.disk_write_started();
        }
    }

    #[inline]
    fn disk_write_completed(&self, bytes: u64, tier: DiskTier) {
        if let Some(client) = self {
            client.disk_write_completed(bytes, tier);
        }
    }

    #[inline]
    fn disk_cache_initial_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.disk_cache_initial_size(bytes);
        }
    }

    #[inline]
    fn disk_cache_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.disk_cache_size(bytes);
        }
    }

    #[inline]
    fn dds_disk_cache_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.dds_disk_cache_size(bytes);
        }
    }

    #[inline]
    fn memory_cache_hit(&self, is_fuse: bool) {
        if let Some(client) = self {
            client.memory_cache_hit(is_fuse);
        }
    }

    #[inline]
    fn memory_cache_miss(&self, is_fuse: bool) {
        if let Some(client) = self {
            client.memory_cache_miss(is_fuse);
        }
    }

    #[inline]
    fn memory_cache_size(&self, bytes: u64) {
        if let Some(client) = self {
            client.memory_cache_size(bytes);
        }
    }

    #[inline]
    fn mem_cache_write_started(&self) {
        if let Some(client) = self {
            client.mem_cache_write_started();
        }
    }

    #[inline]
    fn mem_cache_write_completed(&self) {
        if let Some(client) = self {
            client.mem_cache_write_completed();
        }
    }

    #[inline]
    fn job_submitted(&self, is_fuse: bool) {
        if let Some(client) = self {
            client.job_submitted(is_fuse);
        }
    }

    #[inline]
    fn job_started(&self) {
        if let Some(client) = self {
            client.job_started();
        }
    }

    #[inline]
    fn job_completed(&self, success: bool, duration_us: u64, is_fuse: bool) {
        if let Some(client) = self {
            client.job_completed(success, duration_us, is_fuse);
        }
    }

    #[inline]
    fn job_coalesced(&self) {
        if let Some(client) = self {
            client.job_coalesced();
        }
    }

    #[inline]
    fn job_timed_out(&self, duration_us: u64) {
        if let Some(client) = self {
            client.job_timed_out(duration_us);
        }
    }

    #[inline]
    fn encode_started(&self) {
        if let Some(client) = self {
            client.encode_started();
        }
    }

    #[inline]
    fn encode_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.encode_completed(bytes, duration_us);
        }
    }

    #[inline]
    fn encode_failed(&self) {
        if let Some(client) = self {
            client.encode_failed();
        }
    }

    #[inline]
    fn assembly_completed(&self, duration_us: u64) {
        if let Some(client) = self {
            client.assembly_completed(duration_us);
        }
    }

    #[inline]
    fn fuse_request_started(&self) {
        if let Some(client) = self {
            client.fuse_request_started();
        }
    }

    #[inline]
    fn fuse_request_completed(&self) {
        if let Some(client) = self {
            client.fuse_request_completed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::metrics::MetricEvent;

    fn drain(rx: &mut mpsc::UnboundedReceiver<MetricEvent>) -> Vec<MetricEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn test_encode_guard_completed_sends_started_then_completed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let metrics = Some(MetricsClient::new(tx));

        let encode = EncodeGuard::start(&metrics);
        encode.completed(4096, 250);

        let events = drain(&mut rx);
        assert!(
            matches!(
                events[..],
                [
                    MetricEvent::EncodeStarted,
                    MetricEvent::EncodeCompleted {
                        bytes: 4096,
                        duration_us: 250
                    }
                ]
            ),
            "got {events:?}"
        );
    }

    #[test]
    fn test_encode_guard_dropped_sends_started_then_failed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let metrics = Some(MetricsClient::new(tx));

        drop(EncodeGuard::start(&metrics));

        let events = drain(&mut rx);
        assert!(
            matches!(
                events[..],
                [MetricEvent::EncodeStarted, MetricEvent::EncodeFailed]
            ),
            "got {events:?}"
        );
    }

    #[test]
    fn test_encode_guard_without_a_client_is_a_no_op() {
        let metrics: Option<MetricsClient> = None;
        EncodeGuard::start(&metrics).completed(1, 1);
        drop(EncodeGuard::start(&metrics));
    }

    #[test]
    fn test_optional_metrics_some() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = MetricsClient::new(tx);
        let optional: Option<MetricsClient> = Some(client);

        // Should not panic
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
        optional.mem_cache_write_started();
        optional.mem_cache_write_completed();
    }

    #[test]
    fn test_optional_metrics_none() {
        let optional: Option<MetricsClient> = None;

        // Should be no-ops
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
        optional.mem_cache_write_started();
        optional.mem_cache_write_completed();
    }
}
