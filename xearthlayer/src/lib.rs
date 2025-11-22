//! XEarthLayer - Satellite imagery streaming for X-Plane
//!
//! This library provides the core functionality for streaming satellite imagery
//! to X-Plane flight simulator via a FUSE virtual filesystem.

pub mod coord;

/// Returns a greeting message from XEarthLayer.
///
/// This is a placeholder function demonstrating the library architecture.
/// The actual implementation will be replaced with real functionality.
pub fn greeting() -> String {
    String::from("Hello from XEarthLayer!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greeting_returns_hello_message() {
        let message = greeting();
        assert_eq!(message, "Hello from XEarthLayer!");
    }

    #[test]
    fn test_greeting_is_not_empty() {
        let message = greeting();
        assert!(!message.is_empty(), "Greeting should not be empty");
    }

    #[test]
    fn test_coord_module_exists() {
        // Verify coord module is accessible
        use crate::coord::to_tile_coords;
        let result = to_tile_coords(40.7128, -74.0060, 16);
        assert!(result.is_ok());
    }
}
