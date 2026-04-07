//! Speed-to-extent mapping for the sliding prefetch box.
//!
//! Computes the prefetch box extent in degrees based on aircraft ground speed,
//! using a clamped linear ramp. At low speeds (approach, taxi) the box shrinks
//! to avoid prefetching tiles the aircraft will never reach; at cruise speed
//! the box expands to cover the full lookahead region.
//!
//! # Ramp Formula
//!
//! ```text
//! t      = clamp((speed - min_speed) / (max_speed - min_speed), 0, 1)
//! extent = min_extent + t * (max_extent - min_extent)
//! ```
//!
//! # Design References
//!
//! - Issue #125 — speed-proportional prefetch box sizing

/// Compute the prefetch box extent (degrees) for a given ground speed.
///
/// Returns a value linearly interpolated between `min_extent` and `max_extent`
/// based on where `ground_speed_kt` falls in the `[min_speed_kt, max_speed_kt]`
/// range. The result is clamped so speeds outside the range always return the
/// corresponding boundary extent.
///
/// # Parameters
///
/// - `ground_speed_kt` — Current aircraft ground speed in knots.
/// - `min_speed_kt` — Speed at which the box is at its smallest (e.g. 40 kt).
/// - `max_speed_kt` — Speed at which the box is at its largest (e.g. 450 kt).
/// - `min_extent` — Box extent in degrees at `min_speed_kt` (e.g. 3.5°).
/// - `max_extent` — Box extent in degrees at `max_speed_kt` (e.g. 6.5°).
///
/// # Returns
///
/// Box extent in degrees, clamped to `[min_extent, max_extent]`.
pub fn compute_extent(
    ground_speed_kt: f32,
    min_speed_kt: f32,
    max_speed_kt: f32,
    min_extent: f64,
    max_extent: f64,
) -> f64 {
    let speed_range = max_speed_kt - min_speed_kt;
    let t = if speed_range <= 0.0 {
        1.0_f64
    } else {
        let t_raw = (ground_speed_kt - min_speed_kt) as f64 / speed_range as f64;
        t_raw.clamp(0.0, 1.0)
    };
    min_extent + t * (max_extent - min_extent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_SPEED: f32 = 40.0;
    const MAX_SPEED: f32 = 450.0;
    const MIN_EXTENT: f64 = 3.5;
    const MAX_EXTENT: f64 = 6.5;

    #[test]
    fn at_min_speed_returns_min_extent() {
        let result = compute_extent(MIN_SPEED, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            (result - MIN_EXTENT).abs() < 1e-9,
            "Expected {MIN_EXTENT}, got {result}"
        );
    }

    #[test]
    fn below_min_speed_clamped_to_min_extent() {
        // Taxiing at 5 kt — well below MIN_SPEED
        let result = compute_extent(5.0, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            (result - MIN_EXTENT).abs() < 1e-9,
            "Expected {MIN_EXTENT}, got {result}"
        );
    }

    #[test]
    fn at_max_speed_returns_max_extent() {
        let result = compute_extent(MAX_SPEED, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            (result - MAX_EXTENT).abs() < 1e-9,
            "Expected {MAX_EXTENT}, got {result}"
        );
    }

    #[test]
    fn above_max_speed_clamped_to_max_extent() {
        // Concorde at ~1150 kt
        let result = compute_extent(1150.0, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            (result - MAX_EXTENT).abs() < 1e-9,
            "Expected {MAX_EXTENT} for supersonic, got {result}"
        );
    }

    #[test]
    fn midpoint_speed_returns_midpoint_extent() {
        // Midpoint: (40 + 450) / 2 = 245 kt → extent = (3.5 + 6.5) / 2 = 5.0
        let midpoint_speed = (MIN_SPEED + MAX_SPEED) / 2.0;
        let expected = (MIN_EXTENT + MAX_EXTENT) / 2.0;
        let result = compute_extent(midpoint_speed, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            (result - expected).abs() < 1e-6,
            "Midpoint speed {midpoint_speed} kt: expected extent {expected}, got {result}"
        );
    }

    #[test]
    fn approach_speed_returns_extent_in_range() {
        // Approach at 140 kt — between min and midpoint, expect extent in [4.0, 4.5]
        let result = compute_extent(140.0, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
        assert!(
            result >= 4.0 && result <= 4.5,
            "Approach speed 140 kt: expected extent in [4.0, 4.5], got {result}"
        );
    }

    #[test]
    fn result_always_within_min_max_extent() {
        let speeds = [0.0_f32, 5.0, 40.0, 100.0, 245.0, 450.0, 600.0, 1150.0];
        for &speed in &speeds {
            let result = compute_extent(speed, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT);
            assert!(
                result >= MIN_EXTENT && result <= MAX_EXTENT,
                "Speed {speed} kt produced out-of-range extent {result}"
            );
        }
    }

    #[test]
    fn linear_interpolation_is_monotone() {
        // Higher speed → larger or equal extent
        let speeds = [40.0_f32, 100.0, 200.0, 300.0, 400.0, 450.0];
        let extents: Vec<f64> = speeds
            .iter()
            .map(|&s| compute_extent(s, MIN_SPEED, MAX_SPEED, MIN_EXTENT, MAX_EXTENT))
            .collect();
        for window in extents.windows(2) {
            assert!(
                window[1] >= window[0],
                "Extent not monotone: {:.4} then {:.4}",
                window[0],
                window[1]
            );
        }
    }
}
