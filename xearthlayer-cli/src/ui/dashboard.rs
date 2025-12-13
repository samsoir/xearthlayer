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
use xearthlayer::telemetry::TelemetrySnapshot;

use super::widgets::{
    CacheConfig, CacheWidget, ErrorsWidget, NetworkHistory, NetworkWidget, PipelineWidget,
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

/// The main dashboard UI.
pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    config: DashboardConfig,
    network_history: NetworkHistory,
    shutdown: Arc<AtomicBool>,
    start_time: Instant,
}

impl Dashboard {
    /// Create a new dashboard.
    pub fn new(config: DashboardConfig, shutdown: Arc<AtomicBool>) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self {
            terminal,
            config,
            network_history: NetworkHistory::new(60), // 60 samples for sparkline
            shutdown,
            start_time: Instant::now(),
        })
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
        // Update network history
        self.network_history.push(snapshot.bytes_per_second);

        let uptime = self.start_time.elapsed();
        let cache_config = CacheConfig {
            memory_max_bytes: self.config.memory_cache_max,
            disk_max_bytes: self.config.disk_cache_max,
        };

        self.terminal.draw(|frame| {
            Self::render_ui(
                frame,
                snapshot,
                &self.network_history,
                uptime,
                &cache_config,
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
    fn render_ui(
        frame: &mut Frame,
        snapshot: &TelemetrySnapshot,
        network_history: &NetworkHistory,
        uptime: Duration,
        cache_config: &CacheConfig,
    ) {
        let size = frame.area();

        // Main layout: header, pipeline, network, cache, errors
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Length(6), // Pipeline
                Constraint::Length(3), // Network (increased from 2)
                Constraint::Length(6), // Cache
                Constraint::Length(3), // Errors (increased from 2)
                Constraint::Min(0),    // Padding
            ])
            .split(size);

        // Header
        Self::render_header(frame, chunks[0], uptime);

        // Pipeline widget
        let pipeline_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(pipeline_block, chunks[1]);
        let pipeline_inner = Self::inner_rect(chunks[1], 1, 1);
        frame.render_widget(PipelineWidget::new(snapshot), pipeline_inner);

        // Network widget
        let network_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(network_block, chunks[2]);
        let network_inner = Self::inner_rect(chunks[2], 1, 1);
        frame.render_widget(NetworkWidget::new(snapshot, network_history), network_inner);

        // Cache widget
        let cache_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(cache_block, chunks[3]);
        let cache_inner = Self::inner_rect(chunks[3], 1, 1);
        frame.render_widget(
            CacheWidget::new(snapshot).with_config(cache_config.clone()),
            cache_inner,
        );

        // Errors widget
        let errors_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(errors_block, chunks[4]);
        let errors_inner = Self::inner_rect(chunks[4], 1, 1);
        frame.render_widget(ErrorsWidget::new(snapshot), errors_inner);
    }

    fn render_header(frame: &mut Frame, area: Rect, uptime: Duration) {
        let uptime_str = format_duration(uptime);

        let header_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " XEarthLayer Pipeline Status ",
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
