//! Pipeline stage visualization widget.
//!
//! Shows pipeline stages with sparklines and throughput rates in 4 columns:
//! DOWNLOAD | ASSEMBLY | ENCODE | TILES
//! Each with sparkline, rate, and active count using grid layout.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::telemetry::TelemetrySnapshot;

/// Sparkline characters for rate visualization (8 levels).
const SPARKLINE_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Pipeline rate history for sparkline display.
#[derive(Debug, Clone)]
pub struct PipelineHistory {
    /// Download rate history (chunks/s)
    download_rates: Vec<f64>,
    /// Assembly rate history (jobs/s)
    assembly_rates: Vec<f64>,
    /// Encode rate history (encodes/s)
    encode_rates: Vec<f64>,
    /// Tiles completed rate history (tiles/s)
    tiles_rates: Vec<f64>,
    /// Previous snapshot values for delta calculation
    prev_chunks_downloaded: u64,
    prev_jobs_completed: u64,
    prev_encodes_completed: u64,
    prev_tiles_completed: u64,
    /// Maximum samples to keep
    max_samples: usize,
}

impl PipelineHistory {
    /// Create new pipeline history with specified sample count.
    pub fn new(max_samples: usize) -> Self {
        Self {
            download_rates: Vec::with_capacity(max_samples),
            assembly_rates: Vec::with_capacity(max_samples),
            encode_rates: Vec::with_capacity(max_samples),
            tiles_rates: Vec::with_capacity(max_samples),
            prev_chunks_downloaded: 0,
            prev_jobs_completed: 0,
            prev_encodes_completed: 0,
            prev_tiles_completed: 0,
            max_samples,
        }
    }

    /// Update history with new telemetry snapshot.
    pub fn update(&mut self, snapshot: &TelemetrySnapshot, sample_interval: f64) {
        if sample_interval <= 0.0 {
            return;
        }

        // Calculate deltas
        let chunks_delta = snapshot
            .chunks_downloaded
            .saturating_sub(self.prev_chunks_downloaded);
        let jobs_delta = snapshot
            .jobs_completed
            .saturating_sub(self.prev_jobs_completed);
        let encodes_delta = snapshot
            .encodes_completed
            .saturating_sub(self.prev_encodes_completed);
        let tiles_delta = snapshot
            .jobs_completed
            .saturating_sub(self.prev_tiles_completed);

        // Calculate rates
        let download_rate = chunks_delta as f64 / sample_interval;
        let assembly_rate = jobs_delta as f64 / sample_interval;
        let encode_rate = encodes_delta as f64 / sample_interval;
        let tiles_rate = tiles_delta as f64 / sample_interval;

        // Store current values for next delta
        self.prev_chunks_downloaded = snapshot.chunks_downloaded;
        self.prev_jobs_completed = snapshot.jobs_completed;
        self.prev_encodes_completed = snapshot.encodes_completed;
        self.prev_tiles_completed = snapshot.jobs_completed;

        // Add to history, removing oldest if at capacity
        Self::push_rate(&mut self.download_rates, download_rate, self.max_samples);
        Self::push_rate(&mut self.assembly_rates, assembly_rate, self.max_samples);
        Self::push_rate(&mut self.encode_rates, encode_rate, self.max_samples);
        Self::push_rate(&mut self.tiles_rates, tiles_rate, self.max_samples);
    }

    fn push_rate(rates: &mut Vec<f64>, rate: f64, max_samples: usize) {
        if rates.len() >= max_samples {
            rates.remove(0);
        }
        rates.push(rate);
    }

    /// Get download rate sparkline string.
    pub fn download_sparkline(&self) -> String {
        Self::sparkline_from_rates(&self.download_rates)
    }

    /// Get assembly rate sparkline string.
    pub fn assembly_sparkline(&self) -> String {
        Self::sparkline_from_rates(&self.assembly_rates)
    }

    /// Get encode rate sparkline string.
    pub fn encode_sparkline(&self) -> String {
        Self::sparkline_from_rates(&self.encode_rates)
    }

    /// Get tiles rate sparkline string.
    pub fn tiles_sparkline(&self) -> String {
        Self::sparkline_from_rates(&self.tiles_rates)
    }

    /// Get current download rate (chunks/s).
    pub fn current_download_rate(&self) -> f64 {
        self.download_rates.last().copied().unwrap_or(0.0)
    }

    /// Get current assembly rate (tiles/s).
    pub fn current_assembly_rate(&self) -> f64 {
        self.assembly_rates.last().copied().unwrap_or(0.0)
    }

    /// Get current encode rate (encodes/s).
    pub fn current_encode_rate(&self) -> f64 {
        self.encode_rates.last().copied().unwrap_or(0.0)
    }

    /// Get current tiles rate (tiles/s).
    pub fn current_tiles_rate(&self) -> f64 {
        self.tiles_rates.last().copied().unwrap_or(0.0)
    }

    /// Generate sparkline string from rate history.
    fn sparkline_from_rates(rates: &[f64]) -> String {
        if rates.is_empty() {
            return "▁▁▁▁▁▁▁▁▁▁▁▁".to_string();
        }

        let max_rate = rates.iter().cloned().fold(0.0_f64, f64::max);
        if max_rate == 0.0 {
            return rates.iter().map(|_| '▁').collect();
        }

        rates
            .iter()
            .map(|&rate| {
                let normalized = (rate / max_rate).min(1.0);
                let index = ((normalized * 7.0).round() as usize).min(7);
                SPARKLINE_CHARS[index]
            })
            .collect()
    }
}

