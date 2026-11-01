//! FUSE generation latency line.
//!
//! One line: the session p50, p95 and max of FUSE waits that needed a tile
//! generation (coloured from the latest sample against the generation
//! timeout), the share of FUSE requests served from cache, and the count of
//! waits that hit the timeout. Rendered in the Scenery System pane beside the
//! tile request metrics it describes.

use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::LatencySummary;

const LABEL_COLOR: Color = Color::DarkGray;

/// Widget displaying the FUSE generation latency summary on one line.
pub struct FuseLatencyWidget<'a> {
    latency: Option<&'a LatencySummary>,
    recent: Option<&'a LatencySummary>,
    generation_timeout: Duration,
    timeouts: usize,
    cached: u64,
    generated: u64,
}

impl<'a> FuseLatencyWidget<'a> {
    pub fn new(latency: Option<&'a LatencySummary>, generation_timeout: Duration) -> Self {
        Self {
            latency,
            recent: None,
            generation_timeout,
            timeouts: 0,
            cached: 0,
            generated: 0,
        }
    }

    /// The most recent sample that had FUSE generations. The colour comes
    /// from it so one early stall does not keep the session line red; until
    /// the first sample closes the session figures colour the line.
    pub fn with_recent(mut self, recent: Option<&'a LatencySummary>) -> Self {
        self.recent = recent;
        self
    }

    /// FUSE waits that hit the generation timeout this session. Shown only
    /// when non-zero: the no-harm bar counts them.
    pub fn with_timeouts(mut self, timeouts: usize) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Session FUSE requests served from cache against those that needed a
    /// generation. Latency covers generations only, so without this the line
    /// is empty on a well-prefetched flight and reads as a broken component.
    pub fn with_cache_served(mut self, cached: u64, generated: u64) -> Self {
        self.cached = cached;
        self.generated = generated;
        self
    }

    /// Whole percent, but never 100 unless every request was cache-served.
    fn cached_share(&self) -> Option<String> {
        let total = self.cached + self.generated;
        if total == 0 {
            return None;
        }
        let pct = (self.cached as f64 * 100.0 / total as f64).round() as u64;
        Some(if pct >= 100 && self.generated > 0 {
            ">99%".to_string()
        } else {
            format!("{pct}%")
        })
    }

    /// Red once a wait outlasts the timeout, amber when p95 or max is past
    /// half of it, green otherwise. The summary includes failed and timed-out
    /// waits, so red is reachable.
    fn latency_color(&self, summary: &LatencySummary) -> Color {
        let timeout_us = self.generation_timeout.as_micros();
        if u128::from(summary.max_us) > timeout_us {
            Color::Red
        } else if u128::from(summary.p95_us) * 2 > timeout_us
            || u128::from(summary.max_us) * 2 > timeout_us
        {
            Color::Yellow
        } else {
            Color::Green
        }
    }

    fn line(&self) -> Line<'static> {
        // Same indent and label style as the TILE REQUESTS metrics above it.
        let label = Span::styled("  Latency: ", Style::default().fg(LABEL_COLOR));
        let neutral = Style::default().fg(LABEL_COLOR);
        let share = self.cached_share();
        let Some(summary) = self.latency else {
            let text = match &share {
                None => "no FUSE requests yet".to_string(),
                Some(share) => format!("no FUSE generations yet │ {share} cached"),
            };
            return Line::from(vec![label, Span::styled(text, neutral)]);
        };
        let figures = format!(
            "p50 {} │ p95 {} │ max {}",
            format_latency(summary.median_us),
            format_latency(summary.p95_us),
            format_latency(summary.max_us)
        );
        let mut spans = vec![
            label,
            Span::styled(
                figures,
                Style::default().fg(self.latency_color(self.recent.unwrap_or(summary))),
            ),
        ];
        // The share keeps the neutral style so it never competes with the
        // amber or red of the latency figures.
        if let Some(share) = share {
            spans.push(Span::styled(format!(" │ {share} cached"), neutral));
        }
        // Timeouts stay amber whatever the latency colour: each one handed
        // X-Plane a placeholder, so the count must not read as routine.
        if self.timeouts > 0 {
            let noun = if self.timeouts == 1 {
                "timeout"
            } else {
                "timeouts"
            };
            spans.push(Span::styled(
                format!(" │ {} {noun}", self.timeouts),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }
}

impl Widget for FuseLatencyWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.line()).render(area, buf);
    }
}

