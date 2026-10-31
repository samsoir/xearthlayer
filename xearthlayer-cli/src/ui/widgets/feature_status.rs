//! Feature status widget.
//!
//! One line per key feature, each saying what the feature is actually doing
//! this session rather than what the configuration asked for: the tile
//! processing engine the encoder selected, whether night lighting found the
//! X-Plane installation, and which X-World Pro continents XEarthLayer is
//! yielding overlays to. Feature lines lead with an emoji, or with a plain
//! mark where the terminal cannot draw one (see [`crate::ui::symbols`]).

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::night::NightLighting;
use xearthlayer::system::{GpuAdapter, GpuKind};
use xearthlayer::xworld_pro::Detection;

use crate::ui::symbols::{self, SymbolSet};

/// Texture encoding: the GPU that was selected, or the CPU encoder in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderStatus {
    Gpu(GpuAdapter),
    /// `texture.compressor = gpu` honoured on a host whose only GPU is the
    /// one X-Plane renders with, so encoding competes with the sim (#203).
    GpuSharedWithSim(GpuAdapter),
    /// The ISPC SIMD encoder on the CPU.
    Ispc,
    /// The pure-Rust encoder on the CPU.
    Software,
}

/// X-World Pro support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XworldProStatus {
    /// `packages.xworld_pro` is off, so nothing was looked for.
    SupportOff,
    /// Support is on but `scenery_packs.ini` names no X-World Pro entry.
    NotDetected,
    /// Installed, but no layer is enabled with its folder present.
    NoActiveContinents,
    /// The continents with an active layer, in X-World Pro's own names.
    Active(Vec<&'static str>),
}

impl XworldProStatus {
    /// From `packages.xworld_pro` and the detection preflight contributed,
    /// which it does only while that setting is on.
    pub fn from_detection(enabled: bool, detection: Option<&Detection>) -> Self {
        let Some(detection) = detection.filter(|_| enabled) else {
            return if enabled {
                Self::NotDetected
            } else {
                Self::SupportOff
            };
        };
        if !detection.is_present() {
            return Self::NotDetected;
        }
        let continents: Vec<&'static str> = detection
            .active_continents()
            .into_iter()
            .map(|c| c.label())
            .collect();
        if continents.is_empty() {
            Self::NoActiveContinents
        } else {
            Self::Active(continents)
        }
    }
}

/// Everything the status pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureStatus {
    pub encoder: EncoderStatus,
    pub night: NightLighting,
    pub xworld_pro: XworldProStatus,
    /// How many warnings the startup checks printed before the dashboard
    /// replaced the screen; they are in the log (#326).
    pub startup_warnings: usize,
}

impl FeatureStatus {
    /// Lines the pane draws: one more while there are startup warnings.
    pub fn line_count(&self) -> u16 {
        4 + u16::from(self.startup_warnings > 0)
    }
}

/// Text of a line whose feature is working.
const TEXT: Color = Color::Gray;
/// Text and mark of a line whose feature is off or absent.
const OFF: Color = Color::DarkGray;
/// Asked for but not working as intended, or working at a cost.
const WARN: Color = Color::Yellow;

/// Widget displaying the feature status lines.
pub struct FeatureStatusWidget<'a> {
    status: &'a FeatureStatus,
    symbols: SymbolSet,
    frame_rate: Option<u16>,
}

impl<'a> FeatureStatusWidget<'a> {
    pub fn new(status: &'a FeatureStatus) -> Self {
        Self {
            status,
            symbols: SymbolSet::default(),
            frame_rate: None,
        }
    }

    /// Emoji or plain marks on the feature lines.
    pub fn with_symbols(mut self, symbols: SymbolSet) -> Self {
        self.symbols = symbols;
        self
    }

    /// X-Plane's current frame rate, or `None` while X-Plane is not
    /// connected or has not reported one. Live, unlike the feature states.
    pub fn with_frame_rate(mut self, frame_rate: Option<u16>) -> Self {
        self.frame_rate = frame_rate;
        self
    }

