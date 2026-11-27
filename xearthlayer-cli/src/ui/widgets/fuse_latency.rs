//! FUSE generation latency line.
//!
//! One line describing the recent past (#481): the p50, p95 and max of the
//! FUSE waits that needed a tile generation in the recent window, coloured
//! against the generation timeout, the window's share of FUSE requests served
//! from cache, and its timeouts beside the session's. When nothing waited in
//! the window the line says so, with the time since the last wait. Rendered in
//! the Scenery System pane beside the tile request metrics it describes.
//!
//! The line was once the session's, so a cold start's waits kept it red for
//! the rest of a flight that went well. The session figures are in the log's
//! `Prefetch sample` lines, one per sample.

use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::{LatencySummary, RecentFuseDemand, RECENT_WINDOW};

const LABEL_COLOR: Color = Color::DarkGray;

/// Widget displaying the FUSE generation latency summary on one line.
pub struct FuseLatencyWidget {
    recent: RecentFuseDemand,
    generation_timeout: Duration,
    session_timeouts: usize,
    session_cached: u64,
    session_generated: u64,
}

impl FuseLatencyWidget {
    /// The line for FUSE demand over the recent window, coloured against the
    /// FUSE generation timeout.
    pub fn new(recent: RecentFuseDemand, generation_timeout: Duration) -> Self {
        Self {
            recent,
            generation_timeout,
            session_timeouts: 0,
            session_cached: 0,
            session_generated: 0,
        }
    }

    /// FUSE waits that hit the generation timeout this session. Shown beside
    /// the window's count, so a timeout that has left the window still reads
    /// as having happened, but not as a current problem.
    pub fn with_session_timeouts(mut self, timeouts: usize) -> Self {
        self.session_timeouts = timeouts;
        self
    }

    /// Session FUSE requests served from cache against those that needed a
    /// generation, for the line before the first wait. Latency covers
    /// generations only, so without this the line is empty on a
    /// well-prefetched start and reads as a broken component.
    pub fn with_session_cache_served(mut self, cached: u64, generated: u64) -> Self {
        self.session_cached = cached;
        self.session_generated = generated;
        self
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
        let Some(since_last_wait) = self.recent.since_last_wait else {
            // Before the first wait the line reads as it always has, from the
            // session's counts.
            let text = match cached_share(self.session_cached, self.session_generated) {
                None => "no FUSE requests yet".to_string(),
                Some(share) => format!("no FUSE generations yet │ {share} cached"),
            };
            return Line::from(vec![label, Span::styled(text, neutral)]);
        };
        let mut spans = vec![label];
        match &self.recent.latency {
            Some(summary) => spans.push(Span::styled(
                format!(
                    "p50 {} │ p95 {} │ max {}",
                    format_latency(summary.median_us),
                    format_latency(summary.p95_us),
                    format_latency(summary.max_us)
                ),
                Style::default().fg(self.latency_color(summary)),
            )),
            None => spans.push(Span::styled(
                format!(
                    "none waited in {} (last {} ago)",
                    format_age(RECENT_WINDOW),
                    format_age(since_last_wait)
                ),
                Style::default().fg(Color::Green),
            )),
        }
        // The share keeps the neutral style so it never competes with the
        // amber or red of the latency figures.
        let window = self.recent.counts;
        if let Some(share) = cached_share(window.cached, window.generated) {
            spans.push(Span::styled(format!(" │ {share} cached"), neutral));
        }
        spans.extend(timeout_spans(window.timeouts, self.session_timeouts));
        Line::from(spans)
    }
}

impl Widget for FuseLatencyWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.line()).render(area, buf);
    }
}

/// Whole percent of requests served from cache, but never 100 unless every
/// request was; `None` without requests.
fn cached_share(cached: u64, generated: u64) -> Option<String> {
    let total = cached + generated;
    if total == 0 {
        return None;
    }
    let pct = (cached as f64 * 100.0 / total as f64).round() as u64;
    Some(if pct >= 100 && generated > 0 {
        ">99%".to_string()
    } else {
        format!("{pct}%")
    })
}

