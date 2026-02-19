//! Aircraft Position widget.
//!
//! Displays aircraft position from the APT (Aircraft Position & Telemetry) module.
//!
//! Layout:
//! ```text
//! ┌─ Aircraft Position ──────────────────────────────────────────────────────────────┐
//! │ Position : 9.99°E, 53.63°N | Trk: 088° | Hdg: 090° | GS: 489kt | Alt: 33532ft    │
//! │ Accuracy : 10m (GPS) | Track: Telemetry                                          │
//! │ GPS: * Connected | VATSIM: x Disconnected | PILOTEDGE: * Connected               │
//! └──────────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The provider status line is extensible for future VATSIM/PilotEdge integration.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::aircraft_position::{
    AircraftPositionStatus, PositionAccuracy, PositionSource, TelemetryStatus, TrackSource,
};

/// Widget displaying aircraft position from APT module.
pub struct AircraftPositionWidget<'a> {
    status: &'a AircraftPositionStatus,
}

impl<'a> AircraftPositionWidget<'a> {
    /// Create a new aircraft position widget.
    pub fn new(status: &'a AircraftPositionStatus) -> Self {
        Self { status }
    }

    /// Format longitude with direction suffix.
    fn format_lon(lon: f64) -> String {
        let dir = if lon >= 0.0 { "E" } else { "W" };
        format!("{:.2}°{}", lon.abs(), dir)
    }

    /// Format latitude with direction suffix.
    fn format_lat(lat: f64) -> String {
        let dir = if lat >= 0.0 { "N" } else { "S" };
        format!("{:.2}°{}", lat.abs(), dir)
    }

    /// Get color based on accuracy confidence.
    ///
    /// - Green: High confidence (telemetry, <100m)
    /// - Yellow: Medium confidence (manual reference, 100m-10km)
    /// - Red: Low confidence (inference, >10km)
    fn accuracy_color(accuracy: &PositionAccuracy) -> Color {
        let meters = accuracy.meters();
        if meters < 100.0 {
            Color::Green
        } else if meters <= 10_000.0 {
            Color::Yellow
        } else {
            Color::Red
        }
    }

    /// Format accuracy value with appropriate unit.
    fn format_accuracy(accuracy: &PositionAccuracy) -> String {
        let meters = accuracy.meters();
        if meters >= 1000.0 {
            format!("{:.0}km", meters / 1000.0)
        } else {
            format!("{:.0}m", meters)
        }
    }

    /// Get source label for display.
    fn source_label(source: &PositionSource) -> &'static str {
        match source {
            PositionSource::Telemetry => "GPS",
            PositionSource::OnlineNetwork => "Network",
            PositionSource::ManualReference => "Airport",
            PositionSource::SceneInference => "Inferred",
        }
    }

    /// Build the position line.
    fn build_position_line(&self) -> Line<'static> {
        match &self.status.state {
            Some(state) => {
                let mut spans = vec![
                    Span::styled("   Position : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!(
                            "{}, {}",
                            Self::format_lon(state.longitude),
                            Self::format_lat(state.latitude)
                        ),
                        Style::default().fg(Color::White),
                    ),
                ];

                // Add vectors if available (only from telemetry)
                if self.status.vectors_available {
                    // Track (ground path direction)
                    spans.extend([
                        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                        Span::styled("Trk: ", Style::default().fg(Color::DarkGray)),
                    ]);
                    if let Some(track) = state.track {
                        // Color code by track source
                        let track_color = match state.track_source {
                            TrackSource::Telemetry => Color::Green, // Authoritative
                            TrackSource::Derived => Color::Yellow,  // Calculated
                            TrackSource::Unavailable => Color::DarkGray,
                        };
                        spans.push(Span::styled(
                            format!("{:03.0}°", track),
                            Style::default().fg(track_color),
                        ));
                    } else {
                        spans.push(Span::styled("---", Style::default().fg(Color::DarkGray)));
                    }

                    // Heading (nose direction)
                    spans.extend([
                        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                        Span::styled("Hdg: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{:03.0}°", state.heading),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                        Span::styled("GS: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{:.0}kt", state.ground_speed),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                        Span::styled("Alt: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{:.0}ft", state.altitude),
                            Style::default().fg(Color::White),
                        ),
                    ]);
                }

                Line::from(spans)
            }
            None => {
                // No data available
                Line::from(vec![
                    Span::styled("   Position : ", Style::default().fg(Color::DarkGray)),
                    Span::styled("NO DATA AVAILABLE", Style::default().fg(Color::DarkGray)),
                ])
            }
        }
    }

    /// Build the accuracy line.
    fn build_accuracy_line(&self) -> Line<'static> {
        match &self.status.state {
            Some(state) => {
                let accuracy_str = Self::format_accuracy(&state.accuracy);
                let source_str = Self::source_label(&state.source);
                let color = Self::accuracy_color(&state.accuracy);

                Line::from(vec![
                    Span::styled("   Accuracy : ", Style::default().fg(Color::DarkGray)),
                    Span::styled(accuracy_str, Style::default().fg(color)),
                    Span::styled(
                        format!(" ({})", source_str),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            }
            None => {
                // No accuracy when no data
                Line::from(vec![
                    Span::styled("   Accuracy : ", Style::default().fg(Color::DarkGray)),
                    Span::styled("-", Style::default().fg(Color::DarkGray)),
                ])
            }
        }
    }

    /// Build the provider status line.
    ///
    /// Format: `GPS: * Connected | VATSIM: x Disconnected | PILOTEDGE: * Connected`
    fn build_provider_line(&self) -> Line<'static> {
        let (gps_indicator, gps_text, gps_color) = match self.status.telemetry_status {
            TelemetryStatus::Connected => ("*", "Connected", Color::Green),
            TelemetryStatus::Disconnected => ("x", "Disconnected", Color::DarkGray),
        };

        // Currently only GPS is implemented
        // Future: Add VATSIM, PILOTEDGE status here
        Line::from(vec![
            Span::styled("   GPS: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ", gps_indicator),
                Style::default().fg(gps_color),
            ),
            Span::styled(gps_text, Style::default().fg(gps_color)),
            // Placeholder for future providers (commented out until implemented)
            // Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            // Span::styled("VATSIM: ", Style::default().fg(Color::DarkGray)),
            // Span::styled("x ", Style::default().fg(Color::DarkGray)),
            // Span::styled("Disconnected", Style::default().fg(Color::DarkGray)),
        ])
    }
}

