//! Main TUI dashboard for XEarthLayer.
//!
//! Displays real-time pipeline status, network throughput, cache utilization,
//! and error rates.
//!
//! # Module Structure
//!
//! - `state` - State enums and data structs (no rendering dependencies)
//! - `render` - Main layout orchestration
//! - `render_loading` - Loading state rendering
//! - `render_sections` - Section-specific rendering (prefetch, control plane)
//! - `utils` - Formatting and non-TUI output

mod render;
mod render_loading;
mod render_sections;
pub mod state;
pub mod utils;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use xearthlayer::aircraft_position::{AircraftPositionProvider, SharedAircraftPosition};
use xearthlayer::metrics::TelemetrySnapshot;
use xearthlayer::prefetch::SharedPrefetchStatus;
use xearthlayer::runtime::SharedRuntimeHealth;

use crate::ui::widgets::{CacheConfig, DiskHistory, NetworkHistory, SceneryHistory};

// Re-export public types from state module
pub use state::{
    DashboardConfig, DashboardEvent, DashboardState, JobRates, LoadingPhase, LoadingProgress,
    PrewarmProgress, QUIT_CONFIRM_TIMEOUT, SPINNER_FRAMES,
};

// Re-export utility functions
pub use utils::{print_session_summary, print_simple_status};

/// The main dashboard UI.
pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    config: DashboardConfig,
    network_history: NetworkHistory,
    /// History for scenery system sparklines (new v0.3.0 layout).
    scenery_history: SceneryHistory,
    /// History for disk I/O sparklines (new v0.3.0 layout).
    disk_history: DiskHistory,
    shutdown: Arc<AtomicBool>,
    start_time: Instant,
    last_draw: Instant,
    /// Current state of the dashboard (Loading or Running).
    state: DashboardState,
    /// Spinner frame index for loading animations.
    spinner_frame: usize,
    /// Optional prefetch status for display.
    prefetch_status: Option<Arc<SharedPrefetchStatus>>,
    /// Optional runtime health for display.
    runtime_health: Option<SharedRuntimeHealth>,
    /// Maximum concurrent jobs for the control plane display.
    max_concurrent_jobs: usize,
    /// Quit confirmation state - Some(timestamp) when awaiting confirmation.
    quit_confirmation: Option<Instant>,
    /// Background prewarm status (None = no prewarm, Some = prewarm in progress or complete).
    prewarm_status: Option<PrewarmProgress>,
    /// Aircraft position provider from APT module.
    aircraft_position: Option<SharedAircraftPosition>,
}

impl Dashboard {
    /// Create a new dashboard in the Running state.
    #[allow(dead_code)]
    pub fn new(config: DashboardConfig, shutdown: Arc<AtomicBool>) -> io::Result<Self> {
        Self::with_state(config, shutdown, DashboardState::Running)
    }