    /// The feature's emoji, or `plain` where emoji are not drawn.
    fn feature_mark(&self, emoji: &'static str, plain: Mark) -> Mark {
        match self.symbols {
            SymbolSet::Emoji => Mark::emoji(emoji),
            SymbolSet::Plain => plain,
        }
    }

    fn frame_rate_line(&self) -> Line<'static> {
        let Some(fps) = self.frame_rate else {
            return line(
                Mark::CROSS,
                vec![styled("X-Plane frame rate: not connected", OFF)],
            );
        };
        let (mark, colour) = match fps {
            31.. => (Mark::CHECK, Color::Green),
            20..=30 => (Mark::WARNING, WARN),
            _ => (Mark::POOR, Color::Red),
        };
        line(
            mark,
            vec![
                styled("X-Plane frame rate: ", TEXT),
                styled(format!("{fps} fps"), colour),
            ],
        )
    }

    fn encoder_line(&self) -> Line<'static> {
        let mut spans = vec![styled("Tile Processing Engine, ", TEXT)];
        let (mark, adapter) = match &self.status.encoder {
            EncoderStatus::Software => {
                spans.push(styled("Software", Color::Red));
                (self.feature_mark(symbols::ENGINE, Mark::CHECK), None)
            }
            EncoderStatus::Ispc => {
                spans.push(styled("ISPC", Color::LightBlue));
                (self.feature_mark(symbols::ENGINE, Mark::CHECK), None)
            }
            EncoderStatus::Gpu(adapter) => (
                self.feature_mark(symbols::ENGINE, Mark::CHECK),
                Some(adapter),
            ),
            // The warning outranks the emoji: it is the one thing to see.
            EncoderStatus::GpuSharedWithSim(adapter) => (Mark::WARNING, Some(adapter)),
        };
        if let Some(adapter) = adapter {
            spans.push(styled(
                format!("GPU {}", adapter.kind.display().to_lowercase()),
                gpu_colour(adapter.kind),
            ));
            spans.push(styled(format!(" ({})", adapter.short_name()), TEXT));
        }
        if matches!(self.status.encoder, EncoderStatus::GpuSharedWithSim(_)) {
            spans.push(styled(", ", TEXT));
            spans.push(styled("shared with X-Plane", WARN));
        }
        line(mark, spans)
    }

    fn night_line(&self) -> Line<'static> {
        let (mark, text, colour) = match self.status.night {
            NightLighting::Lit => (
                self.feature_mark(symbols::NIGHT_LIT, Mark::CHECK),
                "Night lighting enabled",
                TEXT,
            ),
            NightLighting::Dark => (
                self.feature_mark(symbols::NIGHT_DARK, Mark::WARNING),
                "Night lighting enabled, but X-Plane was not found: lit textures stay dark",
                WARN,
            ),
            NightLighting::Off => (
                self.feature_mark(symbols::NIGHT_DARK, Mark::CROSS),
                "Night lighting disabled",
                OFF,
            ),
        };
        line(mark, vec![styled(text, colour)])
    }

    fn xworld_pro_line(&self) -> Line<'static> {
        let (text, working) = match &self.status.xworld_pro {
            XworldProStatus::Active(continents) => (
                format!("X-World Pro detected: {}", continents.join(", ")),
                true,
            ),
            XworldProStatus::NoActiveContinents => (
                "X-World Pro installed, no layers enabled".to_string(),
                false,
            ),
            XworldProStatus::NotDetected => ("X-World Pro not detected".to_string(), false),
            XworldProStatus::SupportOff => ("X-World Pro support off".to_string(), false),
        };
        let (plain, colour) = if working {
            (Mark::CHECK, TEXT)
        } else {
            (Mark::CROSS, OFF)
        };
        line(
            self.feature_mark(symbols::XWORLD_PRO, plain),
            vec![styled(text, colour)],
        )
    }
}

