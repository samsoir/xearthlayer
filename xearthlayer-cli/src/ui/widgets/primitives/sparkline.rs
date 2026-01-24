//! Sparkline chart primitive.
//!
//! A sparkline is a compact chart showing recent values as a series of
//! vertical bars. Useful for visualizing throughput, rates, or any
//! time-series data in limited space.
//!
//! Note: Some methods are not yet used but are included for reuse.

#![allow(dead_code)] // Reusable primitive - some methods not yet used

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Unicode characters for sparkline visualization (8 height levels).
/// From lowest (▁) to highest (█).
const SPARKLINE_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Default placeholder when no data is available.
const DEFAULT_PLACEHOLDER: &str = "▁▁▁▁▁▁▁▁▁▁▁▁";

/// Rolling history buffer for sparkline data.
///
/// Tracks a series of numeric values and provides methods to generate
/// sparkline visualizations. Automatically manages capacity and oldest
/// value eviction.
#[derive(Debug, Clone)]
pub struct SparklineHistory {
    /// Value samples (most recent last).
    samples: Vec<f64>,
    /// Maximum samples to retain.
    max_samples: usize,
}

impl SparklineHistory {
    /// Create a new history buffer with specified capacity.
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: Vec::with_capacity(max_samples),
            max_samples,
        }
    }

    /// Add a new value to the history.
    ///
    /// If at capacity, the oldest value is removed first.
    pub fn push(&mut self, value: f64) {
        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(value);
    }

    /// Get the most recent value, or 0.0 if empty.
    pub fn current(&self) -> f64 {
        self.samples.last().copied().unwrap_or(0.0)
    }

    /// Get the peak (maximum) value in the history.
    pub fn peak(&self) -> f64 {
        self.samples.iter().cloned().fold(0.0_f64, f64::max)
    }

    /// Get the average value in the history.
    pub fn average(&self) -> f64 {
        if self.samples.is_empty() {
            0.0
        } else {
            self.samples.iter().sum::<f64>() / self.samples.len() as f64
        }
    }

    /// Check if the history is empty.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Get the number of samples in the history.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Clear all samples.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Get a reference to the underlying samples.
    pub fn values(&self) -> &[f64] {
        &self.samples
    }

    /// Create a history from existing values.
    pub fn from_values(values: Vec<f64>) -> Self {
        let max_samples = values.len().max(1);
        Self {
            samples: values,
            max_samples,
        }
    }

    /// Generate a sparkline string from the history.
    ///
    /// The sparkline is normalized against the maximum value in the history,
    /// so values are always relative to the recent peak.
    pub fn to_sparkline(&self) -> String {
        Self::render_sparkline(&self.samples)
    }

    /// Generate a sparkline string from a slice of values.
    ///
    /// This is a static method that can be used without maintaining history.
    pub fn render_sparkline(values: &[f64]) -> String {
        if values.is_empty() {
            return DEFAULT_PLACEHOLDER.to_string();
        }

        let max_val = values.iter().cloned().fold(0.0_f64, f64::max);
        if max_val == 0.0 {
            return values.iter().map(|_| SPARKLINE_CHARS[0]).collect();
        }

        values
            .iter()
            .map(|&val| {
                let normalized = (val / max_val).clamp(0.0, 1.0);
                let index = ((normalized * 7.0).round() as usize).min(7);
                SPARKLINE_CHARS[index]
            })
            .collect()
    }

    /// Generate a sparkline string with a fixed maximum value.
    ///
    /// Use this when you want consistent scaling across multiple sparklines.
    pub fn render_sparkline_with_max(values: &[f64], max_val: f64) -> String {
        if values.is_empty() {
            return DEFAULT_PLACEHOLDER.to_string();
        }

        let max_val = max_val.max(0.001); // Avoid division by zero

        values
            .iter()
            .map(|&val| {
                let normalized = (val / max_val).clamp(0.0, 1.0);
                let index = ((normalized * 7.0).round() as usize).min(7);
                SPARKLINE_CHARS[index]
            })
            .collect()
    }

    /// Generate a sparkline string with fixed width.
    ///
    /// Takes the last `width` samples from values and renders them.
    /// If there are fewer samples than width, pads with spaces on the left.
    /// If values is empty, returns a string of spaces.
    ///
    /// # Arguments
    ///
    /// * `values` - The sample values to render
    /// * `width` - The fixed width of the output string
    ///
    /// # Example
    ///
    /// ```ignore
    /// let sparkline = SparklineHistory::render_sparkline_fixed_width(&[1.0, 2.0, 3.0], 5);
    /// // Returns "  ▁▄█" (2 spaces + 3 chars)
    /// ```
    pub fn render_sparkline_fixed_width(values: &[f64], width: usize) -> String {
        if values.is_empty() {
            return " ".repeat(width);
        }

        // Take the last `width` samples
        let start = values.len().saturating_sub(width);
        let visible = &values[start..];

        // Render the visible portion
        let sparkline = Self::render_sparkline(visible);

        // Pad with spaces if we don't have enough samples
        let sparkline_len = sparkline.chars().count();
        if sparkline_len < width {
            let padding = width - sparkline_len;
            format!("{}{}", " ".repeat(padding), sparkline)
        } else {
            sparkline
        }
    }
}

