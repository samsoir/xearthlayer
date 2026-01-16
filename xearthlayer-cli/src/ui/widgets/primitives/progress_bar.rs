//! Progress bar primitive.
//!
//! A flexible progress bar that supports different visual styles
//! and precision levels using Unicode block characters.
//!
//! Note: Some methods/styles are not yet used but are included for reuse.

#![allow(dead_code)] // Reusable primitive - some methods not yet used
#![allow(clippy::inherent_to_string)] // to_string() is intentional for embedding

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Progress bar visual style.
#[derive(Debug, Clone, Copy, Default)]
pub enum ProgressBarStyle {
    /// Standard block style: █ for filled, ░ for empty.
    #[default]
    Block,
    /// High-precision fractional blocks (8x resolution).
    /// Uses partial block characters: ▏▎▍▌▋▊▉█
    Fractional,
    /// Simple ASCII style: # for filled, - for empty.
    Ascii,
}

/// A progress bar widget.
///
/// Displays a horizontal bar showing progress toward a maximum value.
/// Supports multiple visual styles and color customization.
pub struct ProgressBar {
    /// Current value.
    current: f64,
    /// Maximum value.
    max: f64,
    /// Width in characters.
    width: usize,
    /// Visual style.
    style: ProgressBarStyle,
    /// Foreground style (filled portion).
    filled_style: Style,
    /// Background style (empty portion).
    empty_style: Style,
}

impl ProgressBar {
    /// Create a new progress bar.
    ///
    /// # Arguments
    /// * `current` - Current value
    /// * `max` - Maximum value (100% point)
    /// * `width` - Width in characters
    pub fn new(current: f64, max: f64, width: usize) -> Self {
        Self {
            current,
            max,
            width,
            style: ProgressBarStyle::default(),
            filled_style: Style::default(),
            empty_style: Style::default().fg(Color::DarkGray),
        }
    }

    /// Create from integer values (convenience method).
    pub fn from_usize(current: usize, max: usize, width: usize) -> Self {
        Self::new(current as f64, max as f64, width)
    }

    /// Create from u64 values (convenience method).
    pub fn from_u64(current: u64, max: u64, width: usize) -> Self {
        Self::new(current as f64, max as f64, width)
    }

    /// Set the visual style.
    pub fn bar_style(mut self, style: ProgressBarStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the color for the filled portion.
    pub fn filled_color(mut self, color: Color) -> Self {
        self.filled_style = self.filled_style.fg(color);
        self
    }

    /// Set the full style for the filled portion.
    pub fn filled_style(mut self, style: Style) -> Self {
        self.filled_style = style;
        self
    }

    /// Set the color for the empty portion.
    pub fn empty_color(mut self, color: Color) -> Self {
        self.empty_style = self.empty_style.fg(color);
        self
    }

    /// Set the full style for the empty portion.
    pub fn empty_style(mut self, style: Style) -> Self {
        self.empty_style = style;
        self
    }

    /// Calculate the fill ratio (0.0 to 1.0).
    fn ratio(&self) -> f64 {
        if self.max <= 0.0 {
            if self.current > 0.0 {
                1.0 // If max is 0 but we have data, show as full
            } else {
                0.0
            }
        } else {
            (self.current / self.max).clamp(0.0, 1.0)
        }
    }

    /// Render as a string (useful for embedding in other widgets).
    pub fn to_string(&self) -> String {
        match self.style {
            ProgressBarStyle::Block => self.render_block(),
            ProgressBarStyle::Fractional => self.render_fractional(),
            ProgressBarStyle::Ascii => self.render_ascii(),
        }
    }

    /// Create spans for embedding in other widgets.
    pub fn to_spans(&self) -> Vec<Span<'static>> {
        let ratio = self.ratio();

        match self.style {
            ProgressBarStyle::Block => {
                let filled = (ratio * self.width as f64).round() as usize;
                let empty = self.width.saturating_sub(filled);
                vec![
                    Span::styled("█".repeat(filled), self.filled_style),
                    Span::styled("░".repeat(empty), self.empty_style),
                ]
            }
            ProgressBarStyle::Fractional => {
                // Fractional blocks for sub-character precision
                const BLOCKS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

                let total_eighths = (ratio * (self.width * 8) as f64) as usize;
                let full_blocks = total_eighths / 8;
                let remainder_eighths = total_eighths % 8;

                let mut filled = String::new();
                for _ in 0..full_blocks {
                    filled.push('█');
                }

                let partial_block = if full_blocks < self.width && remainder_eighths > 0 {
                    Some(BLOCKS[remainder_eighths])
                } else {
                    None
                };

                let empty_count =
                    self.width - full_blocks - if partial_block.is_some() { 1 } else { 0 };

                if let Some(partial) = partial_block {
                    filled.push(partial);
                }

                vec![
                    Span::styled(filled, self.filled_style),
                    Span::styled("░".repeat(empty_count), self.empty_style),
                ]
            }
            ProgressBarStyle::Ascii => {
                let filled = (ratio * self.width as f64).round() as usize;
                let empty = self.width.saturating_sub(filled);
                vec![
                    Span::styled("#".repeat(filled), self.filled_style),
                    Span::styled("-".repeat(empty), self.empty_style),
                ]
            }
        }
    }