/// Integrated and other real GPUs green, discrete yellow, and a software
/// rasteriser posing as a GPU red, like the software encoder.
fn gpu_colour(kind: GpuKind) -> Color {
    match kind {
        GpuKind::Discrete => Color::Yellow,
        GpuKind::Cpu => Color::Red,
        GpuKind::Integrated | GpuKind::Virtual | GpuKind::Other => Color::Green,
    }
}

/// The marker in front of each line. An emoji carries its own colour; a
/// plain mark is coloured by state.
#[derive(Clone, Copy)]
struct Mark {
    glyph: &'static str,
    colour: Option<Color>,
}

impl Mark {
    const CHECK: Self = Self::plain("✓", Color::Green);
    const CROSS: Self = Self::plain("✗", OFF);
    const WARNING: Self = Self::plain("!", WARN);
    /// A frame rate too low to fly smoothly.
    const POOR: Self = Self::plain("●", Color::Red);

    const fn plain(glyph: &'static str, colour: Color) -> Self {
        Self {
            glyph,
            colour: Some(colour),
        }
    }

    fn emoji(glyph: &'static str) -> Self {
        Self {
            glyph,
            colour: None,
        }
    }
}

/// Columns every marker occupies, so text lines up whether it follows an
/// emoji (two columns) or a plain mark (one, padded).
const MARK_COLUMNS: usize = 2;

fn styled(text: impl Into<String>, colour: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(colour))
}

fn line(mark: Mark, text: Vec<Span<'static>>) -> Line<'static> {
    let glyph = match mark.colour {
        Some(colour) => styled(mark.glyph, colour),
        None => Span::raw(mark.glyph),
    };
    let padding = MARK_COLUMNS.saturating_sub(glyph.width());
    let mut spans = vec![Span::raw("  "), glyph, Span::raw(" ".repeat(padding + 1))];
    spans.extend(text);
    Line::from(spans)
}

impl FeatureStatusWidget<'_> {
    /// Shown only when there were any: the screen they were printed on is gone.
    fn startup_warnings_line(&self) -> Option<Line<'static>> {
        let n = self.status.startup_warnings;
        (n > 0).then(|| {
            let noun = if n == 1 { "warning" } else { "warnings" };
            line(
                Mark::WARNING,
                vec![styled(
                    format!("{n} startup {noun}: see xearthlayer.log"),
                    WARN,
                )],
            )
        })
    }
}