/// A sparkline chart widget.
///
/// Renders a compact time-series visualization using Unicode block characters.
/// Can be used inline or as a standalone widget.
pub struct Sparkline<'a> {
    /// The sparkline string to render.
    data: &'a str,
    /// Style for the sparkline characters.
    style: Style,
    /// Whether to center the sparkline in the available area.
    centered: bool,
}

impl<'a> Sparkline<'a> {
    /// Create a new sparkline widget from a pre-rendered string.
    pub fn new(data: &'a str) -> Self {
        Self {
            data,
            style: Style::default(),
            centered: false,
        }
    }

    /// Create a sparkline from a history buffer.
    pub fn from_history(history: &SparklineHistory) -> String {
        history.to_sparkline()
    }

    /// Set the style for the sparkline.
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set the foreground color.
    pub fn fg(mut self, color: Color) -> Self {
        self.style = self.style.fg(color);
        self
    }

    /// Center the sparkline in the available width.
    pub fn centered(mut self) -> Self {
        self.centered = true;
        self
    }

    /// Create a styled span from the sparkline data.
    ///
    /// Useful for embedding in other widgets without rendering to a buffer.
    pub fn to_span(&self) -> Span<'a> {
        Span::styled(self.data, self.style)
    }
}

impl Widget for Sparkline<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let content = if self.centered {
            let width = self.data.chars().count();
            let padding = (area.width as usize).saturating_sub(width) / 2;
            format!("{:padding$}{}", "", self.data, padding = padding)
        } else {
            self.data.to_string()
        };

        let line = Line::from(Span::styled(content, self.style));
        Paragraph::new(line).render(Rect { height: 1, ..area }, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparkline_history_push() {
        let mut history = SparklineHistory::new(5);
        history.push(1.0);
        history.push(2.0);
        history.push(3.0);
        assert_eq!(history.len(), 3);
        assert_eq!(history.current(), 3.0);
    }

    #[test]
    fn test_sparkline_history_capacity() {
        let mut history = SparklineHistory::new(3);
        history.push(1.0);
        history.push(2.0);
        history.push(3.0);
        history.push(4.0); // Should evict 1.0
        assert_eq!(history.len(), 3);
        assert_eq!(history.current(), 4.0);
    }

    #[test]
    fn test_sparkline_render_empty() {
        let result = SparklineHistory::render_sparkline(&[]);
        assert_eq!(result, DEFAULT_PLACEHOLDER);
    }

    #[test]
    fn test_sparkline_render_all_zeros() {
        let result = SparklineHistory::render_sparkline(&[0.0, 0.0, 0.0]);
        assert_eq!(result, "▁▁▁");
    }

    #[test]
    fn test_sparkline_render_ascending() {
        let result = SparklineHistory::render_sparkline(&[0.0, 0.5, 1.0]);
        // 0.0 -> index 0 (▁), 0.5 -> index ~4 (▄ or ▅), 1.0 -> index 7 (█)
        assert!(result.starts_with('▁'));
        assert!(result.ends_with('█'));
    }

    #[test]
    fn test_sparkline_peak() {
        let mut history = SparklineHistory::new(10);
        history.push(1.0);
        history.push(5.0);
        history.push(3.0);
        assert_eq!(history.peak(), 5.0);
    }

    #[test]
    fn test_sparkline_average() {
        let mut history = SparklineHistory::new(10);
        history.push(1.0);
        history.push(2.0);
        history.push(3.0);
        assert_eq!(history.average(), 2.0);
    }

    #[test]
    fn test_sparkline_fixed_width_empty() {
        let result = SparklineHistory::render_sparkline_fixed_width(&[], 5);
        assert_eq!(result, "     "); // 5 spaces
    }

    #[test]
    fn test_sparkline_fixed_width_padding() {
        // 3 values with width 5 should pad with 2 spaces
        let result = SparklineHistory::render_sparkline_fixed_width(&[0.0, 0.5, 1.0], 5);
        assert_eq!(result.chars().count(), 5);
        assert!(result.starts_with("  ")); // 2 space padding
    }

    #[test]
    fn test_sparkline_fixed_width_exact() {
        // 3 values with width 3 should have no padding
        let result = SparklineHistory::render_sparkline_fixed_width(&[0.0, 0.5, 1.0], 3);
        assert_eq!(result.chars().count(), 3);
        assert!(!result.starts_with(' ')); // No padding
    }

    #[test]
    fn test_sparkline_fixed_width_truncation() {
        // 5 values with width 3 should take last 3
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let result = SparklineHistory::render_sparkline_fixed_width(&values, 3);
        assert_eq!(result.chars().count(), 3);
        // Last 3 values: 3.0, 4.0, 5.0 normalized to max 5.0
        // 3.0/5.0 = 0.6 -> ~4, 4.0/5.0 = 0.8 -> ~6, 5.0/5.0 = 1.0 -> 7
        assert!(result.ends_with('█')); // 5.0 should be max
    }
}
