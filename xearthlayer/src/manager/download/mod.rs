//! HTTP download manager for package archives.
//!
//! This module provides functionality for downloading package archive parts,
//! including:
//! - Single file downloads with resume support (`http`)
//! - SHA-256 checksum verification (`checksum`)
//! - Multi-part download state tracking (`state`)
//! - Real-time progress reporting (`progress`)
//! - Multi-part download strategy with bounded concurrency (`strategy`)
//! - High-level download orchestration (`orchestrator`)
//!
//! # Architecture
//!
//! ```text
//! MultiPartDownloader (orchestrator)
//!         │
//!         ├── ParallelStrategy (semaphore-bounded; concurrency=1 → sequential)
//!         │
//!         ├── HttpDownloader (single file downloads)
//!         │
//!         ├── DownloadState (tracks progress)
//!         │
//!         └── ProgressReporter (real-time updates)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use std::path::PathBuf;
//! use xearthlayer::manager::download::{MultiPartDownloader, DownloadState};
//!
//! let downloader = MultiPartDownloader::new();
//!
//! let mut state = DownloadState::new(
//!     vec!["http://example.com/part1.bin".to_string()],
//!     vec!["abc123...".to_string()],
//!     vec![PathBuf::from("/tmp/part1.bin")],
//! );
//!
//! // Query file sizes for accurate progress
//! downloader.query_sizes(&mut state);
//!
//! // Download with progress callback
//! downloader.download_all(&mut state, Some(Box::new(|progress| {
//!     println!("Downloaded {} bytes", progress.total_bytes_downloaded);
//! })))?;
//! ```

mod checksum;
mod http;
mod orchestrator;
mod progress;
pub(crate) mod retry;
pub(crate) mod semaphore;
mod state;
mod strategy;

// Public API - types used by installer and other modules
pub use orchestrator::MultiPartDownloader;
pub use progress::{DownloadProgress, DownloadProgressCallback, PartState};
pub use state::DownloadState;