impl Widget for AircraftPositionWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let position_line = self.build_position_line();
        let accuracy_line = self.build_accuracy_line();
        let provider_line = self.build_provider_line();

        let text = vec![position_line, accuracy_line, provider_line];
        let paragraph = Paragraph::new(text);
        paragraph.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::aircraft_position::AircraftState;

    #[test]
    fn test_format_lon() {
        assert_eq!(AircraftPositionWidget::format_lon(9.99), "9.99°E");
        assert_eq!(AircraftPositionWidget::format_lon(-122.5), "122.50°W");
        assert_eq!(AircraftPositionWidget::format_lon(0.0), "0.00°E");
    }

    #[test]
    fn test_format_lat() {
        assert_eq!(AircraftPositionWidget::format_lat(53.63), "53.63°N");
        assert_eq!(AircraftPositionWidget::format_lat(-33.86), "33.86°S");
        assert_eq!(AircraftPositionWidget::format_lat(0.0), "0.00°N");
    }

    #[test]
    fn test_format_accuracy() {
        assert_eq!(
            AircraftPositionWidget::format_accuracy(&PositionAccuracy::TELEMETRY),
            "10m"
        );
        assert_eq!(
            AircraftPositionWidget::format_accuracy(&PositionAccuracy::MANUAL_REFERENCE),
            "100m"
        );
        assert_eq!(
            AircraftPositionWidget::format_accuracy(&PositionAccuracy::SCENE_INFERENCE),
            "100km"
        );
    }

    #[test]
    fn test_accuracy_color() {
        assert_eq!(
            AircraftPositionWidget::accuracy_color(&PositionAccuracy::TELEMETRY),
            Color::Green
        );
        assert_eq!(
            AircraftPositionWidget::accuracy_color(&PositionAccuracy::MANUAL_REFERENCE),
            Color::Yellow
        );
        assert_eq!(
            AircraftPositionWidget::accuracy_color(&PositionAccuracy::SCENE_INFERENCE),
            Color::Red
        );
    }

    #[test]
    fn test_source_label() {
        assert_eq!(
            AircraftPositionWidget::source_label(&PositionSource::Telemetry),
            "GPS"
        );
        assert_eq!(
            AircraftPositionWidget::source_label(&PositionSource::ManualReference),
            "Airport"
        );
        assert_eq!(
            AircraftPositionWidget::source_label(&PositionSource::SceneInference),
            "Inferred"
        );
    }

    #[test]
    fn test_widget_no_data() {
        let status = AircraftPositionStatus {
            state: None,
            telemetry_status: TelemetryStatus::Disconnected,
            vectors_available: false,
        };
        let widget = AircraftPositionWidget::new(&status);

        // Should not panic
        let position_line = widget.build_position_line();
        assert!(position_line.spans.len() >= 2);
    }

    #[test]
    fn test_widget_with_telemetry() {
        let state = AircraftState::from_telemetry(
            53.63,
            9.99,
            Some(88.0), // track
            TrackSource::Telemetry,
            90.0, // heading
            489.0,
            33532.0,
        );
        let status = AircraftPositionStatus::from_state(state, TelemetryStatus::Connected);
        let widget = AircraftPositionWidget::new(&status);

        // Should have vectors including track
        let position_line = widget.build_position_line();
        // Position + separators + Trk + Hdg + GS + Alt
        assert!(position_line.spans.len() > 4);

        let accuracy_line = widget.build_accuracy_line();
        assert!(accuracy_line.spans.len() >= 2);

        let provider_line = widget.build_provider_line();
        assert!(provider_line.spans.len() >= 2);
    }

    #[test]
    fn test_widget_with_manual_reference() {
        let state = AircraftState::from_manual_reference(53.63, 9.99);
        let status = AircraftPositionStatus::from_state(state, TelemetryStatus::Disconnected);
        let _widget = AircraftPositionWidget::new(&status);

        // Should NOT have vectors (manual reference has no heading/speed/altitude)
        assert!(!status.vectors_available);
    }
}
