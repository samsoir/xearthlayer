//! X-Plane Web API adapter for aircraft telemetry and sim state.
//!
//! Connects to X-Plane's built-in Web API (available since 12.1.1)
//! via REST for dataref ID lookup and WebSocket for 10Hz position
//! and sim state subscriptions. Requires no user configuration.

mod adapter;
pub mod client;
pub mod config;
pub mod datarefs;
pub mod sim_state;

pub use adapter::WebApiAdapter;
pub use sim_state::{shared_sim_state, SharedSimState};
