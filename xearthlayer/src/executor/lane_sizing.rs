//! Sizes the demand and prefetch lanes (#296).
//!
//! Demand keeps the full derived capacity, so prefetch costs FUSE nothing it
//! would have had with prefetch disabled. The prefetch lane is added on top:
//! its CPU, network and disk pools are scaled by `prefetch.capacity_share`,
//! because that work competes with X-Plane for cores. Its HTTP connections
//! are not scaled, because a connection waiting on the network costs the
//! simulator nothing and prefetch throughput is bound by connections in
//! flight. The pool formulas themselves stay in `ResourcePoolConfig::default()`
//! and are not configurable.

use std::sync::Once;

use super::download_config::DEFAULT_MAX_CONCURRENT_HTTP;
use super::resource_pool::{ResourcePoolConfig, BLOCKING_POOL_HEADROOM};
use crate::config::DEFAULT_PREFETCH_CAPACITY_SHARE;

/// Smallest accepted `prefetch.capacity_share`.
pub const MIN_PREFETCH_CAPACITY_SHARE: f64 = 0.05;
/// Largest accepted `prefetch.capacity_share`.
pub const MAX_PREFETCH_CAPACITY_SHARE: f64 = 0.50;

/// Brings a configured share into range. The INI parser does not range-check
/// it, and an out-of-range share would size a lane nobody intended.
pub fn clamp_capacity_share(share: f64) -> f64 {
    let clamped = if share.is_nan() {
        DEFAULT_PREFETCH_CAPACITY_SHARE
    } else {
        share.clamp(MIN_PREFETCH_CAPACITY_SHARE, MAX_PREFETCH_CAPACITY_SHARE)
    };
    if clamped != share {
        // Both the pools and the blocking cap derive from the share, so the
        // same clamp runs twice per start; one warning is enough.
        static WARNED: Once = Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                configured = share,
                used = clamped,
                "prefetch.capacity_share is outside {MIN_PREFETCH_CAPACITY_SHARE} to \
                 {MAX_PREFETCH_CAPACITY_SHARE}; clamping"
            );
        });
    }
    clamped
}

/// Capacity of both lanes, derived from the demand lane's pools.
#[derive(Clone, Debug)]
pub struct LaneSizing {
    pub demand: ResourcePoolConfig,
    pub prefetch: ResourcePoolConfig,
    /// Concurrent prefetch tile jobs allowed in flight.
    pub prefetch_tile_generation: usize,
    /// The prefetch lane's own HTTP connection semaphore. Not scaled by the
    /// share: a connection waiting on the network costs X-Plane no CPU, and
    /// prefetch throughput is bound by connections in flight, so scaling it
    /// down only slows downloads without protecting the simulator.
    pub prefetch_http_permits: usize,
}

impl LaneSizing {
    /// Sizes the prefetch lane's CPU, network and disk pools as `share` of
    /// `demand`, never below one permit. The share limits work that competes
    /// with X-Plane for cores (encoding, and the TLS and JPEG decoding done
    /// by network tasks); HTTP connections are sized separately.
    pub fn derive(demand: ResourcePoolConfig, share: f64) -> Self {
        let scale = |n: usize| ((n as f64 * share).ceil() as usize).max(1);
        let prefetch = ResourcePoolConfig::new(
            scale(demand.network),
            scale(demand.disk_io),
            scale(demand.cpu),
        );
        let prefetch_tile_generation = 2 * (prefetch.cpu + prefetch.network);
        Self {
            prefetch_http_permits: DEFAULT_MAX_CONCURRENT_HTTP,
            prefetch_tile_generation,
            prefetch,
            demand,
        }
    }

    /// Sizes both lanes from the default pools and a configured share. The
    /// runtime builder and the blocking-pool cap both come through here, so
    /// the pools and the threads that serve them cannot disagree.
    pub fn from_configured_share(share: f64) -> Self {
        Self::derive(ResourcePoolConfig::default(), clamp_capacity_share(share))
    }

