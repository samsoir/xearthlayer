//! Aircraft Position & Telemetry (APT) Module
//!
//! This module provides a **single source of truth** for aircraft position and vector data,
//! abstracting over multiple data sources with varying accuracy via a Position Model.
//!
//! # Architecture
//!
//! The APT module maintains a persistent Position Model that is continuously refined by
//! the best available data source. This mirrors how real aircraft navigation works:
//!
//! - **GPS/Telemetry** → Primary, recalibrates the model when available (~10m accuracy)
//! - **Online Network** → VATSIM/IVAO/PilotEdge position via REST API (~10m, ~15s updates)
//! - **Prewarm** → Seeds the model at startup from airport ICAO (~100m accuracy)
//! - **Scene Inference** → Maintains the model when telemetry unavailable (~100km accuracy)
//!
//! # Position Model Concept
//!
//! Instead of simple source switching (use GPS if available, else use inference), APT
//! maintains a **persistent position model** that is refined by the best available data:
//!
//! > **The model is the source of truth, not any single input. Inputs refine the model
//! > based on their accuracy and freshness.**
//!
//! Selection logic:
//! 1. Higher accuracy always wins (lower meters = better)
//! 2. Stale high-accuracy can be beaten by fresh lower-accuracy
//! 3. Position persists across source transitions (no "lost" position)
//!
//! # Usage
//!
//! ```ignore
//! use xearthlayer::aircraft_position::{
//!     SharedAircraftPosition, AircraftPositionProvider, TelemetryStatus
//! };
//!
//! // Query current position
//! let status = aircraft_position.status();
//! if let Some(state) = status.state {
//!     println!("Position: {}, {}", state.latitude, state.longitude);
//!     println!("Source: {:?}, Accuracy: {}m", state.source, state.accuracy.0);
//! }
//!
//! // Check telemetry connection
//! match aircraft_position.telemetry_status() {
//!     TelemetryStatus::Connected => println!("Receiving X-Plane telemetry"),
//!     TelemetryStatus::Disconnected => println!("No telemetry, using inference"),
//! }
//!
//! // Subscribe to position updates (broadcast at 1Hz max)
//! let mut rx = aircraft_position.subscribe();
//! while let Ok(state) = rx.recv().await {
//!     // Handle position update
//! }
//! ```
//!
//! # Components
//!
//! - [`state`] - Core types: `AircraftState`, `PositionAccuracy`, `TelemetryStatus`, `PositionSource`, `TrackSource`
//! - [`model`] - `PositionModel` with accuracy-based selection logic
//! - [`telemetry`] - `TelemetryReceiver` for X-Plane UDP telemetry (XGPS2/ForeFlight)
//! - [`inference`] - `InferenceAdapter` for Scene Tracker-based position inference
//! - [`aggregator`] - `StateAggregator` that combines all sources and broadcasts updates
//! - [`provider`] - `AircraftPositionProvider` and `AircraftPositionBroadcaster` traits
//! - [`flight_path`] - `FlightPathHistory` for position history and track derivation
//! - [`network`] - `NetworkAdapter` for online ATC network position (VATSIM/IVAO/PilotEdge)

mod aggregator;
mod flight_path;
mod inference;
mod logger;
mod model;
pub mod network;
mod provider;
mod state;
mod telemetry;

pub use aggregator::{StateAggregator, StateAggregatorConfig};
pub use flight_path::{FlightPathConfig, FlightPathHistory, PositionSample};
pub use inference::{InferenceAdapter, InferenceAdapterConfig};
pub use model::PositionModel;
pub use network::{NetworkAdapter, NetworkAdapterConfig, VatsimClient};
pub use provider::{AircraftPositionBroadcaster, AircraftPositionProvider, SharedAircraftPosition};
pub use state::{
    AircraftPositionStatus, AircraftState, PositionAccuracy, PositionSource, TelemetryStatus,
    TrackSource,
};
pub use telemetry::{TelemetryError, TelemetryReceiver, TelemetryReceiverConfig};

// Position logger for flight analysis (DEBUG level only)
pub use logger::{spawn_position_logger, DEFAULT_LOG_INTERVAL};
