//! Runtime components for XEarthLayer daemon architecture.
//!
//! This module provides the daemon-based architecture for XEarthLayer, where
//! independent daemons communicate via channels. The CLI process owns and
//! orchestrates all daemons.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      CLI / XEarthLayerRuntime                    │
//! │                                                                  │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
//! │  │ FUSE Daemon  │  │  Prefetch    │  │  Prewarm     │           │
//! │  │              │  │  Daemon      │  │  Context     │           │
//! │  │ (on-demand)  │  │ (continuous) │  │ (one-shot)   │           │
//! │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘           │
//! │         │                 │                 │                    │
//! │         │    Job Producers (submit work)    │                    │
//! │         ▼                 ▼                 ▼                    │
//! │  ┌─────────────────────────────────────────────────────────┐    │
//! │  │              JobRequest Channel (mpsc)                   │    │
//! │  │              Owned by CLI, bounded                       │    │
//! │  └─────────────────────────┬───────────────────────────────┘    │
//! │                            │                                     │
//! │                            │    Job Consumer                     │
//! │                            ▼                                     │
//! │                 ┌──────────────────────┐                        │
//! │                 │  Job Executor Daemon │                        │
//! │                 │                      │                        │
//! │                 │  • Receives requests │                        │
//! │                 │  • Schedules jobs    │                        │
//! │                 │  • Manages resources │                        │
//! │                 │  • Sends responses   │                        │
//! │                 └──────────────────────┘                        │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Design Decisions
//!
//! ## Channel as Mediator
//!
//! Instead of a Mediator object that knows about all participants, we use
//! channels as the decoupling mechanism:
//! - Producers only know: "I can send `JobRequest` to this sender"
//! - Consumer only knows: "I receive `JobRequest` from this receiver"
//! - Neither knows about the other - maximum decoupling
//!
//! ## Trait-Based Producer Abstraction
//!
//! Producers receive `Arc<dyn DdsClient>` instead of raw channel senders:
//! - Provides clean API with convenience methods
//! - Allows mocking for tests
//! - Hides channel implementation details
//!
//! ## Optional Response Channels
//!
//! Different producers have different response needs:
//!
//! | Producer | Response Need | Pattern |
//! |----------|--------------|---------|
//! | FUSE | Must wait (X-Plane blocks) | `oneshot::Sender` in request |
//! | Prefetch | Fire-and-forget | No response channel |
//! | Prewarm | Track progress | Aggregate callback |
//!
//! ## Peer Daemons, Not Hierarchy
//!
//! Neither daemon owns the other. Both are:
//! - Created by CLI
//! - Started/stopped by CLI
//! - Independent lifecycles
//! - Testable in isolation
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::runtime::{JobRequest, DdsResponse, RequestOrigin};
//! use xearthlayer::executor::DdsClient;
//!
//! // FUSE handler using DdsClient
//! async fn handle_fuse_request(
//!     client: &dyn DdsClient,
//!     tile: TileCoord,
//!     timeout: Duration,
//! ) -> Vec<u8> {
//!     let cancellation = CancellationToken::new();
//!
//!     let rx = client.request_dds(tile, cancellation.clone());
//!
//!     match tokio::time::timeout(timeout, rx).await {
//!         Ok(Ok(response)) => response.data,
//!         _ => {
//!             cancellation.cancel();
//!             generate_placeholder()
//!         }
//!     }
//! }
//! ```

mod health;
mod orchestrator;
mod request;
mod tile_progress;

pub use health::{HealthSnapshot, HealthStatus, RuntimeHealth, SharedRuntimeHealth};
pub use orchestrator::{RuntimeConfig, XEarthLayerRuntime};
pub use request::{DdsResponse, JobRequest, RequestOrigin};
pub use tile_progress::{
    RegionProgressEntry, SharedTileProgressTracker, TileProgressSink, TileProgressTracker,
    MAX_DISPLAY_REGIONS,
};