    /// Both lanes dispatch CPU and disk work through `spawn_blocking`, so the
    /// blocking pool must cover their sum (#227).
    pub fn blocking_threads_required(&self) -> usize {
        self.demand.cpu
            + self.demand.disk_io
            + self.prefetch.cpu
            + self.prefetch.disk_io
            + BLOCKING_POOL_HEADROOM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_machine_at_default_share() {
        let sizing = LaneSizing::derive(ResourcePoolConfig::new(60, 64, 40), 0.35);
        assert_eq!(
            (
                sizing.prefetch.cpu,
                sizing.prefetch.network,
                sizing.prefetch.disk_io
            ),
            (14, 21, 23)
        );
        assert_eq!(sizing.prefetch_tile_generation, 70);
        assert_eq!(sizing.prefetch_http_permits, DEFAULT_MAX_CONCURRENT_HTTP);
        assert_eq!(
            sizing.blocking_threads_required(),
            40 + 64 + 14 + 23 + BLOCKING_POOL_HEADROOM
        );
    }

    /// Prefetch downloads are bound by connections in flight, not by anything
    /// that competes with X-Plane: on a 5 Gbit link a prewarm at share 0.35
    /// held 359 permits busy at about 180 ms per chunk while CPU sat near 5%
    /// and the link near 4%, and moved 2.7 times slower than when it had the
    /// whole shared semaphore. The share sizes CPU-bound work only.
    #[test]
    fn http_permits_do_not_scale_with_the_share() {
        for share in [
            MIN_PREFETCH_CAPACITY_SHARE,
            0.35,
            MAX_PREFETCH_CAPACITY_SHARE,
        ] {
            let sizing = LaneSizing::derive(ResourcePoolConfig::new(60, 64, 40), share);
            assert_eq!(
                sizing.prefetch_http_permits, DEFAULT_MAX_CONCURRENT_HTTP,
                "share {share} must not shrink the prefetch lane's connections"
            );
        }
    }

    #[test]
    fn demand_keeps_its_full_capacity() {
        let demand = ResourcePoolConfig::new(60, 64, 40);
        let sizing = LaneSizing::derive(demand, 0.5);
        assert_eq!(
            (
                sizing.demand.cpu,
                sizing.demand.network,
                sizing.demand.disk_io
            ),
            (40, 60, 64)
        );
    }

    #[test]
    fn configured_share_is_clamped_to_its_range() {
        assert_eq!(clamp_capacity_share(0.35), 0.35);
        assert_eq!(clamp_capacity_share(0.0), MIN_PREFETCH_CAPACITY_SHARE);
        assert_eq!(clamp_capacity_share(2.0), MAX_PREFETCH_CAPACITY_SHARE);
        assert_eq!(
            clamp_capacity_share(f64::NAN),
            DEFAULT_PREFETCH_CAPACITY_SHARE
        );
    }

    #[test]
    fn configured_sizing_uses_the_clamped_share() {
        let configured = LaneSizing::from_configured_share(5.0);
        let clamped =
            LaneSizing::derive(ResourcePoolConfig::default(), MAX_PREFETCH_CAPACITY_SHARE);
        assert_eq!(
            configured.blocking_threads_required(),
            clamped.blocking_threads_required()
        );
        assert_eq!(
            configured.prefetch_http_permits,
            clamped.prefetch_http_permits
        );
    }

    #[test]
    fn smallest_share_still_yields_a_working_lane() {
        let sizing = LaneSizing::derive(ResourcePoolConfig::new(8, 64, 6), 0.05);
        assert!(
            sizing.prefetch.cpu >= 1
                && sizing.prefetch.network >= 1
                && sizing.prefetch.disk_io >= 1
        );
        assert!(sizing.prefetch_tile_generation >= 1 && sizing.prefetch_http_permits >= 1);
    }
}