impl Widget for FeatureStatusWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut text = vec![
            self.frame_rate_line(),
            self.encoder_line(),
            self.night_line(),
            self.xworld_pro_line(),
        ];
        text.extend(self.startup_warnings_line());
        Paragraph::new(text).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use xearthlayer::system::GpuKind;
    use xearthlayer::xworld_pro::{Continent, Detection, PackEntry, XwpLayer};

    fn adapter(name: &str, kind: GpuKind) -> GpuAdapter {
        GpuAdapter {
            name: name.to_string(),
            kind,
            backend: "vulkan".to_string(),
        }
    }

    fn radeon() -> GpuAdapter {
        adapter("AMD Radeon 780M", GpuKind::Integrated)
    }

    fn all_on() -> FeatureStatus {
        FeatureStatus {
            encoder: EncoderStatus::Gpu(radeon()),
            night: NightLighting::Lit,
            xworld_pro: XworldProStatus::Active(vec!["America", "Europe"]),
            startup_warnings: 0,
        }
    }

    #[test]
    fn startup_warnings_add_a_line_only_when_there_are_any() {
        let quiet = all_on();
        assert_eq!(quiet.line_count(), 4);
        let widget = FeatureStatusWidget::new(&quiet);
        assert!(widget.startup_warnings_line().is_none());

        let one = FeatureStatus {
            startup_warnings: 1,
            ..all_on()
        };
        assert_eq!(one.line_count(), 5);
        let text: String = FeatureStatusWidget::new(&one)
            .startup_warnings_line()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("1 startup warning: see xearthlayer.log"),
            "{text}"
        );

        let two = FeatureStatus {
            startup_warnings: 2,
            ..all_on()
        };
        let text: String = FeatureStatusWidget::new(&two)
            .startup_warnings_line()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2 startup warnings"), "{text}");
    }

    /// Each row as text, skipping the cell a wide symbol covers so an emoji
    /// reads as written.
    fn render_with(widget: FeatureStatusWidget<'_>) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, 100, 4);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let rows = (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < area.width {
                    let symbol = buf[(x, y)].symbol();
                    row.push_str(symbol);
                    x += Span::raw(symbol).width().max(1) as u16;
                }
                row.trim_end().to_string()
            })
            .collect();
        (buf, rows)
    }

    /// Rendered with X-Plane connected at 45 fps.
    fn render(status: &FeatureStatus, symbols: SymbolSet) -> (Buffer, Vec<String>) {
        render_with(
            FeatureStatusWidget::new(status)
                .with_symbols(symbols)
                .with_frame_rate(Some(45)),
        )
    }

    /// Colour of the marker cell at the start of `row`.
    fn marker(buf: &Buffer, row: u16) -> Color {
        buf[(2, row)].fg
    }

    /// Column every line's text starts in: indent, two-column marker, space.
    const TEXT_COLUMN: u16 = 5;

    /// Colour of the first cell of the text after the marker on `row`.
    fn text(buf: &Buffer, row: u16) -> Color {
        buf[(TEXT_COLUMN, row)].fg
    }

    /// Colour of the engine name, after "Tile Processing Engine, ".
    fn engine(buf: &Buffer) -> Color {
        buf[(TEXT_COLUMN + 24, 1)].fg
    }

    /// Colour of the frame rate value, after "X-Plane frame rate: ".
    fn fps_value(buf: &Buffer) -> Color {
        buf[(TEXT_COLUMN + 20, 0)].fg
    }

    fn layer(continent: Continent, enabled: bool, resolved: bool) -> XwpLayer {
        XwpLayer {
            entry: PackEntry {
                line: 1,
                enabled,
                path: format!(
                    "Custom Scenery/X-World-Pro_{}_10-network/",
                    continent.label()
                ),
            },
            continent,
            layer: "10-network".to_string(),
            dir: std::path::PathBuf::from("/xplane/Custom Scenery"),
            resolved,
        }
    }

    #[test]
    fn x_world_pro_status_follows_the_setting_and_the_detection() {
        assert_eq!(
            XworldProStatus::from_detection(false, None),
            XworldProStatus::SupportOff
        );
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&Detection::default())),
            XworldProStatus::NotDetected
        );

        let disabled = Detection {
            layers: vec![layer(Continent::Europe, false, true)],
            ..Detection::default()
        };
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&disabled)),
            XworldProStatus::NoActiveContinents
        );

        let active = Detection {
            layers: vec![
                layer(Continent::Europe, true, true),
                layer(Continent::America, true, true),
                layer(Continent::Asia, true, false),
            ],
            ..Detection::default()
        };
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&active)),
            XworldProStatus::Active(vec!["America", "Europe"]),
            "only enabled layers whose folder exists count"
        );
    }

    #[test]
    fn everything_on_with_emoji_puts_an_emoji_in_place_of_each_feature_mark() {
        let (_, rows) = render(&all_on(), SymbolSet::Emoji);
        assert_eq!(
            rows,
            vec![
                "  ✓  X-Plane frame rate: 45 fps",
                "  🌐 Tile Processing Engine, GPU integrated (AMD Radeon 780M)",
                "  🌙 Night lighting enabled",
                "  🌍 X-World Pro detected: America, Europe",
            ]
        );
    }

    #[test]
    fn everything_on_with_plain_symbols_reads_as_green_checks() {
        let (buf, rows) = render(&all_on(), SymbolSet::Plain);
        assert_eq!(
            rows,
            vec![
                "  ✓  X-Plane frame rate: 45 fps",
                "  ✓  Tile Processing Engine, GPU integrated (AMD Radeon 780M)",
                "  ✓  Night lighting enabled",
                "  ✓  X-World Pro detected: America, Europe",
            ]
        );
        for row in 0..4 {
            assert_eq!(marker(&buf, row), Color::Green, "marker on row {row}");
        }
    }

    #[test]
    fn working_lines_are_grey_text_not_green() {
        for symbols in [SymbolSet::Emoji, SymbolSet::Plain] {
            let (buf, _) = render(&all_on(), symbols);
            for row in 0..4 {
                assert_eq!(text(&buf, row), Color::Gray, "row {row}, {symbols:?}");
            }
        }
    }

    #[test]
    fn text_starts_in_the_same_column_on_every_line_with_either_symbol_set() {
        for symbols in [SymbolSet::Emoji, SymbolSet::Plain] {
            let (buf, _) = render(&all_on(), symbols);
            for (row, first) in [(0, "X"), (1, "T"), (2, "N"), (3, "X")] {
                assert_eq!(buf[(TEXT_COLUMN - 1, row)].symbol(), " ");
                assert_eq!(
                    buf[(TEXT_COLUMN, row)].symbol(),
                    first,
                    "row {row}, {symbols:?}"
                );
            }
        }
    }

    #[test]
    fn frame_rate_mark_and_value_are_coloured_by_how_smooth_the_sim_is() {
        let status = all_on();
        for (fps, glyph, colour) in [
            (31, "✓", Color::Green),
            (30, "!", Color::Yellow),
            (20, "!", Color::Yellow),
            (19, "●", Color::Red),
        ] {
            let (buf, rows) =
                render_with(FeatureStatusWidget::new(&status).with_frame_rate(Some(fps)));
            assert_eq!(rows[0], format!("  {glyph}  X-Plane frame rate: {fps} fps"));
            assert_eq!(marker(&buf, 0), colour, "{fps} fps marker");
            assert_eq!(fps_value(&buf), colour, "{fps} fps value");
            assert_eq!(text(&buf, 0), Color::Gray, "{fps} fps label");
        }
    }

    #[test]
    fn frame_rate_reads_not_connected_without_x_plane() {
        let status = all_on();
        let (buf, rows) = render_with(FeatureStatusWidget::new(&status).with_frame_rate(None));
        assert_eq!(rows[0], "  ✗  X-Plane frame rate: not connected");
        assert_eq!(marker(&buf, 0), Color::DarkGray);
        assert_eq!(text(&buf, 0), Color::DarkGray);
    }

    #[test]
    fn the_engine_name_is_coloured_by_what_does_the_encoding() {
        for (encoder, expected, colour) in [
            (
                EncoderStatus::Software,
                "Tile Processing Engine, Software",
                Color::Red,
            ),
            (
                EncoderStatus::Ispc,
                "Tile Processing Engine, ISPC",
                Color::LightBlue,
            ),
            (
                EncoderStatus::Gpu(radeon()),
                "Tile Processing Engine, GPU integrated (AMD Radeon 780M)",
                Color::Green,
            ),
            (
                EncoderStatus::Gpu(adapter("NVIDIA GeForce RTX 4080", GpuKind::Discrete)),
                "Tile Processing Engine, GPU discrete (NVIDIA GeForce RTX 4080)",
                Color::Yellow,
            ),
            (
                EncoderStatus::Gpu(adapter("virtio-gpu", GpuKind::Virtual)),
                "Tile Processing Engine, GPU virtual (virtio-gpu)",
                Color::Green,
            ),
            (
                EncoderStatus::Gpu(adapter("llvmpipe", GpuKind::Cpu)),
                "Tile Processing Engine, GPU software fallback (llvmpipe)",
                Color::Red,
            ),
        ] {
            let status = FeatureStatus {
                encoder,
                ..all_on()
            };
            let (buf, rows) = render(&status, SymbolSet::Emoji);
            assert_eq!(rows[1], format!("  🌐 {expected}"));
            assert_eq!(engine(&buf), colour, "{expected}");
            assert_eq!(text(&buf, 1), Color::Gray, "{expected}");
        }
    }

    #[test]
    fn the_adapter_is_named_without_the_drivers_additions() {
        let status = FeatureStatus {
            encoder: EncoderStatus::Gpu(adapter(
                "AMD Ryzen 9 9950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO)",
                GpuKind::Integrated,
            )),
            ..all_on()
        };
        let (_, rows) = render(&status, SymbolSet::Plain);
        assert_eq!(
            rows[1],
            "  ✓  Tile Processing Engine, GPU integrated (AMD Ryzen 9 9950X3D)"
        );
    }

    #[test]
    fn gpu_encoding_on_the_sims_own_gpu_is_a_warning_with_either_symbol_set() {
        let status = FeatureStatus {
            encoder: EncoderStatus::GpuSharedWithSim(GpuAdapter {
                name: "Apple M2 Max".to_string(),
                kind: GpuKind::Integrated,
                backend: "metal".to_string(),
            }),
            ..all_on()
        };
        for symbols in [SymbolSet::Emoji, SymbolSet::Plain] {
            let (buf, rows) = render(&status, symbols);
            assert_eq!(
                rows[1],
                "  !  Tile Processing Engine, GPU integrated (Apple M2 Max), shared with X-Plane"
            );
            assert_eq!(marker(&buf, 1), Color::Yellow);
            assert_eq!(engine(&buf), Color::Green);
            // The row is ASCII from the marker on, so its byte offsets are columns.
            let suffix = rows[1].find("shared").unwrap() as u16;
            assert_eq!(buf[(suffix, 1)].fg, Color::Yellow, "{symbols:?}");
        }
    }

    #[test]
    fn night_lighting_that_found_no_installation_is_a_warning() {
        let status = FeatureStatus {
            night: NightLighting::Dark,
            ..all_on()
        };
        let text_line = "Night lighting enabled, but X-Plane was not found: lit textures stay dark";
        let (buf, rows) = render(&status, SymbolSet::Emoji);
        assert_eq!(rows[2], format!("  🌑 {text_line}"));
        assert_eq!(text(&buf, 2), Color::Yellow);

        let (buf, rows) = render(&status, SymbolSet::Plain);
        assert_eq!(rows[2], format!("  !  {text_line}"));
        assert_eq!(marker(&buf, 2), Color::Yellow);
        assert_eq!(text(&buf, 2), Color::Yellow);
    }

    #[test]
    fn night_lighting_off_reads_disabled() {
        let status = FeatureStatus {
            night: NightLighting::Off,
            ..all_on()
        };
        let (buf, rows) = render(&status, SymbolSet::Emoji);
        assert_eq!(rows[2], "  🌑 Night lighting disabled");
        assert_eq!(text(&buf, 2), Color::DarkGray);

        let (buf, rows) = render(&status, SymbolSet::Plain);
        assert_eq!(rows[2], "  ✗  Night lighting disabled");
        assert_eq!(marker(&buf, 2), Color::DarkGray);
        assert_eq!(text(&buf, 2), Color::DarkGray);
    }

    #[test]
    fn x_world_pro_states_without_active_continents_are_dark_grey() {
        for (xworld_pro, expected) in [
            (XworldProStatus::SupportOff, "X-World Pro support off"),
            (XworldProStatus::NotDetected, "X-World Pro not detected"),
            (
                XworldProStatus::NoActiveContinents,
                "X-World Pro installed, no layers enabled",
            ),
        ] {
            let status = FeatureStatus {
                xworld_pro,
                ..all_on()
            };
            let (buf, rows) = render(&status, SymbolSet::Emoji);
            assert_eq!(rows[3], format!("  🌍 {expected}"));
            assert_eq!(text(&buf, 3), Color::DarkGray);

            let (buf, rows) = render(&status, SymbolSet::Plain);
            assert_eq!(rows[3], format!("  ✗  {expected}"));
            assert_eq!(marker(&buf, 3), Color::DarkGray);
            assert_eq!(text(&buf, 3), Color::DarkGray);
        }
    }
}
