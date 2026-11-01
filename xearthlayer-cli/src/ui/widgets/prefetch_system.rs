//! Prefetch system status widget.
//!
//! Two lines: the prefetcher's state with its flight phase and cycle size,
//! and the prefetch lane's capacity in use as a meter with its backlog. FUSE
//! latency is a demand-side figure and lives in the Scenery System pane.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::prefetch::{DetailedPrefetchStats, FlightPhase, PrefetchState};

use super::primitives::ProgressBar;

const LABEL_COLOR: Color = Color::DarkGray;

/// Cells in the capacity meter.
const CAPACITY_METER_WIDTH: usize = 10;

/// How full the prefetch lane is: jobs running against its group size, plus
/// the jobs waiting for admission (a full group or the demand pause).
#[derive(Debug, Clone, Copy)]
pub struct LaneView {
    pub running: usize,
    pub group: usize,
    pub queued: usize,
}

impl LaneView {
    /// Running jobs as a whole percent of the group, capped at 100. Zero
    /// while the group size is unknown.
    fn utilisation_percent(&self) -> u64 {
        if self.group == 0 {
            return 0;
        }
        ((self.running as f64 * 100.0 / self.group as f64).round() as u64).min(100)
    }
}

/// Widget displaying prefetch system status.
pub struct PrefetchSystemWidget<'a> {
    state: &'a PrefetchState,
    phase: FlightPhase,
    stats: Option<&'a DetailedPrefetchStats>,
    lane: LaneView,
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
        }
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
    /// percent, then the jobs waiting for admission. Raw job counts against
    /// the group size meant nothing to a user; a share of capacity does.
    fn capacity_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "   Capacity: ",
            Style::default().fg(LABEL_COLOR),
        )];
        spans.extend(
            ProgressBar::from_usize(self.lane.running, self.lane.group, CAPACITY_METER_WIDTH)
                .filled_color(Color::Cyan)
                .to_spans(),
        );
        spans.push(Span::styled(
            format!(" {:>3}%", self.lane.utilisation_percent()),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(
            format!(" │ {} queued", group_thousands(self.lane.queued)),
            Style::default().fg(LABEL_COLOR),
        ));
        Line::from(spans)
    }
}

impl Widget for PrefetchSystemWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = vec![self.state_line(), self.capacity_line()];
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

    /// The capacity row as rendered, from its label onwards.
    fn capacity_row(out: &str) -> &str {
        out.lines()
            .find(|l| l.contains("Capacity:"))
            .expect("capacity row")
            .trim_end()
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
    fn renders_lane_capacity_as_a_percentage_meter_with_the_queue() {
        let state = PrefetchState::Active;
        let out = render(widget_with(&state, LANE));
        // 28 of 30 running is 93%: nine of ten cells filled.
        assert_eq!(
            capacity_row(&out),
            "   Capacity: █████████░  93% │ 1,904 queued"
        );
        assert!(!out.contains("28/30"), "no raw job counts, got:\n{out}");
    }

    #[test]
    fn capacity_meter_never_reads_above_100_percent() {
        let state = PrefetchState::Active;
        let lane = LaneView {
            running: 35,
            group: 30,
            queued: 0,
        };
        let out = render(widget_with(&state, lane));
        assert_eq!(
            capacity_row(&out),
            "   Capacity: ██████████ 100% │ 0 queued"
        );
    }

    #[test]
    fn capacity_meter_is_empty_when_the_lane_size_is_unknown() {
        let state = PrefetchState::Idle;
        let lane = LaneView {
            running: 0,
            group: 0,
            queued: 0,
        };
        let out = render(widget_with(&state, lane));
        assert_eq!(
            capacity_row(&out),
            "   Capacity: ░░░░░░░░░░   0% │ 0 queued"
        );
    }
}
