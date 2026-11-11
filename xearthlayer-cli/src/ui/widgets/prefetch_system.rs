//! Prefetch system status widget.
//!
//! Two lines: the prefetcher's state with its flight phase and cycle size,
//! and the prefetch lane's utilization as a meter, a sparkline of the last
//! twelve seconds, and its backlog. FUSE latency is a demand-side figure and
//! lives in the Scenery System pane.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::prefetch::{DetailedPrefetchStats, FlightPhase, PrefetchState};

use super::primitives::{ProgressBar, SparklineHistory};

const LABEL_COLOR: Color = Color::DarkGray;

/// Cells in the utilization meter.
const UTILIZATION_METER_WIDTH: usize = 10;

/// Cells in the utilization sparkline: one per [`SAMPLE_PERIOD`].
pub const UTILIZATION_SPARKLINE_WIDTH: usize = 12;

/// How much lane time one sparkline cell covers. The dashboard draws every
/// 100 ms, and twelve draws would show 1.2 s of history: too short to read a
/// trend from. Each cell is the mean utilization over this period instead.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// How full the prefetch lane is: jobs running against its group size, plus
/// the jobs waiting for admission (a full group or the demand pause).
#[derive(Debug, Clone, Copy)]
pub struct LaneView {
    pub running: usize,
    pub group: usize,
    pub queued: usize,
}

impl LaneView {
    /// Running jobs as a share of the group, capped at 1.0. Zero while the
    /// group size is unknown.
    fn utilization(&self) -> f64 {
        if self.group == 0 {
            return 0.0;
        }
        (self.running as f64 / self.group as f64).min(1.0)
    }

    /// Running jobs as a whole percent of the group, capped at 100.
    fn utilization_percent(&self) -> u64 {
        (self.utilization() * 100.0).round() as u64
    }
}

/// The lane's utilization over the last [`UTILIZATION_SPARKLINE_WIDTH`]
/// sample periods, one cell per period, each the mean of the samples the
/// dashboard recorded during it. The period in progress counts as the
/// newest cell, so a change shows on the next draw rather than a second
/// later.
#[derive(Debug, Clone)]
pub struct LaneHistory {
    /// Closed periods, oldest first.
    closed: SparklineHistory,
    /// When the open period began; `None` before the first sample.
    open_since: Option<Instant>,
    /// Sum and count of the open period's samples.
    open_sum: f64,
    open_count: u32,
}

impl Default for LaneHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneHistory {
    pub fn new() -> Self {
        Self {
            closed: SparklineHistory::new(UTILIZATION_SPARKLINE_WIDTH),
            open_since: None,
            open_sum: 0.0,
            open_count: 0,
        }
    }

    /// Record the lane as seen at `now`. A sample a full period or more
    /// after the open period began closes that period first.
    pub fn record(&mut self, lane: LaneView, now: Instant) {
        match self.open_since {
            Some(since) if now.duration_since(since) >= SAMPLE_PERIOD => {
                self.closed.push(self.open_mean());
                self.open_since = Some(now);
                self.open_sum = 0.0;
                self.open_count = 0;
            }
            Some(_) => {}
            None => self.open_since = Some(now),
        }
        self.open_sum += lane.utilization();
        self.open_count += 1;
    }

    fn open_mean(&self) -> f64 {
        if self.open_count == 0 {
            0.0
        } else {
            self.open_sum / f64::from(self.open_count)
        }
    }

    /// The cells to draw, oldest first: the closed periods that fit beside
    /// the open one, then the open one. Empty before the first sample.
    pub fn cells(&self) -> Vec<f64> {
        if self.open_since.is_none() {
            return Vec::new();
        }
        let closed = self.closed.values();
        let keep = closed.len().min(UTILIZATION_SPARKLINE_WIDTH - 1);
        let mut cells = closed[closed.len() - keep..].to_vec();
        cells.push(self.open_mean());
        cells
    }

    /// The cells as a sparkline of fixed width, scaled against a full lane
    /// rather than the recent peak so the shape agrees with the percentage
    /// beside it. Padded on the left while the history is short.
    pub fn sparkline(&self) -> String {
        let cells = self.cells();
        if cells.is_empty() {
            return " ".repeat(UTILIZATION_SPARKLINE_WIDTH);
        }
        let drawn = SparklineHistory::render_sparkline_with_max(&cells, 1.0);
        let padding = UTILIZATION_SPARKLINE_WIDTH.saturating_sub(drawn.chars().count());
        format!("{}{drawn}", " ".repeat(padding))
    }
}