/// Widget displaying pipeline stage status with sparklines.
pub struct PipelineWidget<'a> {
    snapshot: &'a TelemetrySnapshot,
    history: Option<&'a PipelineHistory>,
}

impl<'a> PipelineWidget<'a> {
    pub fn new(snapshot: &'a TelemetrySnapshot) -> Self {
        Self {
            snapshot,
            history: None,
        }
    }

    /// Set pipeline history for sparkline display.
    pub fn with_history(mut self, history: &'a PipelineHistory) -> Self {
        self.history = Some(history);
        self
    }
}

impl Widget for PipelineWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Get stage metrics from snapshot
        let download_active = self.snapshot.downloads_active;
        let assembly_active = self.snapshot.jobs_active;
        let encode_active = self.snapshot.encodes_active;
        let tiles_completed = self.snapshot.jobs_completed;

        // Get sparklines and rates from history if available
        let (
            download_sparkline,
            download_rate,
            assembly_sparkline,
            assembly_rate,
            encode_sparkline,
            encode_rate,
            tiles_sparkline,
            tiles_rate,
        ) = if let Some(history) = self.history {
            (
                history.download_sparkline(),
                history.current_download_rate(),
                history.assembly_sparkline(),
                history.current_assembly_rate(),
                history.encode_sparkline(),
                history.current_encode_rate(),
                history.tiles_sparkline(),
                history.current_tiles_rate(),
            )
        } else {
            (
                "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                0.0,
                "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                0.0,
                "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                0.0,
                "▁▁▁▁▁▁▁▁▁▁▁▁".to_string(),
                0.0,
            )
        };

        // Use 4-column grid layout for stages
        // Each column: 1/4 of available width
        let columns = Layout::horizontal([
            Constraint::Ratio(1, 4), // DOWNLOAD
            Constraint::Ratio(1, 4), // ASSEMBLY
            Constraint::Ratio(1, 4), // ENCODE
            Constraint::Ratio(1, 4), // TILES
        ])
        .split(area);

        // Render each stage column
        Self::render_stage_column(
            columns[0],
            buf,
            "DOWNLOAD",
            Color::Yellow,
            &download_sparkline,
            download_rate,
            Some(download_active as u64),
            None,
        );

        Self::render_stage_column(
            columns[1],
            buf,
            "ASSEMBLY",
            Color::Magenta,
            &assembly_sparkline,
            assembly_rate,
            Some(assembly_active as u64),
            None,
        );

        Self::render_stage_column(
            columns[2],
            buf,
            "ENCODE",
            Color::Blue,
            &encode_sparkline,
            encode_rate,
            Some(encode_active as u64),
            None,
        );

        Self::render_stage_column(
            columns[3],
            buf,
            "TILES",
            Color::Green,
            &tiles_sparkline,
            tiles_rate,
            None,
            Some(tiles_completed),
        );
    }
}

impl PipelineWidget<'_> {
    /// Render a single stage column with label, sparkline, and metrics.
    /// Layout: Row 1 = Label, Row 2 = Sparkline, Row 3 = Rate + active/total
    #[allow(clippy::too_many_arguments)]
    fn render_stage_column(
        area: Rect,
        buf: &mut Buffer,
        label: &str,
        color: Color,
        sparkline: &str,
        rate: f64,
        active: Option<u64>,
        total: Option<u64>,
    ) {
        // Row 1: Label (centered)
        let label_line = Line::from(Span::styled(
            format!("{:^width$}", label, width = area.width as usize),
            Style::default().fg(color),
        ));

        // Row 2: Sparkline (centered)
        let sparkline_width = sparkline.chars().count();
        let sparkline_padding = (area.width as usize).saturating_sub(sparkline_width) / 2;
        let sparkline_line = Line::from(Span::styled(
            format!("{:padding$}{}", "", sparkline, padding = sparkline_padding),
            Style::default().fg(color),
        ));

        // Row 3: Rate and active count OR tiles count with suffix
        let metrics_line = if let Some(active_count) = active {
            // DOWNLOAD, ASSEMBLY, ENCODE columns: show rate + active count
            Line::from(vec![
                Span::styled(
                    format!("{:>width$}", format!("{:.0}/s", rate), width = 8),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!(" ({:>2})", active_count),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else if let Some(total_count) = total {
            // TILES column: show rate + total count with "tiles" suffix
            Line::from(vec![
                Span::styled(
                    format!("{:>width$}", format!("{:.0}/s", rate), width = 8),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!(" ({} tiles)", total_count),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else {
            Line::from(vec![Span::styled(
                format!("{:>width$}", format!("{:.0}/s", rate), width = 8),
                Style::default().fg(color),
            )])
        };

        // Render rows: label, sparkline, metrics
        if area.height >= 1 {
            Paragraph::new(label_line).render(
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }

        if area.height >= 2 {
            Paragraph::new(sparkline_line).render(
                Rect {
                    x: area.x,
                    y: area.y + 1,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }

        if area.height >= 3 {
            Paragraph::new(metrics_line).render(
                Rect {
                    x: area.x,
                    y: area.y + 2,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }
}
