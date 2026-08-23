//! Metrics collection and reporting system.
//!
//! This module provides a 3-layer architecture for metrics:
//!
//! 1. **Emission Layer** ([`MetricsClient`]) - Fire-and-forget event emission
//! 2. **Aggregation Layer** ([`MetricsDaemon`]) - Independent event processing
//! 3. **Reporting Layer** ([`MetricsReporter`]) - Transform data for presentation

mod client;
mod daemon;
mod event;
mod memory_probe;
mod optional;
mod reporter;
mod snapshot;
mod state;
mod system;

pub use client::MetricsClient;
pub use daemon::{MetricsDaemon, MetricsStateSnapshot, SharedMetricsState};
pub use event::MetricEvent;
pub use memory_probe::{
    log_allocator_environment, log_malloc_trim_at_shutdown, AllocatorSample, MemoryProbe,
    MemorySample, ProcessMemoryProbe,
};
pub use optional::OptionalMetrics;
pub use reporter::{MetricsReporter, TuiReporter};
pub use snapshot::TelemetrySnapshot;
pub use state::{AggregatedState, RingBuffer, TimeSeriesHistory, DEFAULT_HISTORY_CAPACITY};
pub use system::MetricsSystem;