/// The timeouts segment, last on the line. The window's timeouts are amber:
/// each one handed X-Plane a placeholder, so the count must not read as
/// routine. Once they leave the window the session's count stays, neutral.
fn timeout_spans(window: usize, session: usize) -> Vec<Span<'static>> {
    let neutral = Style::default().fg(LABEL_COLOR);
    let noun = |n: usize| if n == 1 { "timeout" } else { "timeouts" };
    if window > 0 {
        let mut spans = vec![
            Span::styled(" │ ", neutral),
            Span::styled(
                format!("{window} {}", noun(window)),
                Style::default().fg(Color::Yellow),
            ),
        ];
        if session > window {
            spans.push(Span::styled(format!(" ({session} session)"), neutral));
        }
        spans
    } else if session > 0 {
        vec![Span::styled(
            format!(" │ {session} {} (session)", noun(session)),
            neutral,
        )]
    } else {
        Vec::new()
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

/// A span of time in whole minutes, `40 min` or `1 h 20 min`; under a minute
/// in seconds.
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        return format!("{secs} s");
    }
    let (hours, minutes) = (secs / 3600, secs / 60 % 60);
    match (hours, minutes) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::metrics::DemandCounts;

    fn render(widget: FuseLatencyWidget) -> (Buffer, String) {
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

    const MINUTE: Duration = Duration::from_secs(60);

    /// A window with waits, the last one just now.
    fn waited(latency: LatencySummary) -> RecentFuseDemand {
        RecentFuseDemand {
            latency: Some(latency),
            counts: DemandCounts::default(),
            since_last_wait: Some(Duration::ZERO),
        }
    }

    /// A window nothing waited in, the last wait `ago`.
    fn quiet(ago: Duration) -> RecentFuseDemand {
        RecentFuseDemand {
            latency: None,
            counts: DemandCounts::default(),
            since_last_wait: Some(ago),
        }
    }

    fn with_counts(
        mut recent: RecentFuseDemand,
        cached: u64,
        generated: u64,
        timeouts: usize,
    ) -> RecentFuseDemand {
        recent.counts = DemandCounts {
            cached,
            generated,
            timeouts,
        };
        recent
    }

    fn widget(recent: RecentFuseDemand) -> FuseLatencyWidget {
        FuseLatencyWidget::new(recent, Duration::from_secs(10))
    }

    /// Foreground colour of the first cell of `needle`.
    fn fg(buf: &Buffer, out: &str, needle: &str) -> Color {
        let col = out.find(needle).expect("needle");
        let col = out[..col].chars().count();
        buf[(col as u16, 0)].fg
    }

    // --- Before the first wait: the line is unchanged -----------------------

    #[test]
    fn renders_no_requests_before_any_fuse_request() {
        let (_, out) = render(widget(RecentFuseDemand::default()));
        assert!(out.contains("Latency: no FUSE requests yet"), "got:\n{out}");
        assert!(!out.contains("cached"), "got:\n{out}");
    }

    #[test]
    fn renders_a_placeholder_without_a_wait() {
        let (_, out) = render(widget(RecentFuseDemand::default()).with_session_cache_served(5, 0));
        assert!(
            out.contains("Latency: no FUSE generations yet │ 100% cached"),
            "got:\n{out}"
        );
    }

    #[test]
    fn before_the_first_wait_the_share_is_the_sessions() {
        // The window may have no requests, after a pause at the start, but
        // the session has had them; the line must not read as no requests.
        let recent = with_counts(RecentFuseDemand::default(), 0, 0, 0);
        let (_, out) = render(widget(recent).with_session_cache_served(250, 0));
        assert!(
            out.contains("Latency: no FUSE generations yet │ 100% cached"),
            "got:\n{out}"
        );
    }

    // --- A recent wait: the window's figures, coloured as before -------------

    #[test]
    fn renders_the_windows_latency_figures() {
        let (_, out) = render(widget(waited(summary(180, 1_400, 3_900))));
        assert!(
            out.contains("Latency: p50 180ms │ p95 1.4s │ max 3.9s"),
            "got:\n{out}"
        );
        assert!(!out.contains("timeout"), "absent when zero, got:\n{out}");
        assert!(!out.contains("none waited"), "got:\n{out}");
    }

    #[test]
    fn latency_is_green_amber_or_red_against_the_timeout() {
        for (p95, max, expected) in [
            (800, 2_000, Color::Green),
            (6_000, 9_000, Color::Yellow),
            (800, 6_000, Color::Yellow),
            (6_000, 10_500, Color::Red),
        ] {
            let (buf, out) = render(widget(waited(summary(100, p95, max))));
            assert_eq!(fg(&buf, &out, "p50"), expected, "p95 {p95} max {max}");
        }
    }

    #[test]
    fn the_cached_share_is_the_windows_not_the_sessions() {
        let recent = with_counts(waited(summary(180, 1_400, 3_900)), 97, 3, 0);
        let (_, out) = render(widget(recent).with_session_cache_served(5_300, 191));
        assert!(
            out.contains("p50 180ms │ p95 1.4s │ max 3.9s │ 97% cached"),
            "got:\n{out}"
        );
    }

    #[test]
    fn never_rounds_a_partial_share_up_to_100() {
        let recent = with_counts(waited(summary(100, 200, 300)), 999, 1, 0);
        let (_, out) = render(widget(recent));
        assert!(out.contains(">99% cached"), "got:\n{out}");
        assert!(!out.contains("100% cached"), "got:\n{out}");
    }

    #[test]
    fn cached_segment_stays_neutral_when_latency_is_amber_or_red() {
        for (p95, max, expected) in [(800, 6_000, Color::Yellow), (800, 11_000, Color::Red)] {
            let recent = with_counts(waited(summary(100, p95, max)), 90, 10, 0);
            let (buf, out) = render(widget(recent));
            assert_eq!(fg(&buf, &out, "p50"), expected);
            assert_eq!(fg(&buf, &out, "90% cached"), LABEL_COLOR);
        }
    }

    // --- Waits only in an old window: none waited, in green ------------------

    #[test]
    fn waits_only_outside_the_window_read_none_waited_in_green() {
        let recent = with_counts(quiet(40 * MINUTE), 97, 3, 0);
        let (buf, out) = render(widget(recent).with_session_cache_served(5_300, 191));
        assert!(
            out.contains("Latency: none waited in 5 min (last 40 min ago) │ 97% cached"),
            "got:\n{out}"
        );
        assert_eq!(fg(&buf, &out, "none waited"), Color::Green);
        assert_eq!(fg(&buf, &out, "97% cached"), LABEL_COLOR);
        assert!(
            !out.contains("p50"),
            "no figures without waits, got:\n{out}"
        );
    }

    #[test]
    fn none_waited_without_requests_in_the_window_shows_no_share() {
        let (_, out) = render(widget(quiet(6 * MINUTE)).with_session_cache_served(5_300, 191));
        assert!(
            out.contains("Latency: none waited in 5 min (last 6 min ago)"),
            "got:\n{out}"
        );
        assert!(!out.contains("cached"), "got:\n{out}");
    }

    #[test]
    fn the_time_since_the_last_wait_reads_in_hours_past_an_hour() {
        let (_, out) = render(widget(quiet(80 * MINUTE)));
        assert!(out.contains("(last 1 h 20 min ago)"), "got:\n{out}");
        let (_, out) = render(widget(quiet(120 * MINUTE)));
        assert!(out.contains("(last 2 h ago)"), "got:\n{out}");
    }

    // --- Timeouts: amber in the window, a neutral session count after --------

    #[test]
    fn a_timeout_outside_the_window_is_a_neutral_session_count() {
        let recent = with_counts(quiet(40 * MINUTE), 97, 3, 0);
        let (buf, out) = render(widget(recent).with_session_timeouts(1));
        assert!(
            out.contains("97% cached │ 1 timeout (session)"),
            "got:\n{out}"
        );
        assert_eq!(fg(&buf, &out, "1 timeout"), LABEL_COLOR);

        // Also beside figures, when the window's waits all came in time.
        let recent = with_counts(waited(summary(180, 1_400, 3_900)), 97, 3, 0);
        let (buf, out) = render(widget(recent).with_session_timeouts(2));
        assert!(
            out.contains("97% cached │ 2 timeouts (session)"),
            "got:\n{out}"
        );
        assert_eq!(fg(&buf, &out, "2 timeouts"), LABEL_COLOR);
    }

    #[test]
    fn a_timeout_inside_the_window_is_amber() {
        let recent = with_counts(waited(summary(180, 1_400, 10_500)), 97, 3, 1);
        let (buf, out) = render(widget(recent).with_session_timeouts(1));
        assert!(out.contains("97% cached │ 1 timeout"), "got:\n{out}");
        assert!(
            !out.contains("session"),
            "the window holds the session's only timeout, got:\n{out}"
        );
        assert_eq!(fg(&buf, &out, "1 timeout"), Color::Yellow);
        assert_eq!(fg(&buf, &out, "97% cached"), LABEL_COLOR);
    }

    #[test]
    fn window_timeouts_show_the_session_count_beside_them_when_it_is_larger() {
        let recent = with_counts(waited(summary(180, 1_400, 10_500)), 97, 3, 2);
        let (buf, out) = render(widget(recent).with_session_timeouts(5));
        assert!(
            out.contains("97% cached │ 2 timeouts (5 session)"),
            "got:\n{out}"
        );
        assert_eq!(fg(&buf, &out, "2 timeouts"), Color::Yellow);
        assert_eq!(fg(&buf, &out, "(5 session)"), LABEL_COLOR);
    }

    #[test]
    fn format_age_reads_seconds_minutes_and_hours() {
        assert_eq!(format_age(Duration::from_secs(42)), "42 s");
        assert_eq!(format_age(Duration::from_secs(5 * 60 + 59)), "5 min");
        assert_eq!(format_age(Duration::from_secs(3600)), "1 h");
        assert_eq!(format_age(Duration::from_secs(3600 + 60)), "1 h 1 min");
    }
}