    /// Create a new dashboard with a specific initial state.
    pub fn with_state(
        config: DashboardConfig,
        shutdown: Arc<AtomicBool>,
        state: DashboardState,
    ) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let now = Instant::now();
        Ok(Self {
            terminal,
            config,
            network_history: NetworkHistory::new(60), // 60 samples for sparkline
            scenery_history: SceneryHistory::new(12), // 12 samples for scenery sparklines
            disk_history: DiskHistory::new(12),       // 12 samples for disk I/O sparkline
            shutdown,
            start_time: now,
            last_draw: now,
            state,
            spinner_frame: 0,
            prefetch_status: None,
            runtime_health: None,
            max_concurrent_jobs: 0,
            quit_confirmation: None,
            prewarm_status: None,
            aircraft_position: None,
        })
    }

    /// Set the prefetch status source for display.
    pub fn with_prefetch_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.prefetch_status = Some(status);
        self
    }

    /// Set the runtime health source for display.
    pub fn with_runtime_health(
        mut self,
        health: SharedRuntimeHealth,
        max_concurrent_jobs: usize,
    ) -> Self {
        self.runtime_health = Some(health);
        self.max_concurrent_jobs = max_concurrent_jobs;
        self
    }

    /// Set the aircraft position provider from APT module.
    pub fn with_aircraft_position(mut self, apt: SharedAircraftPosition) -> Self {
        self.aircraft_position = Some(apt);
        self
    }

    /// Get the current state.
    #[allow(dead_code)]
    pub fn state(&self) -> &DashboardState {
        &self.state
    }

    /// Set the dashboard state.
    #[allow(dead_code)]
    pub fn set_state(&mut self, state: DashboardState) {
        self.state = state;
    }

    /// Transition to the Running state.
    pub fn transition_to_running(&mut self) {
        self.state = DashboardState::Running;
        self.start_time = Instant::now(); // Reset uptime for running phase
    }

    /// Update loading progress.
    pub fn update_loading_progress(&mut self, progress: LoadingProgress) {
        self.state = DashboardState::Loading(progress);
    }

    /// Update prewarm progress (runs in background during Running state).
    pub fn update_prewarm_progress(&mut self, progress: PrewarmProgress) {
        self.prewarm_status = Some(progress);
    }

    /// Clear prewarm status (called when prewarm completes).
    pub fn clear_prewarm_status(&mut self) {
        self.prewarm_status = None;
    }

    /// Get current prewarm status.
    pub fn prewarm_status(&self) -> Option<&PrewarmProgress> {
        self.prewarm_status.as_ref()
    }

    /// Check if in Loading state.
    #[allow(dead_code)]
    pub fn is_loading(&self) -> bool {
        matches!(self.state, DashboardState::Loading(_))
    }

    /// Check if in Running state.
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        matches!(self.state, DashboardState::Running)
    }

    /// Restore terminal to normal state.
    pub fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Draw the dashboard with the given telemetry snapshot.
    pub fn draw(&mut self, snapshot: &TelemetrySnapshot) -> io::Result<()> {
        // Calculate time since last draw for instantaneous rate calculation
        let now = Instant::now();
        let sample_interval = now.duration_since(self.last_draw).as_secs_f64();
        self.last_draw = now;

        // Update network history with instantaneous throughput
        self.network_history
            .update(snapshot.bytes_downloaded, sample_interval);

        // Update scenery history for new layout sparklines
        self.scenery_history.update(snapshot, sample_interval);

        // Update disk history (both reads and writes for I/O tracking)
        self.disk_history.update(
            snapshot.disk_bytes_written,
            snapshot.disk_bytes_read,
            sample_interval,
        );

        let uptime = self.start_time.elapsed();
        let cache_config = CacheConfig {
            memory_max_bytes: self.config.memory_cache_max,
            disk_max_bytes: self.config.disk_cache_max,
        };

        // Get prefetch status if available
        let prefetch_snapshot = self
            .prefetch_status
            .as_ref()
            .map(|s| s.snapshot())
            .unwrap_or_default();

        // Get control plane health if available
        let control_plane_snapshot = self.runtime_health.as_ref().map(|h| h.snapshot());
        let max_concurrent_jobs = self.max_concurrent_jobs;

        // Clone histories for use in closure
        let scenery_history = self.scenery_history.clone();
        let disk_history = self.disk_history.clone();
        let provider_name = self.config.provider_name.clone();

        // Calculate confirmation remaining time for display
        let confirmation_remaining = self.confirmation_remaining();

        // Clone prewarm status for rendering and advance spinner if active
        let prewarm_status = self.prewarm_status.clone();
        let prewarm_spinner = if prewarm_status.is_some() {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            Some(SPINNER_FRAMES[self.spinner_frame])
        } else {
            None
        };

        // Get aircraft position status from APT module
        let aircraft_position_status = self
            .aircraft_position
            .as_ref()
            .map(|apt| apt.status())
            .unwrap_or_default();

        self.terminal.draw(|frame| {
            render::render_ui(
                frame,
                snapshot,
                &self.network_history,
                &scenery_history,
                &disk_history,
                &provider_name,
                uptime,
                &cache_config,
                &prefetch_snapshot,
                &aircraft_position_status,
                control_plane_snapshot.as_ref(),
                max_concurrent_jobs,
                confirmation_remaining,
                prewarm_status.as_ref(),
                prewarm_spinner,
            );
        })?;

        Ok(())
    }

    /// Check for events (non-blocking).
    ///
    /// Implements a confirmation flow for quit to prevent accidental termination:
    /// - First 'q' press: enters confirmation mode (5 second timeout)
    /// - Second 'q' or 'y'/'Y': confirms quit
    /// - 'n'/'N' or Esc: cancels confirmation
    /// - Timeout: auto-cancels after 5 seconds
    pub fn poll_event(&mut self) -> io::Result<Option<DashboardEvent>> {
        // Check shutdown flag first (e.g., Ctrl+C signal)
        let shutdown_flag = self.shutdown.load(Ordering::SeqCst);
        if shutdown_flag {
            tracing::info!("poll_event: shutdown flag is TRUE, returning Quit");
            return Ok(Some(DashboardEvent::Quit));
        }

        // Check for confirmation timeout (auto-cancel)
        if let Some(confirm_time) = self.quit_confirmation {
            if confirm_time.elapsed() > QUIT_CONFIRM_TIMEOUT {
                self.quit_confirmation = None;
            }
        }

        // Poll for keyboard events
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Handle based on confirmation state
                    if self.quit_confirmation.is_some() {
                        // Currently awaiting confirmation
                        match key.code {
                            // Confirm quit
                            KeyCode::Char('q')
                            | KeyCode::Char('Q')
                            | KeyCode::Char('y')
                            | KeyCode::Char('Y') => {
                                return Ok(Some(DashboardEvent::Quit));
                            }
                            // Cancel confirmation
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                self.quit_confirmation = None;
                            }
                            _ => {}
                        }
                    } else {
                        // Not in confirmation mode
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                // Enter confirmation mode instead of quitting immediately
                                self.quit_confirmation = Some(Instant::now());
                            }
                            KeyCode::Esc => {
                                // Esc also triggers confirmation
                                self.quit_confirmation = Some(Instant::now());
                            }
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                // Cancel current operation (e.g., pre-warm)
                                if self.prewarm_status.is_some() {
                                    return Ok(Some(DashboardEvent::Cancel));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Returns the remaining time for confirmation timeout, if confirming.
    fn confirmation_remaining(&self) -> Option<Duration> {
        self.quit_confirmation
            .map(|t| QUIT_CONFIRM_TIMEOUT.saturating_sub(t.elapsed()))
    }

    /// Draw the loading state UI.
    pub fn draw_loading(&mut self) -> io::Result<()> {
        // Advance spinner
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        let spinner = SPINNER_FRAMES[self.spinner_frame];

        let state = self.state.clone();
        if let DashboardState::Loading(ref progress) = state {
            self.terminal.draw(|frame| {
                render_loading::render_loading_ui(frame, progress, spinner);
            })?;
        }

        Ok(())
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