/// Widget displaying prefetch system status.
pub struct PrefetchSystemWidget<'a> {
    state: &'a PrefetchState,
    phase: FlightPhase,
    stats: Option<&'a DetailedPrefetchStats>,
    lane: LaneView,
    history: Option<&'a LaneHistory>,
}

impl<'a> PrefetchSystemWidget<'a> {
    pub fn new(
        state: &'a PrefetchState,
        phase: FlightPhase,
        stats: Option<&'a DetailedPrefetchStats>,
        lane: LaneView,
    ) -> Self {
        Self {
            state,
            phase,
            stats,
            lane,
            history: None,
        }
    }

    /// Show the lane's recent utilization as a sparkline beside the meter.
    pub fn with_history(mut self, history: &'a LaneHistory) -> Self {
        self.history = Some(history);
        self
    }

    fn state_color(&self) -> Color {
        match self.state {
            PrefetchState::Disabled | PrefetchState::Idle => Color::DarkGray,
            PrefetchState::PausedForDemand => Color::Magenta,
            PrefetchState::HoldingTakeoff | PrefetchState::DeferredLoad => Color::Yellow,
            PrefetchState::Ramping { .. } => Color::Cyan,
            PrefetchState::Active => Color::Green,
        }
    }

    fn state_line(&self) -> Line<'static> {
        let color = self.state_color();
        let per_cycle = self.stats.map_or(0, |s| s.tiles_submitted_last_cycle);
        Line::from(vec![
            Span::styled("   Prefetch: ", Style::default().fg(LABEL_COLOR)),
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(self.state.label(), Style::default().fg(color)),
            Span::styled(
                format!(" │ {} │ {per_cycle}/cyc", self.phase),
                Style::default().fg(LABEL_COLOR),
            ),
        ])
    }

    /// How much of the lane's capacity is in use, as a meter and a whole
    /// percent, then its recent history when given one, then the jobs
    /// waiting for admission. Raw job counts against the group size meant
    /// nothing to a user; a share of capacity does. The label is as wide as
    /// the state row's so the two rows' values align.
    fn utilization_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "Utilization: ",
            Style::default().fg(LABEL_COLOR),
        )];
        spans.extend(
            ProgressBar::from_usize(self.lane.running, self.lane.group, UTILIZATION_METER_WIDTH)
                .filled_color(Color::Cyan)
                .to_spans(),
        );
        spans.push(Span::styled(
            format!(" {:>3}%", self.lane.utilization_percent()),
            Style::default().fg(Color::White),
        ));
        if let Some(history) = self.history {
            spans.push(Span::styled(" │ ", Style::default().fg(LABEL_COLOR)));
            spans.push(Span::styled(
                history.sparkline(),
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.push(Span::styled(
            format!(" │ {} queued", group_thousands(self.lane.queued)),
            Style::default().fg(LABEL_COLOR),
        ));
        Line::from(spans)
    }
}

impl Widget for PrefetchSystemWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = vec![self.state_line(), self.utilization_line()];
        Paragraph::new(text).render(area, buf);
    }
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::prefetch::FlightPhase;

    const LANE: LaneView = LaneView {
        running: 28,
        group: 30,
        queued: 1_904,
    };

    fn render(widget: PrefetchSystemWidget<'_>) -> String {
        let area = Rect::new(0, 0, 100, 3);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn widget_with(state: &PrefetchState, lane: LaneView) -> PrefetchSystemWidget<'_> {
        PrefetchSystemWidget::new(state, FlightPhase::Cruise, None, lane)
    }

    /// A row as rendered, from its label onwards.
    fn row<'a>(out: &'a str, label: &str) -> &'a str {
        out.lines()
            .find(|l| l.contains(label))
            .unwrap_or_else(|| panic!("no {label} row in:\n{out}"))
            .trim_end()
    }

    fn lane(running: usize) -> LaneView {
        LaneView {
            running,
            group: 30,
            queued: 0,
        }
    }

    #[test]
    fn has_no_latency_row() {
        let state = PrefetchState::Active;
        let out = render(widget_with(&state, LANE));
        assert!(
            !out.contains("Latency:"),
            "latency lives in Scenery System, got:\n{out}"
        );
    }

    #[test]
    fn renders_the_state_label() {
        let state = PrefetchState::PausedForDemand;
        let out = render(widget_with(&state, LANE));
        assert!(out.contains("Paused (demand)"), "got:\n{out}");
    }

    #[test]
    fn renders_lane_utilization_as_a_percentage_meter_with_the_queue() {
        let state = PrefetchState::Active;
        let out = render(widget_with(&state, LANE));
        // 28 of 30 running is 93%: nine of ten cells filled.
        assert_eq!(
            row(&out, "Utilization:"),
            "Utilization: █████████░  93% │ 1,904 queued"
        );
        assert!(!out.contains("28/30"), "no raw job counts, got:\n{out}");
        assert!(
            !out.contains("Capacity"),
            "the meter shows utilization, got:\n{out}"
        );
    }

    #[test]
    fn the_two_rows_align_their_values() {
        let state = PrefetchState::Active;
        let out = render(widget_with(&state, LANE));
        let prefetch = row(&out, "Prefetch:");
        let utilization = row(&out, "Utilization:");
        let column = |r: &str, label: &str| r.find(label).unwrap() + label.len();
        assert_eq!(
            column(prefetch, "Prefetch: "),
            column(utilization, "Utilization: "),
            "rows:\n{prefetch}\n{utilization}"
        );
    }

    #[test]
    fn utilization_meter_never_reads_above_100_percent() {
        let state = PrefetchState::Active;
        let out = render(widget_with(&state, lane(35)));
        assert_eq!(
            row(&out, "Utilization:"),
            "Utilization: ██████████ 100% │ 0 queued"
        );
    }

    #[test]
    fn utilization_meter_is_empty_when_the_lane_size_is_unknown() {
        let state = PrefetchState::Idle;
        let lane = LaneView {
            running: 0,
            group: 0,
            queued: 0,
        };
        let out = render(widget_with(&state, lane));
        assert_eq!(
            row(&out, "Utilization:"),
            "Utilization: ░░░░░░░░░░   0% │ 0 queued"
        );
    }

    #[test]
    fn renders_the_utilization_history_between_the_meter_and_the_queue() {
        let state = PrefetchState::Active;
        let t0 = Instant::now();
        let mut history = LaneHistory::new();
        history.record(lane(0), t0);
        history.record(lane(30), t0 + Duration::from_secs(1));
        history.record(lane(15), t0 + Duration::from_secs(2));
        let widget = widget_with(&state, LANE).with_history(&history);
        let out = render(widget);
        // Two closed seconds (0%, 100%) and the live one (50%), padded to
        // twelve cells on the left.
        assert_eq!(
            row(&out, "Utilization:"),
            "Utilization: █████████░  93% │          ▁█▅ │ 1,904 queued"
        );
    }

    #[test]
    fn history_averages_the_samples_of_one_second_into_one_cell() {
        let t0 = Instant::now();
        let mut history = LaneHistory::new();
        for tenth in 0..10 {
            // Alternating empty and full within the first second: 50%.
            let running = if tenth % 2 == 0 { 0 } else { 30 };
            history.record(lane(running), t0 + Duration::from_millis(tenth * 100));
        }
        history.record(lane(30), t0 + Duration::from_secs(1));
        assert_eq!(history.cells(), vec![0.5, 1.0]);
    }

    #[test]
    fn history_scales_against_a_full_lane_not_the_recent_peak() {
        let t0 = Instant::now();
        let mut history = LaneHistory::new();
        history.record(lane(15), t0);
        history.record(lane(15), t0 + Duration::from_secs(1));
        // Half a lane draws at half height even though it is the peak.
        assert_eq!(history.sparkline(), "          ▅▅");
    }

    #[test]
    fn history_keeps_the_last_twelve_seconds() {
        let t0 = Instant::now();
        let mut history = LaneHistory::new();
        for second in 0..20u64 {
            let running = if second < 8 { 30 } else { 0 };
            history.record(lane(running), t0 + Duration::from_secs(second));
        }
        assert_eq!(history.cells().len(), UTILIZATION_SPARKLINE_WIDTH);
        assert_eq!(history.sparkline(), "▁▁▁▁▁▁▁▁▁▁▁▁");
    }

    #[test]
    fn history_counts_an_unknown_lane_size_as_idle() {
        let t0 = Instant::now();
        let mut history = LaneHistory::new();
        history.record(
            LaneView {
                running: 4,
                group: 0,
                queued: 0,
            },
            t0,
        );
        assert_eq!(history.cells(), vec![0.0]);
    }
}