    fn render_block(&self) -> String {
        let ratio = self.ratio();
        let filled = (ratio * self.width as f64).round() as usize;
        let empty = self.width.saturating_sub(filled);
        format!("{}{}", "█".repeat(filled), "░".repeat(empty))
    }

    fn render_fractional(&self) -> String {
        const BLOCKS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

        let ratio = self.ratio();
        let total_eighths = (ratio * (self.width * 8) as f64) as usize;
        let full_blocks = total_eighths / 8;
        let remainder_eighths = total_eighths % 8;

        let mut result = String::with_capacity(self.width);

        // Add full blocks
        for _ in 0..full_blocks {
            result.push('█');
        }

        // Add partial block if there's a remainder
        if full_blocks < self.width && remainder_eighths > 0 {
            result.push(BLOCKS[remainder_eighths]);
        }

        // Fill remaining with empty blocks
        let chars_used = full_blocks + if remainder_eighths > 0 { 1 } else { 0 };
        for _ in chars_used..self.width {
            result.push('░');
        }

        result
    }

    fn render_ascii(&self) -> String {
        let ratio = self.ratio();
        let filled = (ratio * self.width as f64).round() as usize;
        let empty = self.width.saturating_sub(filled);
        format!("{}{}", "#".repeat(filled), "-".repeat(empty))
    }
}

impl Widget for ProgressBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let spans = self.to_spans();
        let line = Line::from(spans);
        Paragraph::new(line).render(Rect { height: 1, ..area }, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_empty() {
        let bar = ProgressBar::new(0.0, 100.0, 10);
        assert_eq!(bar.to_string(), "░░░░░░░░░░");
    }

    #[test]
    fn test_progress_bar_full() {
        let bar = ProgressBar::new(100.0, 100.0, 10);
        assert_eq!(bar.to_string(), "██████████");
    }

    #[test]
    fn test_progress_bar_half() {
        let bar = ProgressBar::new(50.0, 100.0, 10);
        assert_eq!(bar.to_string(), "█████░░░░░");
    }

    #[test]
    fn test_progress_bar_ascii() {
        let bar = ProgressBar::new(50.0, 100.0, 10).bar_style(ProgressBarStyle::Ascii);
        assert_eq!(bar.to_string(), "#####-----");
    }

    #[test]
    fn test_progress_bar_zero_max() {
        let bar = ProgressBar::new(0.0, 0.0, 10);
        assert_eq!(bar.to_string(), "░░░░░░░░░░");
    }

    #[test]
    fn test_progress_bar_overflow() {
        let bar = ProgressBar::new(150.0, 100.0, 10);
        assert_eq!(bar.to_string(), "██████████");
    }
}
