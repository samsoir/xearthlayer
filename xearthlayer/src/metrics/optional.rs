//! OptionalMetrics — extension trait for `Option<MetricsClient>`.

use super::client::MetricsClient;

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
    fn dds_disk_cache_hit(&self, bytes: u64);
    fn dds_disk_cache_miss(&self);
    fn disk_write_started(&self);
    fn disk_write_completed(&self, bytes: u64, duration_us: u64);
    fn disk_cache_initial_size(&self, bytes: u64);
    fn disk_cache_size(&self, bytes: u64);
    fn dds_disk_cache_size(&self, bytes: u64);
    fn memory_cache_hit(&self, is_fuse: bool);
    fn memory_cache_miss(&self, is_fuse: bool);
    fn memory_cache_size(&self, bytes: u64);
    fn job_submitted(&self, is_fuse: bool);
    fn job_started(&self);
    fn job_completed(&self, success: bool, duration_us: u64);
    fn job_coalesced(&self);
    fn job_timed_out(&self);
    fn encode_started(&self);
    fn encode_completed(&self, bytes: u64, duration_us: u64);
    fn assembly_completed(&self, duration_us: u64);
    fn fuse_request_started(&self);
    fn fuse_request_completed(&self);
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
    fn dds_disk_cache_hit(&self, bytes: u64) {
        if let Some(client) = self {
            client.dds_disk_cache_hit(bytes);
        }
    }

    #[inline]
    fn dds_disk_cache_miss(&self) {
        if let Some(client) = self {
            client.dds_disk_cache_miss();
        }
    }

    #[inline]
    fn disk_write_started(&self) {
        if let Some(client) = self {
            client.disk_write_started();
        }
    }

    #[inline]
    fn disk_write_completed(&self, bytes: u64, duration_us: u64) {
        if let Some(client) = self {
            client.disk_write_completed(bytes, duration_us);
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
    fn job_completed(&self, success: bool, duration_us: u64) {
        if let Some(client) = self {
            client.job_completed(success, duration_us);
        }
    }

    #[inline]
    fn job_coalesced(&self) {
        if let Some(client) = self {
            client.job_coalesced();
        }
    }

    #[inline]
    fn job_timed_out(&self) {
        if let Some(client) = self {
            client.job_timed_out();
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

    #[test]
    fn test_optional_metrics_some() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = MetricsClient::new(tx);
        let optional: Option<MetricsClient> = Some(client);

        // Should not panic
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
    }

    #[test]
    fn test_optional_metrics_none() {
        let optional: Option<MetricsClient> = None;

        // Should be no-ops
        optional.download_started();
        optional.download_completed(100, 50);
        optional.job_submitted(true);
    }
}
