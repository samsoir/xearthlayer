//! The jobs a queue runs and the events it reports.

/// Identifies one job for the life of its queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);