/// Microseconds as `180ms` or `1.4s`; sub-millisecond waits as `340us`.
fn format_latency(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{}ms", us / 1_000)
    } else {
        format!("{us}us")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(widget: FuseLatencyWidget<'_>) -> (Buffer, String) {
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let mut out = String::new();
        for x in 0..area.width {
            out.push_str(buf[(x, 0)].symbol());
        }
        (buf, out)
    }

    fn summary(median_ms: u64, p95_ms: u64, max_ms: u64) -> LatencySummary {
        LatencySummary {
            median_us: median_ms * 1_000,
            p95_us: p95_ms * 1_000,
            max_us: max_ms * 1_000,
            count: 40,
        }
    }

    fn widget(latency: Option<&LatencySummary>) -> FuseLatencyWidget<'_> {
        FuseLatencyWidget::new(latency, Duration::from_secs(10))
    }

    /// Foreground colour of the first cell of `needle`.
    fn fg(buf: &Buffer, out: &str, needle: &str) -> Color {
        let col = out.find(needle).expect("needle");
        let col = out[..col].chars().count();
        buf[(col as u16, 0)].fg
    }

    #[test]
    fn renders_the_latency_summary() {
        let s = summary(180, 1_400, 3_900);
        let (_, out) = render(widget(Some(&s)));
        assert!(
            out.contains("p50 180ms │ p95 1.4s │ max 3.9s"),
            "got:\n{out}"
        );
        assert!(!out.contains("timeout"), "absent when zero, got:\n{out}");
    }

    #[test]
    fn renders_timeouts_when_nonzero() {
        let s = summary(180, 1_400, 3_900);
        let (_, out) = render(widget(Some(&s)).with_timeouts(2));
        assert!(out.contains("max 3.9s │ 2 timeouts"), "got:\n{out}");
    }

    #[test]
    fn latency_is_green_amber_or_red_against_the_timeout() {
        let calm = summary(100, 800, 2_000);
        let (buf, out) = render(widget(Some(&calm)));
        assert_eq!(fg(&buf, &out, "p50"), Color::Green);

        let slow = summary(100, 6_000, 9_000);
        let (buf, out) = render(widget(Some(&slow)));
        assert_eq!(fg(&buf, &out, "p50"), Color::Yellow);

        let long_tail = summary(100, 800, 6_000);
        let (buf, out) = render(widget(Some(&long_tail)));
        assert_eq!(fg(&buf, &out, "p50"), Color::Yellow);

        let timed_out = summary(100, 6_000, 10_500);
        let (buf, out) = render(widget(Some(&timed_out)));
        assert_eq!(fg(&buf, &out, "p50"), Color::Red);
    }

    #[test]
    fn numbers_come_from_session_and_colour_from_recent() {
        let session = summary(100, 6_000, 12_000);
        let recent = summary(90, 400, 900);
        let (buf, out) = render(widget(Some(&session)).with_recent(Some(&recent)));
        assert!(out.contains("max 12.0s"), "got:\n{out}");
        assert_eq!(fg(&buf, &out, "p50"), Color::Green);

        let bad_recent = summary(100, 6_000, 11_000);
        let calm_session = summary(100, 800, 2_000);
        let (buf, out) = render(widget(Some(&calm_session)).with_recent(Some(&bad_recent)));
        assert_eq!(fg(&buf, &out, "p50"), Color::Red);
    }

    #[test]
    fn renders_a_placeholder_without_a_sample() {
        let (_, out) = render(widget(None).with_cache_served(5, 0));
        assert!(
            out.contains("Latency: no FUSE generations yet"),
            "got:\n{out}"
        );
    }

    #[test]
    fn renders_no_requests_before_any_fuse_request() {
        let (_, out) = render(widget(None));
        assert!(out.contains("Latency: no FUSE requests yet"), "got:\n{out}");
        assert!(!out.contains("cached"), "got:\n{out}");
    }

    #[test]
    fn renders_fully_cached_when_requests_need_no_generation() {
        let (_, out) = render(widget(None).with_cache_served(250, 0));
        assert!(
            out.contains("Latency: no FUSE generations yet │ 100% cached"),
            "got:\n{out}"
        );
    }

    #[test]
    fn renders_the_cached_share_after_the_latency_figures() {
        let s = summary(180, 1_400, 3_900);
        let (_, out) = render(widget(Some(&s)).with_cache_served(97, 3));
        assert!(
            out.contains("p50 180ms │ p95 1.4s │ max 3.9s │ 97% cached"),
            "got:\n{out}"
        );

        let (_, out) = render(widget(Some(&s)).with_cache_served(97, 3).with_timeouts(2));
        assert!(
            out.contains("97% cached │ 2 timeouts"),
            "timeouts stay last, got:\n{out}"
        );
    }

    #[test]
    fn timeouts_are_amber_while_the_share_stays_neutral() {
        let s = summary(180, 1_400, 3_900);
        let (buf, out) = render(widget(Some(&s)).with_cache_served(97, 3).with_timeouts(2));
        assert_eq!(fg(&buf, &out, "2 timeouts"), Color::Yellow);
        assert_ne!(fg(&buf, &out, "97% cached"), Color::Yellow);
    }

    #[test]
    fn never_rounds_a_partial_share_up_to_100() {
        let s = summary(100, 200, 300);
        let (_, out) = render(widget(Some(&s)).with_cache_served(999, 1));
        assert!(out.contains(">99% cached"), "got:\n{out}");
        assert!(!out.contains("100% cached"), "got:\n{out}");
    }

    #[test]
    fn cached_segment_stays_neutral_when_latency_is_amber_or_red() {
        for (p95, max, expected) in [(800, 6_000, Color::Yellow), (800, 11_000, Color::Red)] {
            let s = summary(100, p95, max);
            let (buf, out) = render(widget(Some(&s)).with_cache_served(90, 10));
            assert_eq!(fg(&buf, &out, "p50"), expected);
            assert_eq!(fg(&buf, &out, "90% cached"), LABEL_COLOR);
        }
    }
}
