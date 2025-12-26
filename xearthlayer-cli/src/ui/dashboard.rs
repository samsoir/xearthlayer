//! Main TUI dashboard for XEarthLayer.
//!
//! Displays real-time pipeline status, network throughput, cache utilization,
//! and error rates.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use xearthlayer::pipeline::control_plane::{ControlPlaneHealth, HealthSnapshot};
use xearthlayer::prefetch::{
    GpsStatus, PrefetchMode, PrefetchStatusSnapshot, SharedPrefetchStatus,
};
use xearthlayer::telemetry::TelemetrySnapshot;

use super::widgets::{
    CacheConfig, CacheWidget, ControlPlaneWidget, ErrorsWidget, NetworkHistory, NetworkWidget,
    PipelineHistory, PipelineWidget,
};

/// Events that can occur in the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardEvent {
    /// User requested quit (Ctrl+C or 'q').
    Quit,
}

/// Dashboard configuration.
pub struct DashboardConfig {
    /// Memory cache max size.
    pub memory_cache_max: usize,
    /// Disk cache max size.
    pub disk_cache_max: usize,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            memory_cache_max: 2 * 1024 * 1024 * 1024,
            disk_cache_max: 20 * 1024 * 1024 * 1024,
        }
    }
}

/// Job rate metrics for the control plane display.
#[derive(Debug, Clone)]
pub struct JobRates {
    /// Jobs submitted per second (instantaneous rate).
    pub submitted_per_sec: f64,
    /// Jobs completed per second (instantaneous rate).
    pub completed_per_sec: f64,
}

impl JobRates {
    /// Calculate the pressure delta (submitted - completed per second).
    pub fn pressure(&self) -> f64 {
        self.submitted_per_sec - self.completed_per_sec
    }
}

/// The main dashboard UI.
pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    config: DashboardConfig,
    network_history: NetworkHistory,
    pipeline_history: PipelineHistory,
    shutdown: Arc<AtomicBool>,
    start_time: Instant,
    last_draw: Instant,
    /// Optional prefetch status for display.
    prefetch_status: Option<Arc<SharedPrefetchStatus>>,
    /// Optional control plane health for display.
    control_plane_health: Option<Arc<ControlPlaneHealth>>,
    /// Maximum concurrent jobs for the control plane display.
    max_concurrent_jobs: usize,
    /// Previous control plane snapshot for rate calculation.
    prev_control_plane_snapshot: Option<HealthSnapshot>,
}

