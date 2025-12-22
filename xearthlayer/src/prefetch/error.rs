//! Error types for the prefetch module.

use std::io;
use thiserror::Error;

/// Errors that can occur during prefetch operations.
#[derive(Debug, Error)]
pub enum PrefetchError {
    /// Failed to bind UDP socket.
    #[error("Failed to bind UDP socket on port {port}: {source}")]
    SocketBindError { port: u16, source: io::Error },

    /// Failed to receive UDP data.
    #[error("Failed to receive UDP data: {0}")]
    ReceiveError(#[from] io::Error),

    /// Invalid X-Plane data packet format.
    #[error("Invalid X-Plane data packet: {0}")]
    InvalidPacket(String),

    /// Coordinate conversion error.
    #[error("Coordinate conversion failed: {0}")]
    CoordError(#[from] crate::coord::CoordError),

    /// Prefetch scheduler error.
    #[error("Prefetch scheduler error: {0}")]
    SchedulerError(String),
}
