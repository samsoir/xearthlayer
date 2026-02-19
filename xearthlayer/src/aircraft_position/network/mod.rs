//! Online network position adapters (VATSIM, IVAO, PilotEdge).
//!
//! This module provides a fourth position source for the APT system,
//! fetching pilot position data from online ATC network REST APIs.
//!
//! # Supported Networks
//!
//! - **VATSIM** — direct HTTP client against V3 JSON data feed (~15s update interval)
//! - IVAO and PilotEdge — planned, same adapter pattern
//!
//! # Architecture
//!
//! ```text
//! NetworkAdapter (poll loop)
//!     │
//!     ├── NetworkClient trait → VatsimClient (direct reqwest)
//!     │
//!     └── mpsc::Sender<AircraftState>
//!             │
//!             └── Orchestrator bridge → SharedAircraftPosition::receive_network_position()
//! ```
//!
//! The adapter runs as an async daemon, polling the network API at a
//! configurable interval and sending [`AircraftState`] updates to the
//! aggregator via an mpsc channel.

mod adapter;
mod client;
mod config;
mod error;

pub use adapter::NetworkAdapter;
pub use client::{NetworkClient, PilotPosition, VatsimClient};
pub use config::{
    NetworkAdapterConfig, DEFAULT_MAX_STALE_SECS, DEFAULT_POLL_INTERVAL_SECS,
    DEFAULT_VATSIM_DATA_URL,
};
pub use error::NetworkError;