impl Dashboard {
    /// Create a new dashboard.
    pub fn new(config: DashboardConfig, shutdown: Arc<AtomicBool>) -> io::Result<Self> {
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
            pipeline_history: PipelineHistory::new(12), // 12 samples for pipeline sparkline
            shutdown,
            start_time: now,
            last_draw: now,
            prefetch_status: None,
            control_plane_health: None,
            max_concurrent_jobs: 0,
            prev_control_plane_snapshot: None,
        })
    }

    /// Set the prefetch status source for display.
    pub fn with_prefetch_status(mut self, status: Arc<SharedPrefetchStatus>) -> Self {
        self.prefetch_status = Some(status);
        self
    }

    /// Set the control plane health source for display.
    pub fn with_control_plane(
        mut self,
        health: Arc<ControlPlaneHealth>,
        max_concurrent_jobs: usize,
    ) -> Self {
        self.control_plane_health = Some(health);
        self.max_concurrent_jobs = max_concurrent_jobs;
        self
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

        // Update pipeline history for sparklines
        self.pipeline_history.update(snapshot, sample_interval);

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
        let control_plane_snapshot = self.control_plane_health.as_ref().map(|h| h.snapshot());
        let max_concurrent_jobs = self.max_concurrent_jobs;

        // Calculate job rates from control plane snapshots
        let job_rates = if let Some(ref current) = control_plane_snapshot {
            if let Some(ref prev) = self.prev_control_plane_snapshot {
                if sample_interval > 0.0 {
                    let submitted_delta = current
                        .total_jobs_submitted
                        .saturating_sub(prev.total_jobs_submitted);
                    let completed_delta = current
                        .total_jobs_completed
                        .saturating_sub(prev.total_jobs_completed);
                    let submitted_rate = submitted_delta as f64 / sample_interval;
                    let completed_rate = completed_delta as f64 / sample_interval;
                    Some(JobRates {
                        submitted_per_sec: submitted_rate,
                        completed_per_sec: completed_rate,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Store current snapshot for next rate calculation
        self.prev_control_plane_snapshot = control_plane_snapshot.clone();

        // Clone pipeline history for use in closure
        let pipeline_history = self.pipeline_history.clone();

        self.terminal.draw(|frame| {
            Self::render_ui(
                frame,
                snapshot,
                &self.network_history,
                &pipeline_history,
                uptime,
                &cache_config,
                &prefetch_snapshot,
                control_plane_snapshot.as_ref(),
                max_concurrent_jobs,
                job_rates.as_ref(),
            );
        })?;

        Ok(())
    }

    /// Check for events (non-blocking).
    pub fn poll_event(&self) -> io::Result<Option<DashboardEvent>> {
        // Check shutdown flag first
        if self.shutdown.load(Ordering::SeqCst) {
            return Ok(Some(DashboardEvent::Quit));
        }

        // Poll for keyboard events
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            return Ok(Some(DashboardEvent::Quit));
                        }
                        KeyCode::Esc => {
                            return Ok(Some(DashboardEvent::Quit));
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(None)
    }

    /// Render the UI to the frame.
    #[allow(clippy::too_many_arguments)]
    fn render_ui(
        frame: &mut Frame,
        snapshot: &TelemetrySnapshot,
        network_history: &NetworkHistory,
        pipeline_history: &PipelineHistory,
        uptime: Duration,
        cache_config: &CacheConfig,
        prefetch_snapshot: &PrefetchStatusSnapshot,
        control_plane_snapshot: Option<&HealthSnapshot>,
        max_concurrent_jobs: usize,
        job_rates: Option<&JobRates>,
    ) {
        let size = frame.area();

        // Main layout: header, prefetch, control plane, pipeline, chunks, network, cache
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Length(5), // Aircraft Position (GPS Status, Position, Prefetch)
                Constraint::Length(5), // Control Plane (needs 4 rows + 1 border)
                Constraint::Length(6), // Pipeline (Tile Pipeline)
                Constraint::Length(3), // Chunk Tasks (moved above Network)
                Constraint::Length(3), // Network
                Constraint::Length(7), // Cache (5 content + 1 border + 1 title)
                Constraint::Min(0),    // Padding
            ])
            .split(size);

        // Header
        Self::render_header(frame, chunks[0], uptime);

        // Prefetch/Aircraft widget
        Self::render_prefetch(frame, chunks[1], prefetch_snapshot);

        // Control plane widget
        Self::render_control_plane(
            frame,
            chunks[2],
            control_plane_snapshot,
            max_concurrent_jobs,
            job_rates,
        );

        // Pipeline widget (Tile Pipeline)
        let pipeline_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Tile Pipeline ",
                Style::default().fg(Color::Blue),
            ));
        frame.render_widget(pipeline_block, chunks[3]);
        let pipeline_inner = Self::inner_rect(chunks[3], 1, 1);
        frame.render_widget(
            PipelineWidget::new(snapshot).with_history(pipeline_history),
            pipeline_inner,
        );

        // Chunk Tasks widget (moved above Network)
        let chunks_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Chunk Tasks ",
                Style::default().fg(Color::Blue),
            ));
        frame.render_widget(chunks_block, chunks[4]);
        let chunks_inner = Self::inner_rect(chunks[4], 1, 1);
        frame.render_widget(ErrorsWidget::new(snapshot), chunks_inner);

        // Network widget
        let network_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" Network ", Style::default().fg(Color::Blue)));
        frame.render_widget(network_block, chunks[5]);
        let network_inner = Self::inner_rect(chunks[5], 1, 1);
        frame.render_widget(NetworkWidget::new(snapshot, network_history), network_inner);

        // Cache widget
        let cache_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" Cache ", Style::default().fg(Color::Blue)));
        frame.render_widget(cache_block, chunks[6]);
        let cache_inner = Self::inner_rect(chunks[6], 1, 1);
        frame.render_widget(
            CacheWidget::new(snapshot).with_config(cache_config.clone()),
            cache_inner,
        );
    }

    /// Render the control plane status section.
    fn render_control_plane(
        frame: &mut Frame,
        area: Rect,
        snapshot: Option<&HealthSnapshot>,
        max_concurrent_jobs: usize,
        job_rates: Option<&JobRates>,
    ) {
        let control_plane_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Control Plane ",
                Style::default().fg(Color::Blue),
            ));

        frame.render_widget(control_plane_block, area);
        let inner = Self::inner_rect(area, 1, 1);

        if let Some(health_snapshot) = snapshot {
            frame.render_widget(
                ControlPlaneWidget::new(health_snapshot, max_concurrent_jobs)
                    .with_job_rates(job_rates),
                inner,
            );
        } else {
            // No control plane configured - show placeholder
            let text = vec![Line::from(vec![Span::styled(
                "Control plane not configured",
                Style::default().fg(Color::DarkGray),
            )])];
            let paragraph = Paragraph::new(text);
            frame.render_widget(paragraph, inner);
        }
    }

    /// Render the aircraft position section.
    fn render_prefetch(frame: &mut Frame, area: Rect, prefetch: &PrefetchStatusSnapshot) {
        let prefetch_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Aircraft Position ",
                Style::default().fg(Color::Magenta),
            ));

        frame.render_widget(prefetch_block, area);
        let inner = Self::inner_rect(area, 1, 1);

        // GPS Status line with colored indicator
        let (gps_indicator, gps_text, gps_color) = match prefetch.gps_status {
            GpsStatus::Connected => ("●", "Connected", Color::Green),
            GpsStatus::Acquiring => ("●", "Acquiring...", Color::Yellow),
            GpsStatus::Inferred => ("●", "Inferred", Color::Red),
        };

        let gps_line = Line::from(vec![
            Span::styled("GPS Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ", gps_indicator),
                Style::default().fg(gps_color),
            ),
            Span::styled(gps_text, Style::default().fg(gps_color)),
        ]);

        // Position line
        let position_line = if prefetch.aircraft.is_some() {
            Line::from(vec![
                Span::styled("Position:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(prefetch.aircraft_line(), Style::default().fg(Color::Green)),
            ])
        } else {
            Line::from(vec![
                Span::styled("Position:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Waiting for X-Plane telemetry...",
                    Style::default().fg(Color::Yellow),
                ),
            ])
        };

        // Prefetch mode line
        let mode_color = match prefetch.prefetch_mode {
            PrefetchMode::Telemetry => Color::Green,
            PrefetchMode::FuseInference => Color::Yellow,
            PrefetchMode::Radial => Color::Cyan,
            PrefetchMode::Idle => Color::DarkGray,
        };

        let prefetch_line = Line::from(vec![
            Span::styled("Prefetch:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", prefetch.prefetch_mode),
                Style::default().fg(mode_color),
            ),
        ]);

        let text = vec![gps_line, position_line, prefetch_line];
        let paragraph = Paragraph::new(text);
        frame.render_widget(paragraph, inner);
    }

    fn render_header(frame: &mut Frame, area: Rect, uptime: Duration) {
        let uptime_str = format_duration(uptime);

        let header_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                format!(" X-Plane Earth Layer {} ", xearthlayer::VERSION),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_alignment(ratatui::layout::Alignment::Left);

        let uptime_text = Paragraph::new(Line::from(vec![
            Span::styled("Uptime: ", Style::default().fg(Color::DarkGray)),
            Span::styled(uptime_str, Style::default().fg(Color::White)),
            Span::styled("  │  Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" to quit", Style::default().fg(Color::DarkGray)),
        ]))
        .block(header_block)
        .alignment(ratatui::layout::Alignment::Right);

        frame.render_widget(uptime_text, area);
    }

    fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
        Rect {
            x: area.x + margin_x,
            y: area.y + margin_y,
            width: area.width.saturating_sub(margin_x * 2),
            height: area.height.saturating_sub(margin_y * 2),
        }
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Format duration as HH:MM:SS.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

/// Simple non-TUI fallback for non-interactive terminals.
pub fn print_simple_status(snapshot: &TelemetrySnapshot) {
    println!(
        "[{}] Tiles: {} completed, {} active | Throughput: {} | Cache: {:.0}% mem, {:.0}% disk",
        snapshot.uptime_human(),
        snapshot.jobs_completed,
        snapshot.jobs_active,
        snapshot.throughput_human(),
        snapshot.memory_cache_hit_rate * 100.0,
        snapshot.disk_cache_hit_rate * 100.0,
    );
}

/// Print final session summary.
pub fn print_session_summary(snapshot: &TelemetrySnapshot) {
    println!();
    println!("Session Summary");
    println!("───────────────");
    println!(
        "  Tiles generated: {} ({} failed)",
        snapshot.jobs_completed, snapshot.jobs_failed
    );
    println!(
        "  Tiles coalesced: {} ({:.0}% savings)",
        snapshot.jobs_coalesced,
        snapshot.coalescing_rate() * 100.0
    );
    println!("  Data downloaded: {}", snapshot.bytes_downloaded_human());
    println!(
        "  Memory cache: {:.1}% hit rate ({} hits)",
        snapshot.memory_cache_hit_rate * 100.0,
        snapshot.memory_cache_hits
    );
    println!(
        "  Disk cache: {:.1}% hit rate ({} hits)",
        snapshot.disk_cache_hit_rate * 100.0,
        snapshot.disk_cache_hits
    );
    println!("  Avg throughput: {}", snapshot.throughput_human());
    println!("  Uptime: {}", snapshot.uptime_human());
}
